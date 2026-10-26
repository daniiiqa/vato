//! Bluetooth: dispositivos emparejados y cercanos (BlueZ por D-Bus). vato es
//! el agente para emparejar ("KeyboardDisplay"): lo que BlueZ pregunta
//! (¿emparejar?, ¿coincide el código?, escribe el PIN, ¿dejar que use un
//! servicio?) sale en la pestaña y nada se acepta solo. Conectar y emparejar
//! tardan: van en otro hilo y avisan al terminar.

use crate::{Tab, Ui, l};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton};
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zbus::blocking::Connection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

type Props = HashMap<String, OwnedValue>;
type Objects = HashMap<OwnedObjectPath, HashMap<String, Props>>;

#[derive(Clone)]
struct Device {
    path: String,
    name: String,
    icon: String,
    paired: bool,
    connected: bool,
    trusted: bool,
    rssi: Option<i16>,
    battery: Option<u8>,
}

/// Lo que BlueZ le pregunta al agente.
#[derive(Clone, PartialEq)]
enum Ask {
    Authorize,       // ¿emparejar?
    Confirm(u32),    // ¿el otro muestra este código?
    Service(String), // ¿dejar que use este servicio (uuid)?
    Passkey,         // escribir el número que muestra el otro
    Pin,             // escribir el PIN (los viejitos)
    Show(String),    // escribir esto en el dispositivo (solo aviso)
}

enum Answer {
    Yes,
    No,
    Text(String),
}

/// Lo que comparten el agente (hilo de zbus) y la pestaña: la pregunta
/// abierta (dispositivo, qué, número) y la respuesta.
#[derive(Default)]
struct Pending {
    seq: u64,
    ask: Option<(String, Ask, u64)>,
    answer: Option<(u64, Answer)>,
}

struct Agent(Arc<Mutex<Pending>>);

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
    Canceled(String),
}

impl Agent {
    fn show(&self, device: &OwnedObjectPath, a: Ask) -> u64 {
        let mut p = self.0.lock().unwrap();
        p.seq += 1;
        p.ask = Some((device.to_string(), a, p.seq));
        p.answer = None;
        p.seq
    }

    /// Pregunta y espera (sin trabar el hilo de zbus) hasta que contesten,
    /// cancelen o pase un minuto.
    async fn ask(&self, device: OwnedObjectPath, a: Ask) -> Result<Answer, AgentError> {
        let id = self.show(&device, a);
        let start = Instant::now();
        loop {
            async_io::Timer::after(Duration::from_millis(100)).await;
            let mut p = self.0.lock().unwrap();
            if p.answer.as_ref().is_some_and(|x| x.0 == id) {
                p.ask = None;
                return match p.answer.take().unwrap().1 {
                    Answer::No => Err(AgentError::Rejected(String::new())),
                    a => Ok(a),
                };
            }
            if p.ask.as_ref().is_none_or(|x| x.2 != id) || start.elapsed() > Duration::from_secs(60) {
                if p.ask.as_ref().is_some_and(|x| x.2 == id) {
                    p.ask = None;
                }
                return Err(AgentError::Canceled(String::new()));
            }
        }
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}
    async fn request_pin_code(&self, device: OwnedObjectPath) -> Result<String, AgentError> {
        match self.ask(device, Ask::Pin).await? {
            Answer::Text(t) if !t.is_empty() => Ok(t),
            _ => Err(AgentError::Rejected(String::new())),
        }
    }
    fn display_pin_code(&self, device: OwnedObjectPath, pincode: String) {
        self.show(&device, Ask::Show(pincode));
    }
    async fn request_passkey(&self, device: OwnedObjectPath) -> Result<u32, AgentError> {
        match self.ask(device, Ask::Passkey).await? {
            Answer::Text(t) => t.trim().parse().map_err(|_| AgentError::Rejected(String::new())),
            _ => Err(AgentError::Rejected(String::new())),
        }
    }
    fn display_passkey(&self, device: OwnedObjectPath, passkey: u32, _entered: u16) {
        let code = format!("{passkey:06}");
        let same = matches!(&self.0.lock().unwrap().ask, Some((_, Ask::Show(c), _)) if *c == code);
        if !same {
            self.show(&device, Ask::Show(code));
        }
    }
    async fn request_confirmation(&self, device: OwnedObjectPath, passkey: u32) -> Result<(), AgentError> {
        self.ask(device, Ask::Confirm(passkey)).await.map(|_| ())
    }
    async fn request_authorization(&self, device: OwnedObjectPath) -> Result<(), AgentError> {
        self.ask(device, Ask::Authorize).await.map(|_| ())
    }
    async fn authorize_service(&self, device: OwnedObjectPath, uuid: String) -> Result<(), AgentError> {
        self.ask(device, Ask::Service(uuid)).await.map(|_| ())
    }
    fn cancel(&self) {
        self.0.lock().unwrap().ask = None;
    }
}

