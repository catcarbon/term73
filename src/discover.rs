//! Safe capability discovery for Bluetooth-serial TNC radios.
//!
//! Never transmits and only sends commands from a curated read-only list.
//! Frequency, power and TNC mode are changed briefly and always put back.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cat::{self, drain};
use crate::link::Link;

/// Read-only commands safe to send bare, and ones that take a band argument.
/// Never add TX, BE, MS, SR, RX, UP, DW, PS <n> or service commands here.
pub const SAFE_BARE: &[&str] = &["ID", "FV", "TY", "AE", "TN", "BC", "DL", "AI", "AG", "BL", "BT", "GP", "GM", "GS",
    "VD", "VG", "VX", "LC", "BS", "PT", "AS", "DS", "RT", "FR", "GW", "SD", "IO", "FS", "FT", "CS"];
pub const SAFE_BAND: &[&str] = &["FQ", "FO", "MD", "PC", "SQ", "SM", "BY", "RA", "VM", "MR", "SF", "FS"];
/// Candidate band edges (MHz), set and read back on the data band, then restored.
pub const BAND_PROBES: &[f64] = &[28.0, 50.0, 118.0, 136.0, 144.0, 147.995, 174.0, 222.0, 224.995, 420.0, 440.0,
    449.995, 470.0];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Discovery {
    pub id: String,
    pub firmware: String,
    pub data_band: u8,
    pub stray_on_connect: String,
    pub duplicate_replies: bool,
    /// command form -> reply shape, e.g. "FO 1" -> "FO:19"; "?" when unsupported
    pub commands: Vec<(String, String)>,
    pub power_levels: Vec<u8>,
    pub power_restored: bool,
    pub accepted_mhz: Vec<f64>,
    pub freq_restored: bool,
    pub kiss_enter: bool,
    pub kiss_exit_needs_trailer: Option<bool>,
    pub kiss_left_cleanly: bool,
}

/// Reply shape: command name and the number of comma fields ("FO:19"), or "?", "N", "none".
pub fn shape(reply: &str) -> String {
    if reply.is_empty() {
        return "none".into();
    }
    if reply == "?" || reply == "N" {
        return reply.into();
    }
    match reply.split_once(' ') {
        Some((head, rest)) => format!("{head}:{}", rest.split(',').count()),
        None => format!("{reply}:0"),
    }
}

fn lines_for(link: &mut dyn Link, cmd: &str, wait: Duration) -> usize {
    drain(link, Duration::from_millis(150));
    let _ = link.write(format!("{cmd}\r").as_bytes());
    let mut buf = Vec::new();
    let end = Instant::now() + wait;
    while Instant::now() < end {
        if let Ok(d) = link.read() {
            buf.extend(d);
        }
    }
    buf.split(|&b| b == b'\r').filter(|l| !l.is_empty()).count()
}

fn q(link: &mut dyn Link, line: &str) -> String {
    cat::cat(link, line, Duration::from_millis(1500), false).unwrap_or_default()
}

pub fn discover(link: &mut dyn Link, data_band: Option<u8>, log: &mut dyn FnMut(String)) -> Discovery {
    let mut d = Discovery::default();
    std::thread::sleep(Duration::from_millis(300));
    d.stray_on_connect = String::from_utf8_lossy(&drain(link, Duration::from_millis(400))).into();
    d.id = q(link, "ID").trim_start_matches("ID ").to_string();
    d.firmware = q(link, "FV").trim_start_matches("FV ").to_string();
    log(format!("discover: {} firmware {}", d.id, d.firmware));

    let counts: Vec<usize> = (0..5).map(|_| lines_for(link, "ID", Duration::from_millis(600))).collect();
    d.duplicate_replies = counts.iter().any(|&c| c > 1);
    log(format!("discover: replies per command {counts:?}"));

    let tn = q(link, "TN");
    d.data_band = data_band.unwrap_or_else(|| tn.split(',').nth(1).and_then(|b| b.parse().ok()).unwrap_or(0));
    let band = d.data_band;
    for c in SAFE_BARE {
        d.commands.push((c.to_string(), shape(&q(link, c))));
    }
    for c in SAFE_BAND {
        for b in 0..2 {
            let form = format!("{c} {b}");
            let r = shape(&q(link, &form));
            d.commands.push((form, r));
        }
    }
    let answered = d.commands.iter().filter(|(_, s)| !matches!(s.as_str(), "?" | "N" | "none")).count();
    log(format!("discover: {answered} of {} read forms answered", d.commands.len()));

    let pc = q(link, &format!("PC {band}"));
    if let Some(orig) = pc.strip_prefix(&format!("PC {band},")).and_then(|v| v.parse::<u8>().ok()) {
        for lv in 0..6u8 {
            if q(link, &format!("PC {band},{lv}")) == format!("PC {band},{lv}") && q(link, &format!("PC {band}")) == format!("PC {band},{lv}") {
                d.power_levels.push(lv);
            }
        }
        q(link, &format!("PC {band},{orig}"));
        d.power_restored = q(link, &format!("PC {band}")) == format!("PC {band},{orig}");
        log(format!("discover: power levels {:?}", d.power_levels));
    }

    let fq = q(link, &format!("FQ {band}"));
    if let Some(orig) = fq.split(',').nth(1).map(str::to_string) {
        for &mhz in BAND_PROBES {
            let want = format!("{:010}", (mhz * 1e6).round() as u64);
            q(link, &format!("FQ {band},{want}"));
            if q(link, &format!("FQ {band}")) == format!("FQ {band},{want}") {
                d.accepted_mhz.push(mhz);
            }
        }
        q(link, &format!("FQ {band},{orig}"));
        d.freq_restored = q(link, &format!("FQ {band}")) == format!("FQ {band},{orig}");
        log(format!("discover: data band accepts {:?}", d.accepted_mhz));
    }

    d.kiss_enter = q(link, &format!("TN 2,{band}")) == format!("TN 2,{band}");
    if d.kiss_enter {
        std::thread::sleep(Duration::from_millis(500));
        let _ = link.write(&crate::kiss::EXIT);
        std::thread::sleep(Duration::from_secs(1));
        if q(link, "TN").starts_with("TN ") {
            d.kiss_exit_needs_trailer = Some(false);
        } else {
            let _ = link.write(b"\r");
            std::thread::sleep(Duration::from_millis(500));
            drain(link, Duration::from_millis(150));
            d.kiss_exit_needs_trailer = Some(q(link, "TN").starts_with("TN "));
        }
        d.kiss_left_cleanly = q(link, "TN").starts_with("TN ");
        log(format!("discover: leaving KISS needs a trailing byte: {:?}", d.kiss_exit_needs_trailer));
    }
    d
}

