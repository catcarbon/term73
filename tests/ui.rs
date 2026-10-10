//! Scripted run of the term73 screen against the simulated TM-D750.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use term73::devices::Device;
use term73::engine::Target;
use term73::link::Link;
use term73::ui::{App, MenuKey, Theme};

mod common;
use common::{tempdir, FakeLink, Radio, ENV};

fn text(app: &App) -> String {
    app.main.join("\n")
}

fn wait(app: &mut App, pred: impl Fn(&App) -> bool, what: &str) {
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        app.tick();
        if pred(app) {
            return;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}:\n{}", text(app));
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn type_line(app: &mut App, s: &str) {
    for ch in s.chars() {
        app.key_char(ch);
    }
    app.enter();
}

fn answer(app: &mut App, s: &str) {
    wait(app, |a| a.waiting_for_answer(), "a question");
    type_line(app, s);
    app.tick();
}

#[test]
fn scripted_session() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempdir::Dir::new("ui");
    unsafe { std::env::set_var("TERM73_HOME", &home.0) };
    let radio = Arc::new(Mutex::new(Radio { freq: [146_850_000, 145_030_000], power: [1, 2], ..Default::default() }));
    let (r1, r2) = (radio.clone(), radio.clone());
    let mut app = App::new(
        Theme::plain(),
        Box::new(move |_t: &Target| Ok(Box::new(FakeLink(r1.clone())) as Box<dyn Link>)),
        Arc::new(move |_t: &Target| Ok(Box::new(FakeLink(r2.clone())) as Box<dyn Link>)),
    );
    // first start: callsign
    answer(&mut app, "notacall");
    answer(&mut app, "N0CALL");
    wait(&mut app, |a| text(a).contains("callsign N0CALL saved"), "callsign saved");
    assert!(!text(&app).split("First start").next().unwrap().contains("/radio select"), "welcome shows no command list");
    // pick the radio and set up its profile with defaults
    app.set_scan(vec![Device { target: Target::Serial("COM10".into()), name: "Bluetooth serial port".into(), serial: true, model: Some("TM-D750".into()) }]);
    type_line(&mut app, "/radio select 1");
    answer(&mut app, "n");
    loop {
        wait(&mut app, |a| a.waiting_for_answer() || text(a).contains("profile saved for TM-D750 COM10"), "the next setting");
        if text(&app).contains("profile saved for TM-D750 COM10") {
            break;
        }
        answer(&mut app, "");
    }
    wait(&mut app, |a| !a.busy(), "setup to finish");
    // everyday commands
    type_line(&mut app, "/frequency 145.09");
    wait(&mut app, |a| text(a).contains("tuned to 145.090 MHz"), "tuning");
    assert_eq!(radio.lock().unwrap().freq[1], 145_090_000);
    type_line(&mut app, "/advanced cat ID");
    wait(&mut app, |a| text(a).contains("ID -> ID TM-D750"), "rig control");
    // a memory write shows what changes and waits for a yes
    let old = "ME 054,0446475000,0005000000,9,9,0,0,0,0,1,0,0,0,18,18,000,3,CQCQCQ,0,00,0";
    let new = "ME 054,0446475000,0005000000,9,9,0,0,0,0,1,1,0,0,18,18,000,3,CQCQCQ,0,00,0";
    radio.lock().unwrap().memories.insert("054".into(), old.into());
    type_line(&mut app, &format!("/advanced cat {new}"));
    wait(&mut app, |a| a.waiting_for_answer() && text(a).contains("reverse: 0 -> 1"), "the write preview");
    answer(&mut app, "n");
    wait(&mut app, |a| text(a).contains("not sent"), "a declined write");
    assert_eq!(radio.lock().unwrap().memories["054"], old);
    type_line(&mut app, &format!("/advanced cat {new}"));
    answer(&mut app, "y");
    wait(&mut app, |a| text(a).contains(&format!("{new} -> {new}")), "a confirmed write");
    assert_eq!(radio.lock().unwrap().memories["054"], new);
    type_line(&mut app, "/connect N0BBS-3");
    wait(&mut app, |a| text(a).contains("this transmits: /transmit on first"), "transmit guard");
    type_line(&mut app, "hello");
    wait(&mut app, |a| text(a).contains("not connected: /connect <CALL>"), "plain-text hint");
    type_line(&mut app, "/bbs add");
    for a in ["bbs3", "N0BBS-3", "145.030", "N0DIGI-2", ""] {
        answer(&mut app, a);
    }
    wait(&mut app, |a| text(a).contains("saved BBS bbs3: N0BBS-3 on 145.030 MHz via N0DIGI-2, 1200 baud"), "BBS saved");
    type_line(&mut app, "/bbs edit bbs3");
    for a in ["", "N0BBS-4", "", "none", ""] {
        answer(&mut app, a);
    }
    wait(&mut app, |a| text(a).contains("saved BBS bbs3: N0BBS-4 on 145.030 MHz, 1200 baud"), "BBS edited");
    type_line(&mut app, "/bogus");
    wait(&mut app, |a| text(a).contains("unknown command /bogus"), "unknown command");
    app.key_char('/');
    app.key_char('b');
    app.complete();
    assert_eq!(app.input, "/bbs ");

    // render at a wide size: title, side panels
    let mut term = Terminal::new(TestBackend::new(120, 34)).unwrap();
    app.tick();
    term.draw(|f| app.draw(f)).unwrap();
    let screen: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
    for needle in ["TERM73", "N0CALL", " RIG ", " STATIONS ", "TM-D750", "145.090 MHz", "bbs3", "not heard yet"] {
        assert!(screen.contains(needle), "screen lacks {needle:?}");
    }
    app.shutdown();
}

