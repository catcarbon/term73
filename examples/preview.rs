//! Render a screenshot page of the term73 screen, driven against the simulated radio.
//! Usage: cargo run --example preview -- out.html [turbo|seafoam|modem73] [menu|popup]
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::Terminal;
use term73::ax25;
use term73::config::{self, AppConfig, Bbs, RadioProfile};
use term73::devices::Device;
use term73::engine::Target;
use term73::kiss;
use term73::link::Link;
use term73::ui::{App, Theme};

#[path = "../tests/common/mod.rs"]
mod common;
use common::{tempdir, FakeLink, Radio};

fn css(c: Color, default: &str) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Black => "#000000".into(),
        Color::White => "#ffffff".into(),
        Color::Reset => default.into(),
        other => format!("{other}").to_lowercase(),
    }
}

fn settle(app: &mut App, ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        app.tick();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn line(app: &mut App, s: &str) {
    for ch in s.chars() {
        app.key_char(ch);
    }
    app.enter();
    settle(app, 400);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = args.get(1).cloned().unwrap_or_else(|| "preview.html".into());
    let theme = match args.get(2).map(String::as_str) {
        Some("seafoam") => Theme::seafoam(),
        Some("modem73") => Theme::modem73(),
        _ => Theme::turbo(),
    };
    let show_menu = args.get(3).is_some_and(|a| a == "menu");
    // a relative home keeps the user's own paths (shown in "wire log ...") out of screenshots
    let home = tempdir::Dir(std::path::PathBuf::from("term73-preview-home"));
    std::fs::create_dir_all(&home.0).unwrap();
    unsafe { std::env::set_var("TERM73_HOME", &home.0) };
    AppConfig { callsign: Some("N0CALL".into()), grid: Some("FN31pr".into()), winlink_api_key: None }.save().unwrap();
    let mut bbs = std::collections::BTreeMap::new();
    bbs.insert("bbs3".to_string(), Bbs { call: "N0BBS-3".into(), mhz: 145.03, path: vec!["N0DIGI-2".into()], baud: 1200, ax25: None });
    bbs.insert("node1".to_string(), Bbs { call: "NODE1".into(), mhz: 145.03, path: vec![], baud: 1200, ax25: None });
    config::save_bbs(&bbs).unwrap();
    config::save_radio("COM10", &RadioProfile { data_band: Some(1), shift_field: Some(11), ..Default::default() }).unwrap();
    let radio = Arc::new(Mutex::new(Radio { freq: [146_850_000, 145_030_000], power: [1, 2], ..Default::default() }));
    let (r1, r2) = (radio.clone(), radio.clone());
    let mut app = App::new(theme,
        Box::new(move |_t: &Target| Ok(Box::new(FakeLink(r1.clone())) as Box<dyn Link>)),
        Arc::new(move |_t: &Target| Ok(Box::new(FakeLink(r2.clone())) as Box<dyn Link>)));
    app.set_scan(vec![
        Device { target: Target::Modem73 { kiss: "127.0.0.1:8001".into(), control: "127.0.0.1:8073".into() }, name: "modem73 N0CALL".into(), serial: true, model: Some("MODEM73".into()) },
        Device { target: Target::Serial("COM10".into()), name: "Bluetooth serial port".into(), serial: true, model: Some("TM-D750".into()) },
    ]);
    line(&mut app, "/radio select 2");
    settle(&mut app, 1500);
    line(&mut app, "/bbs list");
    line(&mut app, "/transmit on");
    line(&mut app, "/frequency 145.030");
    settle(&mut app, 800);
    line(&mut app, "/listen");
    settle(&mut app, 1500);
    let heard: [(&str, &str, &[&str], &str); 4] = [
        ("BEACON", "N0BBS-3", &[], "N0BBS PBBS and N0NODE-7 Node"),
        ("NODE1", "N0PMS-15", &["N0DIGI-2"], "c n0pms-10"),
        ("N0PMS-15", "NODE1", &["N0DIGI-2"], "###LINK MADE"),
        ("APRS", "N1XYZ-7", &["WIDE1-1"], "!4144.12N/07242.50W>mobile"),
    ];
    for (dst, src, path, text) in heard {
        let p: Vec<String> = path.iter().map(|s| s.to_string()).collect();
        let raw = ax25::build(dst, src, &p, ax25::UI, true, Some(ax25::PID_NONE), text.as_bytes()).unwrap();
        radio.lock().unwrap().to_host.extend(kiss::frame(0, &raw, 0));
        settle(&mut app, 150);
    }
    for ch in "/ra".chars() {
        app.key_char(ch);
    }
    settle(&mut app, 600);
    if show_menu {
        app.open_menu(Some('b'));
        app.menu_key(term73::ui::MenuKey::Down);
    }
    if args.get(3).is_some_and(|a| a == "popup") {
        app.open_menu(Some('b'));
        app.menu_key(term73::ui::MenuKey::Char('c'));
        app.popup_key(term73::ui::MenuKey::Down);
    }
    let (w, h) = (132u16, 34u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    let buf = term.backend().buffer().clone();
    let mut html = String::from("<html><body style=\"margin:0;background:#1e1e1e\"><div style=\"margin:14px;font:15px/20px 'Cascadia Mono',Consolas,monospace\">");
    for y in 0..h {
        html.push_str("<div style=\"height:20px;white-space:pre\">");
        for x in 0..w {
            let c = &buf[(x, y)];
            let (mut fg, mut bg) = (css(c.fg, "#dcdfe4"), css(c.bg, "#1e1e1e"));
            if c.modifier.contains(ratatui::style::Modifier::REVERSED) {
                std::mem::swap(&mut fg, &mut bg);
            }
            let bold = if c.modifier.contains(ratatui::style::Modifier::BOLD) { "font-weight:bold;" } else { "" };
            let sym = c.symbol().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            html.push_str(&format!("<span style=\"display:inline-block;width:1ch;height:20px;color:{fg};background:{bg};{bold}\">{sym}</span>"));
        }
        html.push_str("</div>");
    }
    html.push_str("</div></body></html>");
    std::fs::write(&out, html).unwrap();
    app.shutdown();
    println!("wrote {out}");
}