/// El servicio que pide un dispositivo, por su UUID (los comunes).
fn service(uuid: &str) -> &'static str {
    match uuid.get(4..8).unwrap_or("") {
        "110a" | "110b" | "110d" | "110c" | "110e" => l("audio", "audio"),
        "1108" | "1112" | "111e" | "111f" => l("audio con micrófono", "headset audio"),
        "1124" | "1812" => l("teclado/mouse/control", "keyboard/mouse/controller"),
        "1105" | "1106" => l("mandar archivos", "file transfer"),
        "112f" | "1132" => l("tus contactos y mensajes", "your contacts and messages"),
        "1115" | "1116" => l("la red", "networking"),
        _ => l("un servicio", "a service"),
    }
}

pub struct Bluetooth {
    conn: Option<Connection>,
    adapter: Option<String>,
    powered: bool,
    scanning: bool,
    devices: Vec<Device>,
    sel: usize,
    last: Option<Instant>,
    busy: Option<String>,
    tx: Sender<Result<String, String>>,
    rx: Receiver<Result<String, String>>,
    forget: Option<String>,
    error: Option<String>,
    agent: Arc<Mutex<Pending>>,
    /// número de la última pregunta vista (para traer la pestaña al frente)
    seen: u64,
    /// lo escrito para un PIN o código
    typed: String,
    rows: Vec<(u16, usize)>, // fila de pantalla → dispositivo (para el ratón)
}

fn get<'a, T: TryFrom<&'a Value<'a>>>(p: &'a Props, k: &str) -> Option<T> {
    p.get(k).and_then(|v| T::try_from(v).ok())
}

fn call(conn: &Connection, path: &str, iface: &str, method: &str) -> Result<(), String> {
    conn.call_method(Some("org.bluez"), path, Some(iface), method, &()).map(|_| ()).map_err(|e| short(&e.to_string()))
}

fn set_prop(conn: &Connection, path: &str, iface: &str, prop: &str, v: bool) -> Result<(), String> {
    conn.call_method(
        Some("org.bluez"),
        path,
        Some("org.freedesktop.DBus.Properties"),
        "Set",
        &(iface, prop, Value::from(v)),
    )
    .map(|_| ())
    .map_err(|e| short(&e.to_string()))
}

/// "org.bluez.Error.Failed: br-connection-page-timeout" → algo que se entienda
fn short(e: &str) -> String {
    let last = e.rsplit(": ").next().unwrap_or(e);
    let low = e.to_lowercase();
    let human = if low.contains("page-timeout") || low.contains("host is down") || low.contains("connection-abort") {
        l("no responde (¿está prendido y cerca?)", "not responding (is it on and nearby?)")
    } else if low.contains("not ready") {
        l("el bluetooth está apagado (p para prender)", "bluetooth is off (p to turn on)")
    } else if low.contains("authentication") || low.contains("auth") && low.contains("fail") {
        l("no se pudo emparejar (ponlo en modo emparejar)", "pairing failed (put it in pairing mode)")
    } else if low.contains("inprogress") || low.contains("in progress") {
        l("ya está en eso, espera", "already in progress, hold on")
    } else if low.contains("alreadyexists") || low.contains("already exists") {
        l("ya está emparejado", "already paired")
    } else if low.contains("connection refused") || low.contains("profile-unavailable") {
        l("el dispositivo no aceptó (¿está conectado a otro?)", "the device refused (connected elsewhere?)")
    } else if low.contains("doesnotexist") || low.contains("does not exist") {
        l("ya no está", "it's gone")
    } else {
        last
    };
    human.to_string()
}

