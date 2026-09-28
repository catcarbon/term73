//! End-to-end engine tests against a simulated TM-D750 and a simulated BBS / gateway.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use term73::ax25::{self, State, Station};
use term73::config::{self, AppConfig, Bbs, RadioProfile};
use term73::engine::{self, Handle, Job, Out, Target};
use term73::kiss;
use term73::link::Link;

mod common;
use common::{air, bbs, tempdir, FakeLink, Radio, ENV};

struct Rig {
    radio: Arc<Mutex<Radio>>,
    handle: Option<Handle>,
    out: Receiver<Out>,
    text: String,
    _home: tempdir::Dir,
}

impl Rig {
    fn new(tag: &str) -> Self {
        let home = tempdir::Dir::new(tag);
        unsafe { std::env::set_var("TERM73_HOME", &home.0) };
        let radio = Arc::new(Mutex::new(Radio { freq: [146_850_000, 145_030_000], power: [1, 2], ..Default::default() }));
        let r2 = radio.clone();
        let (tx, rx) = mpsc::channel();
        let handle = engine::spawn(tx, Box::new(move |_t: &Target| Ok(Box::new(FakeLink(r2.clone())) as Box<dyn Link>)));
        Rig { radio, handle: Some(handle), out: rx, text: String::new(), _home: home }
    }

    fn h(&self) -> &Handle {
        self.handle.as_ref().unwrap()
    }

    fn pump(&mut self) {
        while let Ok(o) = self.out.try_recv() {
            match o {
                Out::Main(s) | Out::Traffic(s) => {
                    self.text.push_str(&s);
                    self.text.push('\n');
                }
                Out::Remote(s) => self.text.push_str(&s),
            }
        }
    }

    fn wait_for(&mut self, needle: &str, bbs: Option<&mut Station>) {
        let end = Instant::now() + Duration::from_secs(30);
        let mut bbs = bbs;
        loop {
            self.pump();
            if self.text.contains(needle) {
                return;
            }
            if let Some(b) = bbs.as_deref_mut() {
                air(&self.radio, b);
            }
            assert!(Instant::now() < end, "never saw {needle:?}; got:\n{}", self.text);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.shutdown();
        }
    }
}