#[test]
fn last_scan_survives_restart() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempdir::Dir::new("ui-scan");
    unsafe { std::env::set_var("TERM73_HOME", &home.0) };
    term73::config::AppConfig { callsign: Some("N0CALL".into()), ..Default::default() }.save().unwrap();
    let devs = vec![
        Device { target: Target::Serial("COM3".into()), name: "USB mouse".into(), serial: false, model: None },
        Device { target: Target::Serial("COM10".into()), name: "Bluetooth serial port".into(), serial: true, model: Some("TM-D750".into()) },
    ];
    term73::devices::LastScan::save(&devs).unwrap();
    let fail = || std::io::Error::other("no radio in this test");
    let mut app = App::new(Theme::plain(), Box::new(move |_t: &Target| Err(fail())), Arc::new(move |_t: &Target| Err(fail())));
    type_line(&mut app, "/radio list");
    wait(&mut app, |a| text(a).contains("  2  RADIO TM-D750"), "the saved scan");
    assert!(text(&app).contains("1 other devices hidden: /radio list all"));
    type_line(&mut app, "/radio list all");
    wait(&mut app, |a| text(a).contains("USB mouse"), "the full saved scan");
    app.shutdown();
}

#[test]
fn turbo_menus_run_commands() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempdir::Dir::new("ui-turbo");
    unsafe { std::env::set_var("TERM73_HOME", &home.0) };
    term73::config::AppConfig { callsign: Some("N0CALL".into()), ..Default::default() }.save().unwrap();
    let fail = || std::io::Error::other("no radio in this test");
    let mut app = App::new(Theme::turbo(), Box::new(move |_t: &Target| Err(fail())), Arc::new(move |_t: &Target| Err(fail())));
    let screen = |app: &mut App| {
        let mut term = Terminal::new(TestBackend::new(120, 34)).unwrap();
        app.tick();
        term.draw(|f| app.draw(f)).unwrap();
        term.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>()
    };
    // the callsign reaches the screen once the engine has published its first snapshot
    let end = Instant::now() + Duration::from_secs(5);
    let mut s = screen(&mut app);
    while !s.contains("N0CALL") && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
        s = screen(&mut app);
    }
    for needle in ["Radio", "Channel", "Winlink", "[■] term73", " Rig ", " Stations ", "F10 Menu", "N0CALL"] {
        assert!(s.contains(needle), "screen lacks {needle:?}");
    }
    // Alt+R opens Radio; its letters pick items
    assert!(app.open_menu(Some('r')));
    assert!(screen(&mut app).contains("Scan for radios"));
    app.menu_key(MenuKey::Char('l'));
    assert!(!app.menu_is_open());
    wait(&mut app, |a| text(a).contains("no scan yet: /radio scan"), "Radio > Last scan to run /radio list");
    // an item that needs an argument opens a dialog
    app.open_menu(Some('c'));
    app.menu_key(MenuKey::Enter);
    assert!(app.popup_is_open());
    app.popup_key(MenuKey::Esc);
    // arrows move between menus and items; Esc closes
    app.open_menu(None);
    app.menu_key(MenuKey::Right);
    app.menu_key(MenuKey::Down);
    assert!(screen(&mut app).contains("Disconnect"));
    app.menu_key(MenuKey::Esc);
    assert!(!app.menu_is_open());
    assert!(!app.open_menu(Some('q')), "no menu has Q");
    app.shutdown();
}