fn icon(kind: &str) -> &'static str {
    match kind {
        k if k.starts_with("audio-head") => "\u{f02cb}",
        k if k.starts_with("audio") => "\u{f04c3}",
        "input-gaming" => "\u{f0eb5}",
        "input-keyboard" => "\u{f030c}",
        "input-mouse" | "input-tablet" => "\u{f037d}",
        "phone" => "\u{f011c}",
        "computer" => "\u{f0322}",
        _ => "\u{f00af}",
    }
}

impl Bluetooth {
    pub fn new() -> Bluetooth {
        let (tx, rx) = channel();
        let mut b = Bluetooth {
            conn: None,
            adapter: None,
            powered: false,
            scanning: false,
            devices: Vec::new(),
            sel: 0,
            last: None,
            busy: None,
            tx,
            rx,
            forget: None,
            error: None,
            agent: Arc::new(Mutex::new(Pending::default())),
            seen: 0,
            typed: String::new(),
            rows: Vec::new(),
        };
        match Connection::system() {
            Ok(c) => {
                // agente para emparejar
                if c.object_server().at("/vato/agent", Agent(b.agent.clone())).is_ok() {
                    let path = ObjectPath::try_from("/vato/agent").unwrap();
                    let _ = c.call_method(Some("org.bluez"), "/org/bluez", Some("org.bluez.AgentManager1"), "RegisterAgent", &(&path, "KeyboardDisplay"));
                    let _ = c.call_method(Some("org.bluez"), "/org/bluez", Some("org.bluez.AgentManager1"), "RequestDefaultAgent", &(&path,));
                }
                b.conn = Some(c);
            }
            Err(e) => b.error = Some(short(&e.to_string())),
        }
        b
    }

    fn load(&mut self) {
        let Some(c) = &self.conn else { return };
        let objs: Objects = match c
            .call_method(Some("org.bluez"), "/", Some("org.freedesktop.DBus.ObjectManager"), "GetManagedObjects", &())
            .and_then(|m| m.body().deserialize::<Objects>().map_err(Into::into))
        {
            Ok(o) => o,
            Err(e) => {
                self.error = Some(short(&e.to_string()));
                return;
            }
        };
        self.error = None;
        let keep = self.devices.get(self.sel).map(|d| d.path.clone());
        let mut devs = Vec::new();
        self.adapter = None;
        for (path, ifaces) in &objs {
            if let Some(a) = ifaces.get("org.bluez.Adapter1") {
                if self.adapter.is_none() {
                    self.adapter = Some(path.to_string());
                    self.powered = get(a, "Powered").unwrap_or(false);
                    self.scanning = get(a, "Discovering").unwrap_or(false);
                }
            }
            if let Some(d) = ifaces.get("org.bluez.Device1") {
                let alias: Option<String> = get::<&str>(d, "Alias").map(String::from);
                let name = get::<&str>(d, "Name").map(String::from).or(alias).unwrap_or_else(|| "?".into());
                devs.push(Device {
                    path: path.to_string(),
                    name,
                    icon: get::<&str>(d, "Icon").unwrap_or("").to_string(),
                    paired: get(d, "Paired").unwrap_or(false),
                    connected: get(d, "Connected").unwrap_or(false),
                    trusted: get(d, "Trusted").unwrap_or(false),
                    rssi: get(d, "RSSI"),
                    battery: ifaces.get("org.bluez.Battery1").and_then(|b| get(b, "Percentage")),
                });
            }
        }
        // emparejados primero (conectados arriba), luego los cercanos por señal
        devs.sort_by(|a, b| {
            (!a.paired, !a.connected, -(a.rssi.unwrap_or(-200) as i32), a.name.to_lowercase())
                .cmp(&(!b.paired, !b.connected, -(b.rssi.unwrap_or(-200) as i32), b.name.to_lowercase()))
        });
        // los cercanos sin señal son ruido (vistos hace rato)
        devs.retain(|d| d.paired || d.rssi.is_some());
        self.devices = devs;
        self.sel = keep
            .and_then(|p| self.devices.iter().position(|d| d.path == p))
            .unwrap_or(self.sel.min(self.devices.len().saturating_sub(1)));
    }

