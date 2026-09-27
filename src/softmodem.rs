//! Software modems (modem73 and any KISS-over-TCP TNC).
//!
//! `ModemControl` talks to modem73's JSON control port (4-byte big-endian
//! length + JSON). `RigServer` is a small rigctld subset so a modem can key a
//! radio through term73's rig-control link.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

#[derive(Clone)]
pub struct ModemControl {
    pub addr: String,
}

impl ModemControl {
    pub fn new(addr: &str) -> Self {
        ModemControl { addr: addr.to_string() }
    }

    pub fn request(&self, cmd: &str, fields: Value) -> io::Result<Value> {
        let sock = self.addr.parse().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?;
        let mut s = TcpStream::connect_timeout(&sock, Duration::from_secs(5))?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut body = if fields.is_object() { fields } else { json!({}) };
        body["cmd"] = json!(cmd);
        let bytes = serde_json::to_vec(&body)?;
        s.write_all(&(bytes.len() as u32).to_be_bytes())?;
        s.write_all(&bytes)?;
        let mut head = [0u8; 4];
        s.read_exact(&mut head)?;
        let mut resp = vec![0u8; u32::from_be_bytes(head) as usize];
        s.read_exact(&mut resp)?;
        Ok(serde_json::from_slice(&resp)?)
    }
}

type PttFn = dyn Fn(bool) -> bool + Send + Sync;
type FreqFn = dyn Fn() -> Option<u64> + Send + Sync;
type SetFreqFn = dyn Fn(u64) -> bool + Send + Sync;
type LogFn = dyn Fn(String) + Send + Sync;

pub struct RigServer {
    pub port: u16,
    running: Arc<AtomicBool>,
}

impl RigServer {
    /// Serve T/t (PTT), F/f (frequency), m (mode) and q on `127.0.0.1:port` (0 = any free port).
    /// A client that disconnects while keyed is unkeyed.
    pub fn start(port: u16, ptt: Arc<PttFn>, get_freq: Arc<FreqFn>, set_freq: Arc<SetFreqFn>, log: Arc<LogFn>)
        -> io::Result<Self> {
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
                        let (ptt, gf, sf, log) = (ptt.clone(), get_freq.clone(), set_freq.clone(), log.clone());
                        std::thread::spawn(move || serve(conn, &*ptt, &*gf, &*sf, &*log));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
        });
        Ok(RigServer { port, running })
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl Drop for RigServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve(conn: TcpStream, ptt: &PttFn, get_freq: &FreqFn, set_freq: &SetFreqFn, log: &LogFn) {
    let _ = conn.set_nonblocking(false);
    let mut out = match conn.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut keyed = false;
    for line in BufReader::new(conn).lines() {
        let Ok(line) = line else { break };
        let parts: Vec<&str> = line.split_whitespace().collect();
        let reply = match parts.as_slice() {
            [] => continue,
            ["T" | "\\set_ptt", v, ..] => {
                let on = *v != "0";
                if ptt(on) {
                    keyed = on;
                    "RPRT 0\n".to_string()
                } else {
                    "RPRT -9\n".to_string()
                }
            }
            ["t" | "\\get_ptt", ..] => format!("{}\n", u8::from(keyed)),
            ["f" | "\\get_freq", ..] => format!("{}\n", get_freq().unwrap_or(0)),
            ["F" | "\\set_freq", hz, ..] => match hz.parse::<f64>() {
                Ok(v) if set_freq(v as u64) => "RPRT 0\n".to_string(),
                _ => "RPRT -9\n".to_string(),
            },
            ["m" | "\\get_mode", ..] => "FM\n15000\n".to_string(),
            ["q" | "Q", ..] => break,
            _ => "RPRT -11\n".to_string(),
        };
        if out.write_all(reply.as_bytes()).is_err() {
            break;
        }
    }
    if keyed {
        log("rigctl: client left while keyed; unkeying".into());
        ptt(false);
    }
    let _ = out.shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn rigctl_ptt_and_unkey_on_disconnect() {
        let keyed = Arc::new(Mutex::new(Vec::<bool>::new()));
        let k = keyed.clone();
        let srv = RigServer::start(0, Arc::new(move |on| { k.lock().unwrap().push(on); true }),
                                   Arc::new(|| Some(145_030_000)), Arc::new(|_| true), Arc::new(|_| {})).unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
        let mut ask = |line: &str| {
            c.write_all(format!("{line}\n").as_bytes()).unwrap();
            let mut buf = [0u8; 64];
            let n = c.read(&mut buf).unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        };
        assert_eq!(ask("T 1"), "RPRT 0\n");
        assert_eq!(ask("t"), "1\n");
        assert_eq!(ask("f"), "145030000\n");
        assert_eq!(ask("x"), "RPRT -11\n");
        drop(c);
        for _ in 0..100 {
            if keyed.lock().unwrap().len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(*keyed.lock().unwrap(), vec![true, false]);
    }
}
