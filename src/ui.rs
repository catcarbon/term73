//! The term73 screen: Turbo Vision or CDE SeaFoam layouts, split panels, grouped slash commands,
//! and setup prompts that run alongside background radio I/O.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::config::{self, Bbs, RadioProfile};
use crate::devices::{self, Device, LastScan};
use crate::discover::{self, Discovery};
use crate::engine::{self, Handle, Job, Out, Packet, Snapshot, Target};
use crate::gateways;
use crate::link::Link;

mod popup;
mod turbo;
pub use turbo::{MenuKey, Turbo};

/// Opens a rig for a quick identity check during scans.
pub type OpenFn = Arc<dyn Fn(&Target) -> std::io::Result<Box<dyn Link>> + Send + Sync>;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const WELCOME: &str = "term73: packet terminal for Bluetooth TNC radios and software modems. Type /help for commands.";

pub const HELP: &[(&str, &str)] = &[
    ("basics", "/connect <CALL> [via D1,D2]   talk to a station or node; lines you type then go to it
/disconnect                   end the connection
/listen [MHz]                 show packet traffic (optionally on another frequency)
/listen save <file>           also save what is heard to a file
/listen off
/frequency <MHz>              tune the radio's data band
/power high|mid|low
/transmit on|off              permission to transmit (off at every start)"),
    ("bbs", "/bbs add                      save a BBS (callsign, frequency, digipeaters, speed)
/bbs edit <name>              change a saved BBS (Enter keeps each value)
/bbs list
/bbs connect <name>           tune, set power and speed, connect
/bbs remove <name>"),
    ("winlink", "/winlink setup                grid locator, API key, download the gateway list
/winlink gateways             nearby packet gateways
/winlink start | stop         let a Winlink client (e.g. Pat) connect through term73 on 127.0.0.1:8772"),
    ("radio", "/radio scan [all]             find radios and software modems (all: every device)
/radio list [all]             show the last scan again (kept across restarts)
/radio select <n>             use device <n> from the last scan
/radio setup                  detect capabilities and review the profile
/radio info                   model, firmware, frequency, power
/radio off                    disconnect"),
    ("settings", "/config show
/config callsign <CALL>
/config grid <LOCATOR>
/clear   /quit
Keys: F10 or Alt+letter opens the menus, F1 help, F8 shows or hides the side panels, PageUp/PageDown scroll, Tab completes, Ctrl+C ends a connection (twice to quit), Ctrl+Q or Alt+X quits.
While connected, / lines that are not term73 commands (like /EX) go to the station; // sends a single /. Ctrl+Z sends Ctrl-Z (ends a BBS message)."),
    ("advanced", "/advanced cat <command>       send one raw rig-control command, e.g. /advanced cat FQ 1
/advanced watch <read> | off  repeat a read command (e.g. FO 1) every 2 s and show which fields change
/advanced kiss on|off         switch the radio's TNC by hand
/advanced modem status        software modem state (control port)
/advanced modem set key=value ...
/advanced rigctl start [port] | stop   Hamlib-compatible rig control on 127.0.0.1 (default 4532) for modem73, WSJT-X, Gpredict"),
];
const BASIC: &[&str] = &["basics", "bbs", "winlink", "radio", "settings"];

const COMMANDS: &[(&str, &[&str])] = &[
    ("/connect", &[]), ("/disconnect", &[]), ("/listen", &["off", "save"]), ("/frequency", &[]),
    ("/power", &["high", "mid", "low"]), ("/transmit", &["on", "off"]),
    ("/bbs", &["add", "edit", "list", "connect", "remove"]), ("/winlink", &["setup", "gateways", "start", "stop"]),
    ("/radio", &["scan", "list", "select", "setup", "info", "off"]), ("/config", &["show", "callsign", "grid"]),
    ("/advanced", &["cat", "watch", "kiss", "modem", "rigctl"]),
    ("/help", &["basics", "bbs", "winlink", "radio", "settings", "advanced"]), ("/clear", &[]), ("/quit", &[]),
];

/// In a session, the text a "/" line sends to the station, or None when it is a term73 command.
fn slash_to_station(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("//") {
        return Some(format!("/{rest}"));
    }
    (line.starts_with('/') && !is_own_command(line)).then(|| line.to_string())
}

/// True when the first word of a "/" line names a term73 command.
fn is_own_command(line: &str) -> bool {
    let word = line.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    word == "/tx" || COMMANDS.iter().any(|(c, _)| *c == word)
}

// ------------------------------------------------------------------ theme

#[derive(Clone)]
pub struct Theme {
    pub desktop: Style,
    pub window: Style,
    pub border: Style,
    pub titlebar: Style,
    pub panel: Style,
    pub input: Style,
    pub dim: Style,
    pub label: Style,
    pub value: Style,
    pub good: Style,
    pub warnv: Style,
    pub rx: Style,
    pub tx: Style,
    pub echo: Style,
    pub ask: Style,
    pub session: Style,
    pub monitor: Style,
    pub bridge: Style,
    pub error: Style,
    pub warn: Style,
    pub info: Style,
    pub section: Style,
    /// Set for the Turbo Vision layout (menu bar, shadowed windows); None for the others.
    pub turbo: Option<Turbo>,
}

fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

impl Theme {
    /// SeaFoam: the CDE palette of the D750 boot screen (desktop #7db7bb, title bar #aacbdb, panel #c5afa4).
    pub fn seafoam() -> Self {
        let fg = |c| Style::default().fg(rgb(c));
        let white = rgb(0xffffff);
        Theme {
            desktop: Style::default().bg(rgb(0x7db7bb)),
            window: Style::default().bg(white).fg(Color::Black),
            border: Style::default().bg(white).fg(rgb(0x5f8f93)),
            titlebar: Style::default().bg(rgb(0xaacbdb)).fg(Color::Black),
            panel: Style::default().bg(rgb(0xc5afa4)).fg(Color::Black),
            input: Style::default().bg(rgb(0xf3f7f6)).fg(Color::Black),
            dim: fg(0x4d6b6e),
            label: fg(0x5b4a43),
            value: Style::default().fg(Color::Black).add_modifier(Modifier::BOLD),
            good: fg(0x1f6f73).add_modifier(Modifier::BOLD),
            warnv: fg(0x8a5a00).add_modifier(Modifier::BOLD),
            rx: Style::default().bg(rgb(0x3a7d82)).fg(white).add_modifier(Modifier::BOLD),
            tx: Style::default().bg(rgb(0xb3262e)).fg(white).add_modifier(Modifier::BOLD),
            echo: fg(0x6f7f7c),
            ask: fg(0x1d5d8a).add_modifier(Modifier::BOLD),
            session: fg(0x1f6f73),
            monitor: fg(0x2e6b3a),
            bridge: fg(0x7a3f86),
            error: fg(0xb3262e).add_modifier(Modifier::BOLD),
            warn: fg(0x8a5a00),
            info: fg(0x1f6f73),
            section: fg(0x1f6f73).add_modifier(Modifier::BOLD),
            turbo: None,
        }
    }

    /// modem73-style dark theme on the terminal's own background.
    pub fn modem73() -> Self {
        let fg = |c: Color| Style::default().fg(c);
        let bold = |c: Color| Style::default().fg(c).add_modifier(Modifier::BOLD);
        Theme {
            desktop: Style::default(), window: Style::default(), border: fg(Color::DarkGray),
            titlebar: Style::default(), panel: Style::default(), input: Style::default(),
            dim: fg(Color::DarkGray), label: fg(Color::DarkGray), value: Style::default().add_modifier(Modifier::BOLD),
            good: bold(Color::Green), warnv: bold(Color::Yellow), rx: bold(Color::Green), tx: bold(Color::Red),
            echo: fg(Color::DarkGray), ask: bold(Color::Yellow), session: fg(Color::Cyan), monitor: fg(Color::Green),
            bridge: fg(Color::Magenta), error: bold(Color::Red), warn: fg(Color::Yellow), info: fg(Color::Cyan),
            section: bold(Color::Cyan), turbo: None,
        }
    }

    /// No colour at all: bold and reverse only.
    pub fn plain() -> Self {
        let b = Style::default().add_modifier(Modifier::BOLD);
        let r = Style::default().add_modifier(Modifier::REVERSED);
        let n = Style::default();
        Theme {
            desktop: n, window: n, border: n, titlebar: r, panel: r, input: n, dim: n, label: n, value: b, good: b,
            warnv: b, rx: r, tx: r.add_modifier(Modifier::BOLD), echo: n, ask: b, session: n, monitor: n, bridge: n,
            error: b, warn: n, info: n, section: b, turbo: None,
        }
    }

    fn for_line(&self, line: &str) -> Style {
        let rules: [(&str, Style); 11] = [
            ("> ", self.echo), ("? ", self.ask), ("*** ", self.session), ("[", self.monitor), ("winlink:", self.bridge),
            ("rigctl:", self.bridge), ("error", self.error), ("refused", self.error), ("warning", self.warn),
            ("discover:", self.info), ("usage", self.error),
        ];
        if line.starts_with("[ ") {
            return self.section;
        }
        rules.iter().find(|(p, _)| line.starts_with(p)).map(|(_, s)| *s).unwrap_or_default()
    }
}

/// True only when the terminal clearly supports colour.
pub fn color_supported() -> bool {
    use std::io::IsTerminal;
    if std::env::var_os("NO_COLOR").is_some() || !std::io::stdout().is_terminal() {
        return false;
    }
    let term = std::env::var("TERM").unwrap_or_default();
    if term == "dumb" {
        return false;
    }
    if cfg!(windows) || std::env::var_os("COLORTERM").is_some() {
        return true;
    }
    ["color", "xterm", "screen", "tmux", "rxvt", "linux", "vt220", "ansi"].iter().any(|t| term.contains(t))
}

// ------------------------------------------------------------------ wizards

struct Ask {
    question: String,
    default: Option<String>,
    secret: bool,
    reply: Sender<Option<String>>,
}

/// What a wizard thread uses: say lines, ask questions, and send engine jobs.
#[derive(Clone)]
struct Ctx {
    out: Sender<Out>,
    asks: Sender<Ask>,
    jobs: Sender<Job>,
    scan: Arc<Mutex<Vec<Device>>>,
}

struct Cancelled;

impl Ctx {
    fn say(&self, s: impl Into<String>) {
        let _ = self.out.send(Out::Main(s.into()));
    }

    fn ask(&self, q: &str, default: Option<&str>, secret: bool) -> Result<String, Cancelled> {
        let (tx, rx) = mpsc::channel();
        let _ = self.asks.send(Ask { question: q.into(), default: default.map(str::to_string), secret, reply: tx });
        let ans = rx.recv().map_err(|_| Cancelled)?.ok_or(Cancelled)?;
        let ans = ans.trim().to_string();
        Ok(if ans.is_empty() { default.unwrap_or("").to_string() } else { ans })
    }

    fn call<T>(&self, make: impl FnOnce(Sender<Result<T, String>>) -> Job) -> Result<T, String> {
        let (tx, rx) = mpsc::channel();
        let _ = self.jobs.send(make(tx));
        rx.recv_timeout(Duration::from_secs(600)).map_err(|_| "the radio engine did not answer".to_string())?
    }
}

fn first_run(c: &Ctx) -> Result<(), Cancelled> {
    c.say("First start: set your callsign (stored in term73's config folder only).");
    loop {
        let call = c.ask("Your callsign (with SSID if you use one, e.g. N0CALL or N0CALL-7)", None, false)?.to_ascii_uppercase();
        if config::valid_call(&call) {
            let _ = c.jobs.send(Job::SetCallsign(call));
            break;
        }
        c.say(format!("error: {call:?} does not look like a callsign"));
    }
    c.say("Next: /radio scan to find your radio. /help lists the commands.");
    Ok(())
}

fn scan_flow(c: &Ctx, show_all: bool, open: &OpenFn)
    -> Result<(), Cancelled> {
    c.say("scanning devices (radios answer an ID query; nothing transmits)...");
    let mut devs = devices::scan();
    for d in devs.iter_mut().filter(|d| d.serial && d.model.is_none()) {
        d.model = devices::identify(d, &**open);
    }
    let devs = devices::rank(devs);
    *c.scan.lock().unwrap() = devs.clone();
    if let Err(e) = LastScan::save(&devs) {
        c.say(format!("warning: could not save the scan: {e}"));
    }
    let lines = device_lines(&devs, show_all, "/radio scan all");
    let found = !lines.is_empty();
    for l in lines {
        c.say(l);
    }
    if !found {
        c.say("no radio-like devices found; /radio scan all lists every device");
        return Ok(());
    }
    let ans = c.ask("Device number to use (Enter to skip, 'all' to list everything)", Some(""), false)?;
    if ans.eq_ignore_ascii_case("all") {
        return scan_flow(c, true, open);
    }
    if let Ok(n) = ans.parse::<usize>() {
        select_flow(c, n)?;
    }
    Ok(())
}

/// The numbered device list; numbers index the full scan so /radio select works whichever view was shown.
/// Empty when nothing radio-like was found (the caller then has nothing to ask).
fn device_lines(devs: &[Device], show_all: bool, all_cmd: &str) -> Vec<String> {
    let shown: Vec<usize> = (0..devs.len()).filter(|&i| show_all || devs[i].model.is_some() || devs[i].serial).collect();
    if shown.is_empty() {
        return Vec::new();
    }
    let mut out = vec!["[ DEVICES ]".to_string()];
    for &i in &shown {
        let d = &devs[i];
        let tag = match &d.model {
            Some(m) => format!("RADIO {m}"),
            None if d.serial => "serial port".into(),
            None => "other".into(),
        };
        out.push(format!("  {:2}  {:18} {:34} {}", i + 1, tag, d.name.chars().take(34).collect::<String>(), d.target.address()));
    }
    if shown.len() < devs.len() {
        out.push(format!("  ({} other devices hidden: {all_cmd})", devs.len() - shown.len()));
    }
    out
}

fn select_flow(c: &Ctx, n: usize) -> Result<(), Cancelled> {
    let devs = c.scan.lock().unwrap().clone();
    let Some(d) = n.checked_sub(1).and_then(|i| devs.get(i)) else {
        c.say(if devs.is_empty() { "error: scan first: /radio scan".into() } else { format!("error: pick 1 to {}", devs.len()) });
        return Ok(());
    };
    let _ = c.jobs.send(Job::Open(d.target.clone()));
    let (_, prof) = match c.call(Job::CurrentProfile) {
        Ok(p) => p,
        Err(e) => {
            c.say(format!("error: {e}"));
            return Ok(());
        }
    };
    if !config::load_radios().contains_key(&d.target.key()) && prof == RadioProfile::default() {
        c.say("no profile for this rig yet; setting one up");
        setup_flow(c)?;
    }
    Ok(())
}

fn show<T: serde::Serialize>(v: &Option<T>) -> String {
    match v {
        Some(x) => serde_json::to_string(x).unwrap_or_else(|_| "?".into()).replace(",", ", "),
        None => "unknown".into(),
    }
}

fn review<T: serde::Serialize + Clone>(c: &Ctx, what: &str, detected: Option<T>, saved: Option<T>, default: Option<T>,
    parse: impl Fn(&str) -> Result<T, String>) -> Result<Option<T>, Cancelled> {
    let (current, source) = match (&saved, &detected) {
        (Some(s), _) => (Some(s.clone()), "saved"),
        (None, Some(d)) => (Some(d.clone()), "detected"),
        _ => (default.clone(), "default"),
    };
    loop {
        let shown = show(&current);
        let ans = c.ask(&format!("{what} ({source})"), Some(&shown), false)?;
        if ans == shown {
            return Ok(current);
        }
        match parse(&ans) {
            Ok(v) => return Ok(Some(v)),
            Err(e) => c.say(format!("error: {e}")),
        }
    }
}

fn parse_bool(s: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "y" | "yes" | "true" | "1" => Ok(true),
        "n" | "no" | "false" | "0" => Ok(false),
        _ => Err("yes or no".into()),
    }
}

fn parse_num<T: std::str::FromStr>(s: &str) -> Result<T, String> {
    s.trim().parse().map_err(|_| format!("{s:?} is not a number"))
}

fn setup_flow(c: &Ctx) -> Result<(), Cancelled> {
    let (model, saved) = match c.call(Job::CurrentProfile) {
        Ok(v) => v,
        Err(e) => {
            c.say(format!("error: {e}"));
            return Ok(());
        }
    };
    let model = model.unwrap_or_default();
    let mut p = RadioProfile::default();
    if model == "MODEM73" {
        c.say("[ MODEM73 RIG ]  Review each setting: Enter keeps the value shown, or type a new one.");
        p.tune_via_modem = review(c, "tune the radio through modem73's rig control (modem73 must run with rigctl/Hamlib PTT)",
                                  None, saved.tune_via_modem, Some(false), parse_bool)?;
        p.t1 = review(c, "AX.25 retry timer, seconds (longer for slow HF modes)", None, saved.t1, Some(15.0), parse_num)?;
        p.paclen = review(c, "AX.25 packet length, bytes", None, saved.paclen, Some(128), parse_num)?;
        p.window = review(c, "AX.25 frames in flight", None, saved.window, Some(2), parse_num)?;
    } else {
        let run = c.ask("Detect capabilities now? Frequency, power and packet mode change briefly and are restored; nothing transmits (y/n)",
                        Some("y"), false)?;
        let d: Option<Discovery> = if run.to_ascii_lowercase().starts_with('y') {
            match c.call(Job::Discover) {
                Ok(d) => Some(d),
                Err(e) => {
                    c.say(format!("error: {e}"));
                    None
                }
            }
        } else {
            None
        };
        c.say("[ RADIO PROFILE ]  Review each setting: Enter keeps the value shown, or type a new one.");
        p.data_band = review(c, "data band used for packet (0 = A, 1 = B)", d.as_ref().map(|d| d.data_band), saved.data_band, Some(1),
                             |s| parse_num::<u8>(s).and_then(|b| if b <= 1 { Ok(b) } else { Err("0 or 1".into()) }))?;
        p.power_levels = review(c, "power levels the data band accepts, e.g. [0, 1, 2]",
                                d.as_ref().filter(|d| !d.power_levels.is_empty()).map(|d| d.power_levels.clone()),
                                saved.power_levels.clone(), Some(vec![0, 1, 2]),
                                |s| serde_json::from_str(s).map_err(|_| "a list like [0, 1, 2]".to_string()))?;
        p.power_high = review(c, "power level used for gateway and BBS connections",
                              d.as_ref().filter(|d| d.power_levels.contains(&0)).map(|_| 0), saved.power_high, Some(0), parse_num)?;
        p.bands_mhz = review(c, "transmit ranges for packet, MHz, e.g. [[144.0, 148.0]]",
                             d.as_ref().and_then(|d| discover::detected_bands(&d.accepted_mhz)), saved.bands_mhz.clone(),
                             Some(gateways::default_bands(&model)),
                             |s| serde_json::from_str(s).map_err(|_| "a list like [[144.0, 148.0]]".to_string()))?;
        p.shift_field = review(c, "position of the repeater-shift field in the FO reply (not detectable)", None,
                               saved.shift_field, gateways::default_shift_field(&model), parse_num)?;
        p.duplicate_replies = review(c, "the radio sometimes repeats replies", d.as_ref().map(|d| d.duplicate_replies),
                                     saved.duplicate_replies, Some(false), parse_bool)?;
        p.kiss_exit_trailer = review(c, "leaving packet mode needs a byte after the exit frame",
                                     d.as_ref().and_then(|d| d.kiss_exit_needs_trailer), saved.kiss_exit_trailer, Some(true), parse_bool)?;
        p.ptt_verified = review(c, "rig-control TX keys the DATA band (say yes only after checking on the radio)", None,
                                saved.ptt_verified, Some(false), parse_bool)?;
        p.t1 = review(c, "AX.25 retry timer, seconds", None, saved.t1, Some(10.0), parse_num)?;
        p.paclen = review(c, "AX.25 packet length, bytes", None, saved.paclen, Some(128), parse_num)?;
        p.window = review(c, "AX.25 frames in flight", None, saved.window, Some(4), parse_num)?;
    }
    if let Err(e) = c.call(|tx| Job::SaveProfile(p, tx)) {
        c.say(format!("error: {e}"));
    }
    Ok(())
}

fn bbs_add_flow(c: &Ctx) -> Result<(), Cancelled> {
    bbs_flow(c, None)
}

/// Add a BBS, or edit one (`existing`): every question then offers the saved value.
fn bbs_flow(c: &Ctx, existing: Option<(String, Bbs)>) -> Result<(), Cancelled> {
    let old = existing.as_ref();
    let name = c.ask("BBS name (used with /bbs connect)", old.map(|(n, _)| n.as_str()), false)?;
    if name.is_empty() {
        return Ok(());
    }
    let call = c.ask("BBS callsign or node alias (e.g. N0BBS-3 or NODE1)", old.map(|(_, b)| b.call.as_str()), false)?
        .to_ascii_uppercase();
    let old_mhz = old.map(|(_, b)| format!("{:.3}", b.mhz));
    let mhz: f64 = loop {
        match c.ask("Frequency, MHz (e.g. 145.030)", old_mhz.as_deref(), false)?.parse() {
            Ok(v) => break v,
            Err(_) => c.say("error: a frequency like 145.030"),
        }
    };
    let old_path = old.map(|(_, b)| b.path.join(",")).unwrap_or_default();
    let path_q = if old_path.is_empty() {
        "Digipeaters on the way, comma separated (Enter for none)"
    } else {
        "Digipeaters on the way, comma separated (Enter keeps them, 'none' clears)"
    };
    let path_ans = c.ask(path_q, Some(&old_path), false)?;
    let path: Vec<String> = if path_ans.eq_ignore_ascii_case("none") {
        Vec::new()
    } else {
        path_ans.split(',').map(|p| p.trim().to_ascii_uppercase()).filter(|p| !p.is_empty()).collect()
    };
    let old_baud = old.map(|(_, b)| b.baud.to_string()).unwrap_or_else(|| "1200".into());
    let baud: u32 = loop {
        match c.ask("Speed (1200 or 9600)", Some(&old_baud), false)?.parse() {
            Ok(v @ (1200 | 9600)) => break v,
            _ => c.say("error: 1200 or 9600"),
        }
    };
    let mut all = config::load_bbs();
    if let Some((old_name, _)) = old.filter(|(n, _)| *n != name) {
        all.remove(old_name);
    }
    let via = if path.is_empty() { String::new() } else { format!(" via {}", path.join(",")) };
    // a new callsign may be a different station, so its AX.25 version is learned again
    let ax25 = old.filter(|(_, b)| b.call == call).and_then(|(_, b)| b.ax25.clone());
    all.insert(name.clone(), Bbs { call: call.clone(), mhz, path, baud, ax25 });
    match config::save_bbs(&all) {
        Ok(()) => c.say(format!("saved BBS {name}: {call} on {mhz:.3} MHz{via}, {baud} baud")),
        Err(e) => c.say(format!("error: {e}")),
    }
    Ok(())
}

fn winlink_setup_flow(c: &Ctx) -> Result<(), Cancelled> {
    let cfg = config::AppConfig::load();
    loop {
        let grid = c.ask("Your grid locator (e.g. FN31pr)", cfg.grid.as_deref(), false)?;
        if config::valid_grid(&grid) {
            let _ = c.jobs.send(Job::SetGrid(grid));
            break;
        }
        c.say(format!("error: {grid:?} is not a grid locator"));
    }
    let key = c.ask("Winlink API key for the gateway list (issued by the Winlink Development Team)",
                    cfg.winlink_api_key.as_ref().map(|_| "stored"), true)?;
    let mut cfg = config::AppConfig::load();
    if !key.is_empty() && key != "stored" {
        cfg.winlink_api_key = Some(key);
        let _ = cfg.save();
    }
    let Some(key) = cfg.winlink_api_key.clone() else {
        c.say("error: no API key; the gateway list cannot be downloaded without one");
        return Ok(());
    };
    c.say("downloading the gateway list...");
    match crate::rmslist::download(&key) {
        Ok((n, path)) => {
            c.say(format!("{n} gateways saved to {}", path.display()));
            let _ = c.jobs.send(Job::Gateways(10));
        }
        Err(e) => c.say(format!("error: {e}")),
    }
    Ok(())
}

// ------------------------------------------------------------------ the app

pub struct App {
    pub theme: Theme,
    engine: Handle,
    out_rx: Receiver<Out>,
    out_tx: Sender<Out>,
    ask_rx: Receiver<Ask>,
    ctx: Ctx,
    open: OpenFn,
    pub main: Vec<String>,
    /// Per line of `main`: true when it came from the other station (shown as is, never styled by content).
    remote: Vec<bool>,
    /// The last line of `main` is remote text still waiting for its line end.
    remote_open: bool,
    pub traffic: Vec<String>,
    scroll: usize,
    pub input: String,
    cursor: usize,
    history: Vec<String>,
    hist_pos: Option<usize>,
    pending: Option<Ask>,
    busy: Arc<AtomicUsize>,
    last_ctrl_c: Option<Instant>,
    pub quit: bool,
    snap: Snapshot,
    menu: Option<turbo::MenuState>,
    popup: Option<popup::Popup>,
    /// F8 hides the side panels (rig, stations or session).
    side_hidden: bool,
}

impl App {
    pub fn new(theme: Theme, opener_for_engine: engine::Opener,
               open: OpenFn) -> Self {
        let (out_tx, out_rx) = mpsc::channel();
        let engine = engine::spawn(out_tx.clone(), opener_for_engine);
        let (ask_tx, ask_rx) = mpsc::channel();
        let ctx = Ctx { out: out_tx.clone(), asks: ask_tx, jobs: engine.jobs.clone(), scan: Arc::new(Mutex::new(LastScan::load().devices)) };
        let mut app = App {
            theme, engine, out_rx, out_tx, ask_rx, ctx, open, main: vec![WELCOME.into()], remote: vec![false], remote_open: false, traffic: Vec::new(),
            scroll: 0, input: String::new(), cursor: 0, history: Vec::new(), hist_pos: None, pending: None,
            busy: Arc::new(AtomicUsize::new(0)), last_ctrl_c: None, quit: false, snap: Snapshot::default(), menu: None, popup: None, side_hidden: false,
        };
        if config::AppConfig::load().callsign.is_none() {
            app.wizard(first_run);
        }
        app
    }

    /// Use these as the result of the last scan (tests and scripted demos).
    pub fn set_scan(&mut self, devs: Vec<Device>) {
        *self.ctx.scan.lock().unwrap() = devs;
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snap
    }

    pub fn shutdown(self) {
        self.engine.shutdown();
    }

    /// Shut down within a time budget (the window is closing): a station gets at most `wait` to acknowledge.
    pub fn shutdown_within(self, wait: Duration) {
        self.engine.shutdown_within(wait);
    }

    fn wizard(&mut self, f: impl FnOnce(&Ctx) -> Result<(), Cancelled> + Send + 'static) {
        let ctx = self.ctx.clone();
        let busy = self.busy.clone();
        busy.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            if f(&ctx).is_err() {
                ctx.say("cancelled");
            }
            busy.fetch_sub(1, Ordering::SeqCst);
        });
    }

    fn push(&mut self, text: &str) {
        for l in text.split('\n') {
            self.main.push(l.to_string());
            self.remote.push(false);
        }
        self.remote_open = false;
        self.trim_main();
    }

    /// Remote text arrives in frame-sized pieces; join them so lines only break where the station ended them.
    fn push_remote(&mut self, text: &str) {
        let (body, ends_line) = match text.strip_suffix('\n') {
            Some(b) => (b, true),
            None => (text, false),
        };
        for (i, l) in body.split('\n').enumerate() {
            if i == 0 && self.remote_open && let Some(last) = self.main.last_mut() {
                last.push_str(l);
            } else {
                self.main.push(l.to_string());
                self.remote.push(true);
            }
        }
        self.remote_open = !ends_line;
        self.trim_main();
    }

    fn trim_main(&mut self) {
        let excess = self.main.len().saturating_sub(5000);
        self.main.drain(..excess);
        self.remote.drain(..excess);
    }

    /// Pull engine output and pending questions; call every frame.
    pub fn tick(&mut self) {
        while let Ok(o) = self.out_rx.try_recv() {
            match o {
                Out::Main(s) => self.push(&s),
                Out::Remote(s) => self.push_remote(&s),
                Out::Traffic(s) => {
                    self.traffic.push(s);
                    let excess = self.traffic.len().saturating_sub(500);
                    self.traffic.drain(..excess);
                }
            }
        }
        if self.pending.is_none()
            && let Ok(a) = self.ask_rx.try_recv() {
                let shown = a.default.as_ref().filter(|d| !d.is_empty()).map(|d| format!(" [{d}]")).unwrap_or_default();
                self.push(&format!("? {}{shown}", a.question));
                self.pending = Some(a);
            }
        self.snap = self.engine.snap.lock().unwrap().clone();
    }

    pub fn waiting_for_answer(&self) -> bool {
        self.pending.is_some()
    }

    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst) > 0
    }

    // -------------------------------------------------------------- input

    pub fn key_char(&mut self, ch: char) {
        let at = self.input.char_indices().nth(self.cursor).map(|(i, _)| i).unwrap_or(self.input.len());
        self.input.insert(at, ch);
        self.cursor += 1;
    }

    pub fn key_backspace(&mut self) {
        if self.cursor > 0 {
            let at = self.input.char_indices().nth(self.cursor - 1).map(|(i, _)| i).unwrap_or(0);
            self.input.remove(at);
            self.cursor -= 1;
        }
    }

    pub fn key_delete(&mut self) {
        if self.cursor < self.input.chars().count() {
            let at = self.input.char_indices().nth(self.cursor).map(|(i, _)| i).unwrap_or(self.input.len());
            self.input.remove(at);
        }
    }

    pub fn key_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn key_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
    }

    pub fn key_home(&mut self) {
        self.cursor = 0;
    }

    pub fn key_end(&mut self) {
        self.cursor = self.input.chars().count();
    }

    pub fn history(&mut self, up: bool) {
        if self.history.is_empty() || self.pending.as_ref().is_some_and(|p| p.secret) {
            return;
        }
        let pos = match (self.hist_pos, up) {
            (None, true) => self.history.len() - 1,
            (Some(p), true) => p.saturating_sub(1),
            (Some(p), false) if p + 1 < self.history.len() => p + 1,
            _ => {
                self.hist_pos = None;
                self.input.clear();
                self.cursor = 0;
                return;
            }
        };
        self.hist_pos = Some(pos);
        self.input = self.history[pos].clone();
        self.cursor = self.input.chars().count();
    }

    pub fn scroll(&mut self, up: bool, page: usize) {
        self.scroll = if up { self.scroll + page } else { self.scroll.saturating_sub(page) };
    }

    /// Complete the command word or its sub-command; lists the choices when there are several.
    pub fn complete(&mut self) {
        if !self.input.starts_with('/') {
            return;
        }
        let words: Vec<&str> = self.input.split(' ').collect();
        let (choices, prefix): (Vec<String>, &str) = match words.len() {
            1 => (COMMANDS.iter().map(|(c, _)| c.to_string()).collect(), words[0]),
            2 => (COMMANDS.iter().find(|(c, _)| *c == words[0]).map(|(_, s)| s.iter().map(|s| s.to_string()).collect())
                    .unwrap_or_default(), words[1]),
            _ => return,
        };
        let hits: Vec<&String> = choices.iter().filter(|c| c.starts_with(prefix)).collect();
        match hits.len() {
            0 => {}
            1 => {
                let mut w: Vec<String> = words.iter().map(|s| s.to_string()).collect();
                let last = w.len() - 1;
                w[last] = hits[0].clone();
                self.input = format!("{} ", w.join(" "));
                self.cursor = self.input.chars().count();
            }
            _ => {
                let list: Vec<&str> = hits.iter().map(|s| s.as_str()).collect();
                self.push(&format!("  {}", list.join("   ")));
            }
        }
    }

    /// Send a raw CAT line; one that writes memory or can cut the link is shown first and needs a yes.
    fn cat_with_confirm(&mut self, line: String) {
        self.wizard(move |c| {
            match c.call(|r| Job::CatPreview(line.clone(), r)) {
                Ok(None) => {}
                Ok(Some(what)) => {
                    c.say(format!("{}: {what}", line.trim()));
                    if !c.ask("Send it? (y/n)", Some("n"), false)?.trim().to_ascii_lowercase().starts_with('y') {
                        c.say("not sent");
                        return Ok(());
                    }
                    let _ = c.jobs.send(Job::CatConfirmed(line));
                    return Ok(());
                }
                Err(e) => {
                    c.say(format!("error: {e}"));
                    return Ok(());
                }
            }
            let _ = c.jobs.send(Job::Cat(line));
            Ok(())
        });
    }

    /// F8: show or hide the side panels.
    pub fn toggle_side(&mut self) {
        self.side_hidden = !self.side_hidden;
    }

    pub fn ctrl_c(&mut self) {
        if let Some(a) = self.pending.take() {
            let _ = a.reply.send(None);
            return;
        }
        let recent = self.last_ctrl_c.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
        if self.snap.remote.is_some() && !recent {
            self.push("*** Ctrl+C: ending the connection (press again to quit)");
            self.engine.send(Job::Disconnect);
        } else if recent {
            self.quit = true;
        } else {
            self.push("press Ctrl+C again (or Ctrl+Q) to quit");
        }
        self.last_ctrl_c = Some(Instant::now());
    }

    /// In a session, typed text goes to the station (not the Winlink bridge, which relays its own client).
    fn in_session(&self) -> bool {
        self.snap.packet == Packet::Connected && self.snap.remote.is_some() && !self.snap.winlink_ready
    }

    /// Ctrl+Z ends a BBS message: send the Ctrl-Z character (0x1A) as its own line.
    pub fn ctrl_z(&mut self) {
        if self.in_session() {
            self.push("> ^Z");
            self.engine.send(Job::SendLine("\u{1a}".into()));
        } else {
            self.push("not connected: Ctrl+Z ends a message on a BBS");
        }
    }

    pub fn enter(&mut self) {
        let line = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.hist_pos = None;
        self.scroll = 0;
        if let Some(a) = self.pending.take() {
            if line.starts_with('/') {
                self.push(&format!("> {line}\n(answer the question first, or Ctrl+C to cancel it)"));
                self.pending = Some(a);
                return;
            }
            self.push(&format!("> {}", if a.secret { "*".repeat(line.chars().count()) } else { line.clone() }));
            let _ = a.reply.send(Some(line));
            return;
        }
        if line.trim().is_empty() {
            // an empty line is real text in a session: a blank line in a BBS message, or Enter at a "more" prompt
            if self.in_session() {
                self.push(">");
                self.engine.send(Job::SendLine(line));
            }
            return;
        }
        if self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }
        if self.busy() && !line.starts_with('/') {
            self.push(&format!("> {line}\n(busy: wait for the next question; the line was not sent)"));
            return;
        }
        self.push(&format!("> {line}"));
        // While connected, "/" lines that are not ours (e.g. a BBS's /EX) go to the station; "//" sends one "/".
        if let Some(text) = self.in_session().then(|| slash_to_station(&line)).flatten() {
            self.engine.send(Job::SendLine(text));
        } else if line.starts_with('/') {
            if let Err(e) = self.command(&line) {
                self.push(&format!("usage error: {e}"));
            }
        } else if self.in_session() {
            self.engine.send(Job::SendLine(line));
        } else {
            self.push("not connected: /connect <CALL> or /bbs connect <name> (/help for more)");
        }
    }

    fn command(&mut self, line: &str) -> Result<(), String> {
        let words: Vec<&str> = line.split_whitespace().collect();
        let group = words[0].to_ascii_lowercase();
        let sub = words.get(1).map(|s| s.to_ascii_lowercase()).unwrap_or_default();
        let rest: Vec<&str> = words.iter().skip(2).copied().collect();
        let arg = |i: usize| rest.get(i).copied().ok_or_else(|| format!("{} needs more arguments (/help)", words[0]));
        let e = &self.engine;
        match group.as_str() {
            "/help" => {
                if let Some((_, text)) = HELP.iter().find(|(k, _)| *k == sub) {
                    let text = format!("[ {} ]\n{text}", sub.to_ascii_uppercase());
                    self.push(&text);
                } else {
                    for g in BASIC {
                        let text = HELP.iter().find(|(k, _)| k == g).unwrap().1;
                        self.push(&format!("[ {} ]\n{text}", g.to_ascii_uppercase()));
                    }
                    self.push("/help advanced lists raw rig-control and TNC commands");
                }
            }
            "/clear" => {
                self.main.clear();
                self.remote.clear();
                self.remote_open = false;
            }
            "/quit" => self.quit = true,
            "/connect" => {
                let call = words.get(1).ok_or("/connect <CALL> [via D1,D2]")?;
                let path = if rest.first().is_some_and(|w| w.eq_ignore_ascii_case("via")) {
                    rest.get(1).map(|p| p.split(',').map(str::to_string).collect()).unwrap_or_default()
                } else {
                    Vec::new()
                };
                e.send(Job::Connect(call.to_string(), path));
            }
            "/disconnect" => e.send(Job::Disconnect),
            "/listen" => match sub.as_str() {
                "off" => e.send(Job::Listen(false, None)),
                "save" => e.send(Job::Listen(true, Some(arg(0).unwrap_or("packets.jsonl").into()))),
                "" => e.send(Job::Listen(true, None)),
                mhz => {
                    e.send(Job::Tune(mhz.parse().map_err(|_| "/listen [MHz]")?));
                    e.send(Job::Listen(true, None));
                }
            },
            "/frequency" => e.send(Job::Tune(words.get(1).ok_or("/frequency <MHz>")?.parse().map_err(|_| "/frequency <MHz>")?)),
            "/power" => e.send(Job::Power(match sub.as_str() {
                "high" => 0,
                "mid" => 1,
                "low" => 2,
                n => n.parse().map_err(|_| "/power high|mid|low")?,
            })),
            "/transmit" | "/tx" => e.send(Job::SetTransmit(sub == "on")),
            "/radio" => match sub.as_str() {
                "scan" => {
                    let all = rest.first().is_some_and(|w| w.eq_ignore_ascii_case("all"));
                    let open = self.open.clone();
                    self.wizard(move |c| scan_flow(c, all, &open));
                }
                "list" => {
                    let all = rest.first().is_some_and(|w| w.eq_ignore_ascii_case("all"));
                    let devs = self.ctx.scan.lock().unwrap().clone();
                    let lines = device_lines(&devs, all, "/radio list all");
                    if devs.is_empty() {
                        return Err("no scan yet: /radio scan".into());
                    }
                    let when = LastScan::load().utc_secs;
                    if lines.is_empty() {
                        self.push(&format!("no radio-like devices in the last scan; /radio list all shows all {}", devs.len()));
                    } else {
                        if when > 0 {
                            self.push(&format!("last scan {} UTC; /radio select <n> to use one", engine::hhmmss(when)));
                        }
                        self.push(&lines.join("\n"));
                    }
                }
                "select" => {
                    let n: usize = arg(0)?.parse().map_err(|_| "/radio select <n>")?;
                    self.wizard(move |c| select_flow(c, n));
                }
                "setup" => self.wizard(setup_flow),
                "info" => e.send(Job::Ident),
                "off" => e.send(Job::Close),
                _ => self.push(HELP[3].1),
            },
            "/bbs" => match sub.as_str() {
                "add" => self.wizard(bbs_add_flow),
                "edit" => {
                    let name = arg(0)?.to_string();
                    let b = config::load_bbs().get(&name).cloned().ok_or(format!("no BBS profile {name:?}: /bbs list"))?;
                    self.wizard(move |c| bbs_flow(c, Some((name, b))));
                }
                "list" => {
                    let all: BTreeMap<String, Bbs> = config::load_bbs();
                    self.push("[ BBS ]");
                    if all.is_empty() {
                        self.push("no BBS profiles: /bbs add");
                    }
                    for (name, b) in all {
                        let via = if b.path.is_empty() { String::new() } else { format!(" via {}", b.path.join(",")) };
                        let ver = b.ax25.as_deref().map(|v| format!(", AX.25 v{v}")).unwrap_or_default();
                        self.push(&format!("  {name:12} {:10} {:8.3} MHz {} baud{via}{ver}", b.call, b.mhz, b.baud));
                    }
                }
                "connect" => e.send(Job::BbsConnect(arg(0)?.to_string())),
                "remove" => {
                    let mut all = config::load_bbs();
                    let name = arg(0)?;
                    all.remove(name).ok_or(format!("no BBS profile {name:?}"))?;
                    config::save_bbs(&all).map_err(|e| e.to_string())?;
                    self.push(&format!("removed BBS {name}"));
                }
                _ => self.push(HELP[1].1),
            },
            "/winlink" => match sub.as_str() {
                "setup" => self.wizard(winlink_setup_flow),
                "gateways" => e.send(Job::Gateways(15)),
                "start" => e.send(Job::WinlinkStart(8772)),
                "stop" => e.send(Job::WinlinkStop),
                _ => self.push(HELP[2].1),
            },
            "/config" => match sub.as_str() {
                "callsign" => e.send(Job::SetCallsign(arg(0)?.to_string())),
                "grid" => e.send(Job::SetGrid(arg(0)?.to_string())),
                _ => {
                    let cfg = config::AppConfig::load();
                    let text = format!("[ CONFIG ]\n  config folder  {}\n  callsign       {}\n  grid           {}\n  Winlink key    {}\n  radio profiles {}, BBS profiles {}",
                        config::home().display(), cfg.callsign.as_deref().unwrap_or("(not set)"),
                        cfg.grid.as_deref().unwrap_or("(not set)"),
                        if cfg.winlink_api_key.is_some() { "stored" } else { "(not set)" },
                        config::load_radios().len(), config::load_bbs().len());
                    self.push(&text);
                }
            },
            "/advanced" => match (sub.as_str(), rest.first().map(|s| s.to_ascii_lowercase()).as_deref()) {
                ("cat", _) => self.cat_with_confirm(rest.join(" ")),
                ("watch", Some("off")) => e.send(Job::Watch(None)),
                ("watch", Some(_)) => e.send(Job::Watch(Some(rest.join(" ")))),
                ("kiss", Some("on")) => e.send(Job::KissOn),
                ("kiss", Some("off")) => e.send(Job::KissOff),
                ("modem", Some("status")) => e.send(Job::ModemStatus),
                ("modem", Some("set")) => {
                    let mut m = serde_json::Map::new();
                    for item in &rest[1..] {
                        let (k, v) = item.split_once('=').ok_or("key=value")?;
                        let val = v.parse::<i64>().map(serde_json::Value::from)
                            .or_else(|_| v.parse::<bool>().map(serde_json::Value::from))
                            .unwrap_or_else(|_| serde_json::Value::from(v));
                        m.insert(k.to_string(), val);
                    }
                    e.send(Job::ModemSet(serde_json::Value::Object(m)));
                }
                ("rigctl", Some("start")) => e.send(Job::RigctlStart(rest.get(1).and_then(|p| p.parse().ok()).unwrap_or(4532))),
                ("rigctl", Some("stop")) => e.send(Job::RigctlStop),
                _ => {
                    let text = format!("[ ADVANCED ]\n{}", HELP[5].1);
                    self.push(&text);
                }
            },
            other => self.push(&format!("unknown command {other}; /help lists them")),
        }
        let _ = &self.out_tx;
        Ok(())
    }

    // -------------------------------------------------------------- drawing

    pub fn draw(&mut self, f: &mut Frame) {
        if self.theme.turbo.is_some() {
            return self.draw_turbo(f);
        }
        let t = self.theme.clone();
        let area = f.area();
        f.render_widget(Block::default().style(t.desktop), area);
        let rows = Layout::default().direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(5), Constraint::Length(1), Constraint::Length(1)]).split(area);
        let wide = area.width >= 100 && !self.side_hidden;
        let body = Rect { x: rows[1].x + 2, width: rows[1].width.saturating_sub(4), ..rows[1] };
        let cols = Layout::default().direction(Direction::Horizontal)
            .constraints(if wide { vec![Constraint::Min(40), Constraint::Length(1), Constraint::Length(34)] } else { vec![Constraint::Min(40)] })
            .split(body);
        let left = if self.snap.listening {
            Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(6), Constraint::Length(10)]).split(cols[0])
        } else {
            Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(6)]).split(cols[0])
        };
        self.draw_main(f, left[0]);
        if self.snap.listening {
            self.draw_lines(f, left[1], " TRAFFIC ", &self.traffic.clone(), 0);
        }
        if wide {
            let side = Layout::default().direction(Direction::Vertical)
                .constraints([Constraint::Length(8), Constraint::Min(4)]).split(cols[2]);
            self.draw_panel(f, side[0], " RIG ", self.rig_lines(&t));
            let (title, lines) = self.station_panel(&t);
            self.draw_panel(f, side[1], &title.to_uppercase(), lines);
        }
        f.render_widget(Paragraph::new(self.status_line(wide)).style(t.panel), rows[2]);
        let masked = self.pending.as_ref().is_some_and(|p| p.secret);
        let shown = if masked { "*".repeat(self.input.chars().count()) } else { self.input.clone() };
        f.render_widget(Paragraph::new(format!("> {shown}")).style(t.input), rows[3]);
        let before: usize = self.input.chars().take(self.cursor).map(|_| 1).sum();
        f.set_cursor_position((rows[3].x + 2 + before as u16, rows[3].y));
    }

    fn title_line(&self) -> Line<'static> {
        let t = &self.theme;
        let chip = if self.snap.transmitting { Span::styled(" TX ", t.tx) } else { Span::styled(" RX ", t.rx) };
        Line::from(vec![
            Span::styled("/ / / ", t.dim.patch(t.titlebar)), Span::styled("TERM73", t.value.patch(t.titlebar)),
            Span::styled(format!(" v{VERSION}  "), t.dim.patch(t.titlebar)), chip, Span::styled("  ", t.titlebar),
            Span::styled(self.snap.callsign.clone().unwrap_or_else(|| "no callsign".into()), t.value.patch(t.titlebar)),
        ]).alignment(Alignment::Center)
    }

    fn draw_main(&mut self, f: &mut Frame, area: Rect) {
        let t = self.theme.clone();
        let block = Block::default().borders(Borders::ALL).border_type(BorderType::Plain).border_style(t.border).style(t.window);
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height == 0 {
            return;
        }
        f.render_widget(Paragraph::new(self.title_line()).style(t.titlebar), Rect { height: 1, ..inner });
        let text_area = Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner };
        let (lines, remote) = (self.main.clone(), self.remote.clone());
        self.render_wrapped(f, text_area, &lines, Some(&remote), self.scroll);
    }

    fn draw_lines(&self, f: &mut Frame, area: Rect, title: &str, lines: &[String], scroll: usize) {
        let t = &self.theme;
        let block = Block::default().borders(Borders::ALL).border_style(t.border).style(t.window)
            .title(Line::from(Span::styled(title.to_string(), t.titlebar)).alignment(Alignment::Center));
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.render_wrapped(f, inner, lines, None, scroll);
    }

    fn render_wrapped(&self, f: &mut Frame, area: Rect, lines: &[String], remote: Option<&[bool]>, scroll: usize) {
        let w = area.width.max(1) as usize;
        let mut rows: Vec<Line> = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            let from_station = remote.and_then(|r| r.get(i)).copied().unwrap_or(false);
            let style = if from_station { Style::default() } else { self.theme.for_line(l) };
            let chars: Vec<char> = l.chars().collect();
            if chars.is_empty() {
                rows.push(Line::from(""));
            }
            for chunk in chars.chunks(w) {
                rows.push(Line::from(Span::styled(chunk.iter().collect::<String>(), style)));
            }
        }
        let h = area.height as usize;
        let max_scroll = rows.len().saturating_sub(h);
        let scroll = scroll.min(max_scroll);
        let start = rows.len().saturating_sub(h + scroll);
        let end = rows.len().saturating_sub(scroll);
        f.render_widget(Paragraph::new(rows[start..end].to_vec()).style(self.theme.window), area);
    }

    fn draw_panel(&self, f: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) {
        let t = &self.theme;
        let block = Block::default().borders(Borders::ALL).border_style(t.border).style(t.window)
            .title(Line::from(Span::styled(title.to_string(), t.titlebar)).alignment(Alignment::Center));
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(Paragraph::new(lines).style(t.window), inner);
    }

    fn row(t: &Theme, label: &str, value: String, style: Style) -> Line<'static> {
        Line::from(vec![Span::styled(format!(" {label:<10}"), t.label), Span::styled(value, style)])
    }

    fn rig_lines(&self, t: &Theme) -> Vec<Line<'static>> {
        let s = &self.snap;
        let Some(model) = s.model.clone() else {
            return vec![Self::row(t, "rig", "none".into(), t.value), Line::from(Span::styled(" /radio scan to find one", t.dim))];
        };
        // rig state only; sessions, listening and Winlink belong to the Stations/Session panel
        let tnc = match (s.software_modem, s.packet) {
            (true, _) => "software modem",
            (false, Packet::Idle) => "command mode",
            (false, _) => "packet (KISS)",
        };
        vec![
            Self::row(t, "rig", model, t.value),
            Self::row(t, "address", s.address.clone().unwrap_or_default(), t.value),
            Self::row(t, "profile", if s.profile_saved { "saved".into() } else { "none".into() }, if s.profile_saved { t.value } else { t.warnv }),
            Self::row(t, "frequency", s.freq_mhz.map(|f| format!("{f:.3} MHz")).unwrap_or("?".into()), t.value),
            Self::row(t, "TNC", tnc.into(), if s.packet == Packet::Idle { t.value } else { t.good }),
            Self::row(t, "PTT band", s.bands.map(|(_, p)| engine::band_name(p).to_string()).unwrap_or_else(|| "?".into()), t.value),
            Self::row(t, "transmit", if s.transmit_allowed { "ALLOWED".into() } else { "off".into() }, if s.transmit_allowed { t.tx } else { t.value }),
        ]
    }

    /// The lower side panel: saved stations when idle, the live session while connecting or connected.
    fn station_panel(&self, t: &Theme) -> (String, Vec<Line<'static>>) {
        match &self.snap.session {
            Some(s) => (format!(" Session: {} ", s.remote), self.session_lines(t, s)),
            None => (" Stations ".into(), self.stations_lines(t)),
        }
    }

    fn heard_of(&self, call: &str) -> Option<&engine::HeardEntry> {
        self.snap.heard.iter().find(|h| crate::ax25::same_call(&h.call, call))
    }

    fn stations_lines(&self, t: &Theme) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if self.snap.listening {
            out.push(Line::from(Span::styled(" listening to the channel", t.good)));
        }
        if self.snap.winlink_ready {
            out.push(Line::from(Span::styled(" Winlink: waiting for a client on 127.0.0.1:8772", t.good)));
        }
        let all = config::load_bbs();
        if all.is_empty() {
            out.push(Line::from(Span::styled(" none saved", t.dim)));
            out.push(Line::from(Span::styled(" /bbs add", t.dim)));
            return out;
        }
        for (name, b) in all {
            out.push(Line::from(vec![
                Span::styled(format!(" {name:<10}"), t.value), Span::styled(format!("{:<10}", b.call), t.label),
                Span::styled(format!("{:.3}", b.mhz), t.dim),
            ]));
            let heard = match self.heard_of(&b.call) {
                Some(h) => format!("heard {} ({}x)", &engine::hhmmss(h.last_utc_secs)[..5], h.count),
                None => "not heard yet".into(),
            };
            let ver = b.ax25.as_deref().map(|v| format!(", AX.25 v{v}")).unwrap_or_default();
            out.push(Line::from(Span::styled(format!("   {heard}{ver}"), t.dim)));
        }
        out
    }

    fn session_lines(&self, t: &Theme, s: &engine::SessionInfo) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if let Some(n) = &s.name {
            out.push(Self::row(t, "profile", n.clone(), t.value));
        }
        out.push(Self::row(t, "via", if s.path.is_empty() { "direct".into() } else { s.path.join(",") }, t.value));
        if !s.connected {
            out.push(Self::row(t, "state", "connecting".into(), t.warnv));
            return out;
        }
        out.push(Self::row(t, "protocol", format!("AX.25 v{}", s.version), t.good));
        out.push(Self::row(t, "frames", format!("{} B, {} in flight", s.paclen, s.window), t.value));
        if let Some(since) = s.since_utc_secs {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(since);
            let secs = now.saturating_sub(since);
            out.push(Self::row(t, "connected", format!("{}:{:02}", secs / 60, secs % 60), t.value));
        }
        out.push(Self::row(t, "sent", format!("{} ({} B)", s.stats.sent, s.stats.bytes_out), t.value));
        out.push(Self::row(t, "received", format!("{} ({} B)", s.stats.received, s.stats.bytes_in), t.value));
        out.push(Self::row(t, "resent", s.stats.resent.to_string(), if s.stats.resent > 0 { t.warnv } else { t.value }));
        out.push(Self::row(t, "unacked", s.unacked.to_string(), t.value));
        if let Some(h) = self.heard_of(&s.remote) {
            out.push(Self::row(t, "heard", format!("{}x, last {}", h.count, &engine::hhmmss(h.last_utc_secs)[..5]), t.value));
        }
        out
    }

    fn status_line(&self, wide: bool) -> Line<'static> {
        let t = &self.theme;
        let mut spans = Vec::new();
        if wide {
            for (k, v) in [("F8", "panels"), ("Ctrl+C", "disconnect"), ("PgUp/PgDn", "scroll"), ("Tab", "complete"), ("/help", "commands"), ("Ctrl+Q", "quit")] {
                spans.push(Span::styled(format!(" {k} "), t.value.patch(t.panel)));
                spans.push(Span::styled(format!("{v}  "), t.label.patch(t.panel)));
            }
        } else {
            let s = &self.snap;
            let rig = s.model.clone().map(|m| format!("{m} {}", s.address.clone().unwrap_or_default())).unwrap_or("none".into());
            for (k, v) in [("rig", rig), ("freq", s.freq_mhz.map(|f| format!("{f:.3}")).unwrap_or("?".into())),
                           ("transmit", if s.transmit_allowed { "ALLOWED".into() } else { "off".into() })] {
                spans.push(Span::styled(format!(" {k} "), t.label.patch(t.panel)));
                spans.push(Span::styled(format!("{v}  "), t.value.patch(t.panel)));
            }
        }
        Line::from(spans)
    }
}

