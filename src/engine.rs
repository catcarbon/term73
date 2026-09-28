//! The radio engine: one background thread owns the rig and does all radio I/O.
//!
//! The UI sends `Job`s over a channel and reads a shared `Snapshot` for its
//! panels; text for the user comes back as `Out` messages. Between jobs the
//! engine services packet traffic: listening, the connected session and the
//! Winlink bridge.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::ax25::{self, Config as AxConfig, State, Station};
use crate::cat;
use crate::config::{self, AppConfig, RadioProfile};
use crate::discover::{self, Discovery};
use crate::gateways::{self, Candidate};
use crate::kiss;
use crate::link::{Link, SerialLink, TcpLink};
use crate::rigctld;
use crate::softmodem::ModemControl;

/// Text for the user: the main pane, or the traffic pane while listening.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Main(String),
    Traffic(String),
    /// Text from the connected station, as it arrives: may end mid-line, continued by the next chunk.
    Remote(String),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Target {
    Serial(String),
    Bluetooth(String),
    Modem73 { kiss: String, control: String },
}

impl Target {
    pub fn key(&self) -> String {
        match self {
            Target::Serial(p) => p.to_ascii_uppercase(),
            Target::Bluetooth(m) => m.to_ascii_uppercase(),
            Target::Modem73 { kiss, .. } => kiss.to_ascii_uppercase(),
        }
    }
    pub fn address(&self) -> String {
        match self {
            Target::Serial(p) | Target::Bluetooth(p) => p.clone(),
            Target::Modem73 { kiss, .. } => kiss.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Packet {
    #[default]
    Idle,
    Ready,
    Listening,
    Connecting,
    Connected,
}

#[derive(Debug, Clone, Default)]
pub struct HeardEntry {
    pub call: String,
    pub count: u32,
    pub last_utc_secs: u64,
    /// Seen sending a frame only AX.25 v2.2 has (SABME, XID or SREJ).
    pub v22: bool,
}

/// What the UI panels show; refreshed by the engine after every change.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub callsign: Option<String>,
    pub model: Option<String>,
    pub address: Option<String>,
    pub profile_saved: bool,
    pub freq_mhz: Option<f64>,
    pub packet: Packet,
    pub listening: bool,
    pub remote: Option<String>,
    pub transmit_allowed: bool,
    pub winlink_ready: bool,
    pub transmitting: bool,
    pub software_modem: bool,
    /// (CTRL band, PTT band) from BC: 0 = A, 1 = B.
    pub bands: Option<(u8, u8)>,
    pub heard: Vec<HeardEntry>,
    /// The connected (or connecting) station, for the Session panel.
    pub session: Option<SessionInfo>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    pub remote: String,
    /// Name of the saved station profile with this callsign, if any.
    pub name: Option<String>,
    pub path: Vec<String>,
    pub connected: bool,
    /// "2.2" or "2.0" once connected.
    pub version: String,
    pub window: usize,
    pub paclen: usize,
    pub since_utc_secs: Option<u64>,
    pub unacked: usize,
    pub stats: ax25::LinkStats,
}

type Reply<T> = Sender<Result<T, String>>;

pub enum Job {
    Open(Target),
    Close,
    Cat(String),
    /// Repeat a read command every 2 s and report which fields change; None stops.
    Watch(Option<String>),
    Ident,
    Tune(f64),
    Power(u8),
    KissOn,
    KissOff,
    Listen(bool, Option<PathBuf>),
    SetTransmit(bool),
    SetCallsign(String),
    SetGrid(String),
    Connect(String, Vec<String>),
    SendLine(String),
    Disconnect,
    BbsConnect(String),
    Gateways(usize),
    WinlinkStart(u16),
    WinlinkStop,
    WinlinkClient(TcpStream, String),
    RigctlStart(u16),
    RigctlStop,
    ModemStatus,
    ModemSet(serde_json::Value),
    Discover(Reply<Discovery>),
    SaveProfile(RadioProfile, Reply<()>),
    CurrentProfile(Reply<(Option<String>, RadioProfile)>),
    Ptt(bool, Reply<bool>),
    FreqHz(Reply<u64>),
    /// Longest time the transmitter may stay keyed before term73 unkeys it (default 60 s).
    PttLimit(Duration),
    /// What a raw CAT line would do when it needs the user's confirmation: None when it can be sent as is.
    CatPreview(String, Reply<Option<String>>),
    /// A raw CAT line the user confirmed after seeing its preview.
    CatConfirmed(String),
    /// A Hamlib rigctld request from the rig-control server.
    Rig(rigctld::Req, Sender<Result<rigctld::Resp, i32>>),
    RigCaps(Sender<rigctld::Caps>),
    /// Stop: end any session (waiting at most this long for the station), leave packet mode, release the rig.
    Shutdown(Duration),
}

pub type Opener = Box<dyn Fn(&Target) -> io::Result<Box<dyn Link>> + Send>;

/// Opens real devices: serial ports, Bluetooth RFCOMM (channel from SDP), modem73 over TCP.
pub fn real_opener() -> Opener {
    Box::new(open_real)
}

/// Open a real device: serial port, Bluetooth RFCOMM (channel from SDP), or modem73 over TCP.
pub fn open_real(t: &Target) -> io::Result<Box<dyn Link>> {
        match t {
            Target::Serial(p) => Ok(Box::new(SerialLink::open(p, 9600)?)),
            Target::Modem73 { kiss, .. } => Ok(Box::new(TcpLink::connect(kiss)?)),
            #[cfg(any(windows, target_os = "linux"))]
            Target::Bluetooth(mac) => {
                let ch = crate::bt::spp_channel(mac)?
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{mac} offers no Serial Port service")))?;
                Ok(Box::new(crate::bt::RfcommLink::connect(mac, ch)?))
            }
            #[cfg(not(any(windows, target_os = "linux")))]
            Target::Bluetooth(_) => Err(io::Error::new(io::ErrorKind::Unsupported,
                "direct Bluetooth is not available here; use the radio's serial port")),
        }
}

pub struct Handle {
    pub jobs: Sender<Job>,
    pub snap: Arc<Mutex<Snapshot>>,
    thread: Option<JoinHandle<()>>,
}

impl Handle {
    pub fn send(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    /// Send a job that answers on a reply channel and wait for the answer.
    pub fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> Job, timeout: Duration) -> Result<T, String> {
        let (tx, rx) = mpsc::channel();
        self.send(make(tx));
        rx.recv_timeout(timeout).map_err(|_| "the radio engine did not answer".to_string())?
    }

    pub fn shutdown(self) {
        self.shutdown_within(Duration::from_secs(10));
    }

    /// Shut down, allowing a connected station at most `wait` to acknowledge the disconnect.
    pub fn shutdown_within(mut self, wait: Duration) {
        self.stop(wait);
    }

    fn stop(&mut self, wait: Duration) {
        if let Some(t) = self.thread.take() {
            self.send(Job::Shutdown(wait));
            let _ = t.join();
        }
    }
}

/// Dropped without a shutdown (for example while a panic unwinds): still release the rig.
impl Drop for Handle {
    fn drop(&mut self) {
        self.stop(Duration::from_secs(3));
    }
}

pub fn spawn(out: Sender<Out>, opener: Opener) -> Handle {
    let (tx, rx) = mpsc::channel();
    let snap = Arc::new(Mutex::new(Snapshot::default()));
    let s2 = snap.clone();
    let jobs = tx.clone();
    let thread = std::thread::spawn(move || Engine::new(out, s2, opener, jobs).run(rx));
    Handle { jobs: tx, snap, thread: Some(thread) }
}

struct BridgeSession {
    sock: TcpStream,
    candidates: Vec<Candidate>,
    idx: usize,
    relaying: bool,
}

struct Engine {
    out: Sender<Out>,
    snap: Arc<Mutex<Snapshot>>,
    opener: Opener,
    self_jobs: Sender<Job>,
    cfg: AppConfig,
    target: Option<Target>,
    radio: Option<Box<dyn Link>>,       // rig-control link (and KISS when the radio's own TNC is used)
    modem: Option<Box<dyn Link>>,       // KISS-over-TCP link when the rig is a software modem
    ctl: Option<ModemControl>,
    model: Option<String>,
    prof: RadioProfile,
    profile_saved: bool,
    band: u8,
    freq: Option<f64>,
    kiss: bool,
    tx_allowed: bool,
    monitor: bool,
    record: Option<File>,
    decoder: kiss::Decoder,
    station: Option<Station>,
    rx_cr: bool,
    /// (CTRL band, PTT band) as last read with BC.
    bands: Option<(u8, u8)>,
    watch: Option<Watch>,
    /// AX.25 version each station used this session (saved BBSes also keep it in their profile).
    ax25_seen: BTreeMap<String, &'static str>,
    /// The current session's version has been recorded.
    ax25_noted: bool,
    /// When the current session connected, and the saved profile it belongs to.
    session_since: Option<u64>,
    session_name: Option<String>,
    closing: bool,
    heard: BTreeMap<String, HeardEntry>,
    bridge_listener: Option<(Arc<std::sync::atomic::AtomicBool>, u16)>,
    bridge: Option<BridgeSession>,
    rigserver: Option<rigctld::Server>,
    keyed_at: Option<Instant>,
    ptt_max: Duration,
    close_wait: Duration,
    last_tx: Option<Instant>,
    /// Last frame heard from the radio, to tell a busy channel.
    last_rx: Option<Instant>,
    /// Packet mode should end once the TNC has sent what it holds.
    leave_kiss_pending: bool,
    /// A band a connect took off a memory channel, put back when packet mode ends.
    memory_restore: Option<gateways::MemorySpot>,
    running: bool,
}

/// APRS channels; term73 will not start a connected session on them.
/// 144.390 North America, 144.800 Europe and Africa, 145.175 Australia, 145.825 the ISS digipeater.
const APRS_MHZ: &[f64] = &[144.390, 144.800, 145.175, 145.825];

/// Read BC: the TM-D750 answers "BC ctrl,ptt"; radios with one value use it for both. 0 = A, 1 = B.
fn read_bands(link: &mut dyn Link) -> Option<(u8, u8)> {
    let r = cat::cat(link, "BC", Duration::from_secs(2), false).ok()?;
    let v: Vec<u8> = r.strip_prefix("BC ")?.split(',').map(|x| x.trim().parse().ok()).collect::<Option<_>>()?;
    match v.as_slice() {
        [both] => Some((*both, *both)),
        [ctrl, ptt] => Some((*ctrl, *ptt)),
        _ => None,
    }
}

pub fn band_name(b: u8) -> &'static str {
    if b == 0 { "A" } else { "B" }
}

/// Time for the TNC to send a queued frame: TX delay plus a 256-byte frame at 1200 baud, with margin.
const TNC_DRAIN: Duration = Duration::from_secs(3);
/// The channel counts as free once nothing has been heard for this long.
const CHANNEL_QUIET: Duration = Duration::from_millis(1500);

fn now_utc_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn hhmmss(secs: u64) -> String {
    let s = secs % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

impl Engine {
    fn new(out: Sender<Out>, snap: Arc<Mutex<Snapshot>>, opener: Opener, self_jobs: Sender<Job>) -> Self {
        let cfg = AppConfig::load();
        let e = Engine {
            out, snap, opener, self_jobs, cfg, target: None, radio: None, modem: None, ctl: None, model: None,
            prof: RadioProfile::default(), profile_saved: false, band: 1, freq: None, kiss: false, tx_allowed: false,
            monitor: false, record: None, decoder: kiss::Decoder::new(), station: None, rx_cr: false, bands: None, watch: None, ax25_seen: BTreeMap::new(), ax25_noted: false, session_since: None, session_name: None, closing: false,
            heard: BTreeMap::new(), bridge_listener: None, bridge: None, rigserver: None, keyed_at: None,
            ptt_max: Duration::from_secs(60), close_wait: Duration::from_secs(10), last_tx: None, last_rx: None, leave_kiss_pending: false, memory_restore: None, running: true,
        };
        e.publish();
        e
    }

    fn say(&self, text: impl Into<String>) {
        let _ = self.out.send(Out::Main(text.into()));
    }

    fn publish(&self) {
        let mut s = self.snap.lock().unwrap();
        s.callsign = self.cfg.callsign.clone();
        s.model = self.model.clone();
        s.address = self.target.as_ref().map(|t| t.address());
        s.profile_saved = self.profile_saved;
        s.freq_mhz = self.freq;
        s.listening = self.monitor;
        s.remote = self.station.as_ref().filter(|st| st.state != State::Disconnected).map(|st| st.remote.clone());
        s.packet = match self.station.as_ref().map(|st| st.state) {
            Some(State::Connected) => Packet::Connected,
            Some(State::Connecting) => Packet::Connecting,
            _ if self.monitor => Packet::Listening,
            _ if self.kiss || self.modem.is_some() => Packet::Ready,
            _ => Packet::Idle,
        };
        s.transmit_allowed = self.tx_allowed;
        s.winlink_ready = self.bridge_listener.is_some();
        s.transmitting = self.keyed_at.is_some() || self.last_tx.is_some_and(|t| t.elapsed() < Duration::from_secs(1));
        s.software_modem = self.modem.is_some();
        s.bands = self.bands;
        let mut heard: Vec<HeardEntry> = self.heard.values().cloned().collect();
        heard.sort_by_key(|h| std::cmp::Reverse(h.last_utc_secs));
        heard.truncate(30);
        s.heard = heard;
        s.session = self.station.as_ref().filter(|st| st.state != State::Disconnected).map(|st| {
            let (window, paclen) = st.window_and_paclen();
            SessionInfo {
                remote: st.remote.clone(),
                name: self.session_name.clone(),
                path: st.path.clone(),
                connected: st.state == State::Connected,
                version: if st.state == State::Connected { st.version().to_string() } else { String::new() },
                window,
                paclen,
                since_utc_secs: self.session_since,
                unacked: st.unacked(),
                stats: st.stats().clone(),
            }
        });
    }

    fn run(mut self, rx: Receiver<Job>) {
        while self.running {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(job) => {
                    if let Err(e) = self.handle(job) {
                        self.say(format!("error: {e}"));
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if self.leave_kiss_pending {
                self.auto_leave_kiss();
            }
            self.poll_watch();
            if let Err(e) = self.service() {
                self.say(format!("error: {e}"));
            }
            if self.keyed_at.is_some_and(|t| t.elapsed() > self.ptt_max) {
                self.say(format!("transmitter held over {} s: unkeying", self.ptt_max.as_secs()));
                let _ = self.ptt(false);
            }
            self.publish();
        }
        self.shutdown();
    }

    // ------------------------------------------------------------ jobs

    fn handle(&mut self, job: Job) -> Result<(), String> {
        match job {
            Job::Open(t) => self.open(t)?,
            Job::Close => {
                self.close_rig();
                self.say("rig disconnected");
            }
            Job::CatPreview(line, reply) => {
                let _ = reply.send(self.cat_preview(&line));
            }
            Job::CatConfirmed(line) => {
                self.need_cat()?;
                let r = cat::cat_confirmed(self.radio.as_deref_mut().unwrap(), &line, Duration::from_secs(2), self.tx_allowed)
                    .map_err(|e| format!("refused: {e}"))?;
                self.say(format!("{} -> {}", line.trim(), if r.is_empty() { "(no reply)" } else { &r }));
            }
            Job::Cat(line) => {
                self.need_cat()?;
                cat::check_allowed(&line, self.tx_allowed).map_err(|e| format!("refused: {e}"))?;
                // a raw TX keys the transmitter like PTT does, so it gets the same time limit
                let name: String = line.trim().chars().take(2).collect::<String>().to_ascii_uppercase();
                if name == "TX" {
                    self.keyed_at.get_or_insert_with(Instant::now);
                }
                let r = cat::cat(self.radio.as_deref_mut().unwrap(), &line, Duration::from_secs(2), self.tx_allowed)
                    .map_err(|e| format!("refused: {e}"))?;
                if name == "RX" {
                    self.keyed_at = None;
                }
                self.say(format!("{} -> {}", line.trim(), if r.is_empty() { "(no reply)" } else { &r }));
            }
            Job::Watch(None) => {
                if self.watch.take().is_some() {
                    self.say("watch stopped");
                }
            }
            Job::Watch(Some(line)) => {
                self.need_cat()?;
                if self.kiss {
                    return Err("the radio is in packet mode; watch needs rig control (end the session or /listen off first)".into());
                }
                if !crate::discover::is_read_form(&line) {
                    return Err("watch only takes a read command, e.g. FO 1, SQ 1, BC or ME 900".into());
                }
                let cmd = line.trim().to_ascii_uppercase();
                self.say(format!("watching {cmd} every 2 s; change one setting at a time on the radio. /advanced watch off stops"));
                self.watch = Some(Watch { cmd, last: None, next: Instant::now() });
            }
            Job::Ident => {
                self.need_cat()?;
                let band = self.band;
                for c in ["ID".to_string(), "FV".into(), format!("FQ {band}"), format!("PC {band}")] {
                    let r = cat::cat(self.radio.as_deref_mut().unwrap(), &c, Duration::from_secs(2), false).unwrap_or_default();
                    self.say(format!("{c} -> {}", if r.is_empty() { "(no reply)" } else { &r }));
                }
            }
            Job::Tune(mhz) => {
                self.tune(mhz)?;
                self.memory_restore = None; // an explicit tune stays in VFO mode
            }
            Job::Power(level) => self.power(level)?,
            Job::KissOn => self.enter_kiss()?,
            Job::KissOff => {
                if self.session_active() {
                    return Err("still connected: /disconnect first".into());
                }
                self.leave_kiss();
                self.say("packet mode off");
            }
            Job::Listen(on, record) => self.set_listen(on, record)?,
            Job::SetTransmit(on) => {
                self.tx_allowed = on;
                self.say(if on { "transmit ALLOWED for this session" } else { "transmit off" });
            }
            Job::SetCallsign(call) => {
                if !config::valid_call(&call) {
                    return Err(format!("{call:?} does not look like a callsign"));
                }
                self.cfg.callsign = Some(call.to_ascii_uppercase());
                self.cfg.save().map_err(|e| e.to_string())?;
                self.say(format!("callsign {} saved", call.to_ascii_uppercase()));
            }
            Job::SetGrid(grid) => {
                if !config::valid_grid(&grid) {
                    return Err(format!("{grid:?} is not a grid locator"));
                }
                let g = format!("{}{}", grid[..4].to_ascii_uppercase(), grid[4..].to_ascii_lowercase());
                self.cfg.grid = Some(g.clone());
                self.cfg.save().map_err(|e| e.to_string())?;
                self.say(format!("grid {g} saved"));
            }
            Job::Connect(call, path) => {
                // a saved BBS brings its own frequency, power and speed
                let saved = config::load_bbs().into_iter()
                    .find(|(name, b)| b.call.eq_ignore_ascii_case(&call) || name.eq_ignore_ascii_case(&call));
                match saved {
                    Some((name, mut b)) => {
                        if !path.is_empty() {
                            b.path = path;
                        }
                        self.say(format!("using saved BBS {name} ({:.3} MHz)", b.mhz));
                        self.bbs_connect_to(b)?;
                    }
                    None => self.connect(&call, &path, None)?,
                }
            }
            Job::SendLine(text) => {
                let st = self.station.as_mut().filter(|s| s.state == State::Connected && self.bridge.is_none())
                    .ok_or("not connected")?;
                let mut data = text.into_bytes();
                data.push(b'\r');
                st.send(&data);
            }
            Job::Disconnect => {
                let st = self.station.as_mut().filter(|s| s.state != State::Disconnected).ok_or("not connected")?;
                if st.state == State::Connecting {
                    st.abort();
                } else {
                    self.closing = true;
                }
            }
            Job::BbsConnect(name) => self.bbs_connect(&name)?,
            Job::Gateways(limit) => {
                let found = self.nearby()?;
                self.say("[ GATEWAYS ]");
                if found.is_empty() {
                    self.say("no packet gateways in range");
                }
                for c in found.iter().take(limit) {
                    self.say(format!("  {:10} {:8.3} MHz {:5} baud {:6.1} km  heard {} h ago", c.call, c.mhz, c.baud, c.km, c.age_h));
                }
            }
            Job::WinlinkStart(port) => self.winlink_start(port)?,
            Job::WinlinkStop => {
                let (flag, _) = self.bridge_listener.take().ok_or("Winlink is not running")?;
                flag.store(false, std::sync::atomic::Ordering::Relaxed);
                self.say("Winlink stopped");
                self.auto_leave_kiss();
            }
            Job::WinlinkClient(sock, who) => self.bridge_client(sock, &who)?,
            Job::RigctlStart(port) => self.rigctl_start(port)?,
            Job::RigctlStop => {
                self.rigserver.take().ok_or("rigctl server is not running")?.stop();
                if self.keyed_at.is_some() {
                    self.ptt(false)?;
                }
                self.say("rigctl server stopped");
            }
            Job::ModemStatus => {
                let st = self.ctl.as_ref().ok_or("no software modem")?.request("get_status", serde_json::json!({}))
                    .map_err(|e| e.to_string())?;
                let keys = ["channel_state", "ptt_on", "rx_frame_count", "tx_frame_count", "last_snr", "ber_ema", "audio_connected"];
                let line: Vec<String> = keys.iter().map(|k| format!("{k} {}", st.get(*k).cloned().unwrap_or_default())).collect();
                self.say(format!("  {}", line.join(", ")));
            }
            Job::ModemSet(fields) => {
                let r = self.ctl.as_ref().ok_or("no software modem")?.request("set_config", fields.clone())
                    .map_err(|e| e.to_string())?;
                self.say(format!("modem set {fields}: {}", if r.get("ok") == Some(&serde_json::json!(true)) { "ok".into() } else { r.to_string() }));
            }
            Job::Discover(reply) => {
                let r = self.run_discovery();
                let _ = reply.send(r);
            }
            Job::SaveProfile(p, reply) => {
                let r = self.save_profile(p);
                let _ = reply.send(r);
            }
            Job::CurrentProfile(reply) => {
                let _ = reply.send(Ok((self.model.clone(), self.prof.clone())));
            }
            Job::Ptt(on, reply) => {
                let r = self.ptt(on);
                let _ = reply.send(r);
            }
            Job::PttLimit(d) => self.ptt_max = d,
            Job::FreqHz(reply) => {
                let r = self.freq_hz();
                let _ = reply.send(r);
            }
            Job::Rig(r, reply) => {
                let a = self.rig_request(r);
                let _ = reply.send(a);
            }
            Job::RigCaps(reply) => {
                let _ = reply.send(self.rig_caps());
            }
            Job::Shutdown(wait) => {
                self.close_wait = wait;
                self.running = false;
            }
        }
        Ok(())
    }

    fn need_rig(&self) -> Result<(), String> {
        if self.radio.is_none() && self.modem.is_none() {
            return Err("no rig: /radio scan, then /radio select <n>".into());
        }
        Ok(())
    }

    fn need_cat(&self) -> Result<(), String> {
        if self.radio.is_none() {
            return Err(if self.modem.is_some() { "this rig has no rig-control link".into() } else {
                "no radio: /radio scan, then /radio select <n>".into()
            });
        }
        if self.kiss {
            return Err("the radio is busy with packet (connection or listening): end that first".into());
        }
        Ok(())
    }

    fn need_tx(&self) -> Result<(), String> {
        if !self.tx_allowed {
            return Err("this transmits: /transmit on first".into());
        }
        if self.cfg.callsign.is_none() {
            return Err("no callsign: /config callsign <CALL>".into());
        }
        Ok(())
    }

    fn session_active(&self) -> bool {
        self.station.as_ref().is_some_and(|s| s.state != State::Disconnected)
    }

    // ------------------------------------------------------------ rig

    fn open(&mut self, t: Target) -> Result<(), String> {
        self.close_rig();
        let link = (self.opener)(&t).map_err(|e| {
            // Windows error 121 on a Bluetooth COM port: the radio did not accept the Bluetooth link
            let hint = if e.raw_os_error() == Some(121) || e.to_string().contains("semaphore timeout") {
                ". The radio did not answer over Bluetooth: check it is on and in range, then turn its Bluetooth off and on \
                 (it may still hold an earlier connection)"
            } else {
                ""
            };
            format!("could not open {}: {e}{hint}", t.address())
        })?;
        let logged = crate::link::WireLog::wrap(link, &config::path("logs"), &t.address());
        let log_path = logged.path.clone();
        let link: Box<dyn Link> = Box::new(logged);
        let key = t.key();
        self.prof = config::load_radios().get(&key).cloned().unwrap_or_default();
        self.profile_saved = config::load_radios().contains_key(&key);
        if let Target::Modem73 { control, .. } = &t {
            self.modem = Some(link);
            let ctl = ModemControl::new(control);
            let cfgv = ctl.request("get_config", serde_json::json!({})).unwrap_or_default();
            self.ctl = Some(ctl);
            self.model = Some("MODEM73".into());
            self.target = Some(t.clone());
            self.decoder = kiss::Decoder::new();
            self.say(format!("rig: modem73 at {} ({} {}, payload {} B); wire log {}", t.address(),
                             cfgv.get("modulation").and_then(|v| v.as_str()).unwrap_or("?"),
                             cfgv.get("code_rate").and_then(|v| v.as_str()).unwrap_or("?"),
                             cfgv.get("payload_size").cloned().unwrap_or_default(), log_path.display()));
            return Ok(());
        }
        self.radio = Some(link);
        self.target = Some(t.clone());
        self.say(format!("radio connected ({}); wire log {}", t.address(), log_path.display()));
        let r = self.radio.as_deref_mut().unwrap();
        cat::kiss_off(r);
        self.kiss = false;
        self.model = gateways::radio_model(r);
        let tn = cat::cat(r, "TN", Duration::from_secs(2), false).unwrap_or_default();
        self.band = self.prof.data_band.unwrap_or_else(|| tn.split(',').nth(1).and_then(|b| b.parse().ok()).unwrap_or(1));
        self.bands = read_bands(r);
        let fq = cat::cat(r, &format!("FQ {}", self.band), Duration::from_secs(2), false).unwrap_or_default();
        self.freq = fq.split(',').nth(1).and_then(|f| f.parse::<f64>().ok()).map(|hz| hz / 1e6);
        self.say(format!("{}, data band {}{}", self.model.as_deref().unwrap_or("unknown radio"),
                         if self.band == 0 { "A" } else { "B" },
                         self.freq.map(|f| format!(" {f:.3} MHz")).unwrap_or_default()));
        Ok(())
    }

    fn close_rig(&mut self) {
        if let Some(st) = self.station.as_mut()
            && st.state != State::Disconnected {
                st.disconnect(Instant::now());
                let end = Instant::now() + self.close_wait;
                while self.session_active() && Instant::now() < end {
                    let _ = self.service();
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        if self.keyed_at.is_some() {
            let _ = self.ptt(false);
        }
        let end = Instant::now() + self.close_wait.min(Duration::from_secs(5));
        while self.kiss && !self.tnc_drained() && Instant::now() < end {
            let _ = self.service();
            std::thread::sleep(Duration::from_millis(20));
        }
        self.leave_kiss();
        self.leave_kiss_pending = false;
        self.put_memory_back();
        self.radio = None;
        self.modem = None;
        self.ctl = None;
        self.station = None;
        self.bridge = None;
        self.model = None;
        self.target = None;
        self.freq = None;
        self.profile_saved = false;
    }

    fn tune(&mut self, mhz: f64) -> Result<(), String> {
        if self.modem.is_some() {
            if self.prof.tune_via_modem != Some(true) {
                return Err("this software-modem rig has no tuning (radio profile: tune through the modem)".into());
            }
            let r = self.ctl.as_ref().ok_or("no software modem")?
                .request("rigctl", serde_json::json!({"command": format!("F {}", (mhz * 1e6).round() as u64)}))
                .map_err(|e| e.to_string())?;
            if !r.get("response").and_then(|v| v.as_str()).unwrap_or("").contains("RPRT 0") {
                return Err(format!("the modem could not tune: {r}"));
            }
            self.freq = Some(mhz);
            self.say(format!("tuned to {mhz:.3} MHz through the modem"));
            return Ok(());
        }
        self.need_rig()?;
        let was = self.kiss;
        self.leave_kiss();
        let shift = self.prof.shift_field.or_else(|| gateways::default_shift_field(self.model.as_deref().unwrap_or("")));
        let band = self.band;
        match gateways::ensure_vfo(self.radio.as_deref_mut().ok_or("no radio")?, band) {
            Ok(Some(spot)) => {
                self.say(format!("band {} was on a memory channel; switched it to VFO mode to tune", band_name(band)));
                self.memory_restore.get_or_insert(spot);
            }
            Ok(None) => {}
            Err(e) => {
                if was {
                    self.enter_kiss()?;
                }
                return Err(e);
            }
        }
        let r = gateways::tune(self.radio.as_deref_mut().ok_or("no radio")?, self.band, mhz, shift);
        if r.is_ok() {
            self.freq = Some(mhz);
            self.say(format!("tuned to {mhz:.3} MHz (FM, simplex)"));
        }
        if was {
            self.enter_kiss()?;
        }
        r
    }

    fn power(&mut self, level: u8) -> Result<(), String> {
        if self.modem.is_some() {
            return Ok(()); // the radio behind the modem keeps its own power setting
        }
        self.need_rig()?;
        let was = self.kiss;
        self.leave_kiss();
        let r = gateways::set_power(self.radio.as_deref_mut().ok_or("no radio")?, self.band, level);
        if r.is_ok() {
            self.say(format!("power level {level}{}", if level == 0 { " (high)" } else { "" }));
        }
        if was {
            self.enter_kiss()?;
        }
        r
    }

    fn enter_kiss(&mut self) -> Result<(), String> {
        if self.modem.is_some() || self.kiss {
            return Ok(());
        }
        let band = self.band;
        let r = cat::cat(self.radio.as_deref_mut().ok_or("no radio")?, &format!("TN 2,{band}"), Duration::from_secs(2), false)
            .unwrap_or_default();
        if r != format!("TN 2,{band}") {
            return Err(format!("the radio did not enter packet mode ({r:?})"));
        }
        self.kiss = true;
        self.decoder = kiss::Decoder::new();
        self.say(format!("packet mode on (data band {})", if band == 0 { "A" } else { "B" }));
        Ok(())
    }

    fn leave_kiss(&mut self) {
        if self.kiss {
            if let Some(r) = self.radio.as_deref_mut() {
                cat::kiss_off(r);
            }
            self.kiss = false;
        }
    }

    fn auto_leave_kiss(&mut self) {
        if !(self.kiss && !self.monitor && self.bridge_listener.is_none() && !self.session_active()) {
            self.leave_kiss_pending = false;
            return;
        }
        if !self.tnc_drained() {
            self.leave_kiss_pending = true; // checked again from the main loop
            return;
        }
        self.leave_kiss_pending = false;
        self.leave_kiss();
        self.say("radio back to normal operation");
        self.put_memory_back();
    }

    /// Return a band that a connect took off a memory channel.
    fn put_memory_back(&mut self) {
        let Some(spot) = self.memory_restore.take() else { return };
        let Some(r) = self.radio.as_deref_mut() else { return };
        match gateways::restore_memory(r, &spot) {
            Ok(()) => self.say(format!("band {} back on memory channel {}", band_name(spot.band),
                                       spot.channel.as_deref().unwrap_or("?"))),
            Err(e) => self.say(format!("warning: {e}")),
        }
    }

    /// True once the TNC has had time to send our last frame: leaving packet mode earlier strands it in
    /// the TNC's buffer (the radio shows STA). A busy channel delays sending, so it must also be quiet.
    fn tnc_drained(&self) -> bool {
        let quiet = |t: Option<Instant>, d: Duration| t.is_none_or(|t| t.elapsed() >= d);
        // the longest wait is bounded: after 20 s the frame is not coming out
        quiet(self.last_tx, Duration::from_secs(20))
            || (quiet(self.last_tx, TNC_DRAIN) && quiet(self.last_rx, CHANNEL_QUIET))
    }

    fn set_speed(&mut self, baud: u32) {
        if self.modem.is_none()
            && let Some(r) = self.radio.as_deref_mut() {
                let _ = r.write(&kiss::frame(kiss::cmd::SETHARDWARE, &[if baud == 9600 { 5 } else { 0 }], 0));
            }
    }

    fn set_listen(&mut self, on: bool, record: Option<PathBuf>) -> Result<(), String> {
        self.need_rig()?;
        if on {
            self.enter_kiss()?;
        }
        self.monitor = on;
        self.record = match (&record, on) {
            (Some(p), true) => Some(File::options().append(true).create(true).open(p).map_err(|e| e.to_string())?),
            _ => None,
        };
        self.say(match (on, &record) {
            (true, Some(p)) => format!("listening on, saving to {}", p.display()),
            (true, None) => "listening on".into(),
            _ => "listening off".into(),
        });
        if !on {
            self.auto_leave_kiss();
        }
        Ok(())
    }

    fn tnc_write(&mut self, data: &[u8]) {
        let w = if self.modem.is_some() { self.modem.as_deref_mut() } else { self.radio.as_deref_mut() };
        if let Some(w) = w {
            let _ = w.write(data);
        }
    }

    // ------------------------------------------------------------ sessions

    /// Link settings for a connection to `remote`: try AX.25 v2.2 unless the station is known to be v2.0.
    fn ax_config(&self, remote: &str) -> AxConfig {
        let known = self.ax25_seen.iter().find(|(c, _)| ax25::same_call(c, remote)).map(|(_, v)| v.to_string())
            .or_else(|| config::load_bbs().into_values().find(|b| ax25::same_call(&b.call, remote)).and_then(|b| b.ax25));
        AxConfig {
            t1: Duration::from_secs_f64(self.prof.t1.unwrap_or(10.0)),
            paclen: self.prof.paclen.unwrap_or(128),
            window: self.prof.window.unwrap_or(4),
            v22: known.as_deref() != Some("2.0"),
            ..AxConfig::default()
        }
    }

    /// Remember which AX.25 version a station used, and store it in matching saved BBS entries.
    fn note_ax25_version(&mut self, remote: &str, version: &'static str) {
        self.ax25_seen.insert(remote.to_ascii_uppercase(), version);
        let mut all = config::load_bbs();
        let mut changed = false;
        for b in all.values_mut().filter(|b| ax25::same_call(&b.call, remote)) {
            if b.ax25.as_deref() != Some(version) {
                b.ax25 = Some(version.to_string());
                changed = true;
            }
        }
        if changed {
            let _ = config::save_bbs(&all);
        }
    }

    /// For a raw CAT line: Err when it is never sent, Some(what it does) when it needs a yes first.
    fn cat_preview(&mut self, line: &str) -> Result<Option<String>, String> {
        self.need_cat()?;
        let e = match cat::check_allowed(line, self.tx_allowed) {
            Ok(()) => return Ok(None),
            Err(e) if !e.confirmable() => return Err(format!("refused: {e}")),
            Err(e) => e,
        };
        if e != cat::Refused::MemoryWrite {
            return Ok(Some(e.to_string()));
        }
        let line = line.trim().to_ascii_uppercase();
        let channel = line.get(3..).and_then(|a| a.split(',').next()).unwrap_or("");
        if channel.len() != 3 || !channel.chars().all(|c| c.is_ascii_digit()) {
            return Err("ME needs a three-digit channel, e.g. ME 054,...".into());
        }
        let current = cat::cat(self.radio.as_deref_mut().unwrap(), &format!("ME {channel}"), Duration::from_secs(2), false)
            .map_err(|e| e.to_string())?;
        Ok(Some(cat::describe_memory_write(&current, &line)))
    }

    fn connect(&mut self, call: &str, path: &[String], attempts: Option<u32>) -> Result<(), String> {
        self.need_rig()?;
        self.need_tx()?;
        if self.session_active() {
            return Err("already connected: /disconnect first".into());
        }
        if let Some(f) = self.freq
            && APRS_MHZ.iter().any(|a| (f - a).abs() < 0.005) {
                return Err(format!("the radio is on {f:.3} MHz, the APRS channel, where connected sessions disrupt APRS: tune elsewhere first (/frequency) or use a saved BBS"));
            }
        self.enter_kiss()?;
        let mut st = Station::new(self.cfg.callsign.as_deref().unwrap_or("N0CALL"), self.ax_config(call));
        self.ax25_noted = false;
        self.session_since = None;
        self.session_name = config::load_bbs().into_iter().find(|(_, b)| ax25::same_call(&b.call, call)).map(|(n, _)| n);
        st.connect(call, path, attempts.or(Some(3)), Instant::now());
        self.station = Some(st);
        self.rx_cr = false;
        self.closing = false;
        let via = if path.is_empty() { String::new() } else { format!(" via {}", path.join(",").to_ascii_uppercase()) };
        self.say(format!("*** connecting to {}{via}", call.to_ascii_uppercase()));
        Ok(())
    }

    fn bbs_connect(&mut self, name: &str) -> Result<(), String> {
        let b = config::load_bbs().get(name).cloned().ok_or(format!("no saved BBS {name:?}: /bbs list"))?;
        self.bbs_connect_to(b)
    }

    fn bbs_connect_to(&mut self, b: config::Bbs) -> Result<(), String> {
        self.need_rig()?;
        self.need_tx()?;
        if self.session_active() {
            return Err("already connected: /disconnect first".into());
        }
        self.tune(b.mhz)?;
        self.power(self.prof.power_high.unwrap_or(0))?;
        self.enter_kiss()?;
        self.set_speed(b.baud);
        self.connect(&b.call, &b.path, None)
    }

    fn service(&mut self) -> Result<(), String> {
        let ready = self.modem.is_some() || (self.radio.is_some() && self.kiss);
        if !ready {
            return Ok(());
        }
        let data = if self.modem.is_some() { self.modem.as_deref_mut().unwrap().read() } else { self.radio.as_deref_mut().unwrap().read() };
        let data = match data {
            Ok(d) => d,
            Err(e) => {
                self.say(format!("error: {e}"));
                self.close_rig();
                return Ok(());
            }
        };
        let now = Instant::now();
        let frames = self.decoder.feed(&data);
        if !frames.is_empty() {
            self.last_rx = Some(now);
        }
        for f in frames {
            if f.is_empty() || f[0] & 0x0F != kiss::cmd::DATA {
                continue;
            }
            let raw = &f[1..];
            self.note_heard(raw);
            let line = ax25::describe(raw);
            if self.monitor {
                let _ = self.out.send(Out::Traffic(format!("[{}] {line}", hhmmss(now_utc_secs()))));
            }
            if let Some(rec) = self.record.as_mut() {
                let hex: String = f.iter().map(|b| format!("{b:02x}")).collect();
                let _ = writeln!(rec, "{}", serde_json::json!({"utc_secs": now_utc_secs(), "raw": hex, "decoded": line}));
            }
            if let Some(st) = self.station.as_mut() {
                st.on_raw(raw, now);
            }
        }
        let Some(st) = self.station.as_mut() else { return Ok(()) };
        st.poll(now);
        let outbox = st.take_outbox();
        let got = st.recv();
        let events = st.take_events();
        let state = st.state;
        let learned = (state == State::Connected && !self.ax25_noted).then(|| (st.remote.clone(), st.version()));
        if let Some((remote, version)) = learned {
            self.ax25_noted = true;
            self.session_since = Some(now_utc_secs());
            self.note_ax25_version(&remote, version);
        }
        for raw in outbox {
            let f = kiss::frame(kiss::cmd::DATA, &raw, 0);
            self.tnc_write(&f);
            self.last_tx = Some(Instant::now());
        }
        let prefix = if self.bridge.is_some() { "winlink: " } else { "*** " };
        for e in events {
            self.say(format!("{prefix}{e}"));
        }
        if !got.is_empty() {
            if let Some(b) = self.bridge.as_mut() {
                if b.sock.write_all(&got).is_err() {
                    self.closing = true;
                }
            } else {
                // a CR LF split across two frames must not count as two line ends
                let skip = usize::from(self.rx_cr && got[0] == b'\n');
                self.rx_cr = got.last() == Some(&b'\r');
                let text = String::from_utf8_lossy(&got[skip..]).replace("\r\n", "\n").replace('\r', "\n");
                if !text.is_empty() {
                    let _ = self.out.send(Out::Remote(text));
                }
            }
        }
        if let Some(b) = self.bridge.as_mut()
            && state == State::Connected {
                b.relaying = true;
                let mut buf = [0u8; 4096];
                match b.sock.read(&mut buf) {
                    Ok(0) => {
                        self.say("winlink: client closed; disconnecting from the gateway");
                        self.closing = true;
                    }
                    Ok(n) => {
                        if let Some(st) = self.station.as_mut() {
                            st.send(&buf[..n]);
                        }
                    }
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                    Err(_) => self.closing = true,
                }
            }
        if self.closing
            && let Some(st) = self.station.as_mut()
                && st.state == State::Connected && st.all_sent() {
                    st.disconnect(now);
                    self.closing = false;
                }
        if state == State::Disconnected {
            self.session_over();
        }
        Ok(())
    }

    fn session_over(&mut self) {
        self.station = None;
        self.closing = false;
        if let Some(mut b) = self.bridge.take() {
            if !b.relaying && b.idx + 1 < b.candidates.len() {
                b.idx += 1;
                self.bridge = Some(b);
                if let Err(e) = self.bridge_try() {
                    self.say(format!("winlink: {e}"));
                }
                return;
            }
            if !b.relaying {
                let _ = b.sock.write_all(b"*** could not connect to any gateway\r");
                self.say("winlink: no gateway answered");
            }
            let _ = b.sock.shutdown(std::net::Shutdown::Both);
        }
        self.say("*** session closed");
        self.auto_leave_kiss();
    }

    fn note_heard(&mut self, raw: &[u8]) {
        if let Some(f) = ax25::parse(raw) {
            let e = self.heard.entry(f.src.clone()).or_insert_with(|| HeardEntry { call: f.src.clone(), ..Default::default() });
            e.count += 1;
            e.last_utc_secs = now_utc_secs();
            e.v22 |= f.pid.is_none() && (matches!(f.ctl & !ax25::PF, ax25::SABME | ax25::XID) || (f.ctl & 3 == 1 && f.ctl & 0x0F == ax25::SREJ));
        }
    }

    // ------------------------------------------------------------ Winlink bridge

    fn nearby(&self) -> Result<Vec<Candidate>, String> {
        let grid = self.cfg.grid.clone().ok_or("no grid locator: /winlink setup")?;
        let list = config::rmslist_path();
        if !list.exists() {
            return Err("no gateway list yet: /winlink setup".into());
        }
        let bands = self.prof.bands_mhz.clone().unwrap_or_else(|| gateways::default_bands(self.model.as_deref().unwrap_or("")));
        gateways::nearby(&list, &grid, &bands, 24.0, 100.0)
    }

    fn winlink_start(&mut self, port: u16) -> Result<(), String> {
        self.need_rig()?;
        self.need_tx()?;
        if self.bridge_listener.is_some() {
            return Err("Winlink is already running".into());
        }
        let candidates = self.nearby()?;
        if candidates.is_empty() {
            return Err("no packet gateways in range".into());
        }
        let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (f2, jobs) = (flag.clone(), self.self_jobs.clone());
        std::thread::spawn(move || {
            while f2.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((sock, _)) => {
                        let jobs = jobs.clone();
                        std::thread::spawn(move || {
                            if let Ok(call) = telnet_login(&sock) {
                                let _ = jobs.send(Job::WinlinkClient(sock, call));
                            }
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        });
        self.bridge_listener = Some((flag, port));
        let names: Vec<String> = candidates.iter().take(3).map(|c| format!("{} {:.3}", c.call, c.mhz)).collect();
        self.say(format!("Winlink ready on 127.0.0.1:{port}; nearest gateways: {}", names.join(", ")));
        self.say(format!("  point your Winlink client's telnet connection at 127.0.0.1:{port}"));
        Ok(())
    }

    fn bridge_client(&mut self, sock: TcpStream, who: &str) -> Result<(), String> {
        let mut sock = sock;
        if self.session_active() {
            let _ = sock.write_all(b"*** the radio is busy with another connection\r");
            return Err("Winlink client refused: another connection is open".into());
        }
        let mut candidates = self.nearby()?;
        candidates.truncate(3);
        sock.set_read_timeout(Some(Duration::from_millis(5))).map_err(|e| e.to_string())?;
        self.say(format!("winlink: client logged in as {who}"));
        self.bridge = Some(BridgeSession { sock, candidates, idx: 0, relaying: false });
        self.bridge_try()
    }

    fn bridge_try(&mut self) -> Result<(), String> {
        loop {
            let (gw, total, idx) = {
                let b = self.bridge.as_ref().ok_or("no Winlink client")?;
                (b.candidates[b.idx].clone(), b.candidates.len(), b.idx)
            };
            self.say(format!("winlink: trying {} on {:.3} MHz ({} km)", gw.call, gw.mhz, gw.km));
            let prepared = self.tune(gw.mhz)
                .and_then(|_| self.power(self.prof.power_high.unwrap_or(0)))
                .and_then(|_| self.enter_kiss());
            match prepared {
                Ok(()) => {
                    self.set_speed(gw.baud);
                    let mut st = Station::new(self.cfg.callsign.as_deref().unwrap_or("N0CALL"), self.ax_config(&gw.call));
                    self.ax25_noted = false;
                    st.connect(&gw.call, &[], Some(3), Instant::now());
                    self.station = Some(st);
                    self.rx_cr = false;
                    return Ok(());
                }
                Err(e) => {
                    self.say(format!("winlink: skipping {}: {e}", gw.call));
                    if idx + 1 >= total {
                        let mut b = self.bridge.take().unwrap();
                        let _ = b.sock.write_all(b"*** could not connect to any gateway\r");
                        return Err("no gateway could be tried".into());
                    }
                    self.bridge.as_mut().unwrap().idx += 1;
                }
            }
        }
    }

    // ------------------------------------------------------------ software modem keying

    fn rigctl_start(&mut self, port: u16) -> Result<(), String> {
        if self.rigserver.is_some() {
            return Err("rigctl server already running".into());
        }
        let (j1, j2) = (Arc::new(Mutex::new(self.self_jobs.clone())), Arc::new(Mutex::new(self.self_jobs.clone())));
        let out = self.out.clone();
        let handler = move |r: rigctld::Req| -> Result<rigctld::Resp, i32> {
            let (tx, rx) = mpsc::channel();
            let _ = j1.lock().unwrap().send(Job::Rig(r, tx));
            rx.recv_timeout(Duration::from_secs(10)).unwrap_or(Err(rigctld::ENAVAIL))
        };
        let caps = move || {
            let (tx, rx) = mpsc::channel();
            let _ = j2.lock().unwrap().send(Job::RigCaps(tx));
            rx.recv_timeout(Duration::from_secs(10)).unwrap_or(rigctld::Caps {
                model: "term73 (no rig)".into(), rx: vec![], tx: vec![], ptt: false, power: false,
            })
        };
        let srv = rigctld::Server::start(port, Arc::new(handler), Arc::new(caps),
                                         Arc::new(move |m| { let _ = out.send(Out::Main(m)); }))
            .map_err(|e| e.to_string())?;
        self.say(format!("rigctl server on 127.0.0.1:{} (Hamlib NET rigctl; modem73 PTT, WSJT-X, Gpredict)", srv.port));
        self.rigserver = Some(srv);
        Ok(())
    }

    fn rig_caps(&self) -> rigctld::Caps {
        let model = self.model.clone().unwrap_or_else(|| "unknown".into());
        let bands = self.prof.bands_mhz.clone().unwrap_or_else(|| gateways::default_bands(&model));
        let hz: Vec<(u64, u64)> = bands.iter().map(|(a, b)| ((a * 1e6).round() as u64, (b * 1e6).round() as u64)).collect();
        rigctld::Caps {
            model: format!("term73 {model}"),
            rx: hz.clone(),
            tx: hz,
            ptt: self.radio.is_some() && self.prof.ptt_verified == Some(true),
            power: self.radio.is_some() && self.modem.is_none(),
        }
    }

    /// One Hamlib request. Rig-control reads are impossible while the radio's TNC is in packet mode,
    /// so the last known value is used then.
    fn rig_request(&mut self, r: rigctld::Req) -> Result<rigctld::Resp, i32> {
        use rigctld::{Req, Resp, ENAVAIL, ERJCTED, EINVAL};
        if self.radio.is_none() && self.modem.is_none() {
            return Err(ENAVAIL);
        }
        let cat_ok = self.radio.is_some() && !self.kiss;
        match r {
            Req::GetFreq if cat_ok => self.freq_hz().map(Resp::Freq).map_err(|_| ENAVAIL),
            Req::GetFreq => self.freq.map(|f| Resp::Freq((f * 1e6).round() as u64)).ok_or(ENAVAIL),
            Req::SetFreq(hz) => {
                if self.session_active() {
                    return Err(ERJCTED);
                }
                if !self.rig_caps().rx.iter().any(|&(lo, hi)| (lo..=hi).contains(&hz)) {
                    self.say(format!("rigctl: {:.3} MHz is outside this rig's bands", hz as f64 / 1e6));
                    return Err(EINVAL);
                }
                self.memory_restore = None;
                self.tune(hz as f64 / 1e6).map(|_| Resp::Done).map_err(|e| {
                    self.say(format!("rigctl: {e}"));
                    EINVAL
                })
            }
            Req::GetMode if self.modem.is_some() => Err(ENAVAIL),
            Req::GetMode if cat_ok => {
                let band = self.band;
                let r = cat::cat(self.radio.as_deref_mut().unwrap(), &format!("MD {band}"), Duration::from_secs(2), false)
                    .unwrap_or_default();
                r.strip_prefix(&format!("MD {band},")).and_then(|c| c.parse().ok()).and_then(rigctld::kenwood_mode)
                    .map(|(m, pb)| Resp::Mode(m, pb)).ok_or(ENAVAIL)
            }
            Req::SetMode(name) => {
                let code = rigctld::kenwood_mode_code(&name).ok_or(ENAVAIL)?;
                if self.modem.is_some() || self.radio.is_none() {
                    return Err(ENAVAIL);
                }
                if self.session_active() || self.kiss {
                    return Err(ERJCTED); // packet mode needs FM; do not change it under a session or /listen
                }
                let band = self.band;
                let r = self.radio.as_deref_mut().unwrap();
                let _ = cat::cat(r, &format!("MD {band},{code}"), Duration::from_secs(2), false);
                let back = cat::cat(r, &format!("MD {band}"), Duration::from_secs(2), false).unwrap_or_default();
                if back == format!("MD {band},{code}") { Ok(Resp::Done) } else { Err(EINVAL) }
            }
            Req::GetMode => Ok(Resp::Mode("FM", 15000)), // term73 only enters packet mode after tuning FM
            Req::GetVfo => Ok(Resp::Band(self.band)),
            Req::GetTones if cat_ok => {
                // FO layout: 6 tone, 7 CTCSS, 8 DCS, 9 cross-tone, 12 tone index, 13 CTCSS index, 14 DCS index on the
                // TM-D750; the TH-D75 has two more fields before them, which its later shift field shows
                let band = self.band;
                let base = self.prof.shift_field.or_else(|| gateways::default_shift_field(self.model.as_deref().unwrap_or("")))
                    .and_then(|s| s.checked_sub(11)).ok_or(ENAVAIL)?;
                let fo = cat::cat(self.radio.as_deref_mut().unwrap(), &format!("FO {band}"), Duration::from_secs(2), false)
                    .unwrap_or_default();
                let f: Vec<usize> = fo.strip_prefix("FO ").unwrap_or("").split(',').map(|v| v.parse().unwrap_or(usize::MAX)).collect();
                let get = |i: usize| f.get(base + i).copied().filter(|&v| v != usize::MAX);
                let flag = |i: usize| get(i).map(|v| v == 1);
                let tone = |i: usize| get(i).and_then(|k| rigctld::KENWOOD_TONES.get(k).copied());
                let tones = rigctld::Tones {
                    tone_on: flag(6).ok_or(ENAVAIL)?,
                    ctcss_on: flag(7).ok_or(ENAVAIL)?,
                    dcs_on: flag(8).ok_or(ENAVAIL)?,
                    cross_on: flag(9).ok_or(ENAVAIL)?,
                    tone: tone(12).ok_or(ENAVAIL)?,
                    ctcss: tone(13).ok_or(ENAVAIL)?,
                    dcs: get(14).and_then(|k| rigctld::DCS_CODES.get(k).copied()).ok_or(ENAVAIL)?,
                };
                Ok(Resp::Tones(tones))
            }
            Req::GetShift | Req::GetOffset if cat_ok => {
                // FO fields: 2 is the offset in Hz; the shift direction's field differs by model (profile)
                let band = self.band;
                let shift = self.prof.shift_field.or_else(|| gateways::default_shift_field(self.model.as_deref().unwrap_or("")));
                let fo = cat::cat(self.radio.as_deref_mut().unwrap(), &format!("FO {band}"), Duration::from_secs(2), false)
                    .unwrap_or_default();
                let fields: Vec<&str> = fo.strip_prefix("FO ").unwrap_or("").split(',').collect();
                if matches!(r, Req::GetOffset) {
                    fields.get(2).and_then(|v| v.parse().ok()).map(Resp::Freq).ok_or(ENAVAIL)
                } else {
                    shift.and_then(|i| fields.get(i)).and_then(|v| v.parse().ok()).and_then(rigctld::kenwood_shift)
                        .map(Resp::Shift).ok_or(ENAVAIL)
                }
            }
            Req::GetPtt => Ok(Resp::Flag(self.keyed_at.is_some())),
            Req::SetPtt(on) => match self.ptt(on) {
                Ok(true) => Ok(Resp::Done),
                _ => Err(ERJCTED),
            },
            Req::GetDcd if cat_ok => {
                let band = self.band;
                let r = cat::cat(self.radio.as_deref_mut().unwrap(), &format!("BY {band}"), Duration::from_secs(2), false)
                    .unwrap_or_default();
                match r.strip_prefix(&format!("BY {band},")) {
                    Some("0") => Ok(Resp::Flag(false)),
                    Some("1") => Ok(Resp::Flag(true)),
                    _ => Err(ENAVAIL),
                }
            }
            Req::GetSquelch if cat_ok => {
                let band = self.band;
                let r = cat::cat(self.radio.as_deref_mut().unwrap(), &format!("SQ {band}"), Duration::from_secs(2), false)
                    .unwrap_or_default();
                r.strip_prefix(&format!("SQ {band},")).and_then(|v| v.parse::<u8>().ok())
                    .map(|n| Resp::Level(n.min(rigctld::SQUELCH_MAX) as f32 / rigctld::SQUELCH_MAX as f32)).ok_or(ENAVAIL)
            }
            Req::SetSquelch(f) if cat_ok => {
                let band = self.band;
                let n = (f * rigctld::SQUELCH_MAX as f32).round() as u8;
                let r = self.radio.as_deref_mut().unwrap();
                let _ = cat::cat(r, &format!("SQ {band},{n}"), Duration::from_secs(2), false);
                let back = cat::cat(r, &format!("SQ {band}"), Duration::from_secs(2), false).unwrap_or_default();
                if back == format!("SQ {band},{n}") { Ok(Resp::Done) } else { Err(EINVAL) }
            }
            Req::GetPower if cat_ok => gateways::get_power(self.radio.as_deref_mut().unwrap(), self.band)
                .map(|l| Resp::Level(rigctld::level_to_fraction(l))).map_err(|_| ENAVAIL),
            Req::SetPower(f) if self.modem.is_none() => {
                if self.session_active() {
                    return Err(ERJCTED);
                }
                self.power(rigctld::fraction_to_level(f)).map(|_| Resp::Done).map_err(|_| EINVAL)
            }
            _ => Err(ENAVAIL),
        }
    }

    fn ptt(&mut self, on: bool) -> Result<bool, String> {
        if !on {
            if !self.kiss
                && let Some(r) = self.radio.as_deref_mut() {
                    let _ = cat::cat(r, "RX", Duration::from_secs(2), true);
                }
            self.keyed_at = None;
            return Ok(true);
        }
        self.need_cat()?;
        if !self.tx_allowed {
            self.say("rigctl: keying refused: /transmit on first");
            return Ok(false);
        }
        if self.prof.ptt_verified != Some(true) {
            self.say("rigctl: keying refused: this radio's TX command is not confirmed to key the data band (radio profile)");
            return Ok(false);
        }
        // TX keys the PTT band, which the front panel can move at any time: check it now
        self.bands = read_bands(self.radio.as_deref_mut().unwrap());
        match self.bands {
            Some((_, ptt)) if ptt == self.band => {}
            Some((_, ptt)) => {
                self.say(format!("rigctl: keying refused: PTT is on band {} but packet uses band {}",
                                 band_name(ptt), band_name(self.band)));
                return Ok(false);
            }
            None => {
                self.say("rigctl: keying refused: cannot read which band PTT is on (BC)");
                return Ok(false);
            }
        }
        let r = cat::cat(self.radio.as_deref_mut().unwrap(), "TX", Duration::from_secs(2), true).unwrap_or_default();
        if !r.starts_with("TX") {
            self.say(format!("rigctl: keying failed ({r:?})"));
            return Ok(false);
        }
        self.keyed_at = Some(Instant::now());
        // "TX b" names the band that keyed; anything but the data band is unkeyed at once
        let keyed = r.strip_prefix("TX ").and_then(|b| b.trim().parse::<u8>().ok());
        if keyed != Some(self.band) {
            let _ = self.ptt(false);
            self.say(format!("rigctl: the radio keyed band {} instead of data band {}; unkeyed",
                             keyed.map(band_name).unwrap_or("?"), band_name(self.band)));
            return Ok(false);
        }
        Ok(true)
    }

    fn freq_hz(&mut self) -> Result<u64, String> {
        self.need_cat()?;
        let band = self.band;
        let r = cat::cat(self.radio.as_deref_mut().unwrap(), &format!("FQ {band}"), Duration::from_secs(2), false).unwrap_or_default();
        r.split(',').nth(1).and_then(|v| v.parse().ok()).ok_or(format!("cannot read frequency ({r:?})"))
    }

    // ------------------------------------------------------------ profiles

    fn run_discovery(&mut self) -> Result<Discovery, String> {
        self.need_cat()?;
        if self.session_active() {
            return Err("still connected: /disconnect first".into());
        }
        let out = self.out.clone();
        let band = self.band;
        let d = discover::discover(self.radio.as_deref_mut().unwrap(), Some(band), &mut |m| { let _ = out.send(Out::Main(m)); });
        cat::kiss_off(self.radio.as_deref_mut().unwrap());
        Ok(d)
    }

    fn save_profile(&mut self, mut p: RadioProfile) -> Result<(), String> {
        let t = self.target.clone().ok_or("no rig")?;
        p.model = self.model.clone();
        p.address = Some(t.address());
        config::save_radio(&t.key(), &p).map_err(|e| e.to_string())?;
        if let Some(b) = p.data_band {
            self.band = b;
        }
        self.prof = p;
        self.profile_saved = true;
        self.say(format!("profile saved for {} {}", self.model.as_deref().unwrap_or("rig"), t.address()));
        Ok(())
    }

    fn shutdown(&mut self) {
        if let Some((flag, _)) = self.bridge_listener.take() {
            flag.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(r) = self.rigserver.take() {
            r.stop();
        }
        self.close_rig();
    }
}

/// If the engine thread panics, never leave the transmitter keyed or the radio in packet mode.
impl Drop for Engine {
    fn drop(&mut self) {
        if self.keyed_at.is_some() {
            let _ = self.ptt(false);
        }
        self.leave_kiss();
    }
}

struct Watch {
    cmd: String,
    last: Option<String>,
    next: Instant,
}

impl Engine {
    fn poll_watch(&mut self) {
        let Some(w) = self.watch.as_mut() else { return };
        if Instant::now() < w.next {
            return;
        }
        w.next = Instant::now() + Duration::from_secs(2);
        let cmd = w.cmd.clone();
        if self.kiss || self.radio.is_none() {
            self.watch = None;
            self.say("watch stopped: rig control is no longer available");
            return;
        }
        let r = cat::cat(self.radio.as_deref_mut().unwrap(), &cmd, Duration::from_secs(2), false).unwrap_or_default();
        let w = self.watch.as_mut().unwrap();
        match w.last.replace(r.clone()) {
            None => {
                let fields: Vec<String> = r.split_once(' ').map(|(_, f)| f).unwrap_or(&r).split(',')
                    .enumerate().map(|(i, v)| format!("{i}={v}")).collect();
                self.say(format!("{cmd} now: {}", if r.is_empty() { "(no reply)".into() } else { fields.join(" ") }));
            }
            Some(prev) if prev != r => {
                let changes = crate::discover::field_changes(&prev, &r);
                self.say(format!("{cmd} changed: {}", changes.join("; ")));
            }
            Some(_) => {}
        }
    }
}

/// Play the Winlink CMS telnet login: prompt for callsign and password, return the callsign.
pub fn telnet_login(sock: &TcpStream) -> io::Result<String> {
    sock.set_nonblocking(false)?; // accepted from a non-blocking listener; Windows inherits that
    let mut s = sock.try_clone()?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut answers = Vec::new();
    for prompt in [&b"Callsign :\r\n"[..], &b"Password :\r\n"[..]] {
        s.write_all(prompt)?;
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if s.read(&mut byte)? == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "client left during login"));
            }
            if byte[0] == b'\r' || byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
        answers.push(String::from_utf8_lossy(&line).trim().to_string());
    }
    Ok(answers.remove(0))
}
