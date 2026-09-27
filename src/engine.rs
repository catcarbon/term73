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
use crate::softmodem::{ModemControl, RigServer};

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
    pub heard: Vec<HeardEntry>,
}

type Reply<T> = Sender<Result<T, String>>;

pub enum Job {
    Open(Target),
    Close,
    Cat(String),
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
    closing: bool,
    heard: BTreeMap<String, HeardEntry>,
    bridge_listener: Option<(Arc<std::sync::atomic::AtomicBool>, u16)>,
    bridge: Option<BridgeSession>,
    rigserver: Option<RigServer>,
    keyed_at: Option<Instant>,
    ptt_max: Duration,
    close_wait: Duration,
    last_tx: Option<Instant>,
    running: bool,
}

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
            monitor: false, record: None, decoder: kiss::Decoder::new(), station: None, rx_cr: false, closing: false,
            heard: BTreeMap::new(), bridge_listener: None, bridge: None, rigserver: None, keyed_at: None,
            ptt_max: Duration::from_secs(60), close_wait: Duration::from_secs(10), last_tx: None, running: true,
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
        let mut heard: Vec<HeardEntry> = self.heard.values().cloned().collect();
        heard.sort_by_key(|h| std::cmp::Reverse(h.last_utc_secs));
        heard.truncate(30);
        s.heard = heard;
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
            Job::Cat(line) => {
                self.need_cat()?;
                let r = cat::cat(self.radio.as_deref_mut().unwrap(), &line, Duration::from_secs(2), self.tx_allowed)
                    .map_err(|e| format!("refused: {e}"))?;
                self.say(format!("{} -> {}", line.trim(), if r.is_empty() { "(no reply)" } else { &r }));
            }
            Job::Ident => {
                self.need_cat()?;
                let band = self.band;
                for c in ["ID".to_string(), "FV".into(), format!("FQ {band}"), format!("PC {band}")] {
                    let r = cat::cat(self.radio.as_deref_mut().unwrap(), &c, Duration::from_secs(2), false).unwrap_or_default();
                    self.say(format!("{c} -> {}", if r.is_empty() { "(no reply)" } else { &r }));
                }
            }
            Job::Tune(mhz) => self.tune(mhz)?,
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
            Job::Connect(call, path) => self.connect(&call, &path, None)?,
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
            Job::FreqHz(reply) => {
                let r = self.freq_hz();
                let _ = reply.send(r);
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
        self.leave_kiss();
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
        if self.kiss && !self.monitor && self.bridge_listener.is_none() && !self.session_active() {
            self.leave_kiss();
            self.say("radio back to normal operation");
        }
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

    fn ax_config(&self) -> AxConfig {
        AxConfig {
            t1: Duration::from_secs_f64(self.prof.t1.unwrap_or(10.0)),
            paclen: self.prof.paclen.unwrap_or(128),
            window: self.prof.window.unwrap_or(4),
            ..AxConfig::default()
        }
    }

    fn connect(&mut self, call: &str, path: &[String], attempts: Option<u32>) -> Result<(), String> {
        self.need_rig()?;
        self.need_tx()?;
        if self.session_active() {
            return Err("already connected: /disconnect first".into());
        }
        self.enter_kiss()?;
        let mut st = Station::new(self.cfg.callsign.as_deref().unwrap_or("N0CALL"), self.ax_config());
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
        self.need_rig()?;
        self.need_tx()?;
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
        for f in self.decoder.feed(&data) {
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
            if let (Some(st), Some(fr)) = (self.station.as_mut(), ax25::parse(raw)) {
                st.on_frame(&fr, now);
            }
        }
        let Some(st) = self.station.as_mut() else { return Ok(()) };
        st.poll(now);
        let outbox = st.take_outbox();
        let got = st.recv();
        let events = st.take_events();
        let state = st.state;
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
                    let mut st = Station::new(self.cfg.callsign.as_deref().unwrap_or("N0CALL"), self.ax_config());
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
        let jobs = Arc::new(Mutex::new(self.self_jobs.clone()));
        let (j1, j2, j3) = (jobs.clone(), jobs.clone(), jobs.clone());
        let out = self.out.clone();
        let call = move |jobs: &Arc<Mutex<Sender<Job>>>, make: Box<dyn FnOnce(Reply<bool>) -> Job>| -> bool {
            let (tx, rx) = mpsc::channel();
            let _ = jobs.lock().unwrap().send(make(tx));
            rx.recv_timeout(Duration::from_secs(10)).ok().and_then(|r| r.ok()).unwrap_or(false)
        };
        let srv = RigServer::start(
            port,
            Arc::new(move |on| call(&j1, Box::new(move |tx| Job::Ptt(on, tx)))),
            Arc::new(move || {
                let (tx, rx) = mpsc::channel();
                let _ = j2.lock().unwrap().send(Job::FreqHz(tx));
                rx.recv_timeout(Duration::from_secs(10)).ok().and_then(|r| r.ok())
            }),
            Arc::new(move |hz| j3.lock().unwrap().send(Job::Tune(hz as f64 / 1e6)).is_ok()),
            Arc::new(move |m| { let _ = out.send(Out::Main(m)); }),
        ).map_err(|e| e.to_string())?;
        self.say(format!("rigctl server on 127.0.0.1:{} (point the modem's rigctl PTT here)", srv.port));
        self.rigserver = Some(srv);
        Ok(())
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
        let r = cat::cat(self.radio.as_deref_mut().unwrap(), "TX", Duration::from_secs(2), true).unwrap_or_default();
        if !r.starts_with("TX") {
            self.say(format!("rigctl: keying failed ({r:?})"));
            return Ok(false);
        }
        self.keyed_at = Some(Instant::now());
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