#[test]
fn turbo_popups_build_commands() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempdir::Dir::new("ui-popup");
    unsafe { std::env::set_var("TERM73_HOME", &home.0) };
    term73::config::AppConfig { callsign: Some("N0CALL".into()), ..Default::default() }.save().unwrap();
    let mut bbs = std::collections::BTreeMap::new();
    bbs.insert("bbs3".to_string(), term73::config::Bbs { call: "N0BBS-3".into(), mhz: 145.03, path: vec![], baud: 1200, ax25: None });
    bbs.insert("node1".to_string(), term73::config::Bbs { call: "NODE1".into(), mhz: 145.03, path: vec![], baud: 1200, ax25: None });
    term73::config::save_bbs(&bbs).unwrap();
    let fail = || std::io::Error::other("no radio in this test");
    let mut app = App::new(Theme::turbo(), Box::new(move |_t: &Target| Err(fail())), Arc::new(move |_t: &Target| Err(fail())));
    let screen = |app: &mut App| {
        let mut term = Terminal::new(TestBackend::new(120, 34)).unwrap();
        app.tick();
        term.draw(|f| app.draw(f)).unwrap();
        term.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>()
    };
    let keys = |app: &mut App, s: &str| s.chars().for_each(|c| app.popup_key(MenuKey::Char(c)));
    // Channel > Connect...: two fields, Enter moves on, then runs the command
    app.open_menu(Some('c'));
    app.menu_key(MenuKey::Char('c'));
    assert!(app.popup_is_open());
    assert!(screen(&mut app).contains("Callsign or node"));
    app.popup_key(MenuKey::Enter);
    app.popup_key(MenuKey::Enter);
    assert!(app.popup_is_open(), "an empty callsign keeps the dialog open");
    assert!(screen(&mut app).contains("a callsign is needed"));
    app.popup_key(MenuKey::BackTab);
    keys(&mut app, "N0BBS-3");
    app.popup_key(MenuKey::Tab);
    keys(&mut app, "N0DIGI-2");
    app.popup_key(MenuKey::Enter);
    assert!(!app.popup_is_open());
    wait(&mut app, |a| text(a).contains("> /connect N0BBS-3 via N0DIGI-2"), "the connect command");
    // BBS > Connect...: a pick list of saved BBSes
    app.open_menu(Some('b'));
    app.menu_key(MenuKey::Char('c'));
    assert!(screen(&mut app).contains("node1"));
    app.popup_key(MenuKey::Down);
    app.popup_key(MenuKey::Enter);
    wait(&mut app, |a| text(a).contains("> /bbs connect node1"), "the BBS connect command");
    // Esc cancels without running anything
    app.open_menu(Some('s'));
    app.menu_key(MenuKey::Char('g'));
    assert!(app.popup_is_open());
    app.popup_key(MenuKey::Esc);
    assert!(!app.popup_is_open());
    assert!(!text(&app).contains("/config grid"));
    app.shutdown();
}

