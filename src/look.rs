//! El look, igual que rola: los colores salen del theme de kitty que tenga
//! puesto el look de Hyprland, y el estilo (bordes, barras, mayúsculas, cómo se
//! marca la fila elegida) de `~/.config/vato/look.json` o, si no hay, del
//! `look.json` de rola, que ya cambia con cada look. Todo se recarga solo.

use ratatui::style::{Color, Style, Stylize};
use ratatui::symbols::border;
use ratatui::widgets::Block;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Copy)]
pub struct Theme {
    pub bg: Color,   // Reset: el fondo de la terminal
    pub base: Color, // el fondo del tema (texto sobre el color principal)
    pub primary: Color,
    pub accent: Color,
    pub fg: Color,
    pub muted: Color,
    pub border: Color,
    pub hl: Color,
    pub red: Color,
    pub green: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            bg: Color::Reset,
            base: Color::Black,
            primary: Color::Blue,
            accent: Color::Yellow,
            fg: Color::Reset,
            muted: Color::DarkGray,
            border: Color::DarkGray,
            hl: Color::DarkGray,
            red: Color::Red,
            green: Color::Green,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Borders {
    Auto,
    Rounded,
    Plain,
    Double,
    Thick,
    Dashed,
    None,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Bar {
    Line,
    Knob,
    Blocks,
    Dots,
    Ascii,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Case {
    Normal,
    Upper,
    Lower,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Select {
    Fill,
    Bar,
    Inverse,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Tint {
    Color,
    Phosphor,
    Ink,
}

const DASHED: border::Set = border::Set {
    horizontal_top: "┄",
    horizontal_bottom: "┄",
    vertical_left: "┆",
    vertical_right: "┆",
    ..border::PLAIN
};

pub struct Look {
    pub theme: Theme,
    pub borders: Borders,
    pub bar: Bar,
    pub case: Case,
    pub select: Select,
    tint: Tint,
    transparent: bool,
    rounded: bool,
    stamp: Vec<Option<SystemTime>>,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|_| home().join(".config"))
}

fn look_file() -> PathBuf {
    let own = config_dir().join("vato/look.json");
    if own.exists() { own } else { config_dir().join("rola/look.json") }
}

/// El theme que incluye kitty.conf ahora mismo (el último `include` con colores).
fn kitty_theme() -> PathBuf {
    let dir = config_dir().join("kitty");
    let conf = dir.join("kitty.conf");
    std::fs::read_to_string(&conf)
        .ok()
        .and_then(|text| {
            text.lines()
                .filter_map(|l| l.trim().strip_prefix("include"))
                .map(|p| match p.trim().strip_prefix("~/") {
                    Some(rest) => home().join(rest),
                    None => dir.join(p.trim()),
                })
                .rev()
                .find(|p| std::fs::read_to_string(p).is_ok_and(|t| t.lines().any(|l| l.starts_with("background"))))
        })
        .unwrap_or(conf)
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

type Rgb = (u8, u8, u8);

fn hex(s: &str) -> Option<Rgb> {
    let s = s.strip_prefix('#')?;
    let n = u32::from_str_radix(s, 16).ok()?;
    (s.len() == 6).then_some(((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Color {
    let m = |x: u8, y: u8| (x as f32 * t + y as f32 * (1.0 - t)).round() as u8;
    Color::Rgb(m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

fn rgb((r, g, b): Rgb) -> Color {
    Color::Rgb(r, g, b)
}

fn hypr_rounded() -> bool {
    std::process::Command::new("hyprctl")
        .args(["getoption", "decoration:rounding", "-j"])
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .and_then(|v| v["int"].as_i64())
        .is_some_and(|r| r > 0)
}

impl Look {
    pub fn load() -> Look {
        let mut l = Look {
            theme: Theme::default(),
            borders: Borders::Auto,
            bar: Bar::Line,
            case: Case::Normal,
            select: Select::Fill,
            tint: Tint::Color,
            transparent: true,
            rounded: hypr_rounded(),
            stamp: Vec::new(),
        };
        l.read();
        l
    }

    fn sources() -> Vec<PathBuf> {
        vec![kitty_theme(), look_file()]
    }

    /// Si cambió el look (otro theme de kitty, otro look.json), releer.
    pub fn refresh(&mut self) -> bool {
        let now: Vec<_> = Self::sources().iter().map(|p| mtime(p)).collect();
        if now != self.stamp {
            self.rounded = hypr_rounded();
            self.read();
            return true;
        }
        false
    }

    fn read(&mut self) {
        let srcs = Self::sources();
        self.stamp = srcs.iter().map(|p| mtime(p)).collect();
        let v: Value = std::fs::read_to_string(&srcs[1])
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        self.borders = match s("borders").as_str() {
            "rounded" => Borders::Rounded,
            "plain" => Borders::Plain,
            "double" => Borders::Double,
            "thick" => Borders::Thick,
            "dashed" => Borders::Dashed,
            "none" => Borders::None,
            _ => Borders::Auto,
        };
        self.bar = match s("bar").as_str() {
            "knob" => Bar::Knob,
            "blocks" => Bar::Blocks,
            "dots" => Bar::Dots,
            "ascii" => Bar::Ascii,
            _ => Bar::Line,
        };
        self.case = match s("case").as_str() {
            "upper" => Case::Upper,
            "lower" => Case::Lower,
            _ => Case::Normal,
        };
        self.select = match s("select").as_str() {
            "bar" => Select::Bar,
            "inverse" => Select::Inverse,
            _ => Select::Fill,
        };
        self.tint = match s("tint").as_str() {
            "phosphor" => Tint::Phosphor,
            "ink" => Tint::Ink,
            _ => Tint::Color,
        };
        self.transparent = v["transparent"].as_bool().unwrap_or(true);
        self.theme = self.read_theme(&srcs[0]).unwrap_or_default();
    }

    fn read_theme(&self, file: &Path) -> Option<Theme> {
        let text = std::fs::read_to_string(file).ok()?;
        let c: HashMap<&str, Rgb> = text
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                Some((it.next()?, hex(it.next()?)?))
            })
            .collect();
        let (&bg, &fg0) = (c.get("background")?, c.get("foreground")?);
        let get = |k: &str, d: Rgb| c.get(k).copied().unwrap_or(d);
        let primary0 = get("color12", fg0);
        let accent = get("color11", primary0);
        let (fg, primary) = match self.tint {
            Tint::Color => (fg0, primary0),
            Tint::Phosphor => (primary0, primary0),
            Tint::Ink => (fg0, fg0),
        };
        Some(Theme {
            bg: if self.transparent { Color::Reset } else { rgb(bg) },
            base: rgb(bg),
            primary: rgb(primary),
            accent: rgb(accent),
            fg: rgb(fg),
            muted: mix(fg, bg, 0.55),
            border: mix(fg, bg, 0.22),
            hl: mix(primary, bg, 0.35),
            red: rgb(get("color9", get("color1", (0xe0, 0x45, 0x2b)))),
            green: rgb(get("color10", get("color2", (0x8f, 0xa8, 0x6a)))),
        })
    }

    pub fn border_set(&self) -> border::Set {
        match self.borders {
            Borders::Auto if self.rounded => border::ROUNDED,
            Borders::Auto | Borders::Plain => border::PLAIN,
            Borders::Rounded => border::ROUNDED,
            Borders::Double => border::DOUBLE,
            Borders::Thick => border::THICK,
            Borders::Dashed => DASHED,
            Borders::None => border::EMPTY,
        }
    }

    pub fn block(&self, focused: bool) -> Block<'static> {
        let c = if focused { self.theme.primary } else { self.theme.border };
        Block::bordered().border_style(Style::new().fg(c)).border_set(self.border_set())
    }

    /// Un rótulo de la interfaz en mayúsculas/minúsculas según el look.
    pub fn tx(&self, s: &str) -> String {
        match self.case {
            Case::Normal => s.to_string(),
            Case::Upper => s.to_uppercase(),
            Case::Lower => s.to_lowercase(),
        }
    }

    /// Cómo se marca la fila elegida: estilo y símbolo a su izquierda.
    pub fn sel(&self, focused: bool) -> (Style, &'static str) {
        let t = self.theme;
        match (self.select, focused) {
            (Select::Fill, true) => (Style::new().bg(t.hl).fg(t.fg).bold(), " "),
            (Select::Bar, true) => (Style::new().fg(t.primary).bold(), "▌"),
            (Select::Bar, false) => (Style::new().fg(t.primary), "▌"),
            (Select::Inverse, true) => (Style::new().bg(t.primary).fg(t.base).bold(), " "),
            (_, false) => (Style::new().bg(t.border), " "),
        }
    }

    /// Una barra de `w` celdas al `frac` (0..1) con los glifos del look.
    pub fn bar(&self, frac: f64, w: usize) -> (String, String) {
        let (done, head, rest) = match self.bar {
            Bar::Line => ("━", "━", "─"),
            Bar::Knob => ("━", "●", "─"),
            Bar::Blocks => ("█", "█", "░"),
            Bar::Dots => ("•", "•", "·"),
            Bar::Ascii => ("=", ">", "-"),
        };
        let n = ((frac.clamp(0.0, 1.0) * w as f64).round() as usize).min(w);
        let filled = if n == 0 { String::new() } else { format!("{}{}", done.repeat(n - 1), head) };
        (filled, rest.repeat(w - n))
    }

    /// Niveles para gráficas chiquitas (sparklines), según el look.
    pub fn levels(&self) -> &'static [char] {
        if self.bar == Bar::Ascii { &['_', '.', '-', '=', '#'] } else { &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'] }
    }
}
