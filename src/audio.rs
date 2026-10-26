//! Audio: salidas, entradas, el volumen de cada app y las que están usando
//! un micrófono (PipeWire vía pactl). `p` abre el puerto y el perfil de la
//! tarjeta (bocinas o audífonos, HDMI 1 o 2, bluetooth en calidad alta o con
//! micrófono).
//! `pactl subscribe` avisa de cualquier cambio y la lista se relee sola.

use crate::{Tab, Ui, l};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton};
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;
use serde_json::Value;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX: u32 = 150;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Out,
    In,
    App,
    Rec, // una app grabando de una entrada
}

#[derive(Clone)]
struct Node {
    kind: Kind,
    index: u32,
    name: String,
    label: String,
    detail: String,
    vol: u32,
    mute: bool,
    default: bool,
    card: String, // la tarjeta (device.name) de una salida o entrada
    ports: Vec<(String, String, bool)>, // nombre, descripción, conectado
    port: String,
}

struct Card {
    name: String,
    label: String,
    active: String,
    profiles: Vec<(String, String, bool, u64)>, // nombre, descripción, disponible, prioridad
}

#[derive(Clone)]
enum Pick {
    Head(String),
    Port { sink: bool, node: String, port: String, label: String, active: bool, ok: bool },
    Profile { card: String, profile: String, label: String, active: bool, ok: bool },
}

pub struct Audio {
    nodes: Vec<Node>,
    sel: usize,
    dirty: Arc<AtomicBool>,
    last: Instant,
    want_inputs: bool,
    cards: Vec<Card>,
    /// el menú de puertos y perfiles abierto: opciones y cuál está elegida
    pick: Option<(Vec<Pick>, usize)>,
    // dónde quedó cada cosa al dibujar (para el ratón)
    rows: Vec<(u16, usize)>, // fila de pantalla → nodo
    bar: (u16, u16),         // x y ancho de las barras de volumen
    pop: Rect,
    pop_rows: Vec<(u16, usize)>, // fila de pantalla → opción del menú
}

fn ports(s: &Value) -> (Vec<(String, String, bool)>, String) {
    let ports = s["ports"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            (
                p["name"].as_str().unwrap_or("").to_string(),
                p["description"].as_str().unwrap_or("").to_string(),
                p["availability"].as_str() != Some("not available"),
            )
        })
        .collect();
    (ports, s["active_port"].as_str().unwrap_or("").to_string())
}

fn card_of(s: &Value) -> String {
    s["properties"]["device.name"].as_str().unwrap_or("").to_string()
}

