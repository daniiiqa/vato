//! vato — el vato que le sabe a todo: audio, bluetooth, red, sistema y
//! notificaciones en la terminal, con el look de discada (como rola).
//!
//!   vato [audio|mic|bluetooth|network|wifi|system|notifications]
//!
//! Con vato ya abierto, escribir la pestaña en $XDG_RUNTIME_DIR/vato.tab
//! la cambia (así la barra de discada reusa la ventana).
//!
//! El ratón, como en rola: clic en una pestaña o en una tecla de abajo hace
//! lo mismo que la tecla; en las listas, clic elige, doble clic es enter,
//! clic derecho la otra acción de la fila, y la rueda va donde esté el puntero.

mod audio;
mod bluetooth;
mod look;
mod network;
mod notifications;
mod system;

use look::Look;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
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
    /// la rueda en (x, y)
    fn scroll(&mut self, _x: u16, _y: u16, _down: bool, _ui: &mut Ui) {}
    /// un clic en (x, y) dentro de la pestaña
    fn click(&mut self, _x: u16, _y: u16, _b: MouseButton, _double: bool, _ui: &mut Ui) {}
    /// arrastrando con el botón izquierdo
    fn drag(&mut self, _x: u16, _y: u16, _ui: &mut Ui) {}
    /// algo pide atención (bluetooth preguntando al emparejar): ir a esta pestaña
    fn wants_focus(&mut self) -> bool {
        false
    }
}

/// Lo que hay bajo un clic en las líneas de arriba y abajo.
enum Hit {
    Tab(usize),
    Key(KeyCode),
}

/// "enter" → Enter, "x" → x; las de dos teclas ("j/k") no son botón.
fn hint_key(k: &str) -> Option<KeyCode> {
    match k {
        "enter" => Some(KeyCode::Enter),
        "esc" => Some(KeyCode::Esc),
        "tab" => Some(KeyCode::Tab),
        _ if k.chars().count() == 1 => k.chars().next().map(KeyCode::Char),
        _ => None,
    }
}

struct App {
    tabs: Vec<Box<dyn Tab>>,
    cur: usize,
    ui: Ui,
    quit: bool,
    hits: Vec<(Rect, Hit)>,
    body: Rect,
    last_click: Option<(Instant, u16, u16)>,
}

