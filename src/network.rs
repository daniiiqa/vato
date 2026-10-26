//! Red: conexiones (cable, wifi, VPN) y redes wifi cerca, con NetworkManager
//! (nmcli). Conectar a una red con contraseña la pide aquí mismo. Lo que
//! tarda (escanear, conectar) va en otro hilo.

use crate::{Tab, Ui, l};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton};
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::collections::HashMap;
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
    /// el perfil guardado de esta red, si hay
    uuid: Option<String>,
}

/// Una VPN guardada (vpn o wireguard), conectada o no.
#[derive(Clone)]
struct Vpn {
    name: String,
    uuid: String,
    active: bool,
}

#[derive(Clone)]
enum Row {
    Dev(Dev),
    Vpn(Vpn),
    Wifi(Wifi),
}

pub struct Network {
    devs: Vec<Dev>,
    vpns: Vec<Vpn>,
    wifis: Vec<Wifi>,
    /// uuid de un perfil wifi → su SSID (el nombre del perfil puede ser otro)
    ssids: HashMap<String, String>,
    radio: bool,
    has_wifi: bool,
    sel: usize,
    last: Option<Instant>,
    busy: Option<String>,
    tx: Sender<Result<String, String>>,
    rx: Receiver<Result<String, String>>,
    /// pidiendo contraseña: (ssid, lo escrito, mostrarla)
    pass: Option<(String, String, bool)>,
    /// olvidar: (ssid, uuid del perfil)
    forget: Option<(String, String)>,
    /// desconectar pide un segundo enter (para no quedarse sin internet sin querer)
    confirm_down: Option<String>,
    rows_at: Vec<(u16, usize)>, // fila de pantalla → fila de la lista (para el ratón)
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

const NM: &str = "org.freedesktop.NetworkManager";

/// Conectar a una red nueva con contraseña hablándole a NetworkManager por
/// D-Bus: con nmcli la contraseña va en la línea de comando y cualquiera la ve
/// en `ps`. NetworkManager completa el tipo de seguridad (WPA2, WPA3) con el
/// punto de acceso. Si no conecta, el perfil a medias se borra.
fn connect_secret(iface: &str, ssid: &str, password: &str) -> Result<(), String> {
    use zbus::blocking::Connection;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
    let short = |e: zbus::Error| e.to_string().rsplit(": ").next().unwrap_or("").to_string();
    let c = Connection::system().map_err(short)?;
    let prop = |path: &OwnedObjectPath, iface: &str, name: &str| -> Option<OwnedValue> {
        c.call_method(Some(NM), path.as_str(), Some("org.freedesktop.DBus.Properties"), "Get", &(iface, name))
            .ok()?
            .body()
            .deserialize::<OwnedValue>()
            .ok()
    };
    let dev: OwnedObjectPath = c
        .call_method(Some(NM), "/org/freedesktop/NetworkManager", Some(NM), "GetDeviceByIpIface", &(iface,))
        .and_then(|m| m.body().deserialize())
        .map_err(short)?;
    // el punto de acceso de esa red con más señal
    let aps: Vec<OwnedObjectPath> = c
        .call_method(Some(NM), dev.as_str(), Some("org.freedesktop.NetworkManager.Device.Wireless"), "GetAllAccessPoints", &())
        .and_then(|m| m.body().deserialize())
        .map_err(short)?;
    let ap_iface = "org.freedesktop.NetworkManager.AccessPoint";
    let ap = aps
        .into_iter()
        .filter(|ap| prop(ap, ap_iface, "Ssid").and_then(|v| Vec::<u8>::try_from(v).ok()).as_deref() == Some(ssid.as_bytes()))
        .max_by_key(|ap| prop(ap, ap_iface, "Strength").and_then(|v| u8::try_from(v).ok()).unwrap_or(0))
        .ok_or_else(|| l("la red ya no está a la vista", "the network is out of range").to_string())?;

    let mut settings: HashMap<&str, HashMap<&str, Value>> = HashMap::new();
    settings.insert("connection", HashMap::from([("id", Value::from(ssid)), ("type", Value::from("802-11-wireless"))]));
    settings.insert("802-11-wireless", HashMap::from([("ssid", Value::from(ssid.as_bytes().to_vec()))]));
    settings.insert("802-11-wireless-security", HashMap::from([("psk", Value::from(password))]));
    let (conn, active): (OwnedObjectPath, OwnedObjectPath) = c
        .call_method(Some(NM), "/org/freedesktop/NetworkManager", Some(NM), "AddAndActivateConnection", &(settings, &dev, &ap))
        .and_then(|m| m.body().deserialize())
        .map_err(short)?;

    // esperar: 2 = conectada; 4 = se cayó, o el objeto desaparece
    let start = Instant::now();
    let ok = loop {
        std::thread::sleep(Duration::from_millis(300));
        match prop(&active, "org.freedesktop.NetworkManager.Connection.Active", "State").and_then(|v| u32::try_from(v).ok()) {
            Some(2) => break true,
            Some(4) | None => break false,
            _ if start.elapsed() > Duration::from_secs(45) => break false,
            _ => {}
        }
    };
    if ok {
        return Ok(());
    }
    let _ = c.call_method(Some(NM), conn.as_str(), Some("org.freedesktop.NetworkManager.Settings.Connection"), "Delete", &());
    Err(l("no conectó (¿contraseña mala?)", "couldn't connect (wrong password?)").to_string())
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
            vpns: Vec::new(),
            wifis: Vec::new(),
            ssids: HashMap::new(),
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
            rows_at: Vec::new(),
        }
    }

    fn press(&mut self, code: KeyCode, ui: &mut Ui) {
        self.key(KeyEvent::new(code, KeyModifiers::NONE), ui);
    }

    fn rows(&self) -> Vec<Row> {
        self.devs
            .iter()
            .cloned()
            .map(Row::Dev)
            .chain(self.vpns.iter().cloned().map(Row::Vpn))
            .chain(self.wifis.iter().cloned().map(Row::Wifi))
            .collect()
    }

    fn load(&mut self) {
        let mut devs = Vec::new();
        for line in nmcli(&["-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "device"]).unwrap_or_default().lines() {
            let f = fields(line);
            if f.len() < 4 || matches!(f[1].as_str(), "loopback" | "bridge" | "wifi-p2p" | "tun" | "wireguard") {
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
        // perfiles guardados: wifi (por SSID, que puede no ser su nombre) y VPNs
        let mut saved: Vec<(String, String)> = Vec::new();
        let mut vpns = Vec::new();
        for f in nmcli(&["-t", "-f", "NAME,UUID,TYPE,ACTIVE", "connection", "show"]).unwrap_or_default().lines().map(fields) {
            if f.len() < 4 {
                continue;
            }
            match f[2].as_str() {
                "802-11-wireless" => {
                    let ssid = self.ssids.entry(f[1].clone()).or_insert_with(|| {
                        nmcli(&["-g", "802-11-wireless.ssid", "connection", "show", &f[1]])
                            .map(|s| s.trim().to_string())
                            .ok()
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| f[0].clone())
                    });
                    saved.push((ssid.clone(), f[1].clone()));
                }
                "vpn" | "wireguard" => vpns.push(Vpn { name: f[0].clone(), uuid: f[1].clone(), active: f[3] == "yes" }),
                _ => {}
            }
        }
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
                    uuid: saved.iter().find(|(s, _)| *s == f[1]).map(|(_, u)| u.clone()),
                    ssid: f[1].clone(),
                };
                // la misma red en varios puntos de acceso: la más fuerte
                match wifis.iter_mut().find(|x| x.ssid == w.ssid) {
                    Some(x) if x.signal < w.signal || w.active => *x = w,
                    Some(_) => {}
                    None => wifis.push(w),
                }
            }
            wifis.sort_by_key(|w| (!w.active, w.uuid.is_none(), std::cmp::Reverse(w.signal)));
        }
        let keep = self.key_of(self.sel);
        self.devs = devs;
        self.vpns = vpns;
        self.wifis = wifis;
        let rows = self.rows();
        self.sel = keep
            .and_then(|k| rows.iter().position(|r| Self::row_key(r) == k))
            .unwrap_or(self.sel.min(rows.len().saturating_sub(1)));
    }

    fn row_key(r: &Row) -> String {
        match r {
            Row::Dev(d) => format!("dev:{}", d.name),
            Row::Vpn(v) => format!("vpn:{}", v.uuid),
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

    /// Una red sin perfil: nmcli lo crea al conectar, y si falla (contraseña
    /// mala) lo deja guardado y ya no la vuelve a pedir; ese se borra.
    fn connect_wifi(&mut self, ssid: String, password: Option<String>) {
        let iface = self.devs.iter().find(|d| d.kind == "wifi").map(|d| d.name.clone()).unwrap_or_default();
        self.spawn(format!("{} {ssid}…", l("Conectando a", "Connecting to")), move || {
            let done = format!("{}: {ssid}", l("Conectado", "Connected"));
            if let Some(p) = password {
                return connect_secret(&iface, &ssid, &p).map(|_| done);
            }
            nmcli(&["device", "wifi", "connect", &ssid]).map(|_| done).inspect_err(|_| {
                let _ = nmcli(&["connection", "delete", "id", &ssid]);
            })
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
            Some(Row::Vpn(v)) => {
                let (name, uuid, up) = (v.name, v.uuid, !v.active);
                self.spawn(
                    format!("{} {name}…", if up { l("Conectando", "Connecting") } else { l("Desconectando", "Disconnecting") }),
                    move || {
                        nmcli(&["connection", if up { "up" } else { "down" }, &uuid])
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
                // bajar el perfil (no el aparato, que deja el wifi sin autoconectar)
                let dev = self.devs.iter().find(|d| d.kind == "wifi" && d.state.starts_with("connected")).map(|d| d.name.clone());
                self.spawn(String::new(), move || {
                    match (w.uuid, dev) {
                        (Some(u), _) => nmcli(&["connection", "down", &u]),
                        (None, Some(d)) => nmcli(&["device", "disconnect", &d]),
                        (None, None) => nmcli(&["connection", "down", "id", &ssid]),
                    }
                    .map(|_| format!("{ssid}: {}", l("desconectado", "disconnected")))
                });
            }
            Some(Row::Wifi(Wifi { ssid, uuid: Some(u), .. })) => {
                self.spawn(format!("{} {ssid}…", l("Conectando a", "Connecting to")), move || {
                    nmcli(&["connection", "up", &u]).map(|_| format!("{}: {ssid}", l("Conectado", "Connected")))
                });
            }
            Some(Row::Wifi(w)) if w.secure => self.pass = Some((w.ssid, String::new(), false)),
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
        let mut item_lines = Vec::new();
        let w = inner.width as usize;
        for (i, d) in self.devs.iter().enumerate() {
            let selected = i == self.sel;
            if selected {
                sel_line = lines.len();
            }
            item_lines.push((lines.len(), i));
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
        if !self.vpns.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled(format!(" {}", look.tx("vpn")), Style::new().fg(t.muted).bold()));
        }
        for (j, v) in self.vpns.iter().enumerate() {
            let selected = self.devs.len() + j == self.sel;
            if selected {
                sel_line = lines.len();
            }
            item_lines.push((lines.len(), self.devs.len() + j));
            let name: String = format!("\u{f0582} {}", v.name).chars().take(w.saturating_sub(22)).collect();
            let pad = w.saturating_sub(22 + name.chars().count());
            let state = if v.active { l("conectada", "connected") } else { "" };
            lines.push(Line::from(vec![
                Span::styled(if selected { mark } else { " " }, Style::new().fg(t.primary)),
                Span::styled(format!("{name}{}", " ".repeat(pad)), if selected { sel_style } else { Style::new().fg(if v.active { t.fg } else { t.muted }) }),
                Span::styled(format!(" {:>18}", look.tx(state)), Style::new().fg(t.primary)),
            ]));
        }
        if self.has_wifi {
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", look.tx("wifi")), Style::new().fg(t.muted).bold()),
                Span::styled(
                    look.tx(if self.radio { l("encendido", "on") } else { l("apagado", "off") }),
                    Style::new().fg(if self.radio { t.primary } else { t.muted }),
                ),
            ]));
        }
        let base = self.devs.len() + self.vpns.len();
        let lv = look.levels();
        for (j, wf) in self.wifis.iter().enumerate() {
            let selected = base + j == self.sel;
            if selected {
                sel_line = lines.len();
            }
            item_lines.push((lines.len(), base + j));
            let bars: String = (0..4)
                .map(|k| if wf.signal as usize > k * 25 { lv[(k * 2 + 1).min(lv.len() - 1)] } else { ' ' })
                .collect();
            let lock = if wf.secure { "\u{f033e}" } else { " " };
            let tag = if wf.active { l("conectado", "connected") } else if wf.uuid.is_some() { l("guardada", "saved") } else { "" };
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
        if let Some((ssid, _)) = &self.forget {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!(" {} {ssid}? (f {} · esc {})", l("¿Olvidar", "Forget"), l("sí", "yes"), l("no", "no")),
                Style::new().fg(t.red).bold(),
            ));
        }
        let h = inner.height as usize;
        let top = sel_line.saturating_sub(h.saturating_sub(4));
        f.render_widget(Paragraph::new(lines).scroll((top as u16, 0)), inner);
        self.rows_at = item_lines
            .into_iter()
            .filter(|(li, _)| *li >= top && li - top < h)
            .map(|(li, i)| (inner.y + (li - top) as u16, i))
            .collect();
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
        if let Some((ssid, uuid)) = self.forget.take() {
            if k.code == KeyCode::Char('f') {
                self.spawn(String::new(), move || nmcli(&["connection", "delete", &uuid]).map(|_| format!("{ssid}: {}", l("olvidada", "forgotten"))));
            }
            return true;
        }
        if self.confirm_down.is_some() && !matches!(k.code, KeyCode::Enter | KeyCode::Char(' ')) {
            self.confirm_down = None;
            return true;
        }
        let n = self.rows().len();
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
                if let Some(Row::Wifi(Wifi { ssid, uuid: Some(u), .. })) = self.rows().get(self.sel) {
                    self.forget = Some((ssid.clone(), u.clone()));
                }
            }
            _ => return false,
        }
        true
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.pass.is_some() {
            return vec![("enter", l("conectar", "connect")), ("tab", l("mostrar", "show")), ("esc", l("cancelar", "cancel"))];
        }
        if self.forget.is_some() {
            return vec![("f", l("olvidar", "forget")), ("esc", l("no", "no"))];
        }
        if self.confirm_down.is_some() {
            return vec![("enter", l("desconectar", "disconnect")), ("esc", l("no", "no"))];
        }
        let enter = match self.rows().get(self.sel) {
            Some(Row::Dev(d)) if d.state.starts_with("connected") => l("desconectar", "disconnect"),
            Some(Row::Wifi(w)) if w.active => l("desconectar", "disconnect"),
            Some(Row::Vpn(v)) if v.active => l("desconectar", "disconnect"),
            Some(_) => l("conectar", "connect"),
            None => "",
        };
        let mut v = vec![("j/k", l("mover", "move"))];
        if !enter.is_empty() {
            v.push(("enter", enter));
        }
        if self.has_wifi {
            v.extend([("r", l("buscar redes", "rescan")), ("w", l("wifi on/off", "wifi on/off"))]);
        }
        if matches!(self.rows().get(self.sel), Some(Row::Wifi(Wifi { uuid: Some(_), .. }))) {
            v.push(("f", l("olvidar", "forget")));
        }
        v
    }

    fn click(&mut self, _x: u16, y: u16, b: MouseButton, double: bool, ui: &mut Ui) {
        if self.pass.is_some() || self.forget.is_some() {
            return;
        }
        let Some(i) = self.rows_at.iter().find(|r| r.0 == y).map(|r| r.1) else { return };
        if self.sel != i {
            self.confirm_down = None;
        }
        self.sel = i;
        match b {
            MouseButton::Left if double => self.press(KeyCode::Enter, ui),
            MouseButton::Right => self.press(KeyCode::Char('f'), ui),
            _ => {}
        }
    }

    fn scroll(&mut self, _x: u16, _y: u16, down: bool, _ui: &mut Ui) {
        if self.pass.is_none() {
            self.sel = if down { (self.sel + 1).min(self.rows().len().saturating_sub(1)) } else { self.sel.saturating_sub(1) };
        }
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
