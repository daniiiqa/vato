//! Red: conexiones (cable, wifi, VPN) y redes wifi cerca, con NetworkManager
//! (nmcli). Conectar a una red con contraseña la pide aquí mismo. Lo que
//! tarda (escanear, conectar) va en otro hilo.

use crate::{Tab, Ui, l};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

#[derive(Clone)]
struct Dev {
    name: String,
    kind: String,
    state: String,
    conn: String,
    ip: String,
}

#[derive(Clone)]
struct Wifi {
    ssid: String,
    signal: u8,
    secure: bool,
    active: bool,
    saved: bool,
}

#[derive(Clone)]
enum Row {
    Dev(Dev),
    Wifi(Wifi),
}

pub struct Network {
    devs: Vec<Dev>,
    wifis: Vec<Wifi>,
    radio: bool,
    has_wifi: bool,
    sel: usize,
    last: Option<Instant>,
    busy: Option<String>,
    tx: Sender<Result<String, String>>,
    rx: Receiver<Result<String, String>>,
    /// pidiendo contraseña: (ssid, lo escrito, mostrarla)
    pass: Option<(String, String, bool)>,
    forget: Option<String>,
    /// desconectar pide un segundo enter (para no quedarse sin internet sin querer)
    confirm_down: Option<String>,
}

fn nmcli(args: &[&str]) -> Result<String, String> {
    let out = Command::new("nmcli").args(args).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        // nmcli avisa en stderr de cosas que no son error (versiones): la última línea manda
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().rfind(|l| !l.starts_with("Warning")).unwrap_or("nmcli").trim_start_matches("Error: ").to_string())
    }
}

/// Campos de `nmcli -t`: separados por ':' y con '\:' para los dos puntos de adentro.
fn fields(line: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut esc = false;
    for c in line.chars() {
        match (esc, c) {
            (false, '\\') => esc = true,
            (false, ':') => out.push(String::new()),
            _ => {
                out.last_mut().unwrap().push(c);
                esc = false;
            }
        }
    }
    out
}

impl Network {
    pub fn new() -> Network {
        let (tx, rx) = channel();
        Network {
            devs: Vec::new(),
            wifis: Vec::new(),
            radio: false,
            has_wifi: false,
            sel: 0,
            last: None,
            busy: None,
            tx,
            rx,
            pass: None,
            forget: None,
            confirm_down: None,
        }
    }

    fn rows(&self) -> Vec<Row> {
        self.devs.iter().cloned().map(Row::Dev).chain(self.wifis.iter().cloned().map(Row::Wifi)).collect()
    }

