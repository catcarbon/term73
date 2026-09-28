//! Shared test fixtures: a simulated TM-D750 and helpers for stations on the air.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use term73::ax25::{Config, Station};
use term73::kiss;
use term73::link::Link;

pub static ENV: Mutex<()> = Mutex::new(());

#[derive(Default)]
pub struct Radio {
    pub kiss: bool,
    pub pending_exit: bool,
    pub freq: [u64; 2],
    pub power: [u8; 2],
    /// MD codes per band: 0 FM, 2 AM.
    pub mode: [u8; 2],
    pub keyed: bool,
    pub cat_log: Vec<String>,
    pub to_host: Vec<u8>,       // bytes the radio sends to term73
    pub to_air: Vec<Vec<u8>>,   // AX.25 frames the radio transmitted
    pub decoder: kiss::Decoder,
    pub cat_buf: Vec<u8>,
}

impl Radio {
    fn answer(&mut self, line: &str) -> String {
        self.cat_log.push(line.to_string());
        let band = |s: &str| s.trim().parse::<usize>().unwrap_or(0).min(1);
        match line {
            "ID" => "ID TM-D750".into(),
            "FV" => "FV 1.02".into(),
            "TN" => "TN 0,1".into(),
            "TX" => { self.keyed = true; "TX 0".into() }
            "RX" => { self.keyed = false; "RX 0".into() }
            l if l.starts_with("TN 2,") => { self.kiss = true; l.into() }
            l if l.starts_with("FQ ") && l.contains(',') => {
                let (b, f) = l[3..].split_once(',').unwrap();
                self.freq[band(b)] = f.parse().unwrap();
                l.into()
            }
            l if l.starts_with("FQ ") => format!("FQ {},{:010}", band(&l[3..]), self.freq[band(&l[3..])]),
            l if l.starts_with("MD ") && l.contains(',') => {
                let (b, m) = l[3..].split_once(',').unwrap();
                self.mode[band(b)] = m.parse().unwrap();
                l.into()
            }
            l if l.starts_with("MD ") => format!("MD {},{}", band(&l[3..]), self.mode[band(&l[3..])]),
            l if l.starts_with("FO ") => format!("FO {},{:010},0000600000,2,2,0,0,0,0,0,0,0,08,08,000,0,CQCQCQ,0,00", band(&l[3..]), self.freq[band(&l[3..])]),
            l if l.starts_with("PC ") && l.contains(',') => {
                let (b, v) = l[3..].split_once(',').unwrap();
                self.power[band(b)] = v.parse().unwrap();
                l.into()
            }
            l if l.starts_with("PC ") => format!("PC {},{}", band(&l[3..]), self.power[band(&l[3..])]),
            _ => "?".into(),
        }
    }
}

pub struct FakeLink(pub Arc<Mutex<Radio>>);

impl Link for FakeLink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        let mut r = self.0.lock().unwrap();
        if r.kiss {
            if data == kiss::EXIT {
                r.pending_exit = true;
                return Ok(());
            }
            if r.pending_exit {
                r.pending_exit = false;
                r.kiss = false;
                r.to_host.extend(b"?\r");
                return Ok(());
            }
            for f in r.decoder.feed(data) {
                if f[0] & 0x0F == 0 {
                    r.to_air.push(f[1..].to_vec());
                }
            }
            return Ok(());
        }
        r.cat_buf.extend_from_slice(data);
        while let Some(p) = r.cat_buf.iter().position(|&b| b == b'\r') {
            let line: Vec<u8> = r.cat_buf.drain(..=p).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]).to_string();
            let reply = r.answer(&text);
            r.to_host.extend(reply.as_bytes());
            r.to_host.push(b'\r');
        }
        Ok(())
    }

    fn read(&mut self) -> std::io::Result<Vec<u8>> {
        std::thread::sleep(Duration::from_millis(2));
        Ok(std::mem::take(&mut self.0.lock().unwrap().to_host))
    }
}


pub mod tempdir {
    pub struct Dir(pub std::path::PathBuf);
    impl Dir {
        pub fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("term73-{tag}-{}-{}", std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}


/// Move frames between the simulated radio and a station on the air.
pub fn air(radio: &Arc<Mutex<Radio>>, other: &mut Station) {
    let now = Instant::now();
    let sent: Vec<Vec<u8>> = std::mem::take(&mut radio.lock().unwrap().to_air);
    for raw in sent {
        other.on_raw(&raw, now);
    }
    other.poll(now);
    for raw in other.take_outbox() {
        radio.lock().unwrap().to_host.extend(kiss::frame(0, &raw, 0));
    }
}

pub fn bbs(call: &str) -> Station {
    let mut s = Station::new(call, Config { t1: Duration::from_millis(300), ack_delay: Duration::from_millis(20), n2: 40, ..Config::default() });
    s.listen = true;
    s
}