/// Group accepted probe points into packet ranges (both edges must be accepted).
pub fn detected_bands(accepted: &[f64]) -> Option<Vec<(f64, f64)>> {
    let ranges = [(144.0, 148.0, 144.0, 147.995), (222.0, 225.0, 222.0, 224.995), (420.0, 450.0, 420.0, 449.995)];
    let has = |v: f64| accepted.iter().any(|a| (a - v).abs() < 1e-9);
    let out: Vec<(f64, f64)> = ranges.iter().filter(|r| has(r.2) && has(r.3)).map(|r| (r.0, r.1)).collect();
    if out.is_empty() { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_and_bands() {
        assert_eq!(shape("FO 1,0145030000,0,0"), "FO:4");
        assert_eq!(shape("?"), "?");
        assert_eq!(shape(""), "none");
        assert_eq!(detected_bands(&[144.0, 147.995, 420.0, 440.0, 449.995]), Some(vec![(144.0, 148.0), (420.0, 450.0)]));
        assert_eq!(detected_bands(&[28.0]), None);
    }
}

/// True for a command that can only read: a bare command from `SAFE_BARE`, a `SAFE_BAND`
/// command with a single band digit, or `ME` with a three-digit channel. Anything with more
/// arguments may be a setting (for example "AG 010" sets the volume), so it is not a read form.
pub fn is_read_form(line: &str) -> bool {
    let line = line.trim().to_ascii_uppercase();
    let (name, arg) = line.split_once(' ').map(|(n, a)| (n, a.trim())).unwrap_or((line.as_str(), ""));
    match arg {
        "" => SAFE_BARE.contains(&name),
        a if a.len() == 1 && a.chars().all(|c| c.is_ascii_digit()) => SAFE_BAND.contains(&name),
        a if name == "ME" && a.len() == 3 && a.chars().all(|c| c.is_ascii_digit()) => true,
        _ => false,
    }
}

/// Field-by-field changes between two replies to the same read ("FO 1,..." style), as "field 11: 0 -> 2".
pub fn field_changes(before: &str, after: &str) -> Vec<String> {
    let fields = |r: &str| -> Vec<String> {
        r.split_once(' ').map(|(_, rest)| rest).unwrap_or(r).split(',').map(str::to_string).collect()
    };
    let (a, b) = (fields(before), fields(after));
    (0..a.len().max(b.len()))
        .filter_map(|i| {
            let (x, y) = (a.get(i).map(String::as_str).unwrap_or("-"), b.get(i).map(String::as_str).unwrap_or("-"));
            (x != y).then(|| format!("field {i}: {x} -> {y}"))
        })
        .collect()
}

#[cfg(test)]
mod read_form_tests {
    use super::*;

    #[test]
    fn only_reads_pass() {
        for ok in ["FO 1", "fo 0", "BC", "SQ 1", "ME 900", "AG", "RT"] {
            assert!(is_read_form(ok), "{ok}");
        }
        for no in ["FO 1,0145030000", "AG 010", "BC 1", "ME 900,", "TX", "SR", "FQ 12", "VX 1"] {
            assert!(!is_read_form(no), "{no}");
        }
    }

    #[test]
    fn changes_are_numbered_from_the_first_argument() {
        let a = "FO 1,0145030000,0000600000,2,2,0,0,0,0,0,0,0,08";
        let b = "FO 1,0145030000,0000600000,2,2,0,0,0,0,0,0,2,08";
        assert_eq!(field_changes(a, b), vec!["field 11: 0 -> 2"]);
        assert!(field_changes(a, a).is_empty());
    }
}