#[test]
fn engine_end_to_end() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());

    // --- open, guards, tuning ------------------------------------------------------
    {
        let mut rig = Rig::new("open");
        rig.h().send(Job::Open(Target::Serial("COM10".into())));
        rig.wait_for("TM-D750, data band B 145.030 MHz", None);
        rig.h().send(Job::Connect("N0BBS-3".into(), vec![]));
        rig.wait_for("this transmits: /transmit on first", None);
        rig.h().send(Job::Tune(145.09));
        rig.wait_for("tuned to 145.090 MHz", None);
        assert_eq!(rig.radio.lock().unwrap().freq[1], 145_090_000);
        rig.h().send(Job::Cat("SR".into()));
        rig.wait_for("refused: SR is a reset or service command", None);
    }

    // --- a BBS session while listening --------------------------------------------
    {
        let mut rig = Rig::new("bbs");
        let mut b = bbs("N0BBS-3");
        rig.h().send(Job::Open(Target::Serial("COM10".into())));
        rig.h().send(Job::SetCallsign("n0call".into()));
        rig.h().send(Job::SetTransmit(true));
        rig.h().send(Job::Listen(true, None));
        rig.wait_for("listening on", None);
        rig.h().send(Job::Connect("N0BBS-3".into(), vec![]));
        let mut greeted = false;
        let end = Instant::now() + Duration::from_secs(30);
        while !rig.text.contains("de N0BBS>") {
            air(&rig.radio, &mut b);
            if b.state == State::Connected && !greeted {
                b.send(b"Hello N0CALL\rde N0BBS>\r");
                greeted = true;
            }
            rig.pump();
            assert!(Instant::now() < end, "{}", rig.text);
            std::thread::sleep(Duration::from_millis(5));
        }
        rig.h().send(Job::SendLine("L".into()));
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            air(&rig.radio, &mut b);
            if b.recv().windows(2).any(|w| w == b"L\r") {
                b.send(b"No messages\r");
                break;
            }
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(5));
        }
        rig.wait_for("No messages", Some(&mut b));
        rig.h().send(Job::Disconnect);
        rig.wait_for("*** session closed", Some(&mut b));
        assert_eq!(b.state, State::Disconnected);
        assert!(rig.text.contains("N0BBS-3 > N0CALL"), "traffic lines missing:\n{}", rig.text);
        assert!(rig.h().snap.lock().unwrap().heard.iter().any(|h| h.call == "N0BBS-3"));
        rig.h().send(Job::Listen(false, None));
        rig.wait_for("radio back to normal operation", None);
        assert!(!rig.radio.lock().unwrap().kiss);
    }

    // --- Winlink bridge: telnet login, nearest gateway, relay, clean close ----------
    {
        let mut rig = Rig::new("winlink");
        AppConfig { callsign: Some("N0CALL".into()), grid: Some("FN31pr".into()), winlink_api_key: None }.save().unwrap();
        std::fs::write(config::rmslist_path(), serde_json::json!({"Gateways": [
            {"Callsign": "N0GW-10", "HoursSinceStatus": 1, "GatewayChannels": [
                {"SupportedModes": "Packet 1200", "Frequency": 145_050_000u64, "Baud": "1200", "ServiceCode": "PUBLIC", "Gridsquare": "FN31pr"}]}]}).to_string()).unwrap();
        // the engine was spawned before the config existed: reopen by restarting it
        if let Some(h) = rig.handle.take() {
            h.shutdown();
        }
        let r2 = rig.radio.clone();
        let (tx, rx) = mpsc::channel();
        rig.handle = Some(engine::spawn(tx, Box::new(move |_t: &Target| Ok(Box::new(FakeLink(r2.clone())) as Box<dyn Link>))));
        rig.out = rx;
        let mut gw = bbs("N0GW-10");
        rig.h().send(Job::Open(Target::Serial("COM10".into())));
        rig.h().send(Job::SetTransmit(true));
        rig.h().send(Job::WinlinkStart(0));
        rig.wait_for("Winlink ready on 127.0.0.1:", None);
        let port: u16 = rig.text.split("127.0.0.1:").nth(1).unwrap().split(';').next().unwrap().parse().unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let mut buf = [0u8; 256];
        let read_until = |c: &mut TcpStream, want: &[u8], radio: &Arc<Mutex<Radio>>, gw: &mut Station, buf: &mut [u8]| {
            let mut got = Vec::new();
            let end = Instant::now() + Duration::from_secs(30);
            while !got.windows(want.len()).any(|w| w == want) {
                air(radio, gw);
                if let Ok(n) = c.read(buf) {
                    got.extend_from_slice(&buf[..n]);
                }
                assert!(Instant::now() < end, "never got {:?}", String::from_utf8_lossy(want));
            }
            got
        };
        read_until(&mut c, b"Callsign :", &rig.radio, &mut gw, &mut buf);
        c.write_all(b"N0CALL\r").unwrap();
        read_until(&mut c, b"Password :", &rig.radio, &mut gw, &mut buf);
        c.write_all(b"CMSTelnet\r").unwrap();
        let mut banner_sent = false;
        let end = Instant::now() + Duration::from_secs(30);
        let mut got = Vec::new();
        while !got.windows(9).any(|w| w == b"CMS via N") {
            air(&rig.radio, &mut gw);
            if gw.state == State::Connected && !banner_sent {
                gw.send(b"[WL2K-5.0-B2FWIHJM$]\r;PQ: 1234\rCMS via N0GW >\r");
                banner_sent = true;
            }
            if let Ok(n) = c.read(&mut buf) {
                got.extend_from_slice(&buf[..n]);
            }
            assert!(Instant::now() < end, "no banner");
        }
        assert_eq!(rig.radio.lock().unwrap().freq[1], 145_050_000, "tuned to the gateway");
        assert_eq!(rig.radio.lock().unwrap().power[1], 0, "high power");
        c.write_all(b"[N0CALL-1.0-B2FHM$]\rFF\r").unwrap();
        let mut seen = Vec::new();
        let end = Instant::now() + Duration::from_secs(30);
        while !seen.ends_with(b"FF\r") {
            air(&rig.radio, &mut gw);
            seen.extend(gw.recv());
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(seen, b"[N0CALL-1.0-B2FHM$]\rFF\r");
        drop(c);
        rig.wait_for("*** session closed", Some(&mut gw));
        assert_eq!(gw.state, State::Disconnected);
    }

    // --- keying for a software modem: guarded, then unkeyed by the watchdog path ----
    {
        let mut rig = Rig::new("ptt");
        rig.h().send(Job::Open(Target::Serial("COM10".into())));
        rig.wait_for("data band B", None);
        let ptt = |rig: &Rig, on| rig.h().ask(|tx| Job::Ptt(on, tx), Duration::from_secs(10)).unwrap();
        assert!(!ptt(&rig, true), "refused while transmit is off");
        rig.h().send(Job::SetTransmit(true));
        assert!(!ptt(&rig, true), "refused while the profile is unverified");
        rig.h().ask(|tx| Job::SaveProfile(RadioProfile { ptt_verified: Some(true), ..Default::default() }, tx),
                    Duration::from_secs(10)).unwrap();
        assert!(ptt(&rig, true));
        assert!(rig.radio.lock().unwrap().keyed);
        assert!(ptt(&rig, false));
        assert!(!rig.radio.lock().unwrap().keyed);
    }

    // --- saved BBS profile: tune, power, connect ------------------------------------
    {
        let mut rig = Rig::new("bbsprofile");
        let mut all = std::collections::BTreeMap::new();
        all.insert("node1".to_string(), Bbs { call: "NODE1".into(), mhz: 145.03, path: vec![], baud: 1200, ax25: None });
        config::save_bbs(&all).unwrap();
        let mut node = bbs("NODE1");
        rig.h().send(Job::Open(Target::Serial("COM10".into())));
        rig.h().send(Job::SetCallsign("N0CALL".into()));
        rig.h().send(Job::SetTransmit(true));
        rig.h().send(Job::BbsConnect("node1".into()));
        rig.wait_for("*** connected to NODE1", Some(&mut node));
        rig.h().send(Job::Disconnect);
        rig.wait_for("*** session closed", Some(&mut node));
    }
}