    fn load(&mut self) {
        let mut devs = Vec::new();
        for line in nmcli(&["-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "device"]).unwrap_or_default().lines() {
            let f = fields(line);
            if f.len() < 4 || matches!(f[1].as_str(), "loopback" | "bridge" | "wifi-p2p" | "tun") {
                continue;
            }
            let ip = if f[2].starts_with("connected") {
                nmcli(&["-t", "-f", "IP4.ADDRESS", "device", "show", &f[0]])
                    .unwrap_or_default()
                    .lines()
                    .find_map(|l| l.split_once(':').map(|(_, v)| v.to_string()))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            devs.push(Dev { name: f[0].clone(), kind: f[1].clone(), state: f[2].clone(), conn: f[3].clone(), ip });
        }
        self.has_wifi = devs.iter().any(|d| d.kind == "wifi");
        self.radio = nmcli(&["-t", "radio", "wifi"]).unwrap_or_default().trim() == "enabled";
        let saved: Vec<String> = nmcli(&["-t", "-f", "NAME,TYPE", "connection", "show"])
            .unwrap_or_default()
            .lines()
            .map(fields)
            .filter(|f| f.len() >= 2 && f[1].contains("wireless"))
            .map(|f| f[0].clone())
            .collect();
        let mut wifis: Vec<Wifi> = Vec::new();
        if self.has_wifi && self.radio {
            for line in nmcli(&["-t", "-f", "IN-USE,SSID,SIGNAL,SECURITY", "device", "wifi", "list", "--rescan", "no"]).unwrap_or_default().lines() {
                let f = fields(line);
                if f.len() < 4 || f[1].is_empty() {
                    continue;
                }
                let w = Wifi {
                    active: f[0] == "*",
                    signal: f[2].parse().unwrap_or(0),
                    secure: !f[3].is_empty() && f[3] != "--",
                    saved: saved.contains(&f[1]),
                    ssid: f[1].clone(),
                };
                // la misma red en varios puntos de acceso: la más fuerte
                match wifis.iter_mut().find(|x| x.ssid == w.ssid) {
                    Some(x) if x.signal < w.signal || w.active => *x = w,
                    Some(_) => {}
                    None => wifis.push(w),
                }
            }
            wifis.sort_by(|a, b| (!a.active, !a.saved, std::cmp::Reverse(a.signal)).cmp(&(!b.active, !b.saved, std::cmp::Reverse(b.signal))));
        }
        let keep = self.key_of(self.sel);
        self.devs = devs;
        self.wifis = wifis;
        let rows = self.rows();
        self.sel = keep
            .and_then(|k| rows.iter().position(|r| Self::row_key(r) == k))
            .unwrap_or(self.sel.min(rows.len().saturating_sub(1)));
    }

    fn row_key(r: &Row) -> String {
        match r {
            Row::Dev(d) => format!("dev:{}", d.name),
            Row::Wifi(w) => format!("wifi:{}", w.ssid),
        }
    }

    fn key_of(&self, i: usize) -> Option<String> {
        self.rows().get(i).map(Self::row_key)
    }

    fn spawn(&mut self, what: String, job: impl FnOnce() -> Result<String, String> + Send + 'static) {
        self.busy = (!what.is_empty()).then_some(what);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job());
        });
    }

    fn connect_wifi(&mut self, ssid: String, password: Option<String>) {
        self.spawn(format!("{} {ssid}…", l("Conectando a", "Connecting to")), move || {
            let mut args = vec!["device", "wifi", "connect", ssid.as_str()];
            if let Some(p) = &password {
                args.extend(["password", p.as_str()]);
            }
            nmcli(&args).map(|_| format!("{}: {ssid}", l("Conectado", "Connected")))
        });
    }

    fn enter(&mut self) {
        match self.rows().get(self.sel).cloned() {
            Some(Row::Dev(d)) => {
                let up = !d.state.starts_with("connected");
                let name = d.name.clone();
                if !up && self.confirm_down.as_deref() != Some(name.as_str()) {
                    self.confirm_down = Some(name);
                    return;
                }
                self.confirm_down = None;
                self.spawn(
                    format!("{} {name}…", if up { l("Conectando", "Connecting") } else { l("Desconectando", "Disconnecting") }),
                    move || {
                        nmcli(&["device", if up { "connect" } else { "disconnect" }, &name])
                            .map(|_| format!("{name}: {}", if up { l("conectado", "connected") } else { l("desconectado", "disconnected") }))
                    },
                );
            }
            Some(Row::Wifi(w)) if w.active => {
                let ssid = w.ssid.clone();
                if self.confirm_down.as_deref() != Some(ssid.as_str()) {
                    self.confirm_down = Some(ssid);
                    return;
                }
                self.confirm_down = None;
                self.spawn(String::new(), move || nmcli(&["connection", "down", &ssid]).map(|_| format!("{ssid}: {}", l("desconectado", "disconnected"))));
            }
            Some(Row::Wifi(w)) if w.secure && !w.saved => self.pass = Some((w.ssid, String::new(), false)),
            Some(Row::Wifi(w)) => self.connect_wifi(w.ssid, None),
            None => {}
        }
    }
}

