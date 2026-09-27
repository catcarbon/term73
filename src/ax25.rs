//! AX.25 frames and a connected-mode link state machine: v2.0 (modulo 8), and v2.2 (modulo 128)
//! when the other station accepts it. A connect tries v2.2 first (SABME) and falls back to v2.0
//! (SABM) on DM, FRMR or no answer, as v2.0 stations answer SABME in all three ways.
//!
//! `Station` is pure: it never touches I/O. Feed it received frames with
//! `on_frame`, call `poll` with the current time, and send whatever it puts
//! in its outbox (raw AX.25 frames; wrap them in KISS to transmit).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub const SABM: u8 = 0x2F;
pub const UA: u8 = 0x63;
pub const DISC: u8 = 0x43;
pub const DM: u8 = 0x0F;
pub const UI: u8 = 0x03;
pub const FRMR: u8 = 0x87;
pub const RR: u8 = 0x01;
pub const RNR: u8 = 0x05;
pub const REJ: u8 = 0x09;
pub const PF: u8 = 0x10;
/// AX.25 v2.2 only: extended (modulo 128) connect, capability exchange, selective reject.
pub const SABME: u8 = 0x6F;
pub const XID: u8 = 0xAF;
pub const SREJ: u8 = 0x0D;
pub const TEST: u8 = 0xE3;
pub const PID_NONE: u8 = 0xF0;

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub dst: String,
    pub src: String,
    /// Digipeaters; `true` when the has-been-repeated bit is set.
    pub path: Vec<(String, bool)>,
    pub ctl: u8,
    /// Second control byte of an I or S frame on a modulo-128 (v2.2) link.
    pub ctl2: Option<u8>,
    pub command: bool,
    pub pid: Option<u8>,
    pub info: Vec<u8>,
}

#[derive(Debug, PartialEq)]
pub struct BadCall(pub String);

fn split_call(call: &str) -> Result<(String, u8), BadCall> {
    let up = call.trim().to_ascii_uppercase();
    let (base, ssid) = match up.split_once('-') {
        Some((b, s)) => (b.to_string(), s.parse::<u8>().map_err(|_| BadCall(call.into()))?),
        None => (up.clone(), 0),
    };
    if base.is_empty() || base.len() > 6 || !base.chars().all(|c| c.is_ascii_alphanumeric()) || ssid > 15 {
        return Err(BadCall(call.into()));
    }
    Ok((base, ssid))
}

pub fn encode_addr(call: &str, high_bit: bool, last: bool) -> Result<[u8; 7], BadCall> {
    let (base, ssid) = split_call(call)?;
    let mut out = [b' ' << 1; 7];
    for (i, c) in base.bytes().enumerate() {
        out[i] = c << 1;
    }
    out[6] = (if high_bit { 0x80 } else { 0 }) | 0x60 | (ssid << 1) | u8::from(last);
    Ok(out)
}

fn decode_addr(b: &[u8]) -> (String, bool) {
    let call: String = b[..6].iter().map(|&c| (c >> 1) as char).collect::<String>().trim().to_string();
    let ssid = (b[6] >> 1) & 0x0F;
    let name = if ssid > 0 { format!("{call}-{ssid}") } else { call };
    (name, b[6] & 0x80 != 0)
}

/// Build a frame. Command frames set C in the destination, responses in the source (AX.25 v2).
pub fn build(dst: &str, src: &str, path: &[String], ctl: u8, command: bool, pid: Option<u8>, info: &[u8])
    -> Result<Vec<u8>, BadCall> {
    build_ctl(dst, src, path, &[ctl], command, pid, info)
}

/// `build` with a one- or two-byte control field (two for I and S frames on a modulo-128 link).
pub fn build_ctl(dst: &str, src: &str, path: &[String], ctl: &[u8], command: bool, pid: Option<u8>, info: &[u8])
    -> Result<Vec<u8>, BadCall> {
    let mut out = Vec::with_capacity(16 + 7 * path.len() + info.len());
    let n = 2 + path.len();
    out.extend(encode_addr(dst, command, n == 1)?);
    out.extend(encode_addr(src, !command, n == 2)?);
    for (i, p) in path.iter().enumerate() {
        out.extend(encode_addr(p, false, i + 3 == n)?);
    }
    out.extend_from_slice(ctl);
    if let Some(p) = pid {
        out.push(p);
    }
    out.extend_from_slice(info);
    Ok(out)
}

