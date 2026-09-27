//! term73: packet radio terminal for Bluetooth TNC radios and software modems.

use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use term73::{engine, signals};
use term73::ui::{self, App, MenuKey, Theme};

fn usage() -> ! {
    println!("term73 {}\n\nUsage: term73 [--color | --no-color] [--theme turbo|seafoam|modem73]", ui::VERSION);
    std::process::exit(0);
}

fn main() -> io::Result<()> {
    let mut color = None;
    let mut theme = "turbo".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--color" => color = Some(true),
            "--no-color" => color = Some(false),
            "--theme" => theme = args.next().unwrap_or_default(),
            "-V" | "--version" => {
                println!("term73 {}", ui::VERSION);
                return Ok(());
            }
            _ => usage(),
        }
    }
    let theme = if !color.unwrap_or_else(ui::color_supported) {
        Theme::plain()
    } else {
        match theme.as_str() {
            "seafoam" => Theme::seafoam(),
            "modem73" => Theme::modem73(),
            "turbo" => Theme::turbo(),
            _ => usage(),
        }
    };

    signals::install();
    // a panic on the main thread must still give the user their terminal back
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            restore_terminal();
        }
        default_hook(info);
    }));
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut app = App::new(theme, engine::real_opener(), Arc::new(engine::open_real));
    let result = run(&mut term, &mut app);

    // the terminal may already be gone (window closed): release the rig whatever these writes do
    restore_terminal();
    println!("term73: ending sessions, leaving packet mode, releasing the rig...");
    if signals::stop_requested() {
        app.shutdown_within(signals::CLOSE_WAIT);
    } else {
        app.shutdown();
    }
    println!("term73: done");
    signals::finished();
    result
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut out = io::stdout();
    let _ = execute!(out, LeaveAlternateScreen);
    // make sure no mouse or paste mode is left switched on in the user's shell
    let _ = write!(out, "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l");
    let _ = out.flush();
}

fn run(term: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> io::Result<()> {
    while !app.quit && !signals::stop_requested() {
        app.tick();
        term.draw(|f| app.draw(f))?;
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if app.menu_is_open() || app.popup_is_open() {
            let key = match k.code {
                KeyCode::Left => MenuKey::Left,
                KeyCode::Right => MenuKey::Right,
                KeyCode::Up => MenuKey::Up,
                KeyCode::Down => MenuKey::Down,
                KeyCode::Enter => MenuKey::Enter,
                KeyCode::Esc | KeyCode::F(10) => MenuKey::Esc,
                KeyCode::Tab => MenuKey::Tab,
                KeyCode::BackTab => MenuKey::BackTab,
                KeyCode::Backspace => MenuKey::Backspace,
                KeyCode::Char(ch) => MenuKey::Char(ch),
                _ => continue,
            };
            if app.popup_is_open() {
                app.popup_key(key);
            } else {
                app.menu_key(key);
            }
            continue;
        }
        match k.code {
            KeyCode::Char('x') if alt => app.quit = true,
            KeyCode::Char(ch) if alt && app.open_menu(Some(ch)) => {}
            KeyCode::F(10) => {
                app.open_menu(None);
            }
            KeyCode::F(1) => app.run_line("/help"),
            KeyCode::Char('c') if ctrl => app.ctrl_c(),
            KeyCode::Char('q') if ctrl => app.quit = true,
            KeyCode::Char('z') if ctrl => app.ctrl_z(),
            KeyCode::Char(ch) => app.key_char(ch),
            KeyCode::Enter => app.enter(),
            KeyCode::Backspace => app.key_backspace(),
            KeyCode::Delete => app.key_delete(),
            KeyCode::Left => app.key_left(),
            KeyCode::Right => app.key_right(),
            KeyCode::Home => app.key_home(),
            KeyCode::End => app.key_end(),
            KeyCode::Up => app.history(true),
            KeyCode::Down => app.history(false),
            KeyCode::PageUp => app.scroll(true, 10),
            KeyCode::PageDown => app.scroll(false, 10),
            KeyCode::Tab => app.complete(),
            _ => {}
        }
    }
    Ok(())
}