impl Tab for Network {
    fn name(&self) -> &'static str {
        "network"
    }

    fn typing(&self) -> bool {
        self.pass.is_some()
    }

    fn tick(&mut self, ui: &mut Ui) {
        while let Ok(r) = self.rx.try_recv() {
            self.busy = None;
            match r {
                Ok(m) => ui.say(m),
                Err(e) => ui.warn(e),
            }
            self.last = None;
        }
        if self.last.is_none_or(|t| t.elapsed() > Duration::from_secs(4)) {
            self.load();
            self.last = Some(Instant::now());
        }
    }

    fn draw(&mut self, f: &mut Frame, area: Rect, ui: &Ui) {
        let look = &ui.look;
        let t = look.theme;
        let block = look.block(true);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let mut lines: Vec<Line> = Vec::new();
        let busy = self.busy.clone().unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", look.tx(l("conexiones", "connections"))), Style::new().fg(t.muted).bold()),
            Span::styled(busy, Style::new().fg(t.accent)),
        ]));
        let (sel_style, mark) = look.sel(true);
        let mut sel_line = 0;
        let w = inner.width as usize;
        for (i, d) in self.devs.iter().enumerate() {
            let selected = i == self.sel;
            if selected {
                sel_line = lines.len();
            }
            let up = d.state.starts_with("connected");
            let icon = match d.kind.as_str() {
                "ethernet" => "\u{f0200}",
                "wifi" => "\u{f05a9}",
                "wireguard" | "vpn" => "\u{f0582}",
                _ => "\u{f0318}",
            };
            let what = if up { d.conn.clone() } else { look.tx(&d.state) };
            let left = format!("{icon} {:<10} {what}", d.name);
            let left: String = left.chars().take(w.saturating_sub(22)).collect();
            let pad = w.saturating_sub(22 + left.chars().count());
            lines.push(Line::from(vec![
                Span::styled(if selected { mark } else { " " }, Style::new().fg(t.primary)),
                Span::styled(format!("{left}{}", " ".repeat(pad)), if selected { sel_style } else { Style::new().fg(if up { t.fg } else { t.muted }) }),
                Span::styled(format!(" {:>18}", d.ip), Style::new().fg(t.primary)),
            ]));
        }
        lines.push(Line::raw(""));
        let wifi_state = if !self.has_wifi {
            l("sin adaptador wifi", "no wifi adapter")
        } else if self.radio {
            l("encendido", "on")
        } else {
            l("apagado", "off")
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", look.tx("wifi")), Style::new().fg(t.muted).bold()),
            Span::styled(look.tx(wifi_state), Style::new().fg(if self.radio && self.has_wifi { t.primary } else { t.muted })),
        ]));
        let base = self.devs.len();
        let lv = look.levels();
        for (j, wf) in self.wifis.iter().enumerate() {
            let selected = base + j == self.sel;
            if selected {
                sel_line = lines.len();
            }
            let bars: String = (0..4)
                .map(|k| if wf.signal as usize > k * 25 { lv[(k * 2 + 1).min(lv.len() - 1)] } else { ' ' })
                .collect();
            let lock = if wf.secure { "\u{f033e}" } else { " " };
            let tag = if wf.active { l("conectado", "connected") } else if wf.saved { l("guardada", "saved") } else { "" };
            let name: String = wf.ssid.chars().take(w.saturating_sub(30)).collect();
            let pad = w.saturating_sub(30 + name.chars().count());
            lines.push(Line::from(vec![
                Span::styled(if selected { mark } else { " " }, Style::new().fg(t.primary)),
                Span::styled(format!("{bars} "), Style::new().fg(if wf.active { t.primary } else { t.fg })),
                Span::styled(format!("{name}{}", " ".repeat(pad)), if selected { sel_style } else { Style::new().fg(t.fg) }),
                Span::styled(format!(" {lock} {:<12} {:>3}%", look.tx(tag), wf.signal), Style::new().fg(t.muted)),
            ]));
        }
        if let Some((ssid, typed, show)) = &self.pass {
            lines.push(Line::raw(""));
            let shown = if *show { typed.clone() } else { "•".repeat(typed.chars().count()) };
            lines.push(Line::from(vec![
                Span::styled(format!(" {} {ssid}: ", l("Contraseña de", "Password for")), Style::new().fg(t.accent).bold()),
                Span::styled(format!("{shown}▏"), Style::new().fg(t.fg)),
            ]));
            lines.push(Line::styled(
                format!(" enter {} · tab {} · esc {}", l("conectar", "connect"), l("mostrar", "show"), l("cancelar", "cancel")),
                Style::new().fg(t.muted),
            ));
        }
        if let Some(what) = &self.confirm_down {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!(" {} {what}? (enter {} · esc {})", l("¿Desconectar", "Disconnect"), l("sí", "yes"), l("no", "no")),
                Style::new().fg(t.red).bold(),
            ));
        }
        if let Some(ssid) = &self.forget {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!(" {} {ssid}? (f {} · esc {})", l("¿Olvidar", "Forget"), l("sí", "yes"), l("no", "no")),
                Style::new().fg(t.red).bold(),
            ));
        }
        let h = inner.height as usize;
        let top = sel_line.saturating_sub(h.saturating_sub(4));
        f.render_widget(Paragraph::new(lines).scroll((top as u16, 0)), inner);
    }

    fn key(&mut self, k: KeyEvent, _ui: &mut Ui) -> bool {
        if let Some((ssid, mut typed, show)) = self.pass.take() {
            match k.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.connect_wifi(ssid, Some(typed)),
                KeyCode::Tab => self.pass = Some((ssid, typed, !show)),
                KeyCode::Backspace => {
                    typed.pop();
                    self.pass = Some((ssid, typed, show));
                }
                KeyCode::Char(c) => {
                    typed.push(c);
                    self.pass = Some((ssid, typed, show));
                }
                _ => self.pass = Some((ssid, typed, show)),
            }
            return true;
        }
        if let Some(ssid) = self.forget.take() {
            if k.code == KeyCode::Char('f') {
                self.spawn(String::new(), move || nmcli(&["connection", "delete", &ssid]).map(|_| format!("{ssid}: {}", l("olvidada", "forgotten"))));
            }
            return true;
        }
        if self.confirm_down.is_some() && !matches!(k.code, KeyCode::Enter | KeyCode::Char(' ')) {
            self.confirm_down = None;
            return true;
        }
        let n = self.devs.len() + self.wifis.len();
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char(' ') => self.enter(),
            KeyCode::Char('r') if self.has_wifi => {
                self.spawn(l("Buscando redes…", "Scanning…").to_string(), || {
                    nmcli(&["device", "wifi", "rescan"]).map(|_| l("Lista de redes al día", "Network list updated").to_string())
                });
            }
            KeyCode::Char('w') if self.has_wifi => {
                let on = !self.radio;
                self.spawn(String::new(), move || {
                    nmcli(&["radio", "wifi", if on { "on" } else { "off" }])
                        .map(|_| if on { l("Wifi encendido", "Wifi on") } else { l("Wifi apagado", "Wifi off") }.to_string())
                });
            }
            KeyCode::Char('f') => {
                if let Some(Row::Wifi(w)) = self.rows().get(self.sel) {
                    if w.saved {
                        self.forget = Some(w.ssid.clone());
                    }
                }
            }
            _ => return false,
        }
        true
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        let enter = match self.rows().get(self.sel) {
            Some(Row::Dev(d)) if d.state.starts_with("connected") => l("desconectar", "disconnect"),
            Some(Row::Wifi(w)) if w.active => l("desconectar", "disconnect"),
            Some(_) => l("conectar", "connect"),
            None => "",
        };
        let mut v = vec![("j/k", l("mover", "move"))];
        if !enter.is_empty() {
            v.push(("enter", enter));
        }
        if self.has_wifi {
            v.extend([("r", l("buscar redes", "rescan")), ("w", l("wifi on/off", "wifi on/off")), ("f", l("olvidar", "forget"))]);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::fields;

    #[test]
    fn nmcli_fields() {
        assert_eq!(fields(r"*:Mi\:Red:72:WPA2"), vec!["*", "Mi:Red", "72", "WPA2"]);
        assert_eq!(fields(r":casa\\x::"), vec!["", "casa\\x", "", ""]);
    }
}
