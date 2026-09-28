//! Nearby Winlink packet gateways (from the downloaded gateway list), grid
//! arithmetic, and rig-control tuning with safety checks.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::cat;
use crate::link::Link;

/// Packet-capable transmit ranges (MHz) for known models, used when a profile has none.
pub fn default_bands(model: &str) -> Vec<(f64, f64)> {
    match model {
        "TH-D75" => vec![(144.0, 148.0), (222.0, 225.0), (420.0, 450.0)],
        _ => vec![(144.0, 148.0), (420.0, 450.0)],
    }
}

/// Position of the repeater-shift field in the FO reply, when known for a model.
/// Worked out by comparing a repeater channel with a simplex one over rig control.
pub fn default_shift_field(model: &str) -> Option<usize> {
    match model {
        "TM-D750" => Some(11),
        "TH-D75" => Some(13),
        _ => None,
    }
}

pub fn grid_to_latlon(grid: &str) -> Option<(f64, f64)> {
    let g: Vec<u8> = grid.trim().to_ascii_uppercase().bytes().collect();
    if g.len() < 4 {
        return None;
    }
    let mut lon = (g[0].checked_sub(b'A')? as f64) * 20.0 - 180.0 + ((g[2].checked_sub(b'0')?) as f64) * 2.0;
    let mut lat = (g[1].checked_sub(b'A')? as f64) * 10.0 - 90.0 + ((g[3].checked_sub(b'0')?) as f64);
    if g.len() >= 6 {
        lon += (g[4].checked_sub(b'A')? as f64) * (5.0 / 60.0) + 2.5 / 60.0;
        lat += (g[5].checked_sub(b'A')? as f64) * (2.5 / 60.0) + 1.25 / 60.0;
    } else {
        lon += 1.0;
        lat += 0.5;
    }
    Some((lat, lon))
}

pub fn distance_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (la1, lo1, la2, lo2) = (a.0.to_radians(), a.1.to_radians(), b.0.to_radians(), b.1.to_radians());
    let h = ((la2 - la1) / 2.0).sin().powi(2) + la1.cos() * la2.cos() * ((lo2 - lo1) / 2.0).sin().powi(2);
    6371.0 * 2.0 * h.sqrt().asin()
}

#[derive(Debug, Deserialize)]
struct List {
    #[serde(rename = "Gateways", default)]
    gateways: Vec<Gw>,
}

#[derive(Debug, Deserialize)]
struct Gw {
    #[serde(rename = "Callsign")]
    call: String,
    #[serde(rename = "HoursSinceStatus", default = "far")]
    age: f64,
    #[serde(rename = "GatewayChannels", default)]
    channels: Vec<Ch>,
}

fn far() -> f64 {
    999.0
}

#[derive(Debug, Deserialize)]
struct Ch {
    #[serde(rename = "SupportedModes", default)]
    modes: String,
    #[serde(rename = "Frequency", default)]
    freq_hz: f64,
    #[serde(rename = "Baud", default)]
    baud: serde_json::Value,
    #[serde(rename = "ServiceCode", default)]
    service: Option<String>,
    #[serde(rename = "Gridsquare", default)]
    grid: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub call: String,
    pub mhz: f64,
    pub baud: u32,
    pub km: f64,
    pub age_h: f64,
}

