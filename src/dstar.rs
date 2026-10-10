//! D-STAR DV data probe: test payloads, a receive monitor and write pacing, for finding out how a
//! radio carries DV (fast) data on its serial link. Nothing here talks to a radio.
//!
//! In DV Data Mode the TH-D75 transmits the bytes that arrive on the interface chosen in its menu 984;
//! whether it also still reads rig commands there is not known, so payloads that could look like one
//! are refused (`command_like`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Most bytes the probe holds for sending at one time.
pub const MAX_SEND: usize = 4096;
/// A reception ends once nothing has arrived for this long.
pub const RX_GAP: Duration = Duration::from_millis(1500);
/// A partial hex-dump line is shown once the link pauses this long.
const DUMP_PAUSE: Duration = Duration::from_millis(300);

pub const SWEEP_START: &[u8] = b"#T73SWEEP ";
pub const SWEEP_END: &[u8] = b" #T73END\r";
const SWEEP_END_MARK: &[u8] = b"#T73END";
const BURST_MARK: &[u8] = b"#T73 BURST ";
/// Every burst line is this long, CR included.
pub const BURST_LINE: usize = 32;

/// "24 24 0d", "24240D" or "0x24,0x0d" -> bytes.
pub fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let digits: String = s.split(|c: char| c.is_whitespace() || c == ',')
        .map(|w| w.strip_prefix("0x").or_else(|| w.strip_prefix("0X")).unwrap_or(w))
        .collect();
    if digits.is_empty() || !digits.len().is_multiple_of(2) || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("hex bytes, e.g. 24 24 0d".into());
    }
    Ok((0..digits.len()).step_by(2).map(|i| u8::from_str_radix(&digits[i..i + 2], 16).unwrap()).collect())
}

/// The start of the first line in `data` that a radio could take as a rig command: two letters or digits
/// at the start of a line (spaces skipped). Lines are split at every control character, since which bytes
/// end a command line in data mode is not known.
pub fn command_like(data: &[u8]) -> Option<String> {
    data.split(|&b| b < 0x20 || b == 0x7F)
        .map(|line| &line[line.iter().position(|&b| b != b' ').unwrap_or(line.len())..])
        .find(|line| line.len() >= 2 && line[0].is_ascii_alphanumeric() && line[1].is_ascii_alphanumeric())
        .map(|line| String::from_utf8_lossy(&line[..line.len().min(12)]).into_owned())
}

/// Every byte value once, in order, between markers a receiving term73 recognises.
pub fn sweep_payload() -> Vec<u8> {
    let mut v = SWEEP_START.to_vec();
    v.extend(0..=255u8);
    v.extend_from_slice(SWEEP_END);
    v
}

/// One burst line: "#T73 BURST 0001/0004 ABCDEFGHIJ\r".
fn burst_line(i: usize, lines: usize) -> Vec<u8> {
    let mut l = format!("#T73 BURST {i:04}/{lines:04} ").into_bytes();
    while l.len() < BURST_LINE - 1 {
        l.push(b'A' + (l.len() % 26) as u8);
    }
    l.push(b'\r');
    l
}

/// At least `bytes` bytes as numbered 32-byte lines, so a receiver can tell which lines were lost.
pub fn burst_payload(bytes: usize) -> Vec<u8> {
    let lines = bytes.div_ceil(BURST_LINE).max(1);
    (1..=lines).flat_map(|i| burst_line(i, lines)).collect()
}

