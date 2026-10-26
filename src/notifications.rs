//! Notificaciones: el historial de arre (el daemon de notificaciones de
//! discada) por D-Bus. Abrir lo que pide cada una, quitarlas, borrar todas, no
//! molestar y dónde salen. arre avisa cuando cambia algo y la lista se relee
//! sola; si arre no está corriendo, preguntarle lo arranca (activación de D-Bus).

use crate::{Tab, Ui, l};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use zbus::blocking::Connection;

const BUS: &str = "org.discada.Arre";
const PATH: &str = "/org/discada/Arre";
const IFACE: &str = "org.discada.Arre";
const POSITIONS: [&str; 6] = ["top-left", "top-center", "top-right", "bottom-left", "bottom-center", "bottom-right"];

#[derive(Clone)]
struct Note {
    id: u32,
    app: String,
    summary: String,
    body: String,
    actions: Vec<(String, String)>,
    urgency: u8,
    value: Option<i64>,
    time: i64,
}

pub struct Notifications {
    conn: Option<Connection>,
    notes: Vec<Note>, // la más nueva primero
    dnd: bool,
    sel: usize,
    dirty: Arc<AtomicBool>,
    last: Option<Instant>,
    error: Option<String>,
    clear: bool, // preguntando si borrar todas
    rows: Vec<(u16, usize)>,
}

fn position_name(p: &str) -> &'static str {
    match p {
        "top-left" => l("arriba a la izquierda", "top left"),
        "top-right" => l("arriba a la derecha", "top right"),
        "bottom-left" => l("abajo a la izquierda", "bottom left"),
        "bottom-center" => l("abajo al centro", "bottom center"),
        "bottom-right" => l("abajo a la derecha", "bottom right"),
        _ => l("arriba al centro", "top center"),
    }
}

/// La posición la guarda arre en su config.json (el comando `arre position`
/// la escribe y el daemon la relee).
fn arre_bin() -> String {
    std::env::var("HOME").map(|h| format!("{h}/.local/bin/arre")).unwrap_or_else(|_| "arre".into())
}

fn position() -> String {
    std::process::Command::new(arre_bin())
        .arg("position")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "top-center".into())
}

/// "ahora", "hace 5 min", "14:32", "26/10 14:32"
fn when(t: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let ago = now - t;
    if ago < 60 {
        return l("ahora", "now").to_string();
    }
    if ago < 3600 {
        return format!("{} {} min", l("hace", ""), ago / 60).trim().to_string() + if crate::spanish() { "" } else { " ago" };
    }
    let local = |t: i64| {
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let t = t as libc::time_t;
        unsafe { libc::localtime_r(&t, &mut tm) };
        tm
    };
    let (a, b) = (local(t), local(now));
    if (a.tm_year, a.tm_yday) == (b.tm_year, b.tm_yday) {
        format!("{:02}:{:02}", a.tm_hour, a.tm_min)
    } else {
        format!("{:02}/{:02} {:02}:{:02}", a.tm_mday, a.tm_mon + 1, a.tm_hour, a.tm_min)
    }
}

impl Notifications {
    pub fn new() -> Notifications {
        let dirty = Arc::new(AtomicBool::new(true));
        // arre avisa cuando cambia algo: releer
        let d = dirty.clone();
        std::thread::spawn(move || {
            loop {
                let r: zbus::Result<()> = (|| {
                    let conn = Connection::session()?;
                    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(IFACE)?.member("Changed")?.build();
                    for _ in zbus::blocking::MessageIterator::for_match_rule(rule, &conn, Some(16))? {
                        d.store(true, Ordering::Relaxed);
                    }
                    Ok(())
                })();
                let _ = r;
                std::thread::sleep(Duration::from_secs(3));
            }
        });
        Notifications {
            conn: Connection::session().ok(),
            notes: Vec::new(),
            dnd: false,
            sel: 0,
            dirty,
            last: None,
            error: None,
            clear: false,
            rows: Vec::new(),
        }
    }

