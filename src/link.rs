//! Byte links to a radio or modem: serial ports, TCP, and (per platform) Bluetooth RFCOMM.
//!
//! `read` returns whatever has arrived (possibly nothing) after a short wait;
//! it never blocks for long, so callers can poll several things in one loop.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub trait Link: Send {
    fn write(&mut self, data: &[u8]) -> io::Result<()>;
    fn read(&mut self) -> io::Result<Vec<u8>>;
}

pub struct SerialLink {
    port: Box<dyn serialport::SerialPort>,
}

impl SerialLink {
    /// Open a serial port. Bluetooth virtual ports ignore the baud rate.
    pub fn open(path: &str, baud: u32) -> io::Result<Self> {
        let port = serialport::new(path, baud)
            .timeout(Duration::from_millis(50))
            .open()
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(SerialLink { port })
    }
}

impl Link for SerialLink {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.port.write_all(data)?;
        self.port.flush()
    }

    fn read(&mut self) -> io::Result<Vec<u8>> {
        let mut buf = [0u8; 4096];
        match self.port.read(&mut buf) {
            Ok(n) => Ok(buf[..n].to_vec()),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }
}

pub struct TcpLink {
    stream: TcpStream,
    pub addr: String,
}

impl TcpLink {
    pub fn connect(addr: &str) -> io::Result<Self> {
        let sock = addr
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{addr}: {e}")))?;
        let stream = TcpStream::connect_timeout(&sock, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(Duration::from_millis(20)))?;
        stream.set_nodelay(true)?;
        Ok(TcpLink { stream, addr: addr.to_string() })
    }
}

impl Link for TcpLink {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.stream.write_all(data)
    }

    fn read(&mut self) -> io::Result<Vec<u8>> {
        let mut buf = [0u8; 65536];
        match self.stream.read(&mut buf) {
            Ok(0) => Err(io::Error::new(io::ErrorKind::ConnectionAborted, format!("{} closed the connection", self.addr))),
            Ok(n) => Ok(buf[..n].to_vec()),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// Answers every CR-terminated line with "<line> ok" and records what was sent.
    #[derive(Default)]
    pub struct EchoRadio {
        pub sent: Vec<Vec<u8>>,
        pub pending: Vec<u8>,
    }

    impl Link for EchoRadio {
        fn write(&mut self, data: &[u8]) -> io::Result<()> {
            self.sent.push(data.to_vec());
            if let Some(line) = data.strip_suffix(b"\r") {
                self.pending.extend_from_slice(line);
                self.pending.extend_from_slice(b" ok\r");
            }
            Ok(())
        }

        fn read(&mut self) -> io::Result<Vec<u8>> {
            Ok(std::mem::take(&mut self.pending))
        }
    }
}

/// Wraps a link and writes every byte in both directions to a timestamped log file:
/// `HH:MM:SS.mmm >> 54 4e 0d  |TN.|` (UTC; >> = to the rig, << = from the rig).
pub struct WireLog<L: Link> {
    inner: L,
    file: Option<std::fs::File>,
    pub path: std::path::PathBuf,
}

impl<L: Link> WireLog<L> {
    pub fn wrap(inner: L, dir: &std::path::Path, label: &str) -> Self {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let safe: String = label.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        let path = dir.join(format!("{safe}-{secs}.log"));
        let file = std::fs::create_dir_all(dir).ok().and_then(|_| std::fs::File::create(&path).ok());
        WireLog { inner, file, path }
    }

    fn record(&mut self, dir: &str, data: &[u8]) {
        let Some(f) = self.file.as_mut() else { return };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let (s, ms) = (now.as_secs() % 86_400, now.subsec_millis());
        let hex: Vec<String> = data.iter().map(|b| format!("{b:02x}")).collect();
        let text: String = data.iter().map(|&b| if (32..127).contains(&b) { b as char } else { '.' }).collect();
        let _ = writeln!(f, "{:02}:{:02}:{:02}.{ms:03} {dir} {}  |{text}|", s / 3600, s / 60 % 60, s % 60, hex.join(" "));
    }
}

impl<L: Link> Link for WireLog<L> {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.record(">>", data);
        self.inner.write(data)
    }

    fn read(&mut self) -> io::Result<Vec<u8>> {
        let d = self.inner.read()?;
        if !d.is_empty() {
            self.record("<<", &d);
        }
        Ok(d)
    }
}

impl Link for Box<dyn Link> {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        (**self).write(data)
    }

    fn read(&mut self) -> io::Result<Vec<u8>> {
        (**self).read()
    }
}

#[cfg(test)]
mod wirelog_tests {
    use super::testing::EchoRadio;
    use super::*;

    #[test]
    fn logs_both_directions() {
        let dir = std::env::temp_dir().join(format!("t73-wirelog-{}", std::process::id()));
        let mut l = WireLog::wrap(EchoRadio::default(), &dir, "COM10");
        l.write(b"ID\r").unwrap();
        l.read().unwrap();
        drop(l.file.take());
        let text = std::fs::read_to_string(&l.path).unwrap();
        assert!(text.contains(">> 49 44 0d  |ID.|"), "{text}");
        assert!(text.contains("<< 49 44 20 6f 6b 0d  |ID ok.|"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
