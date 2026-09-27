//! Run term73's rig-control server against the simulated TM-D750, for testing Hamlib clients.
//! Usage: cargo run --example rigctl_sim -- [port] [seconds]
//! then e.g.: rigctl -m 2 -r 127.0.0.1:4532 f
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use term73::engine::{self, Job, Out, Target};
use term73::link::Link;

#[path = "../tests/common/mod.rs"]
mod common;
use common::{tempdir, FakeLink, Radio};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = args.get(1).and_then(|p| p.parse().ok()).unwrap_or(4532);
    let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(60);
    let home = tempdir::Dir::new("rigctl-sim");
    unsafe { std::env::set_var("TERM73_HOME", &home.0) };
    let radio = Arc::new(Mutex::new(Radio { freq: [146_850_000, 145_030_000], power: [1, 2], ..Default::default() }));
    let r2 = radio.clone();
    let (tx, rx) = mpsc::channel();
    let h = engine::spawn(tx, Box::new(move |_t: &Target| Ok(Box::new(FakeLink(r2.clone())) as Box<dyn Link>)));
    h.send(Job::Open(Target::Serial("COM10".into())));
    h.send(Job::RigctlStart(port));
    let end = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < end {
        while let Ok(Out::Main(s) | Out::Traffic(s) | Out::Remote(s)) = rx.try_recv() {
            println!("{s}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let r = radio.lock().unwrap();
    println!("simulated radio: band B {} Hz, power levels {:?}, keyed {}", r.freq[1], r.power, r.keyed);
    drop(r);
    h.shutdown();
}