pub fn parse(raw: &[u8]) -> Option<Frame> {
    parse_for(raw, false)
}

/// Parse a frame from a link in modulo-128 mode when `extended` (I and S frames then have two control bytes).
pub fn parse_for(raw: &[u8], extended: bool) -> Option<Frame> {
    let mut addrs = Vec::new();
    let mut i = 0;
    while i + 7 <= raw.len() {
        addrs.push(decode_addr(&raw[i..i + 7]));
        let last = raw[i + 6] & 1 != 0;
        i += 7;
        if last {
            break;
        }
    }
    if addrs.len() < 2 || i >= raw.len() {
        return None;
    }
    let ctl = raw[i];
    let (dst, dst_c) = addrs[0].clone();
    let (src, src_c) = addrs[1].clone();
    let ctl2 = if extended && ctl & 3 != 3 { Some(*raw.get(i + 1)?) } else { None };
    let at = i + 1 + usize::from(ctl2.is_some());
    let has_pid = ctl & 1 == 0 || ctl & !PF == UI;
    let (pid, info) = if has_pid {
        (raw.get(at).copied(), raw.get(at + 1..).map(|s| s.to_vec()).unwrap_or_default())
    } else if ctl & !PF == XID || ctl & !PF == FRMR {
        (None, raw.get(at..).map(|s| s.to_vec()).unwrap_or_default())
    } else {
        (None, Vec::new())
    };
    Some(Frame { dst, src, path: addrs[2..].to_vec(), ctl, ctl2, command: dst_c && !src_c, pid, info })
}

/// Human-readable line: "SRC > DST via A,B*: text" (link-control frames marked as such).
pub fn describe(raw: &[u8]) -> String {
    match parse(raw) {
        None => format!("(not AX.25: {} bytes)", raw.len()),
        Some(f) => {
            let via = if f.path.is_empty() {
                String::new()
            } else {
                let p: Vec<String> = f.path.iter().map(|(c, h)| if *h { format!("{c}*") } else { c.clone() }).collect();
                format!(" via {}", p.join(","))
            };
            let text = if f.pid.is_some() {
                String::from_utf8_lossy(&f.info).replace(['\r', '\n'], " ").trim_end().to_string()
            } else {
                format!("<{}>", control_name(f.ctl))
            };
            format!("{} > {}{}: {}", f.src, f.dst, via, text)
        }
    }
}

/// Plain name of a link-control frame; v2.2-only kinds say so, since seeing one means the sender speaks v2.2.
pub fn control_name(ctl: u8) -> String {
    if ctl & 3 == 1 {
        let nr = ctl >> 5;
        return match ctl & 0x0F {
            RR => format!("RR ack, next {nr}"),
            RNR => "RNR busy".into(),
            REJ => format!("REJ resend from {nr}"),
            SREJ => format!("SREJ resend {nr} only, v2.2"),
            _ => format!("S {ctl:02x}"),
        };
    }
    match ctl & !PF {
        SABM => "SABM connect".into(),
        SABME => "SABME connect, v2.2".into(),
        UA => "UA ok".into(),
        DM => "DM refused".into(),
        DISC => "DISC disconnect".into(),
        FRMR => "FRMR frame reject".into(),
        XID => "XID capabilities, v2.2".into(),
        TEST => "TEST".into(),
        UI => "UI".into(),
        c => format!("control {c:02x}"),
    }
}

pub fn same_call(a: &str, b: &str) -> bool {
    let norm = |c: &str| {
        let c = c.to_ascii_uppercase();
        c.strip_suffix("-0").map(str::to_string).unwrap_or(c)
    };
    norm(a) == norm(b)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Disconnected,
    Connecting,
    Connected,
    Disconnecting,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub paclen: usize,
    pub window: usize,
    pub t1: Duration,
    pub n2: u32,
    pub ack_delay: Duration,
    /// Try (and accept) AX.25 v2.2. When false the station behaves as v2.0 and answers SABME with DM.
    pub v22: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { paclen: 128, window: 4, t1: Duration::from_secs(10), n2: 10, ack_delay: Duration::from_millis(500), v22: true }
    }
}