    fn call(&self, method: &str, body: &(impl zbus::export::serde::Serialize + zbus::zvariant::DynamicType)) -> Result<zbus::message::Message, String> {
        let c = self.conn.as_ref().ok_or("D-Bus")?;
        c.call_method(Some(BUS), PATH, Some(IFACE), method, body).map_err(|e| match e {
            zbus::Error::MethodError(name, ..) if name.as_str().ends_with("ServiceUnknown") => {
                l("arre no está instalado (es el que recibe las notificaciones)", "arre isn't installed (it receives the notifications)").to_string()
            }
            e => e.to_string(),
        })
    }

    fn load(&mut self) {
        let keep = self.notes.get(self.sel).map(|n| n.id);
        let list: Result<String, String> = self.call("List", &()).and_then(|m| m.body().deserialize().map_err(|e| e.to_string()));
        let list = match list {
            Ok(l) => l,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        self.error = None;
        self.dnd = self.call("Dnd", &()).ok().and_then(|m| m.body().deserialize().ok()).unwrap_or(false);
        let v: Value = serde_json::from_str(&list).unwrap_or(Value::Null);
        let s = |n: &Value, k: &str| n[k].as_str().unwrap_or("").to_string();
        self.notes = v
            .as_array()
            .into_iter()
            .flatten()
            .rev()
            .map(|n| Note {
                id: n["id"].as_u64().unwrap_or(0) as u32,
                app: s(n, "app"),
                summary: s(n, "summary"),
                body: s(n, "body"),
                actions: n["actions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| Some((a[0].as_str()?.to_string(), a[1].as_str()?.to_string())))
                    .collect(),
                urgency: n["urgency"].as_u64().unwrap_or(1) as u8,
                value: n["value"].as_i64(),
                time: n["time"].as_i64().unwrap_or(0),
            })
            .collect();
        self.sel = keep
            .and_then(|id| self.notes.iter().position(|n| n.id == id))
            .unwrap_or(self.sel.min(self.notes.len().saturating_sub(1)));
    }

    fn press(&mut self, code: KeyCode, ui: &mut Ui) {
        self.key(KeyEvent::new(code, KeyModifiers::NONE), ui);
    }

    fn dismiss(&mut self, ui: &mut Ui) {
        if let Some(n) = self.notes.get(self.sel).cloned() {
            match self.call("Dismiss", &(n.id,)) {
                Ok(_) => self.dirty.store(true, Ordering::Relaxed),
                Err(e) => ui.warn(e),
            }
        }
    }
}

impl Tab for Notifications {
    fn name(&self) -> &'static str {
        "notifications"
    }

    fn tick(&mut self, _ui: &mut Ui) {
        // además de cuando arre avisa, cada tanto (para el "hace N min")
        if self.dirty.swap(false, Ordering::Relaxed) || self.last.is_none_or(|t| t.elapsed() > Duration::from_secs(20)) {
            self.load();
            self.last = Some(Instant::now());
        }
    }

    fn draw(&mut self, f: &mut Frame, area: Rect, ui: &Ui) {
        let look = &ui.look;
        let t = look.theme;
        let detail_h = if self.notes.is_empty() { 0 } else { 9 };
        let [list_a, detail_a] = Layout::vertical([Constraint::Fill(1), Constraint::Length(detail_h)]).areas(area);

        // la lista
        let block = look.block(true);
        let inner = block.inner(list_a);
        f.render_widget(block, list_a);
        let mut lines = vec![Line::from(vec![
            Span::styled(format!(" {} ", look.tx(l("no molestar", "do not disturb"))), Style::new().fg(t.muted).bold()),
            Span::styled(
                look.tx(if self.dnd { l("sí", "on") } else { l("no", "off") }),
                Style::new().fg(if self.dnd { t.accent } else { t.muted }),
            ),
            Span::styled(format!("   {} ", look.tx(l("salen", "position"))), Style::new().fg(t.muted).bold()),
            Span::styled(look.tx(position_name(&position())), Style::new().fg(t.fg)),
            Span::styled(format!("   {} {}", self.notes.len(), look.tx(l("guardadas", "saved"))), Style::new().fg(t.muted)),
        ])];
        lines.push(Line::raw(""));
        if let Some(e) = &self.error {
            lines.push(Line::styled(format!(" {e}"), Style::new().fg(t.red)));
        } else if self.notes.is_empty() {
            lines.push(Line::styled(format!(" {}", l("Nada por aquí", "Nothing here")), Style::new().fg(t.muted)));
        }
        let w = inner.width as usize;
        let h = inner.height as usize;
        let first_row = lines.len();
        let room = h.saturating_sub(first_row);
        let top = self.sel.saturating_sub(room.saturating_sub(1));
        let (sel_style, mark) = look.sel(true);
        self.rows.clear();
        for (i, n) in self.notes.iter().enumerate().skip(top).take(room) {
            let selected = i == self.sel;
            self.rows.push((inner.y + lines.len() as u16, i));
            let time = format!("{:>11} ", when(n.time));
            let app: String = n.app.chars().take(14).collect();
            let app = format!("{app:<14} ");
            let rest = w.saturating_sub(1 + time.chars().count() + app.chars().count());
            let text = if n.body.is_empty() { n.summary.clone() } else { format!("{} — {}", n.summary, n.body.replace('\n', " ")) };
            let text: String = text.chars().take(rest).collect();
            let pad = rest.saturating_sub(text.chars().count());
            let critical = n.urgency >= 2;
            let st = if selected { sel_style } else { Style::new().fg(t.fg) };
            lines.push(Line::from(vec![
                Span::styled(if selected { mark } else { " " }, Style::new().fg(t.primary)),
                Span::styled(time, Style::new().fg(t.muted)),
                Span::styled(app, Style::new().fg(if critical { t.red } else { t.primary }).bold()),
                Span::styled(format!("{text}{}", " ".repeat(pad)), st),
            ]));
        }
        f.render_widget(Paragraph::new(lines), inner);
        if self.clear {
            let msg = format!(" {} C {} · esc {} ", l("¿Borrar todas?", "Clear all?"), l("sí", "yes"), l("no", "no"));
            let r = Rect {
                x: list_a.x + 2,
                y: list_a.bottom().saturating_sub(1),
                width: (msg.chars().count() as u16).min(list_a.width.saturating_sub(4)),
                height: 1,
            };
            f.render_widget(Paragraph::new(Line::styled(msg, Style::new().fg(t.base).bg(t.red).bold())), r);
        }

        // la elegida, completa
        let Some(n) = self.notes.get(self.sel) else { return };
        let title = Span::styled(format!(" {} · {} ", look.tx(&n.app), when(n.time)), Style::new().fg(t.primary).bold());
        let block = look.block(false).title(title);
        let inner = block.inner(detail_a);
        f.render_widget(block, detail_a);
        let mut lines = vec![Line::styled(format!(" {}", n.summary), Style::new().fg(t.fg).bold())];
        for b in n.body.lines() {
            lines.push(Line::styled(format!(" {b}"), Style::new().fg(t.muted)));
        }
        if let Some(v) = n.value {
            let (d, r) = look.bar(v as f64 / 100.0, (inner.width as usize).saturating_sub(8));
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(d, Style::new().fg(t.primary)),
                Span::styled(r, Style::new().fg(t.border)),
                Span::styled(format!(" {v:>3}%"), Style::new().fg(t.fg)),
            ]));
        }
        let buttons: Vec<&(String, String)> = n.actions.iter().filter(|a| a.0 != "default" && !a.1.is_empty()).collect();
        if !buttons.is_empty() {
            let mut spans = vec![Span::raw(" ")];
            for (_, label) in buttons {
                spans.push(Span::styled(format!(" {} ", look.tx(label)), Style::new().fg(t.base).bg(t.primary).bold()));
                spans.push(Span::raw(" "));
            }
            lines.push(Line::raw(""));
            lines.push(Line::from(spans));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn key(&mut self, k: KeyEvent, ui: &mut Ui) -> bool {
        if self.clear {
            self.clear = false;
            if k.code == KeyCode::Char('C') {
                match self.call("Clear", &()) {
                    Ok(_) => {
                        ui.say(l("Historial borrado", "History cleared"));
                        self.dirty.store(true, Ordering::Relaxed);
                    }
                    Err(e) => ui.warn(e),
                }
            }
            return true;
        }
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => self.sel = (self.sel + 1).min(self.notes.len().saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::PageDown => self.sel = (self.sel + 10).min(self.notes.len().saturating_sub(1)),
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(10),
            KeyCode::Home | KeyCode::Char('g') => self.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.sel = self.notes.len().saturating_sub(1),
            // lo que pide la notificación (abrir la app, el archivo…)
            KeyCode::Enter => {
                let Some(n) = self.notes.get(self.sel).cloned() else { return true };
                let key = n.actions.iter().find(|a| a.0 == "default").or_else(|| n.actions.first()).map(|a| a.0.clone());
                match key {
                    Some(key) => {
                        if let Err(e) = self.call("Invoke", &(n.id, key.as_str())) {
                            ui.warn(e);
                        }
                    }
                    None => ui.say(l("Esta no pide nada", "This one has nothing to open")),
                }
            }
            KeyCode::Char('x') | KeyCode::Delete => self.dismiss(ui),
            KeyCode::Char('C') if !self.notes.is_empty() => self.clear = true,
            KeyCode::Char('d') => {
                let on = !self.dnd;
                match self.call("SetDnd", &(on,)) {
                    Ok(_) => {
                        self.dnd = on;
                        ui.say(if on { l("No molestar: sí", "Do not disturb: on") } else { l("No molestar: no", "Do not disturb: off") });
                    }
                    Err(e) => ui.warn(e),
                }
            }
            KeyCode::Char('p') => {
                let cur = position();
                let i = POSITIONS.iter().position(|p| *p == cur).map_or(0, |i| (i + 1) % POSITIONS.len());
                let ok = std::process::Command::new(arre_bin()).args(["position", POSITIONS[i]]).status().is_ok_and(|s| s.success());
                if ok {
                    ui.say(format!("{}: {}", l("Salen", "Position"), position_name(POSITIONS[i])));
                } else {
                    ui.warn(l("No se pudo cambiar (¿está arre?)", "Couldn't change it (is arre installed?)"));
                }
            }
            _ => return false,
        }
        true
    }

    fn scroll(&mut self, _x: u16, _y: u16, down: bool, _ui: &mut Ui) {
        self.sel = if down { (self.sel + 1).min(self.notes.len().saturating_sub(1)) } else { self.sel.saturating_sub(1) };
    }

    fn click(&mut self, _x: u16, y: u16, b: MouseButton, double: bool, ui: &mut Ui) {
        if self.clear {
            self.clear = false;
            return;
        }
        let Some(&(_, i)) = self.rows.iter().find(|r| r.0 == y) else { return };
        self.sel = i;
        match b {
            MouseButton::Left if double => self.press(KeyCode::Enter, ui),
            MouseButton::Right => self.dismiss(ui),
            _ => {}
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.clear {
            return vec![("C", l("borrar todas", "clear all")), ("esc", l("no", "no"))];
        }
        vec![
            ("j/k", l("mover", "move")),
            ("enter", l("abrir", "open")),
            ("x", l("quitar", "dismiss")),
            ("C", l("borrar todas", "clear all")),
            ("d", l("no molestar", "do not disturb")),
            ("p", l("posición", "position")),
        ]
    }
}