#[test]
fn f8_hides_and_shows_the_side_panels() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempdir::Dir::new("ui-f8");
    unsafe { std::env::set_var("TERM73_HOME", &home.0) };
    term73::config::AppConfig { callsign: Some("N0CALL".into()), ..Default::default() }.save().unwrap();
    let fail = || std::io::Error::other("no radio in this test");
    for theme in [Theme::turbo(), Theme::seafoam()] {
        let mut app = App::new(theme, Box::new(move |_t: &Target| Err(fail())), Arc::new(move |_t: &Target| Err(fail())));
        let screen = |app: &mut App| {
            let mut term = Terminal::new(TestBackend::new(120, 34)).unwrap();
            app.tick();
            term.draw(|f| app.draw(f)).unwrap();
            term.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>().to_ascii_uppercase()
        };
        assert!(screen(&mut app).contains(" STATIONS "));
        app.toggle_side();
        assert!(!screen(&mut app).contains(" STATIONS "));
        app.toggle_side();
        assert!(screen(&mut app).contains(" STATIONS "));
        app.shutdown();
    }
}

/// Two radios in one window for testing: /b runs commands on radio B, whose lines start with [B];
/// a sweep sent by radio A and carried "over the air" is checked by radio B's monitor.
#[test]
fn two_radios_for_testing() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempdir::Dir::new("ui2");
    unsafe { std::env::set_var("TERM73_HOME", &home.0) };
    term73::config::AppConfig { callsign: Some("N0CALL".into()), ..Default::default() }.save().unwrap();
    for port in ["COM10", "COM11"] {
        term73::config::save_radio(port, &term73::config::RadioProfile { data_band: Some(1), ..Default::default() }).unwrap();
    }
    let a = Arc::new(Mutex::new(Radio { freq: [146_850_000, 145_030_000], ..Default::default() }));
    let b = Arc::new(Mutex::new(Radio { freq: [146_850_000, 145_030_000], ..Default::default() }));
    let pick = |a: Arc<Mutex<Radio>>, b: Arc<Mutex<Radio>>| {
        move |t: &Target| Ok(Box::new(FakeLink(if t.address() == "COM11" { b.clone() } else { a.clone() })) as Box<dyn Link>)
    };
    let mut app = App::new(Theme::plain(), Box::new(pick(a.clone(), b.clone())), Arc::new(pick(a.clone(), b.clone())));
    let radio = |port: &str| Device { target: Target::Serial(port.into()), name: "serial port".into(), serial: true, model: Some("TM-D750".into()) };
    app.set_scan(vec![radio("COM10"), radio("COM11")]);

    type_line(&mut app, "/radio select 1");
    wait(&mut app, |a| text(a).contains("radio connected (COM10)"), "radio A");
    type_line(&mut app, "/b /radio select 1");
    wait(&mut app, |a| text(a).contains("[B] error: the other radio is using COM10"), "the device guard");
    type_line(&mut app, "/b /radio select 2");
    wait(&mut app, |a| text(a).contains("[B] radio connected (COM11)"), "radio B");
    wait(&mut app, |a| text(a).contains("[B] TM-D750, data band B"), "radio B identified");
    assert!(b.lock().unwrap().cat_log.contains(&"ID".to_string()), "radio B was asked who it is");
    type_line(&mut app, "/b /connect N0BBS-3");
    wait(&mut app, |a| text(a).contains("usage error: radio B is for testing"), "no sessions on radio B");
    type_line(&mut app, "/b /transmit on");
    wait(&mut app, |a| text(a).contains("[B] transmit ALLOWED for this session"), "transmit on radio B");
    assert!(!app.snapshot().transmit_allowed, "radio A keeps transmit off");

    // A sends a sweep; the test carries it over the air to B
    type_line(&mut app, "/b /advanced dv monitor on");
    wait(&mut app, |a| text(a).contains("[B] D-STAR data monitor on the rig-control link"), "radio B monitoring");
    type_line(&mut app, "/advanced dv monitor on");
    type_line(&mut app, "/transmit on");
    wait(&mut app, |a| text(a).contains("transmit ALLOWED") && !text(a).ends_with("[B] transmit ALLOWED for this session"), "transmit on radio A");
    a.lock().unwrap().dv_air = Some(Vec::new());
    type_line(&mut app, "/advanced dv sweep");
    let sweep = term73::dstar::sweep_payload();
    wait(&mut app, |_| a.lock().unwrap().dv_air.as_ref().is_some_and(|v| v.len() >= sweep.len()), "radio A sending");
    let on_air = a.lock().unwrap().dv_air.take().unwrap();
    assert_eq!(on_air, sweep);
    b.lock().unwrap().to_host.extend(on_air);
    wait(&mut app, |a| text(a).contains("[B] dv rx sweep: all 256 byte values arrived unchanged"), "radio B's sweep check");

    // settings that are not per radio reach both
    type_line(&mut app, "/config callsign N0CALL-1");
    wait(&mut app, |a| text(a).contains("[B] callsign N0CALL-1 saved") && text(a).contains("\ncallsign N0CALL-1 saved"), "callsign on both");
    assert_eq!(term73::config::AppConfig::load().callsign.as_deref(), Some("N0CALL-1"));

    let mut term = Terminal::new(TestBackend::new(120, 34)).unwrap();
    app.tick();
    term.draw(|f| app.draw(f)).unwrap();
    let screen: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
    let rows: Vec<String> = term.backend().buffer().content().chunks(120).map(|r| r.iter().map(|c| c.symbol()).collect()).collect();
    for needle in [" RIG A ", "radio B   TM-D750", "COM11", "D-STAR data probe", "transmit ALLOWED"] {
        assert!(screen.contains(needle), "screen lacks {needle:?}:\n{}", rows.join("\n"));
    }
    app.shutdown();
}

