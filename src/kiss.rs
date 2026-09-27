//! KISS framing (FEND/FESC escaping) and a streaming decoder.

pub const FEND: u8 = 0xC0;
pub const FESC: u8 = 0xDB;
pub const TFEND: u8 = 0xDC;
pub const TFESC: u8 = 0xDD;

/// The frame that takes a TNC out of KISS mode.
pub const EXIT: [u8; 3] = [FEND, 0xFF, FEND];

/// KISS command numbers (low nibble of the command byte).
pub mod cmd {
    pub const DATA: u8 = 0;
    pub const TXDELAY: u8 = 1;
    pub const P: u8 = 2;
    pub const SLOTTIME: u8 = 3;
    pub const TXTAIL: u8 = 4;
    pub const FULLDUPLEX: u8 = 5;
    pub const SETHARDWARE: u8 = 6;
}

pub fn escape(data: &[u8], out: &mut Vec<u8>) {
    for &b in data {
        match b {
            FEND => out.extend_from_slice(&[FESC, TFEND]),
            FESC => out.extend_from_slice(&[FESC, TFESC]),
            _ => out.push(b),
        }
    }
}

/// A complete KISS frame: FEND, command byte (port in the high nibble), escaped payload, FEND.
pub fn frame(command: u8, payload: &[u8], port: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.push(FEND);
    out.push(((port & 0x0F) << 4) | (command & 0x0F));
    escape(payload, &mut out);
    out.push(FEND);
    out
}

/// Streaming decoder: feed bytes as they arrive, get back complete frames
/// (command byte followed by the unescaped payload).
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    esc: bool,
    in_frame: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        for &b in data {
            if b == FEND {
                if self.in_frame && !self.buf.is_empty() {
                    frames.push(std::mem::take(&mut self.buf));
                }
                self.buf.clear();
                self.in_frame = true;
                self.esc = false;
            } else if !self.in_frame {
                continue;
            } else if self.esc {
                self.buf.push(match b {
                    TFEND => FEND,
                    TFESC => FESC,
                    other => other,
                });
                self.esc = false;
            } else if b == FESC {
                self.esc = true;
            } else {
                self.buf.push(b);
            }
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_escapes() {
        let payload = [0x00, 0xC0, 0xDB, 0x41, 0xC0];
        let f = frame(cmd::DATA, &payload, 0);
        assert_eq!(&f[..2], &[FEND, 0x00]);
        assert!(!f[2..f.len() - 1].contains(&FEND));
        let mut d = Decoder::new();
        let got = d.feed(&f);
        let mut want = vec![0x00];
        want.extend_from_slice(&payload);
        assert_eq!(got, vec![want]);
    }

    #[test]
    fn split_chunks_and_back_to_back() {
        let mut stream = frame(0, b"abc", 0);
        stream.extend(frame(0, &[0xDB, b'x'], 0));
        let mut d = Decoder::new();
        let mut got = Vec::new();
        for chunk in stream.chunks(3) {
            got.extend(d.feed(chunk));
        }
        assert_eq!(got, vec![b"\x00abc".to_vec(), vec![0x00, 0xDB, b'x']]);
    }

    #[test]
    fn port_nibble_and_exit() {
        assert_eq!(frame(0, b"", 1)[1], 0x10);
        assert_eq!(EXIT, [0xC0, 0xFF, 0xC0]);
    }
}
