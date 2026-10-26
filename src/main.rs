//! vato — el vato que le sabe a todo: audio, bluetooth, red y sistema en la
//! terminal, con el look de discada (como rola).
//!
//!   vato [audio|mic|bluetooth|network|wifi|system]

mod audio;
mod bluetooth;
mod look;
mod network;
mod system;

use look::Look;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use std::time::{Duration, Instant};

/// Idioma de discada (hypr/discada.conf → LANGUAGE=es|en); por defecto inglés.
pub fn spanish() -> bool {
    static ES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ES.get_or_init(|| {
        let f = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/hypr/discada.conf");
        std::fs::read_to_string(f)
            .ok()
            .and_then(|t| t.lines().find_map(|l| l.strip_prefix("LANGUAGE=").map(|v| v.trim_start().starts_with("es"))))
            .unwrap_or(false)
    })
}

/// es / en
pub fn l(es: &'static str, en: &'static str) -> &'static str {
    if spanish() { es } else { en }
}

/// Lo que comparten las pestañas para dibujar: el look y un aviso abajo.
pub struct Ui {
    pub look: Look,
    status: Option<(String, Instant, bool)>,
}

impl Ui {
    pub fn say(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now(), false));
    }
    pub fn warn(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now(), true));
    }
}

pub trait Tab {
    fn name(&self) -> &'static str;
    /// refrescar datos (cada vuelta del ciclo; cada pestaña decide cada cuándo)
    fn tick(&mut self, ui: &mut Ui);
    fn draw(&mut self, f: &mut Frame, area: Rect, ui: &Ui);
    /// true si la tecla era suya
    fn key(&mut self, k: KeyEvent, ui: &mut Ui) -> bool;
    /// teclas para la línea de abajo: (tecla, qué hace)
    fn hints(&self) -> Vec<(&'static str, &'static str)>;
    /// escribiendo (contraseña): las teclas globales no aplican
    fn typing(&self) -> bool {
        false
    }
    fn scroll(&mut self, _down: bool, _ui: &mut Ui) {}
}

struct App {
    tabs: Vec<Box<dyn Tab>>,
    cur: usize,
    ui: Ui,
    quit: bool,
}

impl App {
    fn draw(&mut self, f: &mut Frame) {
        let t = self.ui.look.theme;
        if t.bg != ratatui::style::Color::Reset {
            f.render_widget(Block::new().style(Style::new().bg(t.bg).fg(t.fg)), f.area());
        }
        let [head, body, foot] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)]).areas(f.area());
        // pestañas
        let mut spans = vec![
            Span::styled(format!(" {} ", self.ui.look.tx("vato")), Style::new().fg(t.base).bg(t.primary).bold()),
            Span::raw("  "),
        ];
        for (i, tab) in self.tabs.iter().enumerate() {
            let name = self.ui.look.tx(tab.name());
            let st = if i == self.cur { Style::new().fg(t.primary).bold().underlined() } else { Style::new().fg(t.muted) };
            spans.push(Span::styled(format!("{} {}", i + 1, name), st));
            spans.push(Span::raw("   "));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), head);

        let cur = self.cur;
        self.tabs[cur].draw(f, body, &self.ui);

        // abajo: aviso reciente, o las teclas
        let line = match &self.ui.status {
            Some((msg, at, bad)) if at.elapsed() < Duration::from_secs(5) => {
                Line::from(Span::styled(format!(" {msg}"), Style::new().fg(if *bad { t.red } else { t.accent })))
            }
            _ => {
                let mut v = Vec::new();
                let mut hints = self.tabs[cur].hints();
                hints.extend([("tab", l("pestaña", "tab")), ("q", l("salir", "quit"))]);
                for (k, what) in hints {
                    v.push(Span::styled(format!(" {k} "), Style::new().fg(t.primary).bold()));
                    v.push(Span::styled(format!("{}  ", self.ui.look.tx(what)), Style::new().fg(t.muted)));
                }
                Line::from(v)
            }
        };
        f.render_widget(Paragraph::new(line), foot);
    }

    fn key(&mut self, k: KeyEvent) {
        let cur = self.cur;
        if self.tabs[cur].typing() {
            self.tabs[cur].key(k, &mut self.ui);
            return;
        }
        if self.tabs[cur].key(k, &mut self.ui) {
            return;
        }
        let n = self.tabs.len();
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Tab => self.cur = (self.cur + 1) % n,
            KeyCode::BackTab => self.cur = (self.cur + n - 1) % n,
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if i < n {
                    self.cur = i;
                }
            }
            _ => {}
        }
    }
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    if arg == "-h" || arg == "--help" {
        println!("vato [audio|mic|bluetooth|network|wifi|system]");
        return;
    }
    let mut audio = audio::Audio::new();
    let cur = match arg.as_str() {
        "bluetooth" | "bt" => 1,
        "network" | "wifi" | "net" => 2,
        "system" | "sys" => 3,
        "mic" => {
            audio.focus_inputs();
            0
        }
        _ => 0,
    };
    let mut app = App {
        tabs: vec![
            Box::new(audio),
            Box::new(bluetooth::Bluetooth::new()),
            Box::new(network::Network::new()),
            Box::new(system::System::new()),
        ],
        cur,
        ui: Ui { look: Look::load(), status: None },
        quit: false,
    };

    let mut term = ratatui::init();
    let _ = execute!(std::io::stdout(), event::EnableMouseCapture);
    let mut last_look = Instant::now();
    while !app.quit {
        if last_look.elapsed() > Duration::from_secs(1) {
            app.ui.look.refresh();
            last_look = Instant::now();
        }
        let cur = app.cur;
        app.tabs[cur].tick(&mut app.ui);
        let _ = term.draw(|f| app.draw(f));
        if event::poll(Duration::from_millis(150)).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => app.key(k),
                Ok(Event::Mouse(m)) => match m.kind {
                    MouseEventKind::ScrollDown => app.tabs[cur].scroll(true, &mut app.ui),
                    MouseEventKind::ScrollUp => app.tabs[cur].scroll(false, &mut app.ui),
                    _ => {}
                },
                _ => {}
            }
        }
    }
    let _ = execute!(std::io::stdout(), event::DisableMouseCapture);
    ratatui::restore();
}
