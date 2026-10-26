//! Audio: salidas, entradas y el volumen de cada app (PipeWire vía pactl).
//! `pactl subscribe` avisa de cualquier cambio y la lista se relee sola.

use crate::{Tab, Ui, l};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
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
}

pub struct Audio {
    nodes: Vec<Node>,
    sel: usize,
    dirty: Arc<AtomicBool>,
    last: Instant,
    want_inputs: bool,
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
        let d = dirty.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            if let Ok(mut child) = Command::new("pactl").arg("subscribe").stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
                if let Some(out) = child.stdout.take() {
                    for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                        if !line.contains("client") {
                            d.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });
        Audio { nodes: Vec::new(), sel: 0, dirty, last: Instant::now(), want_inputs: false }
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
            nodes.push(Node {
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
            nodes.push(Node {
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

    fn set_volume(&self, n: &Node, v: u32) {
        let v = format!("{}%", v.min(MAX));
        match n.kind {
            Kind::Out => run(&["set-sink-volume", &n.name, &v]),
            Kind::In => run(&["set-source-volume", &n.name, &v]),
            Kind::App => run(&["set-sink-input-volume", &n.index.to_string(), &v]),
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
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn nudge(&mut self, d: i32) {
        if let Some(n) = self.nodes.get(self.sel).cloned() {
            let v = (n.vol as i32 + d).clamp(0, MAX as i32) as u32;
            self.set_volume(&n, v);
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
    }

    fn key(&mut self, k: KeyEvent, ui: &mut Ui) -> bool {
        match k.code {
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
                    self.set_volume(&n, 100);
                    self.dirty.store(true, Ordering::Relaxed);
                }
            }
            _ => return false,
        }
        true
    }

    fn scroll(&mut self, down: bool, _ui: &mut Ui) {
        self.nudge(if down { -5 } else { 5 });
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        let enter = match self.nodes.get(self.sel).map(|n| n.kind) {
            Some(Kind::App) => l("cambiar salida", "move output"),
            _ => l("predeterminada", "make default"),
        };
        vec![("j/k", l("mover", "move")), ("h/l", l("volumen", "volume")), ("m", l("mudo", "mute")), ("enter", enter), ("0", "100%")]
    }
}