/// "0010  24 24 4d 73 ...  |$$Ms...|": 16 bytes a line, offsets counted from `start`.
pub fn hexdump(start: usize, data: &[u8]) -> Vec<String> {
    data.chunks(16)
        .enumerate()
        .map(|(i, c)| {
            let hex: Vec<String> = c.iter().map(|b| format!("{b:02x}")).collect();
            let text: String = c.iter().map(|&b| if (32..127).contains(&b) { b as char } else { '.' }).collect();
            format!("{:04x}  {:<47}  |{text}|", start + i * 16, hex.join(" "))
        })
        .collect()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn hex_list(v: &[u8]) -> String {
    v.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// For a reception holding a sweep: which byte values did not arrive.
pub fn check_sweep(rx: &[u8]) -> Option<String> {
    let body = &rx[find(rx, SWEEP_START)? + SWEEP_START.len()..];
    let (body, ended) = match find(body, SWEEP_END_MARK) {
        // the space before the end marker belongs to the marker
        Some(end) => (&body[..end.saturating_sub(1)], true),
        None => (body, false),
    };
    let unmarked = if ended { "" } else { "; the end marker did not arrive" };
    let expected: Vec<u8> = (0..=255).collect();
    if body == expected.as_slice() {
        return Some("sweep: all 256 byte values arrived unchanged".into());
    }
    let missing: Vec<u8> = expected.iter().copied().filter(|b| !body.contains(b)).collect();
    let extra = body.len().saturating_sub(256 - missing.len());
    let mut s = format!("sweep: {} of 256 byte values arrived", 256 - missing.len());
    if !missing.is_empty() {
        s.push_str(&format!("; missing {}", hex_list(&missing)));
    }
    if extra > 0 {
        s.push_str(&format!("; {extra} extra or changed bytes (see the dump)"));
    } else if missing.is_empty() {
        s.push_str("; some arrived out of order (see the dump)");
    }
    Some(s + unmarked)
}

/// "1-3, 7".
fn ranges(v: &[usize]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let mut j = i;
        while j + 1 < v.len() && v[j + 1] == v[j] + 1 {
            j += 1;
        }
        out.push(if i == j { v[i].to_string() } else { format!("{}-{}", v[i], v[j]) });
        i = j + 1;
    }
    out.join(", ")
}

/// For a reception holding burst lines: which lines arrived, which were damaged, which are missing.
pub fn check_burst(rx: &[u8]) -> Option<String> {
    let mut total = None;
    let mut intact = std::collections::BTreeSet::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut at = 0;
    while let Some(p) = find(&rx[at..], BURST_MARK) {
        let start = at + p;
        at = start + BURST_MARK.len();
        let num = |r: std::ops::Range<usize>| -> Option<usize> {
            std::str::from_utf8(rx.get(at + r.start..at + r.end)?).ok()?.parse().ok()
        };
        let (Some(i), Some(n)) = (num(0..4), num(5..9)) else { continue };
        if n == 0 || i == 0 || i > n || total.is_some_and(|t| t != n) {
            continue;
        }
        total = Some(n);
        seen.insert(i);
        if rx.get(start..start + BURST_LINE) == Some(burst_line(i, n).as_slice()) {
            intact.insert(i);
        }
    }
    let total = total?;
    let missing: Vec<usize> = (1..=total).filter(|i| !seen.contains(i)).collect();
    let damaged: Vec<usize> = seen.iter().copied().filter(|i| !intact.contains(i)).collect();
    if missing.is_empty() && damaged.is_empty() {
        return Some(format!("burst: all {total} lines arrived intact ({} bytes)", total * BURST_LINE));
    }
    let mut s = format!("burst: {} of {total} lines arrived intact", intact.len());
    if !damaged.is_empty() {
        s.push_str(&format!("; damaged {}", ranges(&damaged)));
    }
    if !missing.is_empty() {
        s.push_str(&format!("; missing {}", ranges(&missing)));
    }
    Some(s)
}

/// Collects what arrives on the data link: shows it as a hex dump as it comes, and once the link has been
/// quiet for `RX_GAP`, sums up the reception and checks it for a sweep or burst sent by another term73.
#[derive(Default)]
pub struct Monitor {
    rx: Vec<u8>,
    shown: usize,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Monitor {
    /// Bytes as they arrive; `clock` labels the start of a reception. Returns lines to show.
    pub fn feed(&mut self, data: &[u8], now: Instant, clock: &str) -> Vec<String> {
        if data.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.first.is_none() {
            self.first = Some(now);
            out.push(format!("reception at {clock}"));
        }
        self.last = Some(now);
        self.rx.extend_from_slice(data);
        let whole = (self.rx.len() - self.shown) / 16 * 16;
        out.extend(hexdump(self.shown, &self.rx[self.shown..self.shown + whole]));
        self.shown += whole;
        out
    }

    /// Call regularly: shows a partial last line after a short pause and ends the reception after `RX_GAP`.
    pub fn poll(&mut self, now: Instant) -> Vec<String> {
        let Some(last) = self.last else { return Vec::new() };
        let quiet = now.duration_since(last);
        let mut out = Vec::new();
        if quiet >= DUMP_PAUSE && self.shown < self.rx.len() {
            out.extend(hexdump(self.shown, &self.rx[self.shown..]));
            self.shown = self.rx.len();
        }
        if quiet >= RX_GAP {
            out.extend(self.flush());
        }
        out
    }

    /// End the reception now: the rest of the dump and the summary.
    pub fn flush(&mut self) -> Vec<String> {
        let Some((first, last)) = self.first.zip(self.last) else { return Vec::new() };
        let mut out = hexdump(self.shown, &self.rx[self.shown..]);
        let rx = std::mem::take(&mut self.rx);
        *self = Monitor::default();
        out.push(format!("reception: {} bytes over {:.1} s", rx.len(), last.duration_since(first).as_secs_f64()));
        out.extend(check_sweep(&rx));
        out.extend(check_burst(&rx));
        out
    }
}

/// Holds bytes to send and releases them at `rate` bytes a second; without a rate, all at once.
#[derive(Default)]
pub struct Pacer {
    rate: Option<u32>,
    queue: VecDeque<u8>,
    /// When the current run of writes started, and the bytes written in it.
    run: Option<(Instant, usize)>,
}

impl Pacer {
    pub fn new(rate: Option<u32>) -> Self {
        Pacer { rate, ..Default::default() }
    }

    pub fn rate(&self) -> Option<u32> {
        self.rate
    }

    pub fn set_rate(&mut self, rate: Option<u32>) {
        self.rate = rate;
        self.run = None;
    }

    pub fn push(&mut self, data: &[u8]) {
        self.queue.extend(data);
    }

    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.run = None;
    }

    /// The bytes that may be written now.
    pub fn take(&mut self, now: Instant) -> Vec<u8> {
        if self.queue.is_empty() {
            return Vec::new();
        }
        let (start, sent) = self.run.get_or_insert((now, 0));
        let n = match self.rate {
            None => self.queue.len(),
            // the first byte goes at once, then `rate` a second
            Some(r) => ((now.duration_since(*start).as_secs_f64() * r as f64) as usize + 1)
                .saturating_sub(*sent)
                .min(self.queue.len()),
        };
        *sent += n;
        self.queue.drain(..n).collect()
    }

    /// Once the last queued byte is written: how many bytes the run had and how long it took.
    pub fn finished(&mut self, now: Instant) -> Option<(usize, Duration)> {
        if !self.queue.is_empty() {
            return None;
        }
        self.run.take().map(|(start, n)| (n, now.duration_since(start)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_input() {
        assert_eq!(parse_hex("24 24 0d").unwrap(), vec![0x24, 0x24, 0x0d]);
        assert_eq!(parse_hex("24240D").unwrap(), vec![0x24, 0x24, 0x0d]);
        assert_eq!(parse_hex("0x24,0x0d").unwrap(), vec![0x24, 0x0d]);
        for bad in ["", "2", "zz", "24 2"] {
            assert!(parse_hex(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn lines_that_look_like_commands_are_found() {
        for bad in [&b"hello\r"[..], b"$$Msg\rTX\r", b"  FQ 1\r", b"0M PROGRAM\r", b"#ok\nsr\r", b"ab"] {
            assert!(command_like(bad).is_some(), "{:?}", String::from_utf8_lossy(bad));
        }
        assert_eq!(command_like(b"$$x\rTX 1\r").as_deref(), Some("TX 1"));
        for ok in [&b"$$Msg,N0CALL,CQCQCQ\r"[..], b"[SOB]x[EOB]", b"#T73\r", b"a\r", b"\r\r", b" 1\r"] {
            assert_eq!(command_like(ok), None, "{:?}", String::from_utf8_lossy(ok));
        }
        assert_eq!(command_like(&sweep_payload()), None);
        assert_eq!(command_like(&burst_payload(500)), None);
    }

    #[test]
    fn sweep_round_trip_and_losses() {
        let p = sweep_payload();
        assert_eq!(p.len(), SWEEP_START.len() + 256 + SWEEP_END.len());
        assert_eq!(check_sweep(&p).unwrap(), "sweep: all 256 byte values arrived unchanged");
        let lossy: Vec<u8> = p.iter().copied().enumerate()
            .filter(|&(i, b)| !(i >= SWEEP_START.len() && i < SWEEP_START.len() + 256 && (b == 0x11 || b == 0x13)))
            .map(|(_, b)| b).collect();
        assert_eq!(check_sweep(&lossy).unwrap(), "sweep: 254 of 256 byte values arrived; missing 11 13");
        let mut escaped = p.clone();
        escaped.insert(SWEEP_START.len() + 0xE7, 0xEF);
        assert_eq!(check_sweep(&escaped).unwrap(),
                   "sweep: 256 of 256 byte values arrived; 1 extra or changed bytes (see the dump)");
        let cut = &p[..SWEEP_START.len() + 100];
        assert!(check_sweep(cut).unwrap().ends_with("; the end marker did not arrive"));
        assert_eq!(check_sweep(b"?\r"), None);
    }

    #[test]
    fn burst_lines_are_numbered_and_checked() {
        let p = burst_payload(100);
        assert_eq!(p.len(), 4 * BURST_LINE);
        assert!(p.starts_with(b"#T73 BURST 0001/0004 "));
        assert!(p.chunks(BURST_LINE).all(|l| l.len() == BURST_LINE && l.ends_with(b"\r")));
        assert_eq!(check_burst(&p).unwrap(), "burst: all 4 lines arrived intact (128 bytes)");
        let mut lossy = p.clone();
        lossy.drain(2 * BURST_LINE..3 * BURST_LINE); // line 3 lost
        lossy[BURST_LINE + 25] = b'?'; // line 2 damaged
        assert_eq!(check_burst(&lossy).unwrap(), "burst: 2 of 4 lines arrived intact; damaged 2; missing 3");
        let tail = &p[2 * BURST_LINE..];
        assert_eq!(check_burst(tail).unwrap(), "burst: 2 of 4 lines arrived intact; missing 1-2");
        assert_eq!(ranges(&[1, 2, 3, 5, 7, 8]), "1-3, 5, 7-8");
        assert_eq!(check_burst(b"#T73 BURST xx"), None);
    }

    #[test]
    fn dump_lines() {
        assert_eq!(hexdump(0, b"$$Msg\r"), vec![format!("0000  {:<47}  |$$Msg.|", "24 24 4d 73 67 0d")]);
        let d = hexdump(16, &[0x41; 17]);
        assert_eq!(d.len(), 2);
        assert!(d[0].starts_with("0010  41 41") && d[1].starts_with("0020  41 "), "{d:?}");
    }

    #[test]
    fn monitor_dumps_then_sums_up() {
        let t0 = Instant::now();
        let mut m = Monitor::default();
        assert!(m.poll(t0).is_empty());
        let out = m.feed(&[0x41; 20], t0, "12:00:00");
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0], "reception at 12:00:00");
        assert!(out[1].starts_with("0000  41"));
        assert!(m.poll(t0 + Duration::from_millis(100)).is_empty());
        let out = m.poll(t0 + Duration::from_millis(400));
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("0010  41 41 41 41  "), "{out:?}");
        let out = m.poll(t0 + RX_GAP);
        assert_eq!(out, vec!["reception: 20 bytes over 0.0 s"]);
        assert!(m.poll(t0 + RX_GAP * 2).is_empty());

        let p = sweep_payload();
        m.feed(&p[..100], t0, "12:00:05");
        m.feed(&p[100..], t0 + Duration::from_millis(500), "12:00:05");
        let out = m.flush();
        assert!(out.contains(&format!("reception: {} bytes over 0.5 s", p.len())), "{out:?}");
        assert!(out.contains(&"sweep: all 256 byte values arrived unchanged".to_string()), "{out:?}");
    }

    #[test]
    fn pacing() {
        let t0 = Instant::now();
        let mut p = Pacer::default();
        p.push(b"abc");
        assert_eq!(p.take(t0), b"abc");
        assert_eq!(p.finished(t0), Some((3, Duration::ZERO)));
        assert_eq!(p.finished(t0), None);

        let mut p = Pacer::new(Some(100));
        p.push(&[0; 200]);
        assert_eq!(p.take(t0).len(), 1);
        assert_eq!(p.take(t0 + Duration::from_millis(500)).len(), 50);
        assert_eq!(p.pending(), 149);
        assert_eq!(p.finished(t0), None);
        assert_eq!(p.take(t0 + Duration::from_secs(5)).len(), 149);
        assert_eq!(p.finished(t0 + Duration::from_secs(2)), Some((200, Duration::from_secs(2))));
    }
}