/// Public packet gateways within `max_km`, heard in the last `max_age_h` hours,
/// on frequencies inside `bands`, nearest first (1200 baud before 9600 at the same site).
pub fn nearby(list: &Path, grid: &str, bands: &[(f64, f64)], max_age_h: f64, max_km: f64)
    -> Result<Vec<Candidate>, String> {
    let here = grid_to_latlon(grid).ok_or_else(|| format!("{grid} is not a grid locator"))?;
    let text = std::fs::read_to_string(list).map_err(|e| format!("gateway list: {e}"))?;
    let list: List = serde_json::from_str(&text).map_err(|e| format!("gateway list: {e}"))?;
    let mut out: Vec<Candidate> = Vec::new();
    for gw in list.gateways.iter().filter(|g| g.age <= max_age_h) {
        for ch in &gw.channels {
            let baud: u32 = match &ch.baud {
                serde_json::Value::String(s) => s.parse().unwrap_or(0),
                serde_json::Value::Number(n) => n.as_u64().unwrap_or(0) as u32,
                _ => 0,
            };
            let mhz = ch.freq_hz / 1e6;
            if !ch.modes.starts_with("Packet") || !(baud == 1200 || baud == 9600) {
                continue;
            }
            if ch.service.as_deref().unwrap_or("PUBLIC") != "PUBLIC" || !bands.iter().any(|(lo, hi)| (*lo..=*hi).contains(&mhz)) {
                continue;
            }
            let there = ch.grid.as_deref().and_then(grid_to_latlon).unwrap_or(here);
            let km = (distance_km(here, there) * 10.0).round() / 10.0;
            if km <= max_km && !out.iter().any(|c| c.call == gw.call && (c.mhz - mhz).abs() < 1e-6) {
                out.push(Candidate { call: gw.call.clone(), mhz, baud, km, age_h: gw.age });
            }
        }
    }
    out.sort_by(|a, b| a.km.total_cmp(&b.km).then((a.baud != 1200).cmp(&(b.baud != 1200))));
    Ok(out)
}

const T: Duration = Duration::from_secs(2);

fn reply(link: &mut dyn Link, line: &str) -> String {
    cat::cat(link, line, T, false).unwrap_or_default()
}

pub fn radio_model(link: &mut dyn Link) -> Option<String> {
    let r = reply(link, "ID");
    r.strip_prefix("ID ").map(|m| m.trim().to_string())
}

/// Where a band was before `ensure_vfo` switched it: its VM mode and its memory channel.
#[derive(Debug, Clone, PartialEq)]
pub struct MemorySpot {
    pub band: u8,
    pub mode: String,
    pub channel: Option<String>,
}

/// Put `band` in VFO mode (VM b,0) so a frequency change does not land on a memory channel.
/// Returns where the band was when it had to switch. A radio without VM is left alone.
pub fn ensure_vfo(link: &mut dyn Link, band: u8) -> Result<Option<MemorySpot>, String> {
    let vm = reply(link, &format!("VM {band}"));
    let Some(mode) = vm.strip_prefix(&format!("VM {band},")).map(str::to_string) else { return Ok(None) };
    if mode == "0" {
        return Ok(None);
    }
    let channel = reply(link, &format!("MR {band}")).strip_prefix("MR ").map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty() && c.chars().all(|x| x.is_ascii_digit()));
    cat::cat(link, &format!("VM {band},0"), T, false).map_err(|e| e.to_string())?;
    let back = reply(link, &format!("VM {band}"));
    if back != format!("VM {band},0") {
        return Err(format!("could not switch band {band} to VFO mode (it answered {back:?})"));
    }
    Ok(Some(MemorySpot { band, mode, channel }))
}

/// Put a band back where `ensure_vfo` found it: its VM mode, then its memory channel.
pub fn restore_memory(link: &mut dyn Link, spot: &MemorySpot) -> Result<(), String> {
    let b = spot.band;
    cat::cat(link, &format!("VM {b},{}", spot.mode), T, false).map_err(|e| e.to_string())?;
    if reply(link, &format!("VM {b}")) != format!("VM {b},{}", spot.mode) {
        return Err(format!("could not put band {b} back in memory mode"));
    }
    if let Some(ch) = &spot.channel
        && reply(link, &format!("MR {b}")).strip_prefix("MR ").map(str::trim) != Some(ch.as_str()) {
            cat::cat(link, &format!("MR {b},{ch}"), T, false).map_err(|e| e.to_string())?;
        }
    Ok(())
}