    fn press(&mut self, code: KeyCode, ui: &mut Ui) {
        self.key(KeyEvent::new(code, KeyModifiers::NONE), ui);
    }

    fn asking(&self) -> Option<(String, Ask, u64)> {
        self.agent.lock().unwrap().ask.clone()
    }

    fn answer(&mut self, id: u64, a: Answer) {
        let mut p = self.agent.lock().unwrap();
        if matches!(&p.ask, Some((_, Ask::Show(_), _))) {
            p.ask = None; // solo era aviso
        } else {
            p.answer = Some((id, a));
        }
        self.typed.clear();
    }

    fn dev_name(&self, path: &str) -> String {
        self.devices
            .iter()
            .find(|d| d.path == path)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path).trim_start_matches("dev_").replace('_', ":"))
    }

    /// Algo que tarda: en otro hilo; el resultado llega por el canal.
    fn spawn(&mut self, what: String, job: impl FnOnce(&Connection) -> Result<String, String> + Send + 'static) {
        let Some(c) = self.conn.clone() else { return };
        self.busy = (!what.is_empty()).then_some(what);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job(&c));
        });
    }

    fn enter(&mut self) {
        let Some(d) = self.devices.get(self.sel).cloned() else { return };
        let name = d.name.clone();
        if d.connected {
            self.spawn(format!("{} {name}…", l("Desconectando", "Disconnecting")), move |c| {
                call(c, &d.path, "org.bluez.Device1", "Disconnect").map(|_| format!("{name}: {}", l("desconectado", "disconnected")))
            });
        } else if d.paired {
            self.spawn(format!("{} {name}…", l("Conectando", "Connecting")), move |c| {
                call(c, &d.path, "org.bluez.Device1", "Connect").map(|_| format!("{name}: {}", l("conectado", "connected")))
            });
        } else {
            self.spawn(format!("{} {name}…", l("Emparejando", "Pairing")), move |c| {
                call(c, &d.path, "org.bluez.Device1", "Pair")?;
                let _ = set_prop(c, &d.path, "org.bluez.Device1", "Trusted", true);
                call(c, &d.path, "org.bluez.Device1", "Connect")
                    .map(|_| format!("{name}: {}", l("emparejado y conectado", "paired and connected")))
            });
        }
    }
}

