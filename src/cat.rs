//! Rig-control (CAT) commands for the TH-D74 / TH-D75 / TM-D750 family:
//! the command table, the safety rules, and request/reply matching.

use std::time::{Duration, Instant};

use crate::link::Link;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Safe to send bare.
    Read,
    /// Changes radio state.
    Act,
    /// Can transmit.
    Tx,
    /// Always refused.
    Never,
}

/// Commands known on this radio family, from public command notes and live
/// read-only probing. Meanings with "?" are unknown.
pub const COMMANDS: &[(&str, Risk, &str)] = &[
    ("AI", Risk::Read, "report every change unasked (AI 1 is refused)"),
    ("AG", Risk::Read, "AF gain"),
    ("BC", Risk::Read, "band control (bare = read, BC n = set)"),
    ("BY", Risk::Read, "squelch open on band b (BY b)"),
    ("DL", Risk::Read, "dual/single band"),
    ("DW", Risk::Act, "step frequency down"),
    ("ME", Risk::Read, "memory channel contents (ME nnn; ME nnn,... writes or erases and is refused)"),
    ("MR", Risk::Read, "memory channel recall (MR b = read, MR b,nnn = set)"),
    ("PC", Risk::Read, "output power (PC b)"),
    ("RX", Risk::Act, "return to receive"),
    ("SQ", Risk::Read, "squelch level (SQ b)"),
    ("SR", Risk::Never, "reset"),
    ("SH", Risk::Read, "filter cutoff/width for SSB, CW, AM (SH m)"),
    ("TX", Risk::Tx, "transmit"),
    ("UP", Risk::Act, "step frequency up"),
    ("VM", Risk::Read, "VFO/memory/call mode (VM b)"),
    ("FQ", Risk::Read, "frequency (FQ b)"),
    ("FO", Risk::Read, "frequency and tone detail (FO b)"),
    ("PS", Risk::Read, "power status (bare only; PS 0 may power off)"),
    ("FV", Risk::Read, "firmware version"),
    ("BE", Risk::Tx, "beacon transmit"),
    ("ID", Risk::Read, "model identity"),
    ("CS", Risk::Read, "callsign"),
    ("TN", Risk::Read, "TNC mode (bare = read, TN m,b = set)"),
    ("BL", Risk::Read, "battery level / backlight"),
    ("GP", Risk::Read, "GPS on/off, PC output"),
    ("GM", Risk::Read, "GPS-only mode (GM 1 turns the radio off and is refused)"),
    ("SM", Risk::Read, "S-meter squelch on band b (SM b)"),
    ("RA", Risk::Read, "attenuator (RA b)"),
    ("BT", Risk::Read, "Bluetooth on/off (BT with a value is refused)"),
    ("FS", Risk::Read, "fine step (FS b)"),
    ("FT", Risk::Read, "fine tune"),
    ("MD", Risk::Read, "mode (MD b)"),
    ("SF", Risk::Read, "frequency step (SF b)"),
    ("VD", Risk::Read, "VOX delay"),
    ("VG", Risk::Read, "VOX gain"),
    ("VX", Risk::Read, "VOX on/off"),
    ("IO", Risk::Read, "IF output mode"),
    ("BS", Risk::Read, "band scope?"),
    ("LC", Risk::Read, "lock"),
    ("GS", Risk::Read, "GPS sentence selection?"),
    ("MS", Risk::Read, "APRS position source"),
    ("PT", Risk::Read, "?"),
    ("AS", Risk::Read, "?"),
    ("DC", Risk::Read, "D-STAR callsign slot (DC n)"),
    ("DS", Risk::Read, "D-STAR slot?"),
    ("RT", Risk::Read, "?"),
    ("FR", Risk::Read, "FM broadcast radio"),
    ("US", Risk::Read, "?"),
    ("GW", Risk::Read, "DV gateway?"),
    ("SD", Risk::Read, "SD card?"),
    ("AE", Risk::Read, "serial number / region"),
    ("TY", Risk::Read, "type / region"),
];

pub fn risk(name: &str) -> Option<Risk> {
    COMMANDS.iter().find(|(n, _, _)| *n == name).map(|(_, r, _)| *r)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    ResetOrService(String),
    Transmits(String),
    PowerOff,
    MemoryWrite,
    /// A setting that would cut this connection or stop the radio answering.
    DropsLink(&'static str),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::ResetOrService(n) => write!(f, "{n} is a reset or service command"),
            Refused::Transmits(n) => write!(f, "{n} can transmit; allow transmitting first"),
            Refused::PowerOff => write!(f, "PS with an argument can power the radio off"),
            Refused::MemoryWrite => write!(f, "ME with more than a channel number writes or erases a memory channel"),
            Refused::DropsLink(why) => write!(f, "{why}"),
        }
    }
}