#[cfg(test)]
mod remote_text_tests {
    use super::*;

    fn app() -> App {
        let fail = || std::io::Error::other("no radio in this test");
        App::new(Theme::plain(), Box::new(move |_: &Target| Err(fail())), Arc::new(move |_: &Target| Err(fail())))
    }

    #[test]
    fn slash_lines_in_a_session() {
        assert_eq!(slash_to_station("/EX").as_deref(), Some("/EX"));
        assert_eq!(slash_to_station("//disconnect").as_deref(), Some("/disconnect"));
        assert_eq!(slash_to_station("/disconnect"), None);
        assert_eq!(slash_to_station("/TX on"), None);
        assert_eq!(slash_to_station("hello"), None, "plain lines take the normal path");
    }

    #[test]
    fn frames_split_mid_line_join_up() {
        let mut a = app();
        let start = a.main.len();
        // a node menu that arrived in three frames, cut mid-word and between CR and LF
        for chunk in ["[H]elp....Displays node help\n[L]inks....C", "urrent AX.25 sessions\nRMS....Connect to Winlink CMS", " or Relay\n[R]outes"] {
            a.push_remote(chunk);
        }
        assert_eq!(&a.main[start..], ["[H]elp....Displays node help", "[L]inks....Current AX.25 sessions",
            "RMS....Connect to Winlink CMS or Relay", "[R]outes"]);
        assert!(a.remote[start..].iter().all(|&r| r), "station text is never styled by its content");
        a.push("*** disconnected");
        a.push_remote("new line");
        assert_eq!(a.main.last().unwrap(), "new line", "our own lines close an open remote line");
        a.shutdown();
    }
}