impl Tab for Bluetooth {
    fn name(&self) -> &'static str {
        "bluetooth"
    }

    fn tick(&mut self, ui: &mut Ui) {
        while let Ok(r) = self.rx.try_recv() {
            self.busy = None;
            // ya terminó de emparejar: el código en pantalla ya no hace falta
            let mut p = self.agent.lock().unwrap();
            if matches!(&p.ask, Some((_, Ask::Show(_), _))) {
                p.ask = None;
            }
            drop(p);
            match r {
                Ok(msg) => ui.say(msg),
                Err(e) => ui.warn(e),
            }
            self.last = None;
        }
        if self.last.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
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

        // el adaptador
        let state = match (&self.adapter, self.powered) {
            (None, _) => l("sin adaptador", "no adapter"),
            (_, false) => l("apagado", "off"),
            (_, true) if self.scanning => l("encendido · buscando…", "on · scanning…"),
            _ => l("encendido", "on"),
        };
        let busy = self.busy.clone().map(|b| format!("   {b}")).unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", look.tx("bluetooth")), Style::new().fg(t.muted).bold()),
            Span::styled(look.tx(state), Style::new().fg(if self.powered { t.primary } else { t.muted })),
            Span::styled(busy, Style::new().fg(t.accent)),
        ]));
        if let Some(e) = &self.error {
            lines.push(Line::styled(format!(" {e}"), Style::new().fg(t.red)));
        }
        lines.push(Line::raw(""));
        if let Some((path, ask, _)) = self.asking() {
            let name = self.dev_name(&path);
            let yes_no = format!("   enter {} · esc {}", l("sí", "yes"), l("no", "no"));
            let (q, hint) = match ask {
                Ask::Authorize => (format!("{} {name}?", l("¿Emparejar con", "Pair with")), yes_no),
                Ask::Confirm(n) => (
                    if crate::spanish() { format!("¿{name} muestra el código {n:06}?") } else { format!("Does {name} show the code {n:06}?") },
                    yes_no,
                ),
                Ask::Service(u) => (format!("{name} {} {}. {}", l("quiere usar", "wants to use"), service(&u), l("¿Lo dejas?", "Allow it?")), yes_no),
                Ask::Passkey => (
                    format!("{} {name}: {}▏", l("Escribe el número que muestra", "Type the number shown on"), self.typed),
                    format!("   enter {} · esc {}", l("listo", "done"), l("cancelar", "cancel")),
                ),
                Ask::Pin => (
                    format!("{} {name}: {}▏", l("PIN de", "PIN for"), self.typed),
                    format!("   enter {} · esc {}", l("listo", "done"), l("cancelar", "cancel")),
                ),
                Ask::Show(code) => (
                    format!("{} {code} {} {name} {}", l("Escribe", "Type"), l("en", "on"), l("y dale enter ahí", "and press enter there")),
                    String::new(),
                ),
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {q}"), Style::new().fg(t.accent).bold()),
                Span::styled(hint, Style::new().fg(t.muted)),
            ]));
            lines.push(Line::raw(""));
        }

        let mut sel_line = 0;
        let mut item_lines = Vec::new();
        let mut header = None;
        for (i, d) in self.devices.iter().enumerate() {
            let h = if d.paired { l("emparejados", "paired") } else { l("cerca", "nearby") };
            if header != Some(h) {
                if header.is_some() {
                    lines.push(Line::raw(""));
                }
                lines.push(Line::styled(format!(" {}", look.tx(h)), Style::new().fg(t.muted).bold()));
                header = Some(h);
            }
            let selected = i == self.sel;
            if selected {
                sel_line = lines.len();
            }
            item_lines.push((lines.len(), i));
            let (sel_style, mark) = look.sel(true);
            let row = if selected { sel_style } else { Style::new().fg(t.fg) };
            let status = if d.connected {
                l("conectado", "connected")
            } else if d.paired {
                if d.trusted { l("emparejado", "paired") } else { l("sin confianza", "untrusted") }
            } else {
                ""
            };
            let mut right = String::new();
            if let Some(b) = d.battery {
                right.push_str(&format!("\u{f0079} {b}%  "));
            }
            if !d.paired {
                if let Some(r) = d.rssi {
                    // señal: -90 casi nada, -40 llena
                    let frac = ((r as f64 + 90.0) / 50.0).clamp(0.0, 1.0);
                    let lv = look.levels();
                    right.push_str(&format!("{}  ", lv[(frac * (lv.len() - 1) as f64).round() as usize]));
                }
            }
            let name_w = (inner.width as usize).saturating_sub(32);
            let name: String = d.name.chars().take(name_w).collect();
            let pad = name_w.saturating_sub(name.chars().count());
            lines.push(Line::from(vec![
                Span::styled(if selected { mark } else { " " }, Style::new().fg(t.primary)),
                Span::styled(format!("{} ", icon(&d.icon)), Style::new().fg(if d.connected { t.primary } else { t.muted })),
                Span::styled(format!("{name}{}", " ".repeat(pad)), row),
                Span::styled(format!(" {:<16}", look.tx(status)), Style::new().fg(if d.connected { t.primary } else { t.muted })),
                Span::styled(right, Style::new().fg(t.fg)),
            ]));
        }
        if self.devices.is_empty() && self.adapter.is_some() {
            lines.push(Line::styled(
                format!(" {}", l("Nada por aquí: s para buscar dispositivos", "Nothing here: press s to scan for devices")),
                Style::new().fg(t.muted),
            ));
        }
        if let Some(p) = &self.forget {
            let name = self.devices.iter().find(|d| &d.path == p).map(|d| d.name.clone()).unwrap_or_default();
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!(" {} {name}? (x {} · esc {})", l("¿Olvidar", "Forget"), l("sí", "yes"), l("no", "no")),
                Style::new().fg(t.red).bold(),
            ));
        }
        let h = inner.height as usize;
        let top = sel_line.saturating_sub(h.saturating_sub(2));
        f.render_widget(Paragraph::new(lines).scroll((top as u16, 0)), inner);
        self.rows = item_lines
            .into_iter()
            .filter(|(li, _)| *li >= top && li - top < h)
            .map(|(li, i)| (inner.y + (li - top) as u16, i))
            .collect();
    }

    fn key(&mut self, k: KeyEvent, _ui: &mut Ui) -> bool {
        if let Some((_, ask, id)) = self.asking() {
            let text = matches!(ask, Ask::Passkey | Ask::Pin);
            match k.code {
                KeyCode::Esc => self.answer(id, Answer::No),
                KeyCode::Enter if text => {
                    let t = std::mem::take(&mut self.typed);
                    self.answer(id, Answer::Text(t));
                }
                KeyCode::Enter | KeyCode::Char('y' | 's') if !text => self.answer(id, Answer::Yes),
                KeyCode::Char('n') if !text => self.answer(id, Answer::No),
                KeyCode::Backspace if text => {
                    self.typed.pop();
                }
                KeyCode::Char(c) if ask == Ask::Passkey && c.is_ascii_digit() && self.typed.len() < 6 => self.typed.push(c),
                KeyCode::Char(c) if ask == Ask::Pin && self.typed.len() < 16 => self.typed.push(c),
                KeyCode::Tab | KeyCode::BackTab if !text => return false,
                KeyCode::Char('q') if !text => return false,
                _ => {}
            }
            return true;
        }
        if let Some(p) = self.forget.take() {
            if k.code == KeyCode::Char('x') {
                if let Some(a) = self.adapter.clone() {
                    self.spawn(l("Olvidando…", "Forgetting…").to_string(), move |c| {
                        let path = ObjectPath::try_from(p.as_str()).map_err(|e| e.to_string())?;
                        c.call_method(Some("org.bluez"), a.as_str(), Some("org.bluez.Adapter1"), "RemoveDevice", &(&path,))
                            .map(|_| l("Olvidado", "Forgotten").to_string())
                            .map_err(|e| short(&e.to_string()))
                    });
                }
            }
            return true;
        }
        let Some(adapter) = self.adapter.clone() else {
            return false;
        };
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => self.sel = (self.sel + 1).min(self.devices.len().saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char(' ') => self.enter(),
            KeyCode::Char('s') => {
                let on = !self.scanning;
                self.spawn(String::new(), move |c| {
                    call(c, &adapter, "org.bluez.Adapter1", if on { "StartDiscovery" } else { "StopDiscovery" })
                        .map(|_| if on { l("Buscando dispositivos…", "Scanning for devices…") } else { l("Búsqueda detenida", "Scan stopped") }.to_string())
                });
            }
            KeyCode::Char('p') => {
                let on = !self.powered;
                self.spawn(String::new(), move |c| {
                    set_prop(c, &adapter, "org.bluez.Adapter1", "Powered", on)
                        .map(|_| if on { l("Bluetooth encendido", "Bluetooth on") } else { l("Bluetooth apagado", "Bluetooth off") }.to_string())
                });
            }
            KeyCode::Char('t') => {
                if let Some(d) = self.devices.get(self.sel).cloned() {
                    let on = !d.trusted;
                    self.spawn(String::new(), move |c| {
                        set_prop(c, &d.path, "org.bluez.Device1", "Trusted", on).map(|_| {
                            format!("{}: {}", d.name, if on { l("de confianza", "trusted") } else { l("sin confianza", "untrusted") })
                        })
                    });
                }
            }
            KeyCode::Char('x') => {
                if let Some(d) = self.devices.get(self.sel) {
                    self.forget = Some(d.path.clone());
                }
            }
            _ => return false,
        }
        true
    }

    fn typing(&self) -> bool {
        matches!(self.asking(), Some((_, Ask::Passkey | Ask::Pin, _)))
    }

    fn wants_focus(&mut self) -> bool {
        let seq = self.agent.lock().unwrap().ask.as_ref().map_or(0, |x| x.2);
        let new = seq > self.seen;
        self.seen = self.seen.max(seq);
        new
    }

    fn scroll(&mut self, _x: u16, _y: u16, down: bool, _ui: &mut Ui) {
        if self.asking().is_none() && self.forget.is_none() {
            self.sel = if down { (self.sel + 1).min(self.devices.len().saturating_sub(1)) } else { self.sel.saturating_sub(1) };
        }
    }

    fn click(&mut self, _x: u16, y: u16, b: MouseButton, double: bool, ui: &mut Ui) {
        if self.asking().is_some() || self.forget.is_some() {
            return;
        }
        let Some(i) = self.rows.iter().find(|r| r.0 == y).map(|r| r.1) else { return };
        self.sel = i;
        match b {
            MouseButton::Left if double => self.press(KeyCode::Enter, ui),
            MouseButton::Right => self.press(KeyCode::Char('t'), ui),
            _ => {}
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.forget.is_some() {
            return vec![("x", l("olvidar", "forget")), ("esc", l("no", "no"))];
        }
        if let Some((_, ask, _)) = self.asking() {
            return match ask {
                Ask::Show(_) => vec![("esc", l("ocultar", "hide"))],
                Ask::Passkey | Ask::Pin => vec![("enter", l("listo", "done")), ("esc", l("cancelar", "cancel"))],
                _ => vec![("enter", l("sí", "yes")), ("esc", l("no", "no"))],
            };
        }
        let enter = match self.devices.get(self.sel) {
            Some(d) if d.connected => l("desconectar", "disconnect"),
            Some(d) if d.paired => l("conectar", "connect"),
            Some(_) => l("emparejar", "pair"),
            None => "",
        };
        let mut v = vec![("j/k", l("mover", "move"))];
        if !enter.is_empty() {
            v.push(("enter", enter));
        }
        v.extend([
            ("s", if self.scanning { l("dejar de buscar", "stop scan") } else { l("buscar", "scan") }),
            ("p", if self.powered { l("apagar", "power off") } else { l("encender", "power on") }),
            ("t", l("confianza", "trust")),
            ("x", l("olvidar", "forget")),
        ]);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// El agente en el bus de sesión: la pregunta llega a `Pending`, la
    /// respuesta vuelve a quien llamó, y mientras espera la conexión sigue
    /// sirviendo (si se trabara, la pestaña se congelaría al emparejar).
    #[test]
    #[ignore = "necesita el bus de sesión"]
    fn agent_waits_for_answer() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let server = Connection::session().unwrap();
        server.object_server().at("/vato/agent", Agent(pending.clone())).unwrap();
        let name = server.unique_name().unwrap().to_string();
        let client = Connection::session().unwrap();
        let dev = ObjectPath::try_from("/org/bluez/hci0/dev_AA_BB").unwrap();

        let ask = |code: u32| {
            let (c, n, d) = (client.clone(), name.clone(), dev.clone());
            std::thread::spawn(move || {
                c.call_method(Some(n.as_str()), "/vato/agent", Some("org.bluez.Agent1"), "RequestConfirmation", &(&d, code))
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
        };
        let wait_ask = || loop {
            if let Some(a) = pending.lock().unwrap().ask.clone() {
                break a;
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        // sí
        let h = ask(123456);
        let (path, what, id) = wait_ask();
        assert_eq!(path, "/org/bluez/hci0/dev_AA_BB");
        assert!(what == Ask::Confirm(123456));
        server.call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "GetId", &()).unwrap();
        pending.lock().unwrap().answer = Some((id, Answer::Yes));
        assert_eq!(h.join().unwrap(), Ok(()));
        assert!(pending.lock().unwrap().ask.is_none());

        // no
        let h = ask(1);
        let (_, _, id) = wait_ask();
        pending.lock().unwrap().answer = Some((id, Answer::No));
        assert!(h.join().unwrap().unwrap_err().contains("org.bluez.Error.Rejected"));

        // Cancel de BlueZ
        let h = ask(2);
        wait_ask();
        client.call_method(Some(name.as_str()), "/vato/agent", Some("org.bluez.Agent1"), "Cancel", &()).unwrap();
        assert!(h.join().unwrap().unwrap_err().contains("org.bluez.Error.Canceled"));
    }
}