/// A simulated modem73: KISS over TCP to an in-memory station, plus the JSON control port.
#[test]
fn modem73_as_rig() {
    use std::net::TcpListener;
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempdir::Dir::new("modem73");
    unsafe { std::env::set_var("TERM73_HOME", &home.0) };
    let kiss_l = TcpListener::bind("127.0.0.1:0").unwrap();
    let ctl_l = TcpListener::bind("127.0.0.1:0").unwrap();
    let kiss_addr = kiss_l.local_addr().unwrap().to_string();
    let ctl_addr = ctl_l.local_addr().unwrap().to_string();
    let rig_cmds = Arc::new(Mutex::new(Vec::<String>::new()));
    let rc = rig_cmds.clone();
    std::thread::spawn(move || {
        for s in ctl_l.incoming() {
            let mut s = s.unwrap();
            let mut head = [0u8; 4];
            if s.read_exact(&mut head).is_err() { continue; }
            let mut body = vec![0u8; u32::from_be_bytes(head) as usize];
            s.read_exact(&mut body).unwrap();
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let resp = match req["cmd"].as_str().unwrap() {
                "get_config" => serde_json::json!({"callsign": "N0CALL", "modulation": "QPSK", "code_rate": "1/2", "payload_size": 180}),
                "rigctl" => { rc.lock().unwrap().push(req["command"].as_str().unwrap().to_string()); serde_json::json!({"ok": true, "response": "RPRT 0\n"}) }
                _ => serde_json::json!({"ok": true}),
            };
            let out = serde_json::to_vec(&resp).unwrap();
            s.write_all(&(out.len() as u32).to_be_bytes()).unwrap();
            s.write_all(&out).unwrap();
        }
    });
    // the "air" behind the modem: one node station
    let node = Arc::new(Mutex::new(bbs("NODE1")));
    let n2 = node.clone();
    std::thread::spawn(move || {
        let (mut s, _) = kiss_l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
        let mut dec = kiss::Decoder::new();
        let mut buf = [0u8; 4096];
        let mut greeted = false;
        loop {
            let now = Instant::now();
            match s.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => for f in dec.feed(&buf[..n]) {
                    if let Some(fr) = ax25::parse(&f[1..]) { n2.lock().unwrap().on_frame(&fr, now); }
                },
                Err(_) => {}
            }
            let mut st = n2.lock().unwrap();
            st.poll(now);
            if st.state == State::Connected && !greeted {
                st.send(b"NODE1 example node\r");
                greeted = true;
            }
            for raw in st.take_outbox() {
                if s.write_all(&kiss::frame(0, &raw, 0)).is_err() { return; }
            }
        }
    });
    config::save_radio(&kiss_addr, &RadioProfile { tune_via_modem: Some(true), t1: Some(0.5), ..Default::default() }).unwrap();
    let mut all = std::collections::BTreeMap::new();
    all.insert("node1".to_string(), Bbs { call: "NODE1".into(), mhz: 145.03, path: vec![], baud: 1200, ax25: None });
    config::save_bbs(&all).unwrap();
    let (tx, rx) = mpsc::channel();
    let h = engine::spawn(tx, engine::real_opener());
    let mut text = String::new();
    let mut wait = |needle: &str| {
        let end = Instant::now() + Duration::from_secs(30);
        while !text.contains(needle) {
            while let Ok(Out::Main(s) | Out::Traffic(s) | Out::Remote(s)) = rx.try_recv() { text.push_str(&s); text.push('\n'); }
            assert!(Instant::now() < end, "never saw {needle:?}:\n{text}");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    h.send(Job::SetCallsign("N0CALL".into()));
    h.send(Job::SetTransmit(true));
    h.send(Job::Open(Target::Modem73 { kiss: kiss_addr.clone(), control: ctl_addr }));
    wait("rig: modem73 at");
    h.send(Job::BbsConnect("node1".into()));
    wait("NODE1 example node");
    assert!(rig_cmds.lock().unwrap().contains(&"F 145030000".to_string()), "tuned through the modem");
    h.send(Job::Disconnect);
    wait("*** session closed");
    h.shutdown();
}

/// Closing the window gives a few seconds: a station that has gone silent must not hold up
/// releasing the radio, and the radio must still leave packet mode.
#[test]
fn shutdown_within_budget_when_the_station_is_silent() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let mut rig = Rig::new("close");
    let mut b = bbs("N0BBS-3");
    rig.h().send(Job::Open(Target::Serial("COM10".into())));
    rig.h().send(Job::SetCallsign("n0call".into()));
    rig.h().send(Job::SetTransmit(true));
    rig.h().send(Job::Connect("N0BBS-3".into(), vec![]));
    rig.wait_for("connected to N0BBS-3", Some(&mut b));
    assert!(rig.radio.lock().unwrap().kiss);
    rig.radio.lock().unwrap().to_air.clear();
    // the station now hears nothing more: no air() calls from here on
    let started = Instant::now();
    rig.handle.take().unwrap().shutdown_within(Duration::from_millis(500));
    let took = started.elapsed();
    assert!(took < Duration::from_secs(2), "shutdown took {took:?}");
    let radio = rig.radio.lock().unwrap();
    assert!(!radio.kiss, "the radio was left in packet mode");
    let discs = radio.to_air.iter().filter_map(|f| ax25::parse(f)).filter(|f| f.ctl & !0x10 == ax25::DISC).count();
    assert!(discs >= 1, "no DISC was sent");
}