pub struct Station {
    pub mycall: String,
    pub cfg: Config,
    pub listen: bool,
    pub state: State,
    pub remote: String,
    pub path: Vec<String>,
    vs: u8,
    vr: u8,
    va: u8,
    unacked: BTreeMap<u8, Vec<u8>>,
    txq: Vec<u8>,
    rx: Vec<u8>,
    t1_at: Option<Instant>,
    ack_at: Option<Instant>,
    retries: u32,
    rej_sent: bool,
    remote_busy: bool,
    connect_attempts: Option<u32>,
    outbox: Vec<Vec<u8>>,
    events: Vec<String>,
    /// Modulo-128 (v2.2) link.
    extended: bool,
    /// A SABME is outstanding; DM, FRMR or a timeout means "try SABM".
    sabme_pending: bool,
    /// Our XID command awaits a response; an FRMR to it just means "use the defaults".
    xid_pending: bool,
    /// Window and frame length in use, after any XID negotiation.
    window: usize,
    paclen: usize,
    stats: LinkStats,
}

/// Counters for the current link, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkStats {
    /// I frames sent, including resends.
    pub sent: u32,
    /// I frames sent again after a loss.
    pub resent: u32,
    /// I frames received in order.
    pub received: u32,
    pub bytes_out: u64,
    pub bytes_in: u64,
}

impl Station {
    pub fn new(mycall: &str, cfg: Config) -> Self {
        Station {
            mycall: mycall.to_ascii_uppercase(), cfg, listen: false, state: State::Disconnected,
            remote: String::new(), path: Vec::new(), vs: 0, vr: 0, va: 0, unacked: BTreeMap::new(),
            txq: Vec::new(), rx: Vec::new(), t1_at: None, ack_at: None, retries: 0, rej_sent: false,
            remote_busy: false, connect_attempts: None, outbox: Vec::new(), events: Vec::new(),
            extended: false, sabme_pending: false, xid_pending: false, window: 0, paclen: 0,
            stats: LinkStats::default(),
        }
    }

    fn reset(&mut self) {
        self.vs = 0;
        self.vr = 0;
        self.va = 0;
        self.unacked.clear();
        self.txq.clear();
        self.rx.clear();
        self.t1_at = None;
        self.ack_at = None;
        self.retries = 0;
        self.rej_sent = false;
        self.remote_busy = false;
        self.extended = false;
        self.sabme_pending = false;
        self.xid_pending = false;
        self.window = self.cfg.window.clamp(1, 7);
        self.paclen = self.cfg.paclen;
        self.stats = LinkStats::default();
    }

    pub fn stats(&self) -> &LinkStats {
        &self.stats
    }

    /// Frames in flight (sent, not yet acknowledged).
    pub fn unacked(&self) -> usize {
        self.unacked.len()
    }

    /// Window and frame length in use, after any negotiation.
    pub fn window_and_paclen(&self) -> (usize, usize) {
        (self.window, self.paclen)
    }

