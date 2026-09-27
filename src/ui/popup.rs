//! Pop-up dialogs for menu items that need an argument ("Connect...", "Frequency...").
//! A dialog only gathers values; it ends by running the same slash command a user could type.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::turbo::{window, MenuKey, Turbo};
use super::{device_lines, App};
use crate::config;

pub enum Field {
    Text { label: &'static str, value: String },
    /// Options are (shown, value).
    Choice { label: &'static str, options: Vec<(String, String)>, sel: usize },
}

pub struct Popup {
    title: &'static str,
    fields: Vec<Field>,
    focus: usize,
    /// Shown under the fields: why OK did nothing, or why there is nothing to pick.
    note: Option<String>,
    /// Build the command from the field values; Err is shown as the note.
    make: fn(&[String]) -> Result<String, String>,
}

fn text(label: &'static str, value: impl Into<String>) -> Field {
    Field::Text { label, value: value.into() }
}

fn bbs_choice() -> Field {
    let options = config::load_bbs().into_iter()
        .map(|(name, b)| (format!("{name:<10} {:<10} {:.3} MHz", b.call, b.mhz), name))
        .collect();
    Field::Choice { label: "BBS", options, sel: 0 }
}

fn required(v: &str, what: &str) -> Result<String, String> {
    let v = v.trim();
    if v.is_empty() { Err(format!("{what} is needed")) } else { Ok(v.to_string()) }
}

impl Popup {
    /// The dialog for a menu command that needs an argument, or None to fall back to the input line.
    pub fn for_command(app: &App, command: &str) -> Option<Popup> {
        let s = &app.snap;
        let p = |title, fields, make| Some(Popup { title, fields, focus: 0, note: None, make });
        match command.trim_end() {
            "/connect" => p(" Connect ", vec![text("Callsign or node", ""), text("Via (digipeaters, comma separated)", "")],
                |v| {
                    let call = required(&v[0], "a callsign")?;
                    let via = v[1].trim();
                    Ok(if via.is_empty() { format!("/connect {call}") } else { format!("/connect {call} via {via}") })
                }),
            "/frequency" => p(" Frequency ", vec![text("Frequency, MHz", s.freq_mhz.map(|f| format!("{f:.3}")).unwrap_or_default())],
                |v| Ok(format!("/frequency {}", required(&v[0], "a frequency")?))),
            "/bbs connect" => p(" Connect to BBS ", vec![bbs_choice()], |v| Ok(format!("/bbs connect {}", required(&v[0], "a saved BBS")?))),
            "/bbs edit" => p(" Edit BBS ", vec![bbs_choice()], |v| Ok(format!("/bbs edit {}", required(&v[0], "a saved BBS")?))),
            "/bbs remove" => p(" Remove BBS ", vec![bbs_choice()], |v| Ok(format!("/bbs remove {}", required(&v[0], "a saved BBS")?))),
            "/radio select" => {
                let devs = app.ctx.scan.lock().unwrap().clone();
                let options = device_lines(&devs, true, "")
                    .into_iter().skip(1).filter(|l| !l.trim_start().starts_with('('))
                    .map(|l| { let n = l.split_whitespace().next().unwrap_or("").to_string(); (l.trim().to_string(), n) })
                    .collect();
                p(" Select device ", vec![Field::Choice { label: "Device (from the last scan)", options, sel: 0 }],
                  |v| Ok(format!("/radio select {}", required(&v[0], "a device: run Radio > Scan first")?)))
            }
            "/listen save" => p(" Save traffic ", vec![text("File name", "traffic.jsonl")],
                |v| Ok(format!("/listen save {}", required(&v[0], "a file name")?))),
            "/config callsign" => p(" Callsign ", vec![text("Your callsign", s.callsign.clone().unwrap_or_default())],
                |v| Ok(format!("/config callsign {}", required(&v[0], "a callsign")?))),
            "/config grid" => p(" Grid locator ", vec![text("Grid locator (e.g. FN31pr)", config::AppConfig::load().grid.unwrap_or_default())],
                |v| Ok(format!("/config grid {}", required(&v[0], "a grid locator")?))),
            _ => None,
        }
        .map(|mut pop| {
            if pop.fields.iter().any(|f| matches!(f, Field::Choice { options, .. } if options.is_empty())) {
                pop.note = Some("nothing to choose from yet".into());
            }
            pop
        })
    }

    fn values(&self) -> Vec<String> {
        self.fields.iter().map(|f| match f {
            Field::Text { value, .. } => value.clone(),
            Field::Choice { options, sel, .. } => options.get(*sel).map(|o| o.1.clone()).unwrap_or_default(),
        }).collect()
    }
}

pub enum Outcome {
    Open,
    Cancel,
    Run(String),
}

impl Popup {
    pub fn key(&mut self, k: MenuKey) -> Outcome {
        let last = self.fields.len() - 1;
        match (k, &mut self.fields[self.focus]) {
            (MenuKey::Esc, _) => return Outcome::Cancel,
            (MenuKey::Up, Field::Choice { sel, options, .. }) if !options.is_empty() => *sel = (*sel + options.len() - 1) % options.len(),
            (MenuKey::Down, Field::Choice { sel, options, .. }) if !options.is_empty() => *sel = (*sel + 1) % options.len(),
            (MenuKey::Tab | MenuKey::Down, _) => self.focus = (self.focus + 1) % self.fields.len(),
            (MenuKey::BackTab | MenuKey::Up, _) => self.focus = (self.focus + last) % self.fields.len(),
            (MenuKey::Char(c), Field::Text { value, .. }) => value.push(c),
            (MenuKey::Backspace, Field::Text { value, .. }) => {
                value.pop();
            }
            (MenuKey::Enter, _) if self.focus < last => self.focus += 1,
            (MenuKey::Enter, _) => match (self.make)(&self.values()) {
                Ok(cmd) => return Outcome::Run(cmd),
                Err(e) => self.note = Some(e),
            },
            _ => {}
        }
        Outcome::Open
    }

    pub fn draw(&self, f: &mut Frame, tv: &Turbo, field_style: Style, desk: Rect) {
        const W: u16 = 58;
        let rows: u16 = self.fields.iter().map(|f| match f {
            Field::Text { .. } => 3,
            Field::Choice { options, .. } => options.len().clamp(1, 8) as u16 + 2,
        }).sum();
        let h = rows + 5;
        let r = Rect { x: desk.x + desk.width.saturating_sub(W) / 2, y: desk.y + desk.height.saturating_sub(h) / 3, width: W.min(desk.width), height: h.min(desk.height) };
        let inner = window(f, r, self.title, tv.menu, tv.menu, desk);
        let mut y = inner.y + 1;
        let x = inner.x + 2;
        let w = inner.width.saturating_sub(4);
        let focused = field_style.fg(ratatui::style::Color::Rgb(0xff, 0xff, 0x55)).add_modifier(Modifier::BOLD);
        let mut cursor = None;
        for (i, fl) in self.fields.iter().enumerate() {
            let on = i == self.focus;
            match fl {
                Field::Text { label, value } => {
                    f.render_widget(Paragraph::new(*label).style(if on { tv.menu.add_modifier(Modifier::BOLD) } else { tv.menu }), Rect { x, y, width: w, height: 1 });
                    let shown: String = value.chars().rev().take(w as usize - 2).collect::<Vec<_>>().into_iter().rev().collect();
                    f.render_widget(Paragraph::new(format!(" {shown:<width$}", width = w as usize - 1)).style(if on { focused } else { field_style }),
                                    Rect { x, y: y + 1, width: w, height: 1 });
                    if on {
                        cursor = Some((x + 1 + shown.chars().count() as u16, y + 1));
                    }
                    y += 3;
                }
                Field::Choice { label, options, sel } => {
                    f.render_widget(Paragraph::new(*label).style(if on { tv.menu.add_modifier(Modifier::BOLD) } else { tv.menu }), Rect { x, y, width: w, height: 1 });
                    let shown = options.len().clamp(1, 8);
                    let start = sel.saturating_sub(shown - 1);
                    for (row, (text, _)) in options.iter().enumerate().skip(start).take(shown) {
                        let st = if row == *sel { if on { tv.menu_sel } else { field_style } } else { tv.menu };
                        let yy = y + 1 + (row - start) as u16;
                        f.render_widget(Paragraph::new(format!(" {text:<width$}", width = w as usize - 1)).style(st), Rect { x, y: yy, width: w, height: 1 });
                    }
                    y += shown as u16 + 2;
                }
            }
        }
        if let Some(n) = &self.note {
            f.render_widget(Paragraph::new(Line::from(Span::styled(n.clone(), tv.menu_hot))), Rect { x, y: inner.bottom() - 2, width: w, height: 1 });
        }
        let buttons = Line::from(vec![
            Span::styled("    OK    ", tv.menu_sel.add_modifier(Modifier::BOLD)), Span::styled("      ", tv.menu),
            Span::styled("  Cancel  ", tv.menu_sel),
        ]);
        f.render_widget(Paragraph::new(buttons).centered(), Rect { x: inner.x, y: inner.bottom() - 1, width: inner.width, height: 1 });
        if let Some(c) = cursor {
            f.set_cursor_position(c);
        }
    }
}