/// Watch repeats a read and reports only the fields a front-panel change touched.
#[test]
fn watch_reports_changed_fields() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let mut rig = Rig::new("watch");
    rig.h().send(Job::Open(Target::Serial("COM10".into())));
    rig.wait_for("TM-D750, data band B", None);
    rig.h().send(Job::Watch(Some("AG 010".into())));
    rig.wait_for("watch only takes a read command", None);
    rig.h().send(Job::Watch(Some("fo 1".into())));
    rig.wait_for("FO 1 now: 0=1 1=0145030000 2=0000600000", None);
    rig.radio.lock().unwrap().freq[1] = 145_090_000; // as if turned on the radio's dial
    rig.wait_for("FO 1 changed: field 1: 0145030000 -> 0145090000", None);
    assert!(!rig.text.contains("field 2:"), "only the changed field is reported:\n{}", rig.text);
    rig.h().send(Job::Watch(None));
    rig.wait_for("watch stopped", None);
    let reads = rig.radio.lock().unwrap().cat_log.iter().filter(|l| l.as_str() == "FO 1").count();
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert_eq!(rig.radio.lock().unwrap().cat_log.iter().filter(|l| l.as_str() == "FO 1").count(), reads, "no reads after stopping");
}

/// /connect to a saved BBS's callsign uses its frequency; nothing connects on an APRS channel.
#[test]
fn connect_uses_saved_bbs_and_avoids_aprs() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let mut rig = Rig::new("connect-bbs");
    let mut saved = std::collections::BTreeMap::new();
    saved.insert("node1".to_string(), Bbs { call: "NODE1".into(), mhz: 145.03, path: vec![], baud: 1200, ax25: None });
    config::save_bbs(&saved).unwrap();
    rig.radio.lock().unwrap().freq[1] = 144_390_000; // left on the APRS channel
    rig.h().send(Job::Open(Target::Serial("COM10".into())));
    rig.h().send(Job::SetCallsign("n0call".into()));
    rig.h().send(Job::SetTransmit(true));
    rig.wait_for("TM-D750, data band B 144.390 MHz", None);
    rig.h().send(Job::Connect("N0XYZ".into(), vec![]));
    rig.wait_for("the APRS channel", None);
    assert!(rig.radio.lock().unwrap().to_air.is_empty(), "nothing was transmitted on the APRS channel");
    rig.h().send(Job::Connect("node1".into(), vec![]));
    rig.wait_for("using saved BBS node1 (145.030 MHz)", None);
    rig.wait_for("*** connecting to NODE1", None);
    assert_eq!(rig.radio.lock().unwrap().freq[1], 145_030_000);
}