    /// Link version in use: "2.2" (modulo 128) or "2.0".
    pub fn version(&self) -> &'static str {
        if self.extended { "2.2" } else { "2.0" }
    }

    /// True on a modulo-128 link; received frames must then be parsed with `parse_for(raw, true)`.
    pub fn extended(&self) -> bool {
        self.extended
    }

    fn mask(&self) -> u8 {
        if self.extended { 127 } else { 7 }
    }

    // ------------------------------------------------------------ user API

    /// Start a connection; give up after `attempts` SABMs in total (default n2 + 1).
    pub fn connect(&mut self, remote: &str, path: &[String], attempts: Option<u32>, now: Instant) {
        self.remote = remote.to_ascii_uppercase();
        self.path = path.iter().map(|p| p.to_ascii_uppercase()).collect();
        self.reset();
        self.connect_attempts = attempts;
        self.state = State::Connecting;
        self.sabme_pending = self.cfg.v22;
        self.send_u(if self.cfg.v22 { SABME } else { SABM } | PF, true);
        self.start_t1(now);
    }

    pub fn send(&mut self, data: &[u8]) {
        self.txq.extend_from_slice(data);
    }

    pub fn recv(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.rx)
    }

    pub fn disconnect(&mut self, now: Instant) {
        if matches!(self.state, State::Connected | State::Connecting) {
            self.state = State::Disconnecting;
            self.retries = 0;
            self.send_u(DISC | PF, true);
            self.start_t1(now);
        }
    }

    /// Drop the link immediately without telling the other side (e.g. user abort while connecting).
    pub fn abort(&mut self) {
        self.closed("aborted");
    }

    pub fn all_sent(&self) -> bool {
        self.txq.is_empty() && self.unacked.is_empty()
    }

    /// Raw AX.25 frames waiting to be transmitted.
    pub fn take_outbox(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.outbox)
    }

    /// Human-readable events ("connected to ...", "... disconnected", ...).
    pub fn take_events(&mut self) -> Vec<String> {
        std::mem::take(&mut self.events)
    }

    // ------------------------------------------------------------ engine

    /// Handle a received raw AX.25 frame, parsed for this link's mode.
    pub fn on_raw(&mut self, raw: &[u8], now: Instant) {
        if let Some(fr) = parse_for(raw, self.extended) {
            self.on_frame(&fr, now);
        }
    }

    /// Handle a received AX.25 frame (parsed for this link's mode; ignores frames not addressed to us).
    pub fn on_frame(&mut self, fr: &Frame, now: Instant) {
        if !same_call(&fr.dst, &self.mycall) {
            return;
        }
        let ctl = fr.ctl;
        let unnumbered = ctl & 3 == 3;
        let base = if unnumbered { ctl & !PF } else { ctl };
        let pf = match fr.ctl2 {
            Some(c2) => c2 & 1 != 0,
            None => ctl & PF != 0,
        };
        if !self.remote.is_empty() && !same_call(&fr.src, &self.remote) {
            if unnumbered && (base == SABM || base == SABME) {
                let back: Vec<String> = fr.path.iter().rev().map(|(c, _)| c.clone()).collect();
                self.push_raw(&fr.src, &[DM | PF], false, None, &[], &back);
            }
            return;
        }
        if !unnumbered {
            if self.state != State::Connected {
                if fr.command && pf {
                    self.send_u(DM | PF, false);
                }
            } else if ctl & 1 == 0 {
                self.on_i(fr, pf, now);
            } else {
                let nr = match fr.ctl2 {
                    Some(c2) => c2 >> 1,
                    None => (ctl >> 5) & 7,
                };
                self.on_s(ctl & 0x0F, nr, pf, fr.command, now);
            }
            return;
        }
        match base {
            SABME if !self.cfg.v22 => {
                let back: Vec<String> = fr.path.iter().rev().map(|(c, _)| c.clone()).collect();
                self.push_raw(&fr.src, &[DM | if pf { PF } else { 0 }], false, None, &[], &back);
            }
            SABM | SABME => {
                if self.listen || self.state == State::Connected {
                    self.remote = fr.src.clone();
                    self.path = fr.path.iter().rev().map(|(c, _)| c.clone()).collect();
                    self.reset();
                    self.extended = base == SABME;
                    self.window = self.cfg.window.clamp(1, if self.extended { 127 } else { 7 });
                    self.state = State::Connected;
                    self.events.push(format!("connected by {} (AX.25 v{})", self.remote, self.version()));
                }
                self.send_u(UA | if pf { PF } else { 0 }, false);
            }
            UA => {
                if self.state == State::Connecting {
                    self.state = State::Connected;
                    self.t1_at = None;
                    self.retries = 0;
                    self.extended = self.sabme_pending;
                    self.sabme_pending = false;
                    self.window = self.cfg.window.clamp(1, if self.extended { 127 } else { 7 });
                    self.events.push(format!("connected to {} (AX.25 v{})", self.remote, self.version()));
                    if self.extended {
                        // negotiate the link parameters; a station that cannot will answer FRMR
                        let info = xid_encode(&self.xid_params());
                        let (remote, path) = (self.remote.clone(), self.path.clone());
                        self.push_raw(&remote, &[XID | PF], true, None, &info, &path);
                        self.xid_pending = true;
                    }
                } else if self.state == State::Disconnecting {
                    self.closed("disconnected");
                }
            }
            XID if fr.command => {
                if let Some(p) = xid_decode(&fr.info) {
                    self.apply_xid(&p);
                }
                let info = xid_encode(&self.xid_params());
                let (remote, path) = (fr.src.clone(), fr.path.iter().rev().map(|(c, _)| c.clone()).collect::<Vec<_>>());
                self.push_raw(&remote, &[XID | if pf { PF } else { 0 }], false, None, &info, &path);
            }
            XID => {
                if self.xid_pending
                    && let Some(p) = xid_decode(&fr.info) {
                        self.apply_xid(&p);
                    }
                self.xid_pending = false;
            }
            DISC => {
                self.send_u(UA | if pf { PF } else { 0 }, false);
                let who = self.remote.clone();
                self.closed(&format!("{who} disconnected"));
            }
            DM | FRMR if self.state == State::Connecting && self.sabme_pending => self.fall_back_to_v20(now, true),
            FRMR if self.xid_pending => self.xid_pending = false, // no XID support: keep the defaults
            DM => {
                if self.state == State::Connecting {
                    let who = self.remote.clone();
                    self.closed(&format!("{who} refused the connection"));
                } else if self.state != State::Disconnected {
                    self.closed("disconnected");
                }
            }
            FRMR => self.closed("frame reject"),
            _ if self.state != State::Connected && fr.command && pf => self.send_u(DM | PF, false),
            _ => {}
        }
    }

    /// The other station did not take SABME: connect again with SABM, as v2.0.
    fn fall_back_to_v20(&mut self, now: Instant, refused: bool) {
        self.sabme_pending = false;
        self.retries = 0;
        let who = self.remote.clone();
        self.events.push(if refused {
            format!("{who} does not use AX.25 v2.2; connecting with v2.0")
        } else {
            format!("no answer from {who} to an AX.25 v2.2 connect; trying v2.0")
        });
        self.send_u(SABM | PF, true);
        self.start_t1(now);
    }

    fn xid_params(&self) -> XidParams {
        XidParams {
            modulo_128: self.extended,
            i_field_rx: Some(self.cfg.paclen as u32),
            window_rx: Some(if self.extended { self.cfg.window.clamp(1, 127) } else { self.cfg.window.clamp(1, 7) } as u8),
            ack_timer_ms: Some(self.cfg.t1.as_millis().min(u16::MAX as u128) as u32),
            retries: Some(self.cfg.n2.min(255) as u8),
        }
    }

    /// Use the smaller of our settings and what the other station says it can receive.
    fn apply_xid(&mut self, p: &XidParams) {
        let cap = if self.extended { 127 } else { 7 };
        if let Some(w) = p.window_rx {
            self.window = self.cfg.window.min(w as usize).clamp(1, cap);
        }
        if let Some(n) = p.i_field_rx {
            self.paclen = self.cfg.paclen.min(n as usize).max(1);
        }
    }

    /// Run timers and push queued data; call regularly.
    pub fn poll(&mut self, now: Instant) {
        if let Some(t) = self.t1_at
            && now >= t {
                self.on_t1(now);
            }
        if self.state == State::Connected {
            self.pump(now);
        }
        if let Some(t) = self.ack_at
            && now >= t {
                self.send_s(RR, false);
                self.ack_at = None;
            }
    }

    fn on_i(&mut self, fr: &Frame, pf: bool, now: Instant) {
        let ns = (fr.ctl >> 1) & self.mask();
        let nr = match fr.ctl2 {
            Some(c2) => c2 >> 1,
            None => (fr.ctl >> 5) & 7,
        };
        self.ack_to(nr, now);
        if ns == self.vr {
            self.rx.extend_from_slice(&fr.info);
            self.stats.received += 1;
            self.stats.bytes_in += fr.info.len() as u64;
            self.vr = (self.vr + 1) & self.mask();
            self.rej_sent = false;
            if pf {
                self.send_s(RR | PF, false);
                self.ack_at = None;
            } else if self.ack_at.is_none() {
                self.ack_at = Some(now + self.cfg.ack_delay);
            }
        } else if !self.rej_sent {
            self.send_s(REJ | if pf { PF } else { 0 }, false);
            self.rej_sent = true;
        } else if pf {
            self.send_s(RR | PF, false);
        }
    }

    fn on_s(&mut self, kind: u8, nr: u8, pf: bool, command: bool, now: Instant) {
        self.ack_to(nr, now);
        self.remote_busy = kind == RNR;
        if kind == REJ {
            self.retransmit_from(nr, now);
        } else if kind == SREJ
            && let Some(info) = self.unacked.get(&nr).cloned() {
                // we do not ask for SREJ, but resend just that frame if a peer uses it anyway
                self.send_i(nr, &info);
                self.start_t1(now);
            }
        if command && pf {
            self.send_s(RR | PF, false);
        } else if !command && pf {
            self.retries = 0;
            if self.unacked.is_empty() {
                self.t1_at = None;
            } else {
                let va = self.va;
                self.retransmit_from(va, now);
            }
        }
    }

    fn ack_to(&mut self, nr: u8, now: Instant) {
        while self.va != nr && self.unacked.contains_key(&self.va) {
            self.unacked.remove(&self.va);
            self.va = (self.va + 1) & self.mask();
            self.retries = 0;
        }
        self.t1_at = if self.unacked.is_empty() { None } else { Some(now + self.cfg.t1) };
    }

    fn retransmit_from(&mut self, nr: u8, now: Instant) {
        let mut n = nr;
        while let Some(info) = self.unacked.get(&n).cloned() {
            self.stats.resent += 1;
            self.send_i(n, &info);
            n = (n + 1) & self.mask();
        }
        self.start_t1(now);
    }

    fn pump(&mut self, now: Instant) {
        while !self.txq.is_empty() && !self.remote_busy && self.unacked.len() < self.window {
            let take = self.txq.len().min(self.paclen);
            let chunk: Vec<u8> = self.txq.drain(..take).collect();
            let vs = self.vs;
            self.stats.bytes_out += chunk.len() as u64;
            self.send_i(vs, &chunk);
            self.unacked.insert(vs, chunk);
            self.vs = (self.vs + 1) & self.mask();
            if self.t1_at.is_none() {
                self.start_t1(now);
            }
        }
    }

    fn on_t1(&mut self, now: Instant) {
        if self.state == State::Connecting && self.sabme_pending {
            // some v2.0 stations ignore SABME entirely
            return self.fall_back_to_v20(now, false);
        }
        self.retries += 1;
        let limit = match (self.state, self.connect_attempts) {
            (State::Connecting, Some(a)) if a > 0 => a - 1,
            _ => self.cfg.n2,
        };
        if self.retries > limit {
            let who = self.remote.clone();
            self.closed(&format!("no response from {who} after {} attempts", limit + 1));
            return;
        }
        match self.state {
            State::Connecting => self.send_u(SABM | PF, true),
            State::Disconnecting => self.send_u(DISC | PF, true),
            _ if !self.unacked.is_empty() => self.send_s(RR | PF, true),
            _ => {}
        }
        self.start_t1(now);
    }

    fn start_t1(&mut self, now: Instant) {
        self.t1_at = Some(now + self.cfg.t1);
    }

    fn closed(&mut self, why: &str) {
        self.sabme_pending = false;
        self.xid_pending = false;
        self.events.push(why.to_string());
        self.state = State::Disconnected;
        self.t1_at = None;
        self.ack_at = None;
    }

    // ------------------------------------------------------------ framing

    fn push_raw(&mut self, dst: &str, ctl: &[u8], command: bool, pid: Option<u8>, info: &[u8], path: &[String]) {
        if let Ok(raw) = build_ctl(dst, &self.mycall, path, ctl, command, pid, info) {
            self.outbox.push(raw);
        }
    }

    fn send_u(&mut self, ctl: u8, command: bool) {
        let (remote, path) = (self.remote.clone(), self.path.clone());
        self.push_raw(&remote, &[ctl], command, None, &[], &path);
    }

    fn send_s(&mut self, kind: u8, command: bool) {
        let (remote, path) = (self.remote.clone(), self.path.clone());
        let ctl = if self.extended {
            vec![kind & 0x0F, (self.vr << 1) | u8::from(kind & PF != 0)]
        } else {
            vec![(self.vr << 5) | kind]
        };
        self.push_raw(&remote, &ctl, command, None, &[], &path);
        if matches!(kind & !PF, RR | REJ) {
            self.ack_at = None;
        }
    }

    fn send_i(&mut self, ns: u8, info: &[u8]) {
        self.stats.sent += 1;
        let (remote, path) = (self.remote.clone(), self.path.clone());
        let ctl = if self.extended { vec![ns << 1, self.vr << 1] } else { vec![(self.vr << 5) | (ns << 1)] };
        self.push_raw(&remote, &ctl, true, Some(PID_NONE), info, &path);
        self.ack_at = None;
    }
}

