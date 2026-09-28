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
