//! Download the Winlink gateway list into term73's config folder.
//!
//! The Winlink API refuses requests without an access key ("InvalidAccessKey");
//! keys are issued by the Winlink Development Team. The key belongs to the user
//! (or to the project once one is issued) and is never taken from other software.

use std::path::PathBuf;

pub const API: &str = "https://api.winlink.org/gateway/status.json";

/// Check an API reply and keep only the gateway list. Returns (count, JSON to store).
pub fn accept(body: &str) -> Result<(usize, String), String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("Winlink API: bad reply ({e})"))?;
    if let Some(code) = v.pointer("/ResponseStatus/ErrorCode").and_then(|c| c.as_str()) {
        let msg = v.pointer("/ResponseStatus/Message").and_then(|m| m.as_str()).unwrap_or(code);
        return Err(format!("Winlink API: {msg}"));
    }
    let gws = v.get("Gateways").and_then(|g| g.as_array()).cloned().unwrap_or_default();
    if gws.is_empty() {
        return Err("Winlink API returned no gateways".into());
    }
    let n = gws.len();
    Ok((n, serde_json::json!({ "Gateways": gws }).to_string()))
}

pub fn download(key: &str) -> Result<(usize, PathBuf), String> {
    let body = ureq::get(API)
        .query("key", key)
        .query("HistoryHours", "48")
        .query("ServiceCodes", "PUBLIC")
        .timeout(std::time::Duration::from_secs(60))
        .call()
        .map_err(|e| format!("Winlink API: {e}"))?
        .into_string()
        .map_err(|e| format!("Winlink API: {e}"))?;
    let (n, json) = accept(&body)?;
    let path = crate::config::rmslist_path();
    std::fs::create_dir_all(crate::config::home()).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok((n, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_key_and_good_reply() {
        let bad = r#"{"Gateways":[],"ResponseStatus":{"ErrorCode":"InvalidAccessKey","Message":"Invalid access key"}}"#;
        assert_eq!(accept(bad).unwrap_err(), "Winlink API: Invalid access key");
        let good = r#"{"Gateways":[{"Callsign":"N0GW-10"}],"ResponseStatus":{}}"#;
        assert_eq!(accept(good).unwrap().0, 1);
    }
}