// ------------------------------------------------------------------ XID (AX.25 v2.2 parameter negotiation)

/// Negotiable link parameters carried in an XID frame (AX.25 v2.2, section 4.3.3.7).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct XidParams {
    pub modulo_128: bool,
    /// Longest I field the sender can receive, in bytes.
    pub i_field_rx: Option<u32>,
    /// Frames the sender can receive before acknowledging.
    pub window_rx: Option<u8>,
    pub ack_timer_ms: Option<u32>,
    pub retries: Option<u8>,
}

const XID_FI: u8 = 0x82;
const XID_GI: u8 = 0x80;
const PI_CLASSES: u8 = 2;
const PI_OPTIONAL: u8 = 3;
const PI_I_FIELD_RX: u8 = 6;
const PI_WINDOW_RX: u8 = 8;
const PI_ACK_TIMER: u8 = 9;
const PI_RETRIES: u8 = 10;
/// Classes of procedures: balanced asynchronous mode, half duplex.
const CLASSES_ABM_HALF_DUPLEX: u32 = 0x0100 | 0x2000;
/// Optional functions: REJ, extended address, 16-bit FCS, synchronous transmit; plus modulo 8 or 128.
const OPT_BASE: u32 = 0x020000 | 0x800000 | 0x008000 | 0x000002;
const OPT_MOD8: u32 = 0x000400;
const OPT_MOD128: u32 = 0x000800;