impl App {
    fn draw(&mut self, f: &mut Frame) {
        let t = self.ui.look.theme;
        if t.bg != ratatui::style::Color::Reset {
            f.render_widget(Block::new().style(Style::new().bg(t.bg).fg(t.fg)), f.area());
        }
        let [head, body, foot] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)]).areas(f.area());
        self.hits.clear();
        self.body = body;
        // pestañas
        let mut x = head.x + self.ui.look.tx("vato").chars().count() as u16 + 4;
        let mut spans = vec![
            Span::styled(format!(" {} ", self.ui.look.tx("vato")), Style::new().fg(t.base).bg(t.primary).bold()),
            Span::raw("  "),
        ];
        for (i, tab) in self.tabs.iter().enumerate() {
            let name = self.ui.look.tx(tab.name());
            let st = if i == self.cur { Style::new().fg(t.primary).bold().underlined() } else { Style::new().fg(t.muted) };
            let label = format!("{} {}", i + 1, name);
            let w = label.chars().count() as u16;
            self.hits.push((Rect { x, y: head.y, width: w, height: 1 }, Hit::Tab(i)));
            x += w + 3;
            spans.push(Span::styled(label, st));
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
                let mut x = foot.x;
                for (k, what) in hints {
                    let key = format!(" {k} ");
                    let what = format!("{}  ", self.ui.look.tx(what));
                    let w = (key.chars().count() + what.chars().count()) as u16;
                    if let Some(code) = hint_key(k) {
                        self.hits.push((Rect { x, y: foot.y, width: w.saturating_sub(1), height: 1 }, Hit::Key(code)));
                    }
                    x += w;
                    v.push(Span::styled(key, Style::new().fg(t.primary).bold()));
                    v.push(Span::styled(what, Style::new().fg(t.muted)));
                }
                Line::from(v)
            }
        };
        f.render_widget(Paragraph::new(line), foot);
    }

    fn mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        let cur = self.cur;
        let in_body = self.body.contains(ratatui::layout::Position { x, y });
        match m.kind {
            MouseEventKind::ScrollDown if in_body => self.tabs[cur].scroll(x, y, true, &mut self.ui),
            MouseEventKind::ScrollUp if in_body => self.tabs[cur].scroll(x, y, false, &mut self.ui),
            MouseEventKind::Drag(MouseButton::Left) if in_body => self.tabs[cur].drag(x, y, &mut self.ui),
            MouseEventKind::Down(b) => {
                let double = self.last_click.is_some_and(|(t, cx, cy)| t.elapsed() < Duration::from_millis(400) && cy == y && cx.abs_diff(x) < 3);
                self.last_click = Some((Instant::now(), x, y));
                if in_body {
                    return self.tabs[cur].click(x, y, b, double, &mut self.ui);
                }
                match self.hits.iter().find(|(r, _)| r.contains(ratatui::layout::Position { x, y })).map(|h| &h.1) {
                    Some(Hit::Tab(i)) if !self.tabs[cur].typing() => self.cur = *i,
                    Some(Hit::Key(code)) => {
                        let code = *code;
                        self.key(KeyEvent::new(code, KeyModifiers::NONE));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
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

/// `vato <pestaña>` con vato ya abierto (lo hace Tui.sh de discada): se
/// escribe aquí y la ventana abierta se cambia a esa pestaña.
fn tab_file() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into())).join("vato.tab")
}

fn tab_index(arg: &str) -> Option<usize> {
    match arg {
        "audio" | "mic" | "" => Some(0),
        "bluetooth" | "bt" => Some(1),
        "network" | "wifi" | "net" => Some(2),
        "system" | "sys" => Some(3),
        "notifications" | "notif" | "notis" => Some(4),
        _ => None,
    }
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    if arg == "-h" || arg == "--help" {
        println!("vato [audio|mic|bluetooth|network|wifi|system|notifications]");
        return;
    }
    let mut audio = audio::Audio::new();
    if arg == "mic" {
        audio.focus_inputs();
    }
    let cur = tab_index(&arg).unwrap_or(0);
    let mut app = App {
        tabs: vec![
            Box::new(audio),
            Box::new(bluetooth::Bluetooth::new()),
            Box::new(network::Network::new()),
            Box::new(system::System::new()),
            Box::new(notifications::Notifications::new()),
        ],
        cur,
        ui: Ui { look: Look::load(), status: None },
        quit: false,
        hits: Vec::new(),
        body: Rect::default(),
        last_click: None,
    };

    let mut term = ratatui::init();
    let _ = execute!(std::io::stdout(), event::EnableMouseCapture);
    let mut last_look = Instant::now();
    let tab_mtime = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut tab_seen = tab_mtime(&tab_file());
    while !app.quit {
        if last_look.elapsed() > Duration::from_secs(1) {
            app.ui.look.refresh();
            last_look = Instant::now();
        }
        // otra pestaña pedida desde fuera (la barra de discada)
        let m = tab_mtime(&tab_file());
        if m != tab_seen {
            tab_seen = m;
            if let Ok(want) = std::fs::read_to_string(tab_file()) {
                if let Some(i) = tab_index(want.trim()) {
                    app.cur = i;
                }
            }
        }
        if let Some(i) = app.tabs.iter_mut().position(|t| t.wants_focus()) {
            app.cur = i;
        }
        let cur = app.cur;
        app.tabs[cur].tick(&mut app.ui);
        let _ = term.draw(|f| app.draw(f));
        if event::poll(Duration::from_millis(150)).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => app.key(k),
                Ok(Event::Mouse(m)) => app.mouse(m),
                _ => {}
            }
        }
    }
    let _ = execute!(std::io::stdout(), event::DisableMouseCapture);
    ratatui::restore();
}