/// The Hamlib-compatible server answers from the real engine: frequency, band limits, power, and a refused PTT.
#[test]
fn rigctl_server_drives_the_engine() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let mut rig = Rig::new("rigctl");
    rig.h().send(Job::Open(Target::Serial("COM10".into())));
    rig.wait_for("TM-D750, data band B", None);
    rig.h().send(Job::RigctlStart(0));
    rig.wait_for("rigctl server on 127.0.0.1:", None);
    let port: u16 = rig.text.split("rigctl server on 127.0.0.1:").nth(1).unwrap()
        .split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap();
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let mut ask = |line: &str, lines: usize| {
        c.write_all(format!("{line}\n").as_bytes()).unwrap();
        let mut got = String::new();
        let mut b = [0u8; 1];
        while got.matches('\n').count() < lines {
            c.read_exact(&mut b).unwrap();
            got.push(b[0] as char);
        }
        got
    };
    assert_eq!(ask("f", 1), "145030000\n");
    assert_eq!(ask("F 146520000", 1), "RPRT 0\n");
    assert_eq!(ask("F 7074000", 1), "RPRT -1\n", "outside the rig's bands");
    assert_eq!(ask("l RFPOWER", 1), "0.200000\n");
    assert_eq!(ask("T 1", 1), "RPRT -9\n", "transmit is not allowed");
    assert_eq!(ask("m", 2), "FM\n15000\n");
    assert_eq!(ask("M AM 10000", 1), "RPRT 0\n");
    assert_eq!(ask("m", 2), "AM\n10000\n");
    assert_eq!(rig.radio.lock().unwrap().mode[1], 2);
    assert_eq!(ask("M D-STAR 6250", 1), "RPRT -11\n", "DV needs D-STAR settings, so it is not offered");
    assert_eq!(ask("M FM 15000", 1), "RPRT 0\n");
    assert_eq!(ask("r", 1), "None\n", "FO field 11 is 0 on the simulated radio");
    assert_eq!(ask("o", 1), "600000\n", "FO field 2 is the offset in Hz");
    assert_eq!(ask(r"\get_lock_mode", 2), "0\nRPRT 0\n");
    assert_eq!(ask("L SQL 1.0", 1), "RPRT 0\n");
    assert_eq!(rig.radio.lock().unwrap().squelch[1], 31, "full squelch is step 31");
    assert_eq!(ask("l SQL", 1), "1.000000\n");
    assert_eq!(ask("L SQL 0", 1), "RPRT 0\n");
    assert_eq!(ask("l SQL", 1), "0.000000\n");
    assert!(ask(r"\dump_state", 1).starts_with('1'));
    assert_eq!(rig.radio.lock().unwrap().freq[1], 146_520_000);
    assert!(!rig.radio.lock().unwrap().keyed);
}

