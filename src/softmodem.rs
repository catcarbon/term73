//! Software modems (modem73 and any KISS-over-TCP TNC).
//!
//! `ModemControl` talks to modem73's JSON control port (4-byte big-endian
//! length + JSON). The rig-control server a modem uses to key the radio is in `rigctld`.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::{json, Value};

#[derive(Clone)]
pub struct ModemControl {
    pub addr: String,
}

impl ModemControl {
    pub fn new(addr: &str) -> Self {
        ModemControl { addr: addr.to_string() }
    }

    pub fn request(&self, cmd: &str, fields: Value) -> io::Result<Value> {
        let sock = self.addr.parse().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?;
        let mut s = TcpStream::connect_timeout(&sock, Duration::from_secs(5))?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut body = if fields.is_object() { fields } else { json!({}) };
        body["cmd"] = json!(cmd);
        let bytes = serde_json::to_vec(&body)?;
        s.write_all(&(bytes.len() as u32).to_be_bytes())?;
        s.write_all(&bytes)?;
        let mut head = [0u8; 4];
        s.read_exact(&mut head)?;
        let mut resp = vec![0u8; u32::from_be_bytes(head) as usize];
        s.read_exact(&mut resp)?;
        Ok(serde_json::from_slice(&resp)?)
    }
}