/// A radio whose address changed (another COM number) can take its saved profile instead of a new setup.
#[test]
fn saved_profile_at_a_new_address() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempdir::Dir::new("ui3");
    unsafe { std::env::set_var("TERM73_HOME", &home.0) };
    term73::config::AppConfig { callsign: Some("N0CALL".into()), ..Default::default() }.save().unwrap();
    let old = term73::config::RadioProfile { model: Some("TM-D750".into()), address: Some("COM10".into()), data_band: Some(0),
        paclen: Some(200), ..Default::default() };
    term73::config::save_radio("COM10", &old).unwrap();
    term73::config::save_radio("COM20", &term73::config::RadioProfile { model: Some("TH-D75".into()), ..Default::default() }).unwrap();
    let radio = Arc::new(Mutex::new(Radio::default()));
    let (r1, r2) = (radio.clone(), radio.clone());
    let mut app = App::new(Theme::plain(), Box::new(move |_t: &Target| Ok(Box::new(FakeLink(r1.clone())) as Box<dyn Link>)),
                           Arc::new(move |_t: &Target| Ok(Box::new(FakeLink(r2.clone())) as Box<dyn Link>)));
    let dev = |port: &str| Device { target: Target::Serial(port.into()), name: "serial port".into(), serial: true, model: Some("TM-D750".into()) };
    app.set_scan(vec![dev("COM12"), dev("COM13")]);

    type_line(&mut app, "/radio select 1");
    wait(&mut app, |a| a.waiting_for_answer(), "the reuse question");
    assert!(text(&app).contains("saved profiles for TM-D750:\n  1  TM-D750 COM10, data band A"), "{}", text(&app));
    assert!(!text(&app).contains("TH-D75"), "only the same model is offered");
    answer(&mut app, "1");
    wait(&mut app, |a| text(a).contains("profile saved for TM-D750 COM12"), "the profile copied");
    let radios = term73::config::load_radios();
    assert_eq!(radios["COM12"].data_band, Some(0));
    assert_eq!(radios["COM12"].paclen, Some(200));
    assert_eq!(radios["COM12"].address.as_deref(), Some("COM12"));
    assert!(radios.contains_key("COM10"), "the old address keeps its profile");

    // Enter declines and goes on to a new setup
    type_line(&mut app, "/radio select 2");
    wait(&mut app, |a| a.waiting_for_answer(), "the reuse question");
    answer(&mut app, "");
    wait(&mut app, |a| text(a).contains("no profile for this rig yet; setting one up"), "a new setup");
    app.ctrl_c();
    wait(&mut app, |a| !a.busy(), "the setup to be cancelled");
    app.shutdown();
}