/// /listen names link-control frames, and a station seen using a v2.2-only frame is marked in the heard list.
#[test]
fn listen_labels_v22_frames() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let mut rig = Rig::new("v22");
    rig.h().send(Job::Open(Target::Serial("COM10".into())));
    rig.h().send(Job::Listen(true, None));
    rig.wait_for("listening on", None);
    let sabme = ax25::build("N0BBS-3", "N1XYZ-7", &[], ax25::SABME | ax25::PF, true, None, &[]).unwrap();
    let rr = ax25::build("N0BBS-3", "NODE1", &[], ax25::RR, false, None, &[]).unwrap();
    for f in [sabme, rr] {
        rig.radio.lock().unwrap().to_host.extend(kiss::frame(0, &f, 0));
    }
    rig.wait_for("N1XYZ-7 > N0BBS-3: <SABME connect, v2.2>", None);
    rig.wait_for("NODE1 > N0BBS-3: <RR ack, next 0>", None);
    let heard = rig.h().snap.lock().unwrap().heard.clone();
    assert!(heard.iter().any(|h| h.call == "N1XYZ-7" && h.v22));
    assert!(heard.iter().any(|h| h.call == "NODE1" && !h.v22));
}

/// A v2.0-only BBS: the first connect falls back from v2.2, the BBS entry remembers "2.0",
/// and the next connect goes straight to v2.0 (a plain SABM first).
#[test]
fn bbs_remembers_its_ax25_version() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let mut rig = Rig::new("ax25ver");
    let mut saved = std::collections::BTreeMap::new();
    saved.insert("bbs3".to_string(), Bbs { call: "N0BBS-3".into(), mhz: 145.03, path: vec![], baud: 1200, ax25: None });
    config::save_bbs(&saved).unwrap();
    let mut b = bbs("N0BBS-3");
    b.cfg.v22 = false; // answers SABME with DM, like many v2.0 TNCs
    rig.h().send(Job::Open(Target::Serial("COM10".into())));
    rig.h().send(Job::SetCallsign("n0call".into()));
    rig.h().send(Job::SetTransmit(true));
    rig.h().send(Job::BbsConnect("bbs3".into()));
    rig.wait_for("does not use AX.25 v2.2", Some(&mut b));
    rig.wait_for("connected to N0BBS-3 (AX.25 v2.0)", Some(&mut b));
    assert_eq!(config::load_bbs()["bbs3"].ax25.as_deref(), Some("2.0"));
    rig.h().send(Job::Disconnect);
    rig.wait_for("*** session closed", Some(&mut b));
    rig.text.clear();
    rig.radio.lock().unwrap().to_air.clear();
    rig.h().send(Job::BbsConnect("bbs3".into()));
    rig.wait_for("*** connecting to N0BBS-3", None);
    let end = Instant::now() + Duration::from_secs(10);
    let first = loop {
        if let Some(f) = rig.radio.lock().unwrap().to_air.first().cloned() {
            break f;
        }
        assert!(Instant::now() < end, "nothing sent");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(ax25::parse(&first).unwrap().ctl & !ax25::PF, ax25::SABM, "straight to v2.0");
    rig.wait_for("connected to N0BBS-3 (AX.25 v2.0)", Some(&mut b));
    assert!(!rig.text.contains("does not use AX.25 v2.2"));
}
