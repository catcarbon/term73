//! Turbo Vision look: menu bar, patterned desktop, double-lined windows with shadows, key bar.
//! Every menu item runs (or starts typing) the same slash command, so menus and commands stay one feature.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::popup::{Outcome, Popup};
use super::{rgb, App, Theme};

/// Colours that only the Turbo Vision layout uses.
#[derive(Clone)]
pub struct Turbo {
    pub desk: Style,
    pub menu: Style,
    pub menu_hot: Style,
    pub menu_sel: Style,
    pub menu_sel_hot: Style,
    pub side: Style,
    pub side_border: Style,
    /// Line styles for text drawn on the light side windows.
    pub side_theme: Box<Theme>,
}

/// A menu: title with its hot letter marked by '~', then items. An item's command ending in a space
/// needs an argument, so choosing it puts the command on the input line instead of running it.
type Menu = (&'static str, &'static [(&'static str, &'static str)]);

pub const MENUS: &[Menu] = &[
    ("~R~adio", &[("~S~can for radios", "/radio scan"), ("Scan, show ~a~ll devices", "/radio scan all"),
                  ("~L~ast scan", "/radio list"), ("S~e~lect device...", "/radio select "), ("Se~t~up profile", "/radio setup"),
                  ("~I~nfo", "/radio info"), ("~O~ff (release the radio)", "/radio off")]),
    ("~C~hannel", &[("~C~onnect...", "/connect "), ("~D~isconnect", "/disconnect"), ("~F~requency...", "/frequency "),
                    ("Power ~h~igh", "/power high"), ("Power ~m~id", "/power mid"), ("Power ~l~ow", "/power low"),
                    ("~T~ransmit on", "/transmit on"), ("Transmit ~o~ff", "/transmit off")]),
    ("~B~BS", &[("~C~onnect...", "/bbs connect "), ("~A~dd", "/bbs add"), ("~E~dit...", "/bbs edit "),
                ("~L~ist", "/bbs list"), ("~R~emove...", "/bbs remove ")]),
    ("~W~inlink", &[("~S~etup", "/winlink setup"), ("~G~ateways", "/winlink gateways"), ("S~t~art", "/winlink start"),
                    ("St~o~p", "/winlink stop")]),
    ("~L~isten", &[("~S~tart listening", "/listen"), ("Save to ~f~ile...", "/listen save "), ("St~o~p", "/listen off")]),
    ("~S~ettings", &[("~S~how", "/config show"), ("~C~allsign...", "/config callsign "), ("~G~rid...", "/config grid "),
                     ("C~l~ear screen", "/clear"), ("~Q~uit", "/quit")]),
    ("~H~elp", &[("~B~asics", "/help basics"), ("BB~S~", "/help bbs"), ("~W~inlink", "/help winlink"),
                 ("~R~adio", "/help radio"), ("S~e~ttings", "/help settings"), ("~A~dvanced", "/help advanced")]),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuState {
    pub top: usize,
    pub item: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKey {
    Left,
    Right,
    Up,
    Down,
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Char(char),
}

/// "~R~adio" -> ("Radio", 'r', 0)
fn split_hot(label: &str) -> (String, char, usize) {
    let plain = label.replace('~', "");
    let at = label.find('~').unwrap_or(0);
    let hot = plain[at..].chars().next().unwrap_or(' ').to_ascii_lowercase();
    let col = plain[..at].chars().count();
    (plain, hot, col)
}

impl Theme {
    /// Turbo Vision in the blues of the start screen: desktop #0077aa, windows #005577.
    pub fn turbo() -> Self {
        let fg = |c| Style::default().fg(rgb(c));
        let bold = |c| fg(c).add_modifier(Modifier::BOLD);
        let (white, grey, black) = (rgb(0xffffff), rgb(0xaaaaaa), rgb(0x000000));
        let win = Style::default().bg(rgb(0x005577)).fg(white);
        let side = Style::default().bg(rgb(0x00aaaa)).fg(black);
        let side_theme = Theme {
            desktop: side, window: side, border: side, titlebar: side, panel: side, input: side,
            dim: fg(0x004455), label: fg(0x003344), value: bold(0x000000), good: bold(0x003300), warnv: bold(0x662200),
            rx: Style::default().bg(rgb(0x00aa00)).fg(white).add_modifier(Modifier::BOLD),
            tx: Style::default().bg(rgb(0xaa0000)).fg(white).add_modifier(Modifier::BOLD),
            echo: fg(0x003344), ask: bold(0x000000), session: fg(0x000000), monitor: fg(0x000000), bridge: fg(0x000000),
            error: bold(0xaa0000), warn: fg(0x662200), info: fg(0x000000), section: bold(0x000000), turbo: None,
        };
        let menu = Style::default().bg(grey).fg(black);
        Theme {
            desktop: Style::default().bg(rgb(0x0077aa)).fg(rgb(0x1a8fc0)),
            window: win,
            border: win.add_modifier(Modifier::BOLD),
            titlebar: win.add_modifier(Modifier::BOLD),
            panel: menu,
            input: win,
            dim: fg(0x88b8cc), label: fg(0xaad4e6), value: bold(0xffffff), good: bold(0x55ff55), warnv: bold(0xffff55),
            rx: Style::default().bg(rgb(0x00aa00)).fg(white).add_modifier(Modifier::BOLD),
            tx: Style::default().bg(rgb(0xaa0000)).fg(white).add_modifier(Modifier::BOLD),
            echo: fg(0xffff55), ask: bold(0xffff55), session: fg(0x55ffff), monitor: fg(0x55ff55), bridge: fg(0xff88ff),
            error: bold(0xff5555), warn: fg(0xffff55), info: fg(0x55ffff), section: bold(0xffffff),
            turbo: Some(Turbo {
                desk: Style::default().bg(rgb(0x0077aa)).fg(rgb(0x1a8fc0)),
                menu,
                menu_hot: Style::default().bg(grey).fg(rgb(0xaa0000)),
                menu_sel: Style::default().bg(rgb(0x00aa00)).fg(black),
                menu_sel_hot: Style::default().bg(rgb(0x00aa00)).fg(rgb(0xaa0000)),
                side,
                side_border: side,
                side_theme: Box::new(side_theme),
            }),
        }
    }
}

impl App {
    pub fn menu_is_open(&self) -> bool {
        self.menu.is_some()
    }

    pub fn popup_is_open(&self) -> bool {
        self.popup.is_some()
    }

    pub fn popup_key(&mut self, k: MenuKey) {
        let Some(p) = self.popup.as_mut() else { return };
        match p.key(k) {
            Outcome::Open => {}
            Outcome::Cancel => self.popup = None,
            Outcome::Run(cmd) => {
                self.popup = None;
                self.run_line(&cmd);
            }
        }
    }

    /// Open the menu bar: at the menu whose hot letter is `hot`, or at the first menu.
    /// Returns false when this theme has no menu bar or no menu has that letter.
    pub fn open_menu(&mut self, hot: Option<char>) -> bool {
        if self.theme.turbo.is_none() {
            return false;
        }
        let top = match hot {
            None => 0,
            Some(c) => match MENUS.iter().position(|(t, _)| split_hot(t).1 == c.to_ascii_lowercase()) {
                Some(i) => i,
                None => return false,
            },
        };
        self.menu = Some(MenuState { top, item: 0 });
        true
    }

    pub fn menu_key(&mut self, k: MenuKey) {
        let Some(mut m) = self.menu else { return };
        let items = MENUS[m.top].1;
        match k {
            MenuKey::Esc => {
                self.menu = None;
                return;
            }
            MenuKey::Left => m = MenuState { top: (m.top + MENUS.len() - 1) % MENUS.len(), item: 0 },
            MenuKey::Right => m = MenuState { top: (m.top + 1) % MENUS.len(), item: 0 },
            MenuKey::Up => m.item = (m.item + items.len() - 1) % items.len(),
            MenuKey::Down => m.item = (m.item + 1) % items.len(),
            MenuKey::Enter => return self.choose(items[m.item].1),
            MenuKey::Tab | MenuKey::BackTab | MenuKey::Backspace => {}
            MenuKey::Char(c) => {
                if let Some(i) = items.iter().position(|(l, _)| split_hot(l).1 == c.to_ascii_lowercase()) {
                    return self.choose(items[i].1);
                }
            }
        }
        self.menu = Some(m);
    }

    fn choose(&mut self, command: &str) {
        self.menu = None;
        if command.ends_with(' ')
            && let Some(p) = Popup::for_command(self, command) {
                self.popup = Some(p);
                return;
            }
        self.input = command.to_string();
        self.cursor = self.input.chars().count();
        if !command.ends_with(' ') {
            self.enter();
        }
    }

    /// Run a line as if typed (F1 help, for example).
    pub fn run_line(&mut self, line: &str) {
        self.input = line.to_string();
        self.cursor = self.input.chars().count();
        self.enter();
    }

    pub(super) fn draw_turbo(&mut self, f: &mut Frame) {
        let t = self.theme.clone();
        let tv = t.turbo.clone().expect("turbo theme");
        let area = f.area();
        if area.height < 8 || area.width < 40 {
            f.render_widget(Paragraph::new("window too small").style(t.window), area);
            return;
        }
        // desktop pattern
        let desk = Rect { y: area.y + 1, height: area.height - 2, ..area };
        let buf = f.buffer_mut();
        for y in desk.top()..desk.bottom() {
            for x in desk.left()..desk.right() {
                buf[(x, y)].set_symbol("░").set_style(tv.desk);
            }
        }
        self.draw_menu_bar(f, area, &tv);
        // windows: main (and traffic while listening) on the left, rig / heard / BBS on the right
        let wide = area.width >= 100 && !self.side_hidden;
        let side_w = if wide { 34 } else { 0 };
        let left = Rect { x: desk.x + 1, y: desk.y, width: desk.width - side_w - 4, height: desk.height - 1 };
        let (main, traffic) = if self.snap.listening && left.height > 16 {
            let th = 9;
            (Rect { height: left.height - th - 1, ..left }, Some(Rect { y: left.y + left.height - th, height: th, ..left }))
        } else {
            (left, None)
        };
        self.draw_main_turbo(f, main, &t, desk);
        if let Some(r) = traffic {
            let lines = self.traffic.clone();
            let inner = window(f, r, " Traffic ", t.window, t.border, desk);
            self.render_wrapped(f, inner, &lines, None, 0);
        }
        if wide {
            let sx = left.right() + 2;
            let st = &*tv.side_theme;
            let rig = Rect { x: sx, y: desk.y, width: side_w - 1, height: 10 };
            let lower = Rect { y: rig.bottom() + 1, height: desk.height.saturating_sub(10 + 1 + 1).max(4), ..rig };
            let (title, lines) = self.station_panel(st);
            for (r, title, lines) in [(rig, " Rig ".to_string(), self.rig_lines(st)), (lower, title, lines)] {
                if r.bottom() < desk.bottom() {
                    let inner = window(f, r, &title, tv.side, tv.side_border, desk);
                    f.render_widget(Paragraph::new(lines).style(tv.side), inner);
                }
            }
        }
        self.draw_key_bar(f, Rect { y: area.bottom() - 1, height: 1, ..area }, &tv);
        if let Some(m) = self.menu {
            self.draw_dropdown(f, m, &tv, desk);
        }
        if let Some(p) = &self.popup {
            p.draw(f, &tv, t.window, desk);
        }
    }

    fn draw_menu_bar(&self, f: &mut Frame, area: Rect, tv: &Turbo) {
        let s = &self.snap;
        let mut spans = vec![Span::styled(" ", tv.menu)];
        for (i, (title, _)) in MENUS.iter().enumerate() {
            let (plain, _, at) = split_hot(title);
            let open = self.menu.is_some_and(|m| m.top == i);
            let (st, hot) = if open { (tv.menu_sel, tv.menu_sel_hot) } else { (tv.menu, tv.menu_hot) };
            let chars: Vec<char> = plain.chars().collect();
            spans.push(Span::styled(" ", st));
            spans.push(Span::styled(chars[..at].iter().collect::<String>(), st));
            spans.push(Span::styled(chars[at].to_string(), hot));
            spans.push(Span::styled(chars[at + 1..].iter().collect::<String>() + " ", st));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(tv.menu), Rect { height: 1, ..area });
        let t = &self.theme;
        let chip = if s.transmitting { Span::styled(" TX ", t.tx) } else { Span::styled(" RX ", t.rx) };
        let right = Line::from(vec![
            Span::styled(s.callsign.clone().unwrap_or_else(|| "no callsign".into()), tv.menu),
            Span::styled(s.freq_mhz.map(|m| format!("  {m:.3} MHz  ")).unwrap_or_else(|| "  ".into()), tv.menu),
            chip,
            Span::styled(" ", tv.menu),
        ]);
        let w = right.width() as u16;
        if area.width > w + 60 {
            f.render_widget(Paragraph::new(right), Rect { x: area.right() - w, width: w, height: 1, ..area });
        }
    }

    fn draw_main_turbo(&mut self, f: &mut Frame, r: Rect, t: &Theme, desk: Rect) {
        let title = match (&self.snap.remote, self.snap.packet) {
            (Some(call), _) => format!(" [■] Session: {call} "),
            (None, crate::engine::Packet::Listening) => " [■] term73: listening ".into(),
            _ => " [■] term73 ".into(),
        };
        let inner = window(f, r, &title, t.window, t.border, desk);
        if inner.height < 3 {
            return;
        }
        let text = Rect { height: inner.height - 2, ..inner };
        let (lines, remote) = (self.main.clone(), self.remote.clone());
        self.render_wrapped(f, text, &lines, Some(&remote), self.scroll);
        // divider and input line inside the window
        let div = Rect { y: inner.bottom() - 2, height: 1, ..inner };
        f.render_widget(Paragraph::new("─".repeat(div.width as usize)).style(t.border), div);
        let input = Rect { y: inner.bottom() - 1, height: 1, ..inner };
        let masked = self.pending.as_ref().is_some_and(|p| p.secret);
        let shown = if masked { "*".repeat(self.input.chars().count()) } else { self.input.clone() };
        let prompt = match (&self.pending, &self.snap.remote) {
            (Some(_), _) => "? ".to_string(),
            (None, Some(call)) => format!("{call}> "),
            _ => "> ".into(),
        };
        let pw = prompt.chars().count() as u16;
        f.render_widget(Paragraph::new(Line::from(vec![Span::styled(prompt, t.ask), Span::styled(shown, t.window)])), input);
        if self.menu.is_none() && self.popup.is_none() {
            let before = self.input.chars().take(self.cursor).count() as u16;
            f.set_cursor_position((input.x + pw + before, input.y));
        }
    }

    fn draw_key_bar(&self, f: &mut Frame, r: Rect, tv: &Turbo) {
        let keys: &[(&str, &str)] = if self.popup.is_some() {
            &[("Tab", "Next field"), ("↑↓", "Choose"), ("Enter", "OK"), ("Esc", "Cancel")]
        } else if self.menu.is_some() {
            &[("←→", "Menus"), ("↑↓", "Items"), ("Enter", "Choose"), ("Esc", "Close")]
        } else if self.snap.remote.is_some() {
            &[("F1", "Help"), ("F8", "Panels"), ("F10", "Menu"), ("Ctrl+C", "Disconnect"), ("Ctrl+Z", "End message"), ("PgUp", "Scroll"), ("Alt+X", "Exit")]
        } else {
            &[("F1", "Help"), ("F8", "Panels"), ("F10", "Menu"), ("Tab", "Complete"), ("PgUp", "Scroll"), ("Alt+X", "Exit")]
        };
        let mut spans = vec![Span::styled(" ", tv.menu)];
        for (k, v) in keys {
            spans.push(Span::styled(*k, tv.menu_hot));
            spans.push(Span::styled(format!(" {v}   "), tv.menu));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(tv.menu), r);
    }

    fn draw_dropdown(&self, f: &mut Frame, m: MenuState, tv: &Turbo, desk: Rect) {
        let mut x = desk.x + 1;
        for (title, _) in &MENUS[..m.top] {
            x += split_hot(title).0.chars().count() as u16 + 2;
        }
        let items = MENUS[m.top].1;
        let w = items.iter().map(|(l, _)| split_hot(l).0.chars().count()).max().unwrap_or(8) as u16 + 6;
        let r = Rect { x: x.min(desk.right().saturating_sub(w + 2)), y: desk.y, width: w, height: items.len() as u16 + 2 };
        let inner = window(f, r, "", tv.menu, tv.menu, desk);
        for (i, (label, _)) in items.iter().enumerate() {
            let (plain, _, at) = split_hot(label);
            let sel = i == m.item;
            let (st, hot) = if sel { (tv.menu_sel, tv.menu_sel_hot) } else { (tv.menu, tv.menu_hot) };
            let chars: Vec<char> = plain.chars().collect();
            let row = Rect { y: inner.y + i as u16, height: 1, ..inner };
            let pad = (inner.width as usize).saturating_sub(chars.len() + 1);
            let line = Line::from(vec![
                Span::styled(" ", st),
                Span::styled(chars[..at].iter().collect::<String>(), st),
                Span::styled(chars[at].to_string(), hot),
                Span::styled(chars[at + 1..].iter().collect::<String>() + &" ".repeat(pad), st),
            ]);
            f.render_widget(Paragraph::new(line), row);
        }
    }
}

/// A double-lined window with a centred title and a shadow; returns the inside.
pub(super) fn window(f: &mut Frame, r: Rect, title: &str, body: Style, border: Style, clip: Rect) -> Rect {
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Double).border_style(border).style(body)
        .title(Line::from(Span::styled(title.to_string(), border)).centered());
    let inner = block.inner(r);
    f.render_widget(Clear, r);
    f.render_widget(block, r);
    shadow(f, r, clip);
    inner
}

/// A thin shadow: half a cell wide on the right and half a cell tall below, drawn with block
/// elements in black over whatever is underneath, so that cell keeps its background.
fn shadow(f: &mut Frame, r: Rect, clip: Rect) {
    let buf = f.buffer_mut();
    let mut put = |x: u16, y: u16, sym: &str| {
        if x < clip.right() && y < clip.bottom() && x >= clip.left() && y >= clip.top() {
            buf[(x, y)].set_symbol(sym).set_fg(Color::Black);
        }
    };
    put(r.right(), r.y, "\u{2596}"); // quadrant lower left: the shadow starts half a row down
    for y in r.y + 1..r.bottom() {
        put(r.right(), y, "\u{258C}"); // left half block
    }
    put(r.right(), r.bottom(), "\u{2598}"); // quadrant upper left
    for x in r.x + 1..r.right() {
        put(x, r.bottom(), "\u{2580}"); // upper half block
    }
}
