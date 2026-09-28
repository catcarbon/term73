//! A server that speaks Hamlib's rigctld network protocol, so programs set to "Hamlib NET rigctl"
//! (WSJT-X, Gpredict, fldigi, modem73's PTT) can control the rig through term73.
//!
//! The wire format follows Hamlib's documented rigctld protocol, including the `\chk_vfo` and
//! `\dump_state` handshake its network client performs on open. No Hamlib code is used.
//! Anything the rig has not been confirmed to support answers "not available" (RPRT -11).

use std::io::{self, BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Hamlib error codes, sent as "RPRT -n".
pub const EINVAL: i32 = 1;
pub const ENIMPL: i32 = 4;
pub const ERJCTED: i32 = 9;
pub const ENAVAIL: i32 = 11;

/// Hamlib bit values used in `\dump_state`.
const MODE_AM: u64 = 1 << 0;
const MODE_FM: u64 = 1 << 5;
const MODE_DSTAR: u64 = 1 << 24;
const LEVEL_RFPOWER: u64 = 1 << 12;
const LEVEL_SQL: u64 = 1 << 5;
const VFO_A: u32 = 1 << 0;
const VFO_B: u32 = 1 << 1;
const PTT_NONE: u32 = 0;
const PTT_RIG: u32 = 1;

#[derive(Debug, Clone, PartialEq)]
pub enum Req {
    GetFreq,
    SetFreq(u64),
    GetMode,
    /// Hamlib mode name, upper case ("FM", "AM").
    SetMode(String),
    GetPtt,
    SetPtt(bool),
    GetDcd,
    GetPower,
    SetPower(f32),
    /// Squelch as a fraction, 0.0 open to 1.0 tightest.
    GetSquelch,
    SetSquelch(f32),
    GetVfo,
    /// Repeater shift direction and offset (read only).
    GetShift,
    GetOffset,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Resp {
    Done,
    Freq(u64),
    Mode(&'static str, u32),
    Flag(bool),
    Level(f32),
    /// Data band: 0 = A, 1 = B.
    Band(u8),
    /// Hamlib repeater shift: "None", "+" or "-".
    Shift(&'static str),
}

/// What the rig can do, reported to clients in `\dump_state`.
#[derive(Debug, Clone)]
pub struct Caps {
    pub model: String,
    /// Receive and transmit ranges in Hz.
    pub rx: Vec<(u64, u64)>,
    pub tx: Vec<(u64, u64)>,
    /// True only when keying the transmitter from rig control is verified for this rig.
    pub ptt: bool,
    pub power: bool,
}

pub type Handler = dyn Fn(Req) -> Result<Resp, i32> + Send + Sync;
pub type CapsFn = dyn Fn() -> Caps + Send + Sync;
type LogFn = dyn Fn(String) + Send + Sync;

pub struct Server {
    pub port: u16,
    running: Arc<AtomicBool>,
}

impl Server {
    /// Listen on `127.0.0.1:port` (0 = any free port). A client that disconnects while keyed is unkeyed.
    pub fn start(port: u16, handler: Arc<Handler>, caps: Arc<CapsFn>, log: Arc<LogFn>) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let running = Arc::new(AtomicBool::new(true));
        let run = running.clone();
        std::thread::spawn(move || {
            while run.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((conn, peer)) => {
                        log(format!("rigctl: client {peer} connected"));
                        let (h, c, l) = (handler.clone(), caps.clone(), log.clone());
                        std::thread::spawn(move || serve(conn, &*h, &*c, &*l));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
        });
        Ok(Server { port, running })
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

fn rprt(code: i32) -> String {
    format!("RPRT {}\n", if code == 0 { 0 } else { -code.abs() })
}

/// Kenwood `MD` mode codes on the TM-D750 and TH-D75: 0 FM, 1 DV, 2 AM, 4 DR (DV via a repeater); 3 is not known.
/// Returns the Hamlib mode name and a nominal passband in Hz.
pub fn kenwood_mode(code: u8) -> Option<(&'static str, u32)> {
    match code {
        0 => Some(("FM", 15000)),
        1 | 4 => Some(("D-STAR", 6250)),
        2 => Some(("AM", 10000)),
        _ => None,
    }
}

/// The `MD` code to set for a Hamlib mode name. Only FM and AM are offered: DV and DR need D-STAR settings.
pub fn kenwood_mode_code(name: &str) -> Option<u8> {
    match name {
        "FM" => Some(0),
        "AM" => Some(2),
        _ => None,
    }
}

/// Kenwood `FO` repeater-shift codes: 0 none, 1 plus, 2 minus.
pub fn kenwood_shift(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("None"),
        1 => Some("+"),
        2 => Some("-"),
        _ => None,
    }
}

/// Kenwood `SQ` squelch steps on the TM-D750: 0 open to 31 tightest.
pub const SQUELCH_MAX: u8 = 31;

/// Power levels are reported as fractions of full power: high 1.0, mid 0.5, low 0.2 (coarse; the radio has three steps).
pub fn level_to_fraction(level: u8) -> f32 {
    match level {
        0 => 1.0,
        1 => 0.5,
        _ => 0.2,
    }
}

pub fn fraction_to_level(f: f32) -> u8 {
    if f > 0.75 { 0 } else if f > 0.35 { 1 } else { 2 }
}

/// Answer one command line. Returns None for quit. `keyed` tracks PTT set by this client.
pub fn answer(line: &str, handler: &Handler, caps: &CapsFn, keyed: &mut bool) -> Option<String> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    let cmd = parts.first().copied().unwrap_or("");
    let arg = |i: usize| parts.get(i).copied();
    let reply = match cmd {
        "" => return Some(String::new()),
        "q" | "Q" | "\\quit" => return None,
        "\\chk_vfo" => "0\n".into(),
        // Hamlib's network client asks this before every set_mode and reads the value and then a
        // report line; without the report line it waits out its timeout (about 20 s)
        "\\get_lock_mode" => format!("0\n{}", rprt(0)),
        "\\dump_state" => dump_state(&caps()),
        "\\get_powerstat" => "1\n".into(),
        "_" | "\\get_info" => format!("{}\n", caps().model),
        "f" | "\\get_freq" => match handler(Req::GetFreq) {
            Ok(Resp::Freq(hz)) => format!("{hz}\n"),
            Ok(_) => rprt(ENIMPL),
            Err(e) => rprt(e),
        },
        "F" | "\\set_freq" => match arg(1).and_then(|v| v.parse::<f64>().ok()) {
            Some(hz) if hz > 0.0 => match handler(Req::SetFreq(hz.round() as u64)) {
                Ok(_) => rprt(0),
                Err(e) => rprt(e),
            },
            _ => rprt(EINVAL),
        },
        "m" | "\\get_mode" => match handler(Req::GetMode) {
            Ok(Resp::Mode(m, pb)) => format!("{m}\n{pb}\n"),
            Ok(_) => rprt(ENIMPL),
            Err(e) => rprt(e),
        },
        "M" | "\\set_mode" => match arg(1) {
            Some(want) => match handler(Req::SetMode(want.to_ascii_uppercase())) {
                Ok(_) => rprt(0),
                Err(e) => rprt(e),
            },
            None => rprt(EINVAL),
        },
        // Clients control only the radio's data band, which is always presented as VFOA: Hamlib's
        // client queries VFOA by default and stalls on set_mode when the current VFO is another one.
        "v" | "\\get_vfo" => "VFOA\n".into(),
        "V" | "\\set_vfo" => match arg(1) {
            Some(v) if ["VFOA", "currVFO", "Main"].iter().any(|n| v.eq_ignore_ascii_case(n)) => rprt(0),
            Some(_) => rprt(ENAVAIL),
            None => rprt(EINVAL),
        },
        "s" | "\\get_split_vfo" => "0\nVFOA\n".into(),
        "r" | "\\get_rptr_shift" => match handler(Req::GetShift) {
            Ok(Resp::Shift(s)) => format!("{s}\n"),
            Ok(_) => rprt(ENIMPL),
            Err(e) => rprt(e),
        },
        "o" | "\\get_rptr_offs" => match handler(Req::GetOffset) {
            Ok(Resp::Freq(hz)) => format!("{hz}\n"),
            Ok(_) => rprt(ENIMPL),
            Err(e) => rprt(e),
        },
        "S" | "\\set_split_vfo" => if arg(1) == Some("0") { rprt(0) } else { rprt(ENAVAIL) },
        "t" | "\\get_ptt" => match handler(Req::GetPtt) {
            Ok(Resp::Flag(on)) => format!("{}\n", u8::from(on)),
            _ => format!("{}\n", u8::from(*keyed)),
        },
        "T" | "\\set_ptt" => match arg(1) {
            Some(v) => {
                let on = v != "0";
                match handler(Req::SetPtt(on)) {
                    Ok(_) => {
                        *keyed = on;
                        rprt(0)
                    }
                    Err(e) => rprt(e),
                }
            }
            None => rprt(EINVAL),
        },
        "\\get_dcd" | "\u{8b}" => match handler(Req::GetDcd) {
            Ok(Resp::Flag(on)) => format!("{}\n", u8::from(on)),
            Ok(_) => rprt(ENIMPL),
            Err(e) => rprt(e),
        },
        "l" | "\\get_level" if arg(1).is_some_and(|l| l.eq_ignore_ascii_case("SQL")) => match handler(Req::GetSquelch) {
            Ok(Resp::Level(f)) => format!("{f:.6}\n"),
            Ok(_) => rprt(ENIMPL),
            Err(e) => rprt(e),
        },
        "L" | "\\set_level" if arg(1).is_some_and(|l| l.eq_ignore_ascii_case("SQL")) => {
            match arg(2).and_then(|v| v.parse::<f32>().ok()).filter(|v| (0.0..=1.0).contains(v)) {
                Some(v) => match handler(Req::SetSquelch(v)) {
                    Ok(_) => rprt(0),
                    Err(e) => rprt(e),
                },
                None => rprt(EINVAL),
            }
        }
        "l" | "\\get_level" => match arg(1) {
            Some(l) if l.eq_ignore_ascii_case("RFPOWER") => match handler(Req::GetPower) {
                Ok(Resp::Level(f)) => format!("{f:.6}\n"),
                Ok(_) => rprt(ENIMPL),
                Err(e) => rprt(e),
            },
            Some(_) => rprt(ENAVAIL),
            None => rprt(EINVAL),
        },
        "L" | "\\set_level" => match (arg(1), arg(2).and_then(|v| v.parse::<f32>().ok())) {
            (Some(l), Some(v)) if l.eq_ignore_ascii_case("RFPOWER") => match handler(Req::SetPower(v)) {
                Ok(_) => rprt(0),
                Err(e) => rprt(e),
            },
            (Some(_), Some(_)) => rprt(ENAVAIL),
            _ => rprt(EINVAL),
        },
        _ => rprt(ENAVAIL),
    };
    Some(reply)
}

/// The capability block Hamlib's network client reads on open (rigctld protocol version 1).
pub fn dump_state(c: &Caps) -> String {
    let mut s = String::new();
    let range = |s: &mut String, (a, b): (u64, u64), modes: u64, high_mw: i32| {
        s.push_str(&format!("{a} {b} 0x{modes:x} {} {high_mw} 0x{:x} 0x0\n", if high_mw < 0 { -1 } else { 1000 }, VFO_A | VFO_B));
    };
    s.push_str("1\n2\n0\n"); // protocol 1, model 2 (NET rigctl), region
    for r in &c.rx {
        range(&mut s, *r, MODE_FM | MODE_AM | MODE_DSTAR, -1);
    }
    s.push_str("0 0 0 0 0 0 0\n");
    for r in &c.tx {
        range(&mut s, *r, MODE_FM, 50_000);
    }
    s.push_str("0 0 0 0 0 0 0\n");
    for step in [5_000, 6_250, 10_000, 12_500, 25_000] {
        s.push_str(&format!("0x{:x} {step}\n", MODE_FM | MODE_AM));
    }
    s.push_str("0 0\n");
    s.push_str(&format!("0x{MODE_FM:x} 15000\n0x{MODE_AM:x} 10000\n0 0\n"));
    s.push_str("0\n0\n0\n0\n\n\n"); // max RIT, XIT, IF shift, announces; no preamp; no attenuator
    let levels = if c.power { LEVEL_RFPOWER | LEVEL_SQL } else { 0 };
    s.push_str(&format!("0x0\n0x0\n0x{levels:x}\n0x{levels:x}\n0x0\n0x0\n"));
    s.push_str(&format!("vfo_ops=0x0\nptt_type=0x{:x}\ntargetable_vfo=0x0\n", if c.ptt { PTT_RIG } else { PTT_NONE }));
    s.push_str("has_set_vfo=0\nhas_get_vfo=1\nhas_set_freq=1\nhas_get_freq=1\nhas_set_conf=0\nhas_get_conf=0\n");
    s.push_str("has_power2mW=0\nhas_mW2power=0\nhas_get_ant=0\nhas_set_ant=0\ntimeout=2000\nrig_model=2\n");
    s.push_str(&format!("rigctld_version=term73 {}\ndone\n", env!("CARGO_PKG_VERSION")));
    s
}

fn serve(conn: TcpStream, handler: &Handler, caps: &CapsFn, log: &LogFn) {
    let _ = conn.set_nonblocking(false);
    let mut out = match conn.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut keyed = false;
    for line in BufReader::new(conn).lines() {
        let Ok(line) = line else { break };
        let Some(reply) = answer(&line, handler, caps, &mut keyed) else {
            let _ = out.write_all(rprt(0).as_bytes()); // the client reads a reply to quit
            break;
        };
        if out.write_all(reply.as_bytes()).is_err() {
            break;
        }
    }
    if keyed {
        log("rigctl: client left while keyed; unkeying".into());
        let _ = handler(Req::SetPtt(false));
    }
    let _ = out.shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn caps() -> Caps {
        Caps { model: "TM-D750".into(), rx: vec![(118_000_000, 174_000_000)], tx: vec![(144_000_000, 148_000_000)], ptt: false, power: true }
    }

    #[test]
    fn handshake_matches_the_network_client() {
        let d = dump_state(&caps());
        let lines: Vec<&str> = d.lines().collect();
        assert_eq!(&lines[..3], ["1", "2", "0"]);
        assert_eq!(lines[3], "118000000 174000000 0x1000021 -1 -1 0x3 0x0");
        assert_eq!(lines[4], "0 0 0 0 0 0 0");
        assert_eq!(lines[5], "144000000 148000000 0x20 1000 50000 0x3 0x0");
        assert!(d.contains("\n0 0\n0x20 15000\n0x1 10000\n0 0\n0\n0\n0\n0\n\n\n0x0\n0x0\n0x1020\n0x1020\n0x0\n0x0\nvfo_ops=0x0\nptt_type=0x0\n"), "{d}");
        assert!(d.ends_with("done\n"));
    }

    #[test]
    fn commands_and_errors() {
        let freq = Mutex::new(145_030_000u64);
        let h = move |r: Req| -> Result<Resp, i32> {
            match r {
                Req::GetFreq => Ok(Resp::Freq(*freq.lock().unwrap())),
                Req::SetFreq(hz) if (144_000_000..148_000_000).contains(&hz) => {
                    *freq.lock().unwrap() = hz;
                    Ok(Resp::Done)
                }
                Req::SetFreq(_) => Err(EINVAL),
                Req::GetMode => Ok(Resp::Mode("FM", 15000)),
                Req::SetMode(m) if m == "FM" => Ok(Resp::Done),
                Req::SetMode(_) => Err(ENAVAIL),
                Req::GetVfo => Ok(Resp::Band(1)),
                Req::GetPower => Ok(Resp::Level(level_to_fraction(1))),
                Req::SetPtt(_) => Err(ERJCTED),
                _ => Err(ENAVAIL),
            }
        };
        let c = || caps();
        let mut k = false;
        let mut ask = |l: &str| answer(l, &h, &c, &mut k).unwrap();
        assert_eq!(ask("f"), "145030000\n");
        assert_eq!(ask("F 145090000"), "RPRT 0\n");
        assert_eq!(ask("\\get_freq"), "145090000\n");
        assert_eq!(ask("F 7074000"), "RPRT -1\n");
        assert_eq!(ask("m"), "FM\n15000\n");
        assert_eq!(ask("M FM 15000"), "RPRT 0\n");
        assert_eq!(ask("M USB 2400"), "RPRT -11\n");
        assert_eq!(kenwood_mode(2), Some(("AM", 10000)));
        assert_eq!(kenwood_mode(4).map(|m| m.0), Some("D-STAR"));
        assert_eq!(kenwood_mode(3), None);
        assert_eq!((kenwood_mode_code("AM"), kenwood_mode_code("D-STAR")), (Some(2), None));
        assert_eq!(ask("v"), "VFOA\n");
        assert_eq!(ask("l RFPOWER"), "0.500000\n");
        assert_eq!(ask("l SQL"), "RPRT -11\n", "the test handler has no squelch");
        assert_eq!(ask("L SQL 1.5"), "RPRT -1\n");
        assert_eq!(ask("T 1"), "RPRT -9\n");
        assert_eq!(ask("t"), "0\n");
        assert_eq!(ask("\\chk_vfo"), "0\n");
        assert_eq!(ask("\\something_new"), "RPRT -11\n");
        assert!(answer("q", &h, &c, &mut k).is_none());
    }

    #[test]
    fn unkeys_when_a_keyed_client_leaves() {
        let keyed = Arc::new(Mutex::new(Vec::<bool>::new()));
        let k = keyed.clone();
        let srv = Server::start(0, Arc::new(move |r| {
            if let Req::SetPtt(on) = r {
                k.lock().unwrap().push(on);
            }
            Ok(Resp::Done)
        }), Arc::new(caps), Arc::new(|_| {})).unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
        c.write_all(b"T 1\n").unwrap();
        let mut buf = [0u8; 16];
        let n = std::io::Read::read(&mut c, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"RPRT 0\n");
        drop(c);
        let end = std::time::Instant::now() + Duration::from_secs(3);
        while keyed.lock().unwrap().len() < 2 && std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(*keyed.lock().unwrap(), vec![true, false]);
    }
}