fn pactl(args: &[&str]) -> Option<Value> {
    let out = Command::new("pactl").args(["-f", "json"]).args(args).stderr(Stdio::null()).output().ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

fn text(args: &[&str]) -> String {
    Command::new("pactl")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn run(args: &[&str]) {
    let _ = Command::new("pactl").args(args).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// promedio de los canales ("53%" → 53)
fn volume(v: &Value) -> u32 {
    let vals: Vec<u32> = v["volume"]
        .as_object()
        .map(|m| {
            m.values()
                .filter_map(|c| c["value_percent"].as_str()?.trim_end_matches('%').parse().ok())
                .collect()
        })
        .unwrap_or_default();
    if vals.is_empty() { 0 } else { vals.iter().sum::<u32>() / vals.len() as u32 }
}

impl Audio {
    pub fn new() -> Audio {
        let dirty = Arc::new(AtomicBool::new(true));
        // cualquier cambio en el servidor de audio: releer
        // (si PipeWire se reinicia, pactl subscribe se muere: volver a empezar)
        let d = dirty.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            loop {
                if let Ok(mut child) = Command::new("pactl").arg("subscribe").stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
                    if let Some(out) = child.stdout.take() {
                        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                            if !line.contains("client") {
                                d.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                    let _ = child.wait();
                }
                d.store(true, Ordering::Relaxed);
                std::thread::sleep(Duration::from_secs(2));
            }
        });
        Audio { nodes: Vec::new(), sel: 0, dirty, last: Instant::now(), want_inputs: false, cards: Vec::new(),
            pick: None,
            rows: Vec::new(),
            bar: (0, 0),
            pop: Rect::default(),
            pop_rows: Vec::new(),
        }
    }

    /// `vato mic`: empezar en las entradas
    pub fn focus_inputs(&mut self) {
        self.want_inputs = true;
    }

    fn load(&mut self) {
        let keep = self.nodes.get(self.sel).map(|n| (n.kind, n.index));
        let def_out = text(&["get-default-sink"]);
        let def_in = text(&["get-default-source"]);
        let mut nodes = Vec::new();
        let sinks = pactl(&["list", "sinks"]).unwrap_or(Value::Null);
        let sink_name = |idx: u64| -> String {
            sinks.as_array()
                .and_then(|a| a.iter().find(|s| s["index"].as_u64() == Some(idx)))
                .and_then(|s| s["description"].as_str())
                .unwrap_or("")
                .to_string()
        };
        for s in sinks.as_array().into_iter().flatten() {
            let name = s["name"].as_str().unwrap_or("").to_string();
            let (ports, port) = ports(s);
            nodes.push(Node {
                card: card_of(s),
                ports,
                port,
                kind: Kind::Out,
                index: s["index"].as_u64().unwrap_or(0) as u32,
                default: name == def_out,
                label: s["description"].as_str().unwrap_or(&name).to_string(),
                detail: String::new(),
                vol: volume(s),
                mute: s["mute"].as_bool().unwrap_or(false),
                name,
            });
        }
        for s in pactl(&["list", "sources"]).unwrap_or(Value::Null).as_array().into_iter().flatten() {
            let name = s["name"].as_str().unwrap_or("").to_string();
            // los .monitor son la salida vista como entrada: no sirven aquí
            if name.ends_with(".monitor") || !s["monitor_of_sink"].is_null() && s["monitor_of_sink"] != "n/a" {
                continue;
            }
            let (ports, port) = ports(s);
            nodes.push(Node {
                card: card_of(s),
                ports,
                port,
                kind: Kind::In,
                index: s["index"].as_u64().unwrap_or(0) as u32,
                default: name == def_in,
                label: s["description"].as_str().unwrap_or(&name).to_string(),
                detail: String::new(),
                vol: volume(s),
                mute: s["mute"].as_bool().unwrap_or(false),
                name,
            });
        }
        for s in pactl(&["list", "sink-inputs"]).unwrap_or(Value::Null).as_array().into_iter().flatten() {
            let p = &s["properties"];
            let app = p["application.name"].as_str().or(p["application.process.binary"].as_str()).unwrap_or("?");
            let media = p["media.name"].as_str().unwrap_or("");
            nodes.push(Node {
                card: String::new(),
                ports: Vec::new(),
                port: String::new(),
                kind: Kind::App,
                index: s["index"].as_u64().unwrap_or(0) as u32,
                name: String::new(),
                label: if media.is_empty() || media == app { app.to_string() } else { format!("{app} · {media}") },
                detail: sink_name(s["sink"].as_u64().unwrap_or(u64::MAX)),
                vol: volume(s),
                mute: s["mute"].as_bool().unwrap_or(false),
                default: false,
            });
        }
        // apps grabando de una entrada de verdad (no de un .monitor ni medidores)
        for s in pactl(&["list", "source-outputs"]).unwrap_or(Value::Null).as_array().into_iter().flatten() {
            let p = &s["properties"];
            let src = s["source"].as_u64().unwrap_or(u64::MAX) as u32;
            let Some(input) = nodes.iter().find(|n| n.kind == Kind::In && n.index == src) else { continue };
            if p["stream.monitor"].as_str() == Some("true") {
                continue;
            }
            let app = p["application.name"].as_str().or(p["application.process.binary"].as_str()).unwrap_or("?");
            nodes.push(Node {
                card: String::new(),
                ports: Vec::new(),
                port: String::new(),
                kind: Kind::Rec,
                index: s["index"].as_u64().unwrap_or(0) as u32,
                name: String::new(),
                label: app.to_string(),
                detail: input.label.clone(),
                vol: volume(s),
                mute: s["mute"].as_bool().unwrap_or(false),
                default: false,
            });
        }
        self.cards = pactl(&["list", "cards"])
            .unwrap_or(Value::Null)
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| {
                let mut profiles: Vec<(String, String, bool, u64)> = c["profiles"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            v["description"].as_str().unwrap_or(k).to_string(),
                            v["available"].as_bool().unwrap_or(true),
                            v["priority"].as_u64().unwrap_or(0),
                        )
                    })
                    .collect();
                profiles.sort_by_key(|p| std::cmp::Reverse(p.3));
                Card {
                    name: c["name"].as_str().unwrap_or("").to_string(),
                    label: c["properties"]["device.description"].as_str().unwrap_or("").to_string(),
                    active: c["active_profile"].as_str().unwrap_or("").to_string(),
                    profiles,
                }
            })
            .collect();
        let first = self.nodes.is_empty();
        self.nodes = nodes;
        // al abrir: en la salida predeterminada (o en la entrada con `vato mic`)
        if first || self.want_inputs {
            let kind = if self.want_inputs { Kind::In } else { Kind::Out };
            self.want_inputs = false;
            if let Some(i) = self.nodes.iter().position(|n| n.kind == kind && n.default) {
                self.sel = i;
                return;
            }
        }
        self.sel = keep
            .and_then(|k| self.nodes.iter().position(|n| (n.kind, n.index) == k))
            .unwrap_or(self.sel.min(self.nodes.len().saturating_sub(1)));
    }

    /// `v` como "70%" (todos los canales igual) o "+5%" (cada canal, sin perder el balance)
    fn set_volume(&self, n: &Node, v: &str) {
        match n.kind {
            Kind::Out => run(&["set-sink-volume", &n.name, v]),
            Kind::In => run(&["set-source-volume", &n.name, v]),
            Kind::App => run(&["set-sink-input-volume", &n.index.to_string(), v]),
            Kind::Rec => run(&["set-source-output-volume", &n.index.to_string(), v]),
        }
    }

    fn act(&mut self, ui: &mut Ui) {
        let Some(n) = self.nodes.get(self.sel).cloned() else { return };
        match n.kind {
            Kind::Out => {
                run(&["set-default-sink", &n.name]);
                // y lo que ya sonaba, que se pase también
                for a in self.nodes.iter().filter(|a| a.kind == Kind::App) {
                    run(&["move-sink-input", &a.index.to_string(), &n.name]);
                }
                ui.say(format!("{}: {}", l("Salida", "Output"), n.label));
            }
            Kind::In => {
                run(&["set-default-source", &n.name]);
                ui.say(format!("{}: {}", l("Entrada", "Input"), n.label));
            }
            Kind::App => {
                // a la siguiente salida
                let outs: Vec<&Node> = self.nodes.iter().filter(|o| o.kind == Kind::Out).collect();
                if outs.is_empty() {
                    return;
                }
                let i = outs.iter().position(|o| o.label == n.detail).map(|i| (i + 1) % outs.len()).unwrap_or(0);
                run(&["move-sink-input", &n.index.to_string(), &outs[i].name]);
                ui.say(format!("{} → {}", n.label, outs[i].label));
            }
            Kind::Rec => {
                // a la siguiente entrada
                let ins: Vec<&Node> = self.nodes.iter().filter(|o| o.kind == Kind::In).collect();
                if ins.is_empty() {
                    return;
                }
                let i = ins.iter().position(|o| o.label == n.detail).map(|i| (i + 1) % ins.len()).unwrap_or(0);
                run(&["move-source-output", &n.index.to_string(), &ins[i].name]);
                ui.say(format!("{} ← {}", n.label, ins[i].label));
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Opciones para la salida o entrada elegida: sus puertos (si hay más de
    /// uno) y los perfiles de su tarjeta. "Apagada" no: la tarjeta
    /// desaparecería de la lista y no habría cómo volver a prenderla aquí.
    fn options(&self) -> Vec<Pick> {
        let Some(n) = self.nodes.get(self.sel) else { return Vec::new() };
        if !matches!(n.kind, Kind::Out | Kind::In) {
            return Vec::new();
        }
        let mut v = Vec::new();
        if n.ports.len() > 1 {
            v.push(Pick::Head(l("puerto", "port").to_string()));
            for (name, desc, ok) in &n.ports {
                v.push(Pick::Port {
                    sink: n.kind == Kind::Out,
                    node: n.name.clone(),
                    port: name.clone(),
                    label: desc.clone(),
                    active: *name == n.port,
                    ok: *ok,
                });
            }
        }
        if let Some(c) = self.cards.iter().find(|c| c.name == n.card) {
            let profiles: Vec<_> = c.profiles.iter().filter(|p| p.0 != "off" && (p.2 || p.0 == c.active)).collect();
            if profiles.len() > 1 {
                v.push(Pick::Head(format!("{} · {}", l("perfil", "profile"), c.label)));
                for (name, desc, ok, _) in profiles {
                    v.push(Pick::Profile {
                        card: c.name.clone(),
                        profile: name.clone(),
                        label: desc.clone(),
                        active: *name == c.active,
                        ok: *ok,
                    });
                }
            }
        }
        v
    }

    fn open_pick(&mut self, ui: &mut Ui) {
        let opts = self.options();
        if opts.is_empty() {
            ui.say(l("Esta no tiene otros puertos ni perfiles", "No other ports or profiles here"));
            return;
        }
        let at = opts
            .iter()
            .position(|p| matches!(p, Pick::Port { active: true, .. } | Pick::Profile { active: true, .. }))
            .or_else(|| opts.iter().position(|p| !matches!(p, Pick::Head(_))))
            .unwrap_or(0);
        self.pick = Some((opts, at));
    }

    fn apply_pick(&mut self, p: Pick, ui: &mut Ui) {
        match p {
            Pick::Port { sink, node, port, label, .. } => {
                run(&[if sink { "set-sink-port" } else { "set-source-port" }, &node, &port]);
                ui.say(format!("{}: {label}", l("Puerto", "Port")));
            }
            Pick::Profile { card, profile, label, .. } => {
                run(&["set-card-profile", &card, &profile]);
                ui.say(format!("{}: {label}", l("Perfil", "Profile")));
            }
            Pick::Head(_) => return,
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn press(&mut self, code: KeyCode, ui: &mut Ui) {
        self.key(KeyEvent::new(code, KeyModifiers::NONE), ui);
    }

    fn row_at(&self, y: u16) -> Option<usize> {
        self.rows.iter().find(|r| r.0 == y).map(|r| r.1)
    }

    /// clic o arrastre sobre la barra: el volumen a esa altura
    fn volume_at(&mut self, x: u16) {
        let (x0, w) = self.bar;
        if w == 0 || x < x0 {
            return;
        }
        let frac = ((x - x0) as f64 + 0.5) / w as f64;
        let v = ((frac * 100.0).round() as u32).min(100);
        if let Some(n) = self.nodes.get(self.sel).cloned() {
            self.set_volume(&n, &format!("{v}%"));
            self.nodes[self.sel].vol = v;
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    fn nudge(&mut self, d: i32) {
        if let Some(n) = self.nodes.get(self.sel).cloned() {
            let v = (n.vol as i32 + d).clamp(0, MAX as i32) as u32;
            if v as i32 == n.vol as i32 + d {
                self.set_volume(&n, &format!("{d:+}%"));
            } else {
                self.set_volume(&n, &format!("{v}%"));
            }
            self.nodes[self.sel].vol = v;
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    fn mute(&mut self) {
        if let Some(n) = self.nodes.get(self.sel).cloned() {
            match n.kind {
                Kind::Out => run(&["set-sink-mute", &n.name, "toggle"]),
                Kind::In => run(&["set-source-mute", &n.name, "toggle"]),
                Kind::App => run(&["set-sink-input-mute", &n.index.to_string(), "toggle"]),
                Kind::Rec => run(&["set-source-output-mute", &n.index.to_string(), "toggle"]),
            }
            self.dirty.store(true, Ordering::Relaxed);
        }
    }
}

impl Tab for Audio {
    fn name(&self) -> &'static str {
        "audio"
    }

    fn tick(&mut self, _ui: &mut Ui) {
        if self.dirty.swap(false, Ordering::Relaxed) || self.last.elapsed() > Duration::from_secs(5) {
            self.load();
            self.last = Instant::now();
        }
    }

    fn draw(&mut self, f: &mut Frame, area: Rect, ui: &Ui) {
        let look = &ui.look;
        let t = look.theme;
        let block = look.block(true);
        let inner = block.inner(area);
        f.render_widget(block, area);

        let bar_w = (inner.width as usize / 4).clamp(10, 30);
        let mut lines: Vec<Line> = Vec::new();
        let mut sel_line = 0;
        let mut node_lines = Vec::new();
        let mut last_kind = None;
        for (i, n) in self.nodes.iter().enumerate() {
            if last_kind != Some(n.kind) {
                if last_kind.is_some() {
                    lines.push(Line::raw(""));
                }
                let title = match n.kind {
                    Kind::Out => l("salidas", "outputs"),
                    Kind::In => l("entradas", "inputs"),
                    Kind::App => l("apps", "apps"),
                    Kind::Rec => l("usando el micrófono", "using the mic"),
                };
                lines.push(Line::styled(format!(" {}", look.tx(title)), Style::new().fg(t.muted).bold()));
                last_kind = Some(n.kind);
            }
            let selected = i == self.sel;
            let (sel_style, mark) = look.sel(true);
            let row_style = if selected { sel_style } else { Style::new().fg(t.fg) };
            let dot = if n.default { Span::styled("● ", Style::new().fg(t.primary)) } else { Span::raw("  ") };
            let vol_frac = n.vol as f64 / 100.0;
            let (done, rest) = look.bar(vol_frac.min(1.0), bar_w);
            let bar_color = if n.mute { t.muted } else if n.vol > 100 { t.accent } else { t.primary };
            let label_w = (inner.width as usize).saturating_sub(bar_w + 16);
            let mut label: String = n.label.chars().take(label_w).collect();
            if !n.detail.is_empty() && label.chars().count() + 4 < label_w {
                let room = label_w - label.chars().count() - 3;
                let det: String = n.detail.chars().take(room).collect();
                label = format!("{label}  {det}");
            }
            let pad = label_w.saturating_sub(label.chars().count());
            let pct = if n.mute { look.tx(l("mudo", "mute")) } else { format!("{}%", n.vol) };
            if selected {
                sel_line = lines.len();
            }
            node_lines.push((lines.len(), i));
            lines.push(Line::from(vec![
                Span::styled(if selected { mark } else { " " }, Style::new().fg(t.primary)),
                dot,
                Span::styled(format!("{label}{}", " ".repeat(pad)), row_style),
                Span::raw(" "),
                Span::styled(done, Style::new().fg(bar_color)),
                Span::styled(rest, Style::new().fg(t.border)),
                Span::styled(format!(" {pct:>5}"), Style::new().fg(if n.mute { t.muted } else { t.fg })),
            ]));
        }
        if self.nodes.is_empty() {
            lines.push(Line::styled(format!(" {}", l("No hay audio (¿corre PipeWire?)", "No audio (is PipeWire running?)")), Style::new().fg(t.muted)));
        }
        let h = inner.height as usize;
        let top = sel_line.saturating_sub(h.saturating_sub(2));
        f.render_widget(Paragraph::new(lines).scroll((top as u16, 0)), inner);
        self.rows = node_lines
            .into_iter()
            .filter(|(li, _)| *li >= top && li - top < h)
            .map(|(li, ni)| (inner.y + (li - top) as u16, ni))
            .collect();
        let label_w = (inner.width as usize).saturating_sub(bar_w + 16) as u16;
        self.bar = (inner.x + 4 + label_w, bar_w as u16);
        self.pop = Rect::default();
        self.pop_rows.clear();

        // el menú de puertos y perfiles, encima
        if let Some((opts, at)) = &self.pick {
            let w = (opts
                .iter()
                .map(|p| match p {
                    Pick::Head(s) => s.chars().count(),
                    Pick::Port { label, .. } | Pick::Profile { label, .. } => label.chars().count() + 4,
                })
                .max()
                .unwrap_or(20)
                + 6)
            .clamp(30, area.width.saturating_sub(4) as usize) as u16;
            let hgt = (opts.len() as u16 + 2).min(area.height.saturating_sub(2));
            let r = Rect { x: area.x + (area.width.saturating_sub(w)) / 2, y: area.y + (area.height.saturating_sub(hgt)) / 2, width: w, height: hgt };
            let block = look.block(true);
            let inner = block.inner(r);
            f.render_widget(Clear, r);
            f.render_widget(block.style(Style::new().bg(t.bg)), r);
            let (sel_style, mark) = look.sel(true);
            let lines: Vec<Line> = opts
                .iter()
                .enumerate()
                .map(|(i, p)| match p {
                    Pick::Head(s) => Line::styled(format!(" {}", look.tx(s)), Style::new().fg(t.muted).bold()),
                    Pick::Port { label, active, ok, .. } | Pick::Profile { label, active, ok, .. } => {
                        let st = if i == *at {
                            sel_style
                        } else {
                            Style::new().fg(if *ok { t.fg } else { t.muted })
                        };
                        let pad = (inner.width as usize).saturating_sub(label.chars().count() + 3);
                        Line::from(vec![
                            Span::styled(if i == *at { mark } else { " " }, Style::new().fg(t.primary)),
                            Span::styled(if *active { "● " } else { "  " }, Style::new().fg(t.primary)),
                            Span::styled(format!("{label}{}", " ".repeat(pad)), st),
                        ])
                    }
                })
                .collect();
            let top = at.saturating_sub(inner.height.saturating_sub(1) as usize);
            f.render_widget(Paragraph::new(lines).scroll((top as u16, 0)), inner);
            self.pop = r;
            self.pop_rows = (top..opts.len())
                .take(inner.height as usize)
                .enumerate()
                .map(|(k, i)| (inner.y + k as u16, i))
                .collect();
        }
    }

    fn key(&mut self, k: KeyEvent, ui: &mut Ui) -> bool {
        if let Some((opts, mut at)) = self.pick.take() {
            let step = |at: usize, down: bool| -> usize {
                let mut i = at;
                loop {
                    let next = if down { i + 1 } else { i.wrapping_sub(1) };
                    match opts.get(next) {
                        Some(Pick::Head(_)) => i = next,
                        Some(_) => return next,
                        None => return at,
                    }
                }
            };
            match k.code {
                KeyCode::Down | KeyCode::Char('j') => at = step(at, true),
                KeyCode::Up | KeyCode::Char('k') => at = step(at, false),
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if let Some(p) = opts.get(at).cloned() {
                        self.apply_pick(p, ui);
                    }
                    return true;
                }
                KeyCode::Esc | KeyCode::Char('p' | 'q') => return true,
                _ => {}
            }
            self.pick = Some((opts, at));
            return true;
        }
        match k.code {
            KeyCode::Char('p') => self.open_pick(ui),
            KeyCode::Down | KeyCode::Char('j') => self.sel = (self.sel + 1).min(self.nodes.len().saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') => self.nudge(5),
            KeyCode::Left | KeyCode::Char('h') => self.nudge(-5),
            KeyCode::Char('L') => self.nudge(1),
            KeyCode::Char('H') => self.nudge(-1),
            KeyCode::Char('m') => self.mute(),
            KeyCode::Enter | KeyCode::Char(' ') => self.act(ui),
            KeyCode::Char('0') => {
                if let Some(n) = self.nodes.get(self.sel).cloned() {
                    self.set_volume(&n, "100%");
                    self.dirty.store(true, Ordering::Relaxed);
                }
            }
            _ => return false,
        }
        true
    }

    fn scroll(&mut self, _x: u16, y: u16, down: bool, ui: &mut Ui) {
        if self.pick.is_some() {
            return self.press(if down { KeyCode::Down } else { KeyCode::Up }, ui);
        }
        // la rueda sobre una fila le mueve el volumen a esa
        if let Some(i) = self.row_at(y) {
            self.sel = i;
        }
        self.nudge(if down { -5 } else { 5 });
    }

    fn click(&mut self, x: u16, y: u16, b: MouseButton, double: bool, ui: &mut Ui) {
        if let Some((opts, _)) = &self.pick {
            // fuera del menú lo cierra; en una opción, la usa
            if !self.pop.contains(Position { x, y }) {
                self.pick = None;
                return;
            }
            if let Some(i) = self.pop_rows.iter().find(|r| r.0 == y).map(|r| r.1) {
                if !matches!(opts.get(i), Some(Pick::Head(_))) {
                    self.pick.as_mut().unwrap().1 = i;
                    self.press(KeyCode::Enter, ui);
                }
            }
            return;
        }
        let Some(i) = self.row_at(y) else { return };
        self.sel = i;
        let on_bar = x >= self.bar.0 && x < self.bar.0 + self.bar.1;
        match b {
            MouseButton::Left if on_bar => self.volume_at(x),
            MouseButton::Left if double => self.press(KeyCode::Enter, ui),
            MouseButton::Right => self.mute(),
            MouseButton::Middle => self.open_pick(ui),
            _ => {}
        }
    }

    fn drag(&mut self, x: u16, _y: u16, _ui: &mut Ui) {
        if self.pick.is_none() {
            self.volume_at(x.clamp(self.bar.0, self.bar.0 + self.bar.1.saturating_sub(1)));
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        let enter = match self.nodes.get(self.sel).map(|n| n.kind) {
            Some(Kind::App) => l("cambiar salida", "move output"),
            Some(Kind::Rec) => l("cambiar entrada", "move input"),
            _ => l("predeterminada", "make default"),
        };
        if self.pick.is_some() {
            return vec![("j/k", l("mover", "move")), ("enter", l("usar", "use")), ("esc", l("cerrar", "close"))];
        }
        let mut v = vec![("j/k", l("mover", "move")), ("h/l", l("volumen", "volume")), ("m", l("mudo", "mute")), ("enter", enter), ("0", "100%")];
        if matches!(self.nodes.get(self.sel).map(|n| n.kind), Some(Kind::Out | Kind::In)) {
            v.push(("p", l("puerto/perfil", "port/profile")));
        }
        v
    }
}
