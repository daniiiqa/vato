//! Bluetooth: dispositivos emparejados y cercanos (BlueZ por D-Bus). vato se
//! registra como agente "NoInputNoOutput", así emparejar audífonos, controles
//! y teclados que solo piden confirmar funciona sin otra app. Conectar y
//! emparejar tardan: van en otro hilo y avisan al terminar.

use crate::{Tab, Ui, l};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
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

struct Agent;

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}
    fn request_pin_code(&self, _device: OwnedObjectPath) -> String {
        "0000".into()
    }
    fn display_pin_code(&self, _device: OwnedObjectPath, _pincode: String) {}
    fn request_passkey(&self, _device: OwnedObjectPath) -> u32 {
        0
    }
    fn display_passkey(&self, _device: OwnedObjectPath, _passkey: u32, _entered: u16) {}
    fn request_confirmation(&self, _device: OwnedObjectPath, _passkey: u32) {}
    fn request_authorization(&self, _device: OwnedObjectPath) {}
    fn authorize_service(&self, _device: OwnedObjectPath, _uuid: String) {}
    fn cancel(&self) {}
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
        };
        match Connection::system() {
            Ok(c) => {
                // agente para emparejar
                if c.object_server().at("/vato/agent", Agent).is_ok() {
                    let path = ObjectPath::try_from("/vato/agent").unwrap();
                    let _ = c.call_method(Some("org.bluez"), "/org/bluez", Some("org.bluez.AgentManager1"), "RegisterAgent", &(&path, "NoInputNoOutput"));
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

        let mut sel_line = 0;
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
    }

    fn key(&mut self, k: KeyEvent, _ui: &mut Ui) -> bool {
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

    fn scroll(&mut self, down: bool, _ui: &mut Ui) {
        self.sel = if down { (self.sel + 1).min(self.devices.len().saturating_sub(1)) } else { self.sel.saturating_sub(1) };
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
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