pub fn xid_encode(p: &XidParams) -> Vec<u8> {
    let mut params = Vec::new();
    let mut put = |pi: u8, len: usize, v: u32| {
        params.push(pi);
        params.push(len as u8);
        for i in (0..len).rev() {
            params.push((v >> (8 * i)) as u8);
        }
    };
    put(PI_CLASSES, 2, CLASSES_ABM_HALF_DUPLEX);
    put(PI_OPTIONAL, 3, OPT_BASE | if p.modulo_128 { OPT_MOD128 } else { OPT_MOD8 });
    if let Some(n) = p.i_field_rx {
        put(PI_I_FIELD_RX, 2, n * 8); // carried in bits
    }
    if let Some(w) = p.window_rx {
        put(PI_WINDOW_RX, 1, w as u32);
    }
    if let Some(t) = p.ack_timer_ms {
        put(PI_ACK_TIMER, 2, t);
    }
    if let Some(r) = p.retries {
        put(PI_RETRIES, 1, r as u32);
    }
    let mut out = vec![XID_FI, XID_GI, (params.len() >> 8) as u8, params.len() as u8];
    out.extend(params);
    out
}

/// Decode an XID information field; unknown parameters are skipped.
pub fn xid_decode(info: &[u8]) -> Option<XidParams> {
    if info.len() < 4 || info[0] != XID_FI || info[1] != XID_GI {
        return None;
    }
    let glen = ((info[2] as usize) << 8) | info[3] as usize;
    let body = info.get(4..4 + glen)?;
    let mut p = XidParams::default();
    let mut i = 0;
    while i + 2 <= body.len() {
        let (pi, pl) = (body[i], body[i + 1] as usize);
        let v = body.get(i + 2..i + 2 + pl)?.iter().fold(0u32, |a, &b| (a << 8) | b as u32);
        match pi {
            PI_OPTIONAL => p.modulo_128 = v & OPT_MOD128 != 0,
            PI_I_FIELD_RX => p.i_field_rx = Some(v / 8),
            PI_WINDOW_RX => p.window_rx = Some(v.min(127) as u8),
            PI_ACK_TIMER => p.ack_timer_ms = Some(v),
            PI_RETRIES => p.retries = Some(v.min(255) as u8),
            _ => {}
        }
        i += 2 + pl;
    }
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xid_matches_the_spec_example() {
        // Figure 4.6 of the AX.25 v2.2 spec (classes-of-procedures bytes as corrected in errata):
        // half duplex, REJ+SREJ, modulo 128, I field 2048 bits, window 7, ack timer 3000 ms, 10 retries
        let example = [0x82, 0x80, 0x00, 0x17, 0x02, 0x02, 0x21, 0x00, 0x03, 0x03, 0x86, 0xA8, 0x02,
                       0x06, 0x02, 0x08, 0x00, 0x08, 0x01, 0x07, 0x09, 0x02, 0x0B, 0xB8, 0x0A, 0x01, 0x0A];
        let p = xid_decode(&example).unwrap();
        assert_eq!(p, XidParams { modulo_128: true, i_field_rx: Some(256), window_rx: Some(7),
                                  ack_timer_ms: Some(3000), retries: Some(10) });
        let ours = xid_encode(&p);
        assert_eq!(&ours[..8], &example[..8], "header and classes of procedures");
        assert_eq!(&ours[13..], &example[13..], "negotiated values");
        assert_eq!(xid_decode(&ours).unwrap(), p);
    }

    #[test]
    fn extended_frames_round_trip() {
        let raw = build_ctl("A1A", "B2B", &[], &[5 << 1, 9 << 1], true, Some(PID_NONE), b"hi").unwrap();
        let f = parse_for(&raw, true).unwrap();
        assert_eq!((f.ctl, f.ctl2, f.pid, f.info.as_slice()), (10, Some(18), Some(PID_NONE), &b"hi"[..]));
        let rr = build_ctl("A1A", "B2B", &[], &[RR, (100 << 1) | 1], false, None, &[]).unwrap();
        assert_eq!(parse_for(&rr, true).unwrap().ctl2, Some(201));
    }

    #[test]
    fn command_response_bits() {
        let raw = build("N0GW-10", "N0CALL", &[], SABM | PF, true, None, &[]).unwrap();
        let f = parse(&raw).unwrap();
        assert!(f.command);
        assert_eq!((f.dst.as_str(), f.src.as_str(), f.ctl), ("N0GW-10", "N0CALL", 0x3F));
        let f = parse(&build("N0CALL", "N0GW-10", &[], UA | PF, false, None, &[]).unwrap()).unwrap();
        assert!(!f.command);
    }

    #[test]
    fn i_frame_fields_and_describe() {
        let raw = build("A1A", "B2B", &["WIDE1-1".into()], (3 << 5) | (5 << 1), true, Some(PID_NONE), b"hi").unwrap();
        let f = parse(&raw).unwrap();
        assert_eq!((f.ctl >> 1) & 7, 5);
        assert_eq!(f.ctl >> 5, 3);
        assert_eq!((f.pid, f.info.as_slice()), (Some(0xF0), b"hi".as_slice()));
        assert_eq!(describe(&raw), "B2B > A1A via WIDE1-1: hi");
        let rr = build("A1A", "B2B", &[], RR, false, None, &[]).unwrap();
        assert_eq!(describe(&rr), "B2B > A1A: <RR ack, next 0>");
        let sabme = build("A1A", "B2B", &[], SABME | PF, true, None, &[]).unwrap();
        assert_eq!(describe(&sabme), "B2B > A1A: <SABME connect, v2.2>");
    }

    #[test]
    fn calls() {
        assert!(same_call("n0call-0", "N0CALL"));
        assert!(!same_call("N0CALL-1", "N0CALL"));
        assert!(encode_addr("TOOLONGCALL", false, true).is_err());
        assert!(encode_addr("N0CALL-16", false, true).is_err());
    }
}
