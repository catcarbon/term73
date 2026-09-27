//! AX.25 v2.0 frames and a connected-mode (modulo 8) link state machine.
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
    let mut out = Vec::with_capacity(16 + 7 * path.len() + info.len());
    let n = 2 + path.len();
    out.extend(encode_addr(dst, command, n == 1)?);
    out.extend(encode_addr(src, !command, n == 2)?);
    for (i, p) in path.iter().enumerate() {
        out.extend(encode_addr(p, false, i + 3 == n)?);
    }
    out.push(ctl);
    if let Some(p) = pid {
        out.push(p);
    }
    out.extend_from_slice(info);
    Ok(out)
}

pub fn parse(raw: &[u8]) -> Option<Frame> {
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
    let has_pid = ctl & 1 == 0 || ctl & !PF == UI;
    let (pid, info) = if has_pid {
        (raw.get(i + 1).copied(), raw.get(i + 2..).map(|s| s.to_vec()).unwrap_or_default())
    } else {
        (None, Vec::new())
    };
    Some(Frame { dst, src, path: addrs[2..].to_vec(), ctl, command: dst_c && !src_c, pid, info })
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
}

impl Default for Config {
    fn default() -> Self {
        Config { paclen: 128, window: 4, t1: Duration::from_secs(10), n2: 10, ack_delay: Duration::from_millis(500) }
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
}

impl Station {
    pub fn new(mycall: &str, cfg: Config) -> Self {
        Station {
            mycall: mycall.to_ascii_uppercase(), cfg, listen: false, state: State::Disconnected,
            remote: String::new(), path: Vec::new(), vs: 0, vr: 0, va: 0, unacked: BTreeMap::new(),
            txq: Vec::new(), rx: Vec::new(), t1_at: None, ack_at: None, retries: 0, rej_sent: false,
            remote_busy: false, connect_attempts: None, outbox: Vec::new(), events: Vec::new(),
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
    }

    // ------------------------------------------------------------ user API

    /// Start a connection; give up after `attempts` SABMs in total (default n2 + 1).
    pub fn connect(&mut self, remote: &str, path: &[String], attempts: Option<u32>, now: Instant) {
        self.remote = remote.to_ascii_uppercase();
        self.path = path.iter().map(|p| p.to_ascii_uppercase()).collect();
        self.reset();
        self.connect_attempts = attempts;
        self.state = State::Connecting;
        self.send_u(SABM | PF, true);
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

    /// Handle a received AX.25 frame (already parsed; ignores frames not addressed to us).
    pub fn on_frame(&mut self, fr: &Frame, now: Instant) {
        if !same_call(&fr.dst, &self.mycall) {
            return;
        }
        let ctl = fr.ctl;
        let base = ctl & !PF;
        let pf = ctl & PF != 0;
        if !self.remote.is_empty() && !same_call(&fr.src, &self.remote) {
            if base == SABM {
                let back: Vec<String> = fr.path.iter().rev().map(|(c, _)| c.clone()).collect();
                self.push_raw(&fr.src, DM | PF, false, None, &[], &back);
            }
            return;
        }
        match base {
            SABM => {
                if self.listen || self.state == State::Connected {
                    self.remote = fr.src.clone();
                    self.path = fr.path.iter().rev().map(|(c, _)| c.clone()).collect();
                    self.reset();
                    self.state = State::Connected;
                    self.events.push(format!("connected by {}", self.remote));
                }
                self.send_u(UA | if pf { PF } else { 0 }, false);
            }
            UA => {
                if self.state == State::Connecting {
                    self.state = State::Connected;
                    self.t1_at = None;
                    self.retries = 0;
                    self.events.push(format!("connected to {}", self.remote));
                } else if self.state == State::Disconnecting {
                    self.closed("disconnected");
                }
            }
            DISC => {
                self.send_u(UA | if pf { PF } else { 0 }, false);
                let who = self.remote.clone();
                self.closed(&format!("{who} disconnected"));
            }
            DM => {
                if self.state == State::Connecting {
                    let who = self.remote.clone();
                    self.closed(&format!("{who} refused the connection"));
                } else if self.state != State::Disconnected {
                    self.closed("disconnected");
                }
            }
            FRMR => self.closed("frame reject"),
            _ if self.state != State::Connected => {
                if fr.command && pf {
                    self.send_u(DM | PF, false);
                }
            }
            _ if ctl & 1 == 0 => self.on_i(fr, pf, now),
            _ if ctl & 3 == 1 => self.on_s(ctl & 0x0F, (ctl >> 5) & 7, pf, fr.command, now),
            _ => {}
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
        let ns = (fr.ctl >> 1) & 7;
        let nr = (fr.ctl >> 5) & 7;
        self.ack_to(nr, now);
        if ns == self.vr {
            self.rx.extend_from_slice(&fr.info);
            self.vr = (self.vr + 1) & 7;
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
            self.va = (self.va + 1) & 7;
            self.retries = 0;
        }
        self.t1_at = if self.unacked.is_empty() { None } else { Some(now + self.cfg.t1) };
    }

    fn retransmit_from(&mut self, nr: u8, now: Instant) {
        let mut n = nr;
        while let Some(info) = self.unacked.get(&n).cloned() {
            self.send_i(n, &info);
            n = (n + 1) & 7;
        }
        self.start_t1(now);
    }

    fn pump(&mut self, now: Instant) {
        while !self.txq.is_empty() && !self.remote_busy && self.unacked.len() < self.cfg.window {
            let take = self.txq.len().min(self.cfg.paclen);
            let chunk: Vec<u8> = self.txq.drain(..take).collect();
            let vs = self.vs;
            self.send_i(vs, &chunk);
            self.unacked.insert(vs, chunk);
            self.vs = (self.vs + 1) & 7;
            if self.t1_at.is_none() {
                self.start_t1(now);
            }
        }
    }

    fn on_t1(&mut self, now: Instant) {
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
        self.events.push(why.to_string());
        self.state = State::Disconnected;
        self.t1_at = None;
        self.ack_at = None;
    }

    // ------------------------------------------------------------ framing

    fn push_raw(&mut self, dst: &str, ctl: u8, command: bool, pid: Option<u8>, info: &[u8], path: &[String]) {
        if let Ok(raw) = build(dst, &self.mycall, path, ctl, command, pid, info) {
            self.outbox.push(raw);
        }
    }

    fn send_u(&mut self, ctl: u8, command: bool) {
        let (remote, path) = (self.remote.clone(), self.path.clone());
        self.push_raw(&remote, ctl, command, None, &[], &path);
    }

    fn send_s(&mut self, kind: u8, command: bool) {
        let (remote, path) = (self.remote.clone(), self.path.clone());
        self.push_raw(&remote, (self.vr << 5) | kind, command, None, &[], &path);
        if matches!(kind & !PF, RR | REJ) {
            self.ack_at = None;
        }
    }

    fn send_i(&mut self, ns: u8, info: &[u8]) {
        let (remote, path) = (self.remote.clone(), self.path.clone());
        self.push_raw(&remote, (self.vr << 5) | (ns << 1), true, Some(PID_NONE), info, &path);
        self.ack_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
