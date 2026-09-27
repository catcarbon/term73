//! Per-user configuration, radio profiles, BBS profiles and the gateway list,
//! stored only in term73's own folder:
//!   Windows `%APPDATA%\term73`, macOS `~/Library/Application Support/term73`,
//!   Linux `$XDG_CONFIG_HOME/term73` (usually `~/.config/term73`).
//! `TERM73_HOME` overrides it (used by the tests).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub fn home() -> PathBuf {
    if let Ok(h) = std::env::var("TERM73_HOME") {
        return PathBuf::from(h);
    }
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("term73")
}

pub fn path(name: &str) -> PathBuf {
    home().join(name)
}

pub(crate) fn load<T: DeserializeOwned + Default>(name: &str) -> T {
    fs::read_to_string(path(name)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub(crate) fn save<T: Serialize>(name: &str, value: &T) -> io::Result<()> {
    fs::create_dir_all(home())?;
    let tmp = path(&format!("{name}.tmp"));
    fs::write(&tmp, serde_json::to_string_pretty(value)?)?;
    fs::rename(tmp, path(name))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AppConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winlink_api_key: Option<String>,
}

impl AppConfig {
    pub fn load() -> Self {
        load("config.json")
    }
    pub fn save(&self) -> io::Result<()> {
        save("config.json", self)
    }
}

/// What term73 knows about one rig. Every field is optional: unknown means "use the default".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RadioProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_band: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_levels: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_high: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bands_mhz: Option<Vec<(f64, f64)>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shift_field: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_replies: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kiss_exit_trailer: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ptt_verified: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t1: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paclen: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<usize>,
    // software modem rigs (modem73)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kiss_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tune_via_modem: Option<bool>,
}

pub fn load_radios() -> BTreeMap<String, RadioProfile> {
    load("radios.json")
}

pub fn save_radio(key: &str, profile: &RadioProfile) -> io::Result<()> {
    let mut all = load_radios();
    all.insert(key.to_ascii_uppercase(), profile.clone());
    save("radios.json", &all)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bbs {
    pub call: String,
    pub mhz: f64,
    #[serde(default)]
    pub path: Vec<String>,
    #[serde(default = "default_baud")]
    pub baud: u32,
    /// AX.25 version learned on the last connect ("2.2" or "2.0"); None until the first connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ax25: Option<String>,
}

fn default_baud() -> u32 {
    1200
}

pub fn load_bbs() -> BTreeMap<String, Bbs> {
    load("bbs.json")
}

pub fn save_bbs(all: &BTreeMap<String, Bbs>) -> io::Result<()> {
    save("bbs.json", all)
}

pub fn rmslist_path() -> PathBuf {
    path("rmslist.json")
}

/// Amateur callsign with optional SSID 0-15, e.g. N0CALL, N0CALL-7, 2E0XYZ.
pub fn valid_call(call: &str) -> bool {
    let up = call.to_ascii_uppercase();
    let (base, ssid) = match up.split_once('-') {
        Some((b, s)) => (b, Some(s)),
        None => (up.as_str(), None),
    };
    if let Some(s) = ssid {
        match s.parse::<u8>() {
            Ok(n) if n <= 15 && !s.starts_with('0') || s == "0" => {}
            _ => return false,
        }
    }
    let b = base.as_bytes();
    if !(3..=7).contains(&b.len()) || !b.iter().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    // prefix of 1-3 chars, then a digit, then 0-3 chars ending in a letter
    let last_digit = match b.iter().rposition(|c| c.is_ascii_digit()) {
        Some(i) => i,
        None => return false,
    };
    (1..=3).contains(&last_digit) && b.len() - last_digit > 1 && b.len() - last_digit - 1 <= 4
        && b[b.len() - 1].is_ascii_alphabetic()
}

/// Maidenhead locator, 4, 6 or 8 characters.
pub fn valid_grid(grid: &str) -> bool {
    let g: Vec<char> = grid.chars().collect();
    let pair = |i: usize, lo: char, hi: char| {
        g.get(i).is_some_and(|c| (lo..=hi).contains(&c.to_ascii_uppercase()))
            && g.get(i + 1).is_some_and(|c| (lo..=hi).contains(&c.to_ascii_uppercase()))
    };
    let digits = |i: usize| g.get(i).is_some_and(|c| c.is_ascii_digit()) && g.get(i + 1).is_some_and(|c| c.is_ascii_digit());
    match g.len() {
        4 => pair(0, 'A', 'R') && digits(2),
        6 => pair(0, 'A', 'R') && digits(2) && pair(4, 'A', 'X'),
        8 => pair(0, 'A', 'R') && digits(2) && pair(4, 'A', 'X') && digits(6),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callsigns() {
        for good in ["N0CALL", "N0CALL-7", "N0GW-10", "VE3ABC", "2E0XYZ", "N0CALL-0"] {
            assert!(valid_call(good), "{good}");
        }
        for bad in ["NOTACALL", "N0CALL-16", "", "8", "N0CALL-07"] {
            assert!(!valid_call(bad), "{bad}");
        }
    }

    #[test]
    fn grids() {
        assert!(valid_grid("FN31pr"));
        assert!(valid_grid("FN31"));
        assert!(!valid_grid("ZZ99"));
        assert!(!valid_grid("FN3"));
    }
}