/// Refuse reset/service commands always, transmit commands unless allowed, and PS with an argument.
pub fn check_allowed(line: &str, allow_tx: bool) -> Result<(), Refused> {
    let line = line.trim();
    let name: String = line.chars().take(2).collect::<String>().to_ascii_uppercase();
    if name.starts_with(|c: char| c.is_ascii_digit()) || risk(&name) == Some(Risk::Never) {
        return Err(Refused::ResetOrService(name));
    }
    if risk(&name) == Some(Risk::Tx) && !allow_tx {
        return Err(Refused::Transmits(name));
    }
    if name == "PS" && !line.eq_ignore_ascii_case("PS") {
        return Err(Refused::PowerOff);
    }
    // "ME nnn" reads a memory channel; "ME nnn," erases it and "ME nnn,<fields>" overwrites it (Hamlib thd74.c)
    if name == "ME" && line.contains(',') {
        return Err(Refused::MemoryWrite);
    }
    let arg = line.get(2..).unwrap_or("").trim();
    match (name.as_str(), arg) {
        ("BT", a) if !a.is_empty() => return Err(Refused::DropsLink("BT with a value can turn Bluetooth off and drop this connection")),
        ("GM", "1") => return Err(Refused::DropsLink("GM 1 puts the radio in GPS-only mode, with the radio off")),
        ("AI", "1") => return Err(Refused::DropsLink("AI 1 makes the radio report every change unasked, which confuses replies")),
        _ => {}
    }
    Ok(())
}

/// Discard input until the link has been quiet for `quiet`. Returns what was discarded.
pub fn drain(link: &mut dyn Link, quiet: Duration) -> Vec<u8> {
    let mut stale = Vec::new();
    let mut end = Instant::now() + quiet;
    while Instant::now() < end {
        match link.read() {
            Ok(d) if !d.is_empty() => {
                stale.extend(d);
                end = Instant::now() + quiet;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    stale
}

/// Send one command and return its reply line (without CR), or "" on timeout.
/// Only a line that starts with the command name, or is "?" / "N", counts as
/// the reply; stale and unrelated lines (some radios repeat replies) are skipped.
pub fn cat(link: &mut dyn Link, line: &str, timeout: Duration, allow_tx: bool) -> Result<String, Refused> {
    check_allowed(line, allow_tx)?;
    let line = line.trim();
    let name: String = line.chars().take(2).collect::<String>().to_ascii_uppercase();
    drain(link, Duration::from_millis(150));
    let mut out = line.as_bytes().to_vec();
    out.push(b'\r');
    if link.write(&out).is_err() {
        return Ok(String::new());
    }
    let mut buf: Vec<u8> = Vec::new();
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        match link.read() {
            Ok(d) => buf.extend(d),
            Err(_) => break,
        }
        while let Some(pos) = buf.iter().position(|&b| b == b'\r') {
            let raw: Vec<u8> = buf.drain(..=pos).collect();
            let reply = String::from_utf8_lossy(&raw[..raw.len() - 1]).trim().to_string();
            if reply == "?" || reply == "N" || reply.get(..2).map(|p| p.eq_ignore_ascii_case(&name)) == Some(true) {
                return Ok(reply);
            }
        }
    }
    Ok(String::from_utf8_lossy(&buf).trim().to_string())
}

/// Leave KISS mode. Some radios act on a KISS frame only when the next byte
/// arrives, so a CR follows the exit frame and its "?" reply is drained.
pub fn kiss_off(link: &mut dyn Link) {
    let _ = link.write(&crate::kiss::EXIT);
    std::thread::sleep(Duration::from_millis(300));
    let _ = link.write(b"\r");
    std::thread::sleep(Duration::from_millis(500));
    drain(link, Duration::from_millis(150));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::testing::EchoRadio;

    #[test]
    fn safety_rules() {
        for l in ["TX", "BE", "SR", "0M PROGRAM", "0G KENWOOD", "PS 0"] {
            assert!(check_allowed(l, false).is_err(), "{l}");
        }
        assert!(check_allowed("TX", true).is_ok());
        assert!(check_allowed("SR", true).is_err());
        assert!(check_allowed("9Z", true).is_err());
        assert!(check_allowed("PS", false).is_ok());
        assert!(check_allowed("FQ 0", false).is_ok());
        assert!(check_allowed("ME 005", false).is_ok());
        assert_eq!(check_allowed("ME 005,", true), Err(Refused::MemoryWrite));
        for l in ["BT 0", "BT 1", "GM 1", "AI 1"] {
            assert!(matches!(check_allowed(l, true), Err(Refused::DropsLink(_))), "{l}");
        }
        for l in ["BT", "GM", "GM 0", "AI", "AI 0", "MS"] {
            assert!(check_allowed(l, false).is_ok(), "{l}");
        }
        assert_eq!(check_allowed("me 005,0145030000,0", true), Err(Refused::MemoryWrite));
    }

    #[test]
    fn stale_and_unrelated_lines_are_skipped() {
        let mut r = EchoRadio::default();
        r.pending.extend(b"?\r");                // sent by some radios on connect
        assert_eq!(cat(&mut r, "ID", Duration::from_millis(300), false).unwrap(), "ID ok");
        r.pending.extend(b"XX junk\r");
        assert_eq!(cat(&mut r, "FV", Duration::from_millis(300), false).unwrap(), "FV ok");
    }

    #[test]
    fn table_size() {
        assert_eq!(COMMANDS.len(), 53);
    }
}