/// Set `band` to `mhz`, read it back, and require FM with no repeater shift.
pub fn tune(link: &mut dyn Link, band: u8, mhz: f64, shift_field: Option<usize>) -> Result<(), String> {
    let want = format!("{:010}", (mhz * 1e6).round() as u64);
    cat::cat(link, &format!("FQ {band},{want}"), T, false).map_err(|e| e.to_string())?;
    let got = reply(link, &format!("FQ {band}"));
    if got != format!("FQ {band},{want}") {
        return Err(format!("the radio did not take {mhz:.3} MHz (it answered {got:?})"));
    }
    let md = reply(link, &format!("MD {band}"));
    if md != format!("MD {band},0") {
        return Err(format!("band {band} is not in FM ({md:?}); set FM on the radio"));
    }
    let idx = shift_field.ok_or("the repeater-shift position is unknown for this radio; set it in the radio profile")?;
    let fo = reply(link, &format!("FO {band}"));
    let fields: Vec<&str> = fo.split(',').collect();
    if fields.get(idx) != Some(&"0") {
        return Err(format!("band {band} has a repeater shift set; set simplex on the radio"));
    }
    Ok(())
}

pub fn get_power(link: &mut dyn Link, band: u8) -> Result<u8, String> {
    let r = reply(link, &format!("PC {band}"));
    r.strip_prefix(&format!("PC {band},")).and_then(|v| v.parse().ok()).ok_or(format!("cannot read power ({r:?})"))
}

pub fn set_power(link: &mut dyn Link, band: u8, level: u8) -> Result<(), String> {
    cat::cat(link, &format!("PC {band},{level}"), T, false).map_err(|e| e.to_string())?;
    if get_power(link, band)? != level {
        return Err(format!("the radio did not take power level {level}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_math() {
        let (lat, lon) = grid_to_latlon("FN31pr").unwrap();
        assert!((lat - 41.729).abs() < 0.01 && (lon + 72.708).abs() < 0.01);
        assert!((distance_km((40.0, -77.0), (41.0, -77.0)) - 111.2).abs() < 0.5);
    }

    #[test]
    fn nearby_filters() {
        let dir = std::env::temp_dir().join(format!("t73-gw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("rmslist.json");
        let ch = |f: u64, modes: &str, svc: &str, grid: &str| {
            serde_json::json!({"SupportedModes": modes, "Frequency": f, "Baud": "1200", "ServiceCode": svc, "Gridsquare": grid})
        };
        let data = serde_json::json!({"Gateways": [
            {"Callsign": "NEAR-10", "HoursSinceStatus": 1, "GatewayChannels": [ch(145_090_000, "Packet 1200", "PUBLIC", "FN31pr")]},
            {"Callsign": "STALE-10", "HoursSinceStatus": 99, "GatewayChannels": [ch(145_010_000, "Packet 1200", "PUBLIC", "FN31pr")]},
            {"Callsign": "HF-10", "HoursSinceStatus": 1, "GatewayChannels": [ch(7_101_000, "ARDOP 2000", "PUBLIC", "FN31pr")]},
            {"Callsign": "220-10", "HoursSinceStatus": 1, "GatewayChannels": [ch(223_500_000, "Packet 1200", "PUBLIC", "FN31pr")]},
            {"Callsign": "EMCOMM-10", "HoursSinceStatus": 1, "GatewayChannels": [ch(145_050_000, "Packet 1200", "EMCOMM", "FN31pr")]},
            {"Callsign": "FAR-10", "HoursSinceStatus": 1, "GatewayChannels": [ch(145_030_000, "Packet 1200", "PUBLIC", "EM10")]}
        ]});
        std::fs::write(&p, data.to_string()).unwrap();
        let d750: Vec<String> = nearby(&p, "FN31pr", &default_bands("TM-D750"), 24.0, 150.0).unwrap().into_iter().map(|c| c.call).collect();
        let mut d75: Vec<String> = nearby(&p, "FN31pr", &default_bands("TH-D75"), 24.0, 150.0).unwrap().into_iter().map(|c| c.call).collect();
        d75.sort();
        assert_eq!(d750, vec!["NEAR-10"]);
        assert_eq!(d75, vec!["220-10", "NEAR-10"]);
    }
}
