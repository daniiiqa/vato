//! Sistema: CPU (total, por núcleo y gráfica), GPU (AMD por sysfs), memoria,
//! temperaturas, discos, red y los procesos que más gastan. Todo sale de /proc
//! y /sys, una vez por segundo.

use crate::{Tab, Ui, l};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::collections::HashMap;
use std::fs;
use std::time::{Duration, Instant};

const HIST: usize = 120;

#[derive(Clone)]
struct Proc {
    pid: i32,
    name: String,
    cpu: f64,
    mem: u64, // bytes
}

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Cpu,
    Mem,
}

pub struct System {
    last: Option<Instant>,
    prev_cpu: Vec<(u64, u64)>, // (ocupado, total) por línea de /proc/stat (0 = total)
    cpu: Vec<f64>,             // 0 = total, luego cada núcleo
    cpu_hist: Vec<f64>,
    gpu: Option<(f64, u64, u64)>, // uso, vram usada, vram total
    gpu_hist: Vec<f64>,
    mem: (u64, u64, u64, u64), // total, disponible, swap total, swap libre
    temps: Vec<(String, f64)>,
    disks: Vec<(String, u64, u64)>, // punto de montaje, usado, total
    net_prev: Option<(u64, u64, Instant)>,
    net: (f64, f64), // bajada, subida (bytes/s)
    prev_ticks: HashMap<i32, u64>,
    procs: Vec<Proc>,
    sort: Sort,
    sel: usize,
    kill: Option<(i32, String)>,
    uptime: u64,
    load: String,
}

fn read(p: &str) -> String {
    fs::read_to_string(p).unwrap_or_default()
}

fn human(b: f64) -> String {
    let units = ["B", "K", "M", "G", "T"];
    let mut v = b;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i >= 3 { format!("{v:.1}{}", units[i]) } else { format!("{v:.0}{}", units[i]) }
}

fn gpu_dir() -> Option<String> {
    (0..4)
        .map(|i| format!("/sys/class/drm/card{i}/device"))
        .find(|d| fs::metadata(format!("{d}/gpu_busy_percent")).is_ok())
}

impl System {
    pub fn new() -> System {
        System {
            last: None,
            prev_cpu: Vec::new(),
            cpu: Vec::new(),
            cpu_hist: Vec::new(),
            gpu: None,
            gpu_hist: Vec::new(),
            mem: (0, 0, 0, 0),
            temps: Vec::new(),
            disks: Vec::new(),
            net_prev: None,
            net: (0.0, 0.0),
            prev_ticks: HashMap::new(),
            procs: Vec::new(),
            sort: Sort::Cpu,
            sel: 0,
            kill: None,
            uptime: 0,
            load: String::new(),
        }
    }

    fn sample(&mut self) {
        // CPU
        let stat = read("/proc/stat");
        let mut now = Vec::new();
        for line in stat.lines().filter(|l| l.starts_with("cpu")) {
            let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            let idle = v.get(3).unwrap_or(&0) + v.get(4).unwrap_or(&0);
            let total: u64 = v.iter().take(8).sum();
            now.push((total - idle, total));
        }
        let total_delta = match (now.first(), self.prev_cpu.first()) {
            (Some(n), Some(p)) => n.1.saturating_sub(p.1),
            _ => 0,
        };
        if self.prev_cpu.len() == now.len() {
            self.cpu = now
                .iter()
                .zip(&self.prev_cpu)
                .map(|(n, p)| {
                    let t = n.1.saturating_sub(p.1);
                    if t == 0 { 0.0 } else { n.0.saturating_sub(p.0) as f64 / t as f64 }
                })
                .collect();
            self.cpu_hist.push(self.cpu[0]);
        }
        self.prev_cpu = now;
        let ncpu = self.prev_cpu.len().saturating_sub(1).max(1) as f64;

        // GPU (AMD)
        self.gpu = gpu_dir().map(|d| {
            let busy: f64 = read(&format!("{d}/gpu_busy_percent")).trim().parse().unwrap_or(0.0);
            let used = read(&format!("{d}/mem_info_vram_used")).trim().parse().unwrap_or(0);
            let total = read(&format!("{d}/mem_info_vram_total")).trim().parse().unwrap_or(0);
            (busy / 100.0, used, total)
        });
        if let Some((b, ..)) = self.gpu {
            self.gpu_hist.push(b);
        }
        for h in [&mut self.cpu_hist, &mut self.gpu_hist] {
            if h.len() > HIST {
                h.drain(..h.len() - HIST);
            }
        }

        // memoria
        let mi = read("/proc/meminfo");
        let kb = |k: &str| -> u64 {
            mi.lines()
                .find(|l| l.starts_with(k))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                .unwrap_or(0)
                * 1024
        };
        self.mem = (kb("MemTotal:"), kb("MemAvailable:"), kb("SwapTotal:"), kb("SwapFree:"));

        // temperaturas: CPU (k10temp/coretemp), GPU, NVMe
        self.temps.clear();
        if let Ok(dir) = fs::read_dir("/sys/class/hwmon") {
            let mut v: Vec<(String, f64)> = dir
                .flatten()
                .filter_map(|e| {
                    let p = e.path();
                    let name = fs::read_to_string(p.join("name")).ok()?.trim().to_string();
                    let label = match name.as_str() {
                        "k10temp" | "coretemp" | "zenpower" => "cpu",
                        "amdgpu" | "nouveau" => "gpu",
                        "nvme" => "nvme",
                        _ => return None,
                    };
                    let t: f64 = fs::read_to_string(p.join("temp1_input")).ok()?.trim().parse().ok()?;
                    Some((label.to_string(), t / 1000.0))
                })
                .collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            v.dedup_by(|a, b| a.0 == b.0);
            self.temps = v;
        }

        // discos: los de verdad (sin repetir dispositivo)
        let mut seen = Vec::new();
        self.disks.clear();
        for line in read("/proc/mounts").lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 3 || !f[0].starts_with("/dev/") || seen.contains(&f[0]) {
                continue;
            }
            if !matches!(f[2], "ext4" | "btrfs" | "xfs" | "vfat" | "f2fs" | "ntfs3" | "exfat") {
                continue;
            }
            seen.push(f[0]);
            let path = std::ffi::CString::new(f[1].replace("\\040", " ")).unwrap_or_default();
            let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::statvfs(path.as_ptr(), &mut s) } == 0 {
                let total = s.f_blocks as u64 * s.f_frsize as u64;
                let free = s.f_bavail as u64 * s.f_frsize as u64;
                if total > 0 {
                    self.disks.push((f[1].replace("\\040", " "), total - free, total));
                }
            }
        }

        // red (todo menos lo, docker y puentes)
        let (mut rx, mut tx) = (0u64, 0u64);
        for line in read("/proc/net/dev").lines().skip(2) {
            let Some((iface, rest)) = line.split_once(':') else { continue };
            let iface = iface.trim();
            if iface == "lo" || iface.starts_with("docker") || iface.starts_with("br-") || iface.starts_with("veth") {
                continue;
            }
            let v: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            rx += v.first().unwrap_or(&0);
            tx += v.get(8).unwrap_or(&0);
        }
        let t = Instant::now();
        if let Some((prx, ptx, pt)) = self.net_prev {
            let dt = t.duration_since(pt).as_secs_f64().max(0.001);
            self.net = (rx.saturating_sub(prx) as f64 / dt, tx.saturating_sub(ptx) as f64 / dt);
        }
        self.net_prev = Some((rx, tx, t));

        self.uptime = read("/proc/uptime").split('.').next().and_then(|s| s.parse().ok()).unwrap_or(0);
        self.load = read("/proc/loadavg").split_whitespace().take(3).collect::<Vec<_>>().join(" ");

        // procesos
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        let mut ticks = HashMap::new();
        let mut procs = Vec::new();
        if let Ok(dir) = fs::read_dir("/proc") {
            for e in dir.flatten() {
                let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
                let s = read(&format!("/proc/{pid}/stat"));
                let (Some(a), Some(b)) = (s.find('('), s.rfind(')')) else { continue };
                let name = s[a + 1..b].to_string();
                let rest: Vec<&str> = s[b + 2..].split_whitespace().collect();
                // después del nombre: estado es [0], utime [11], stime [12], rss [21]
                let used: u64 = rest.get(11).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0)
                    + rest.get(12).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
                let rss = rest.get(21).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0) * page;
                let cpu = match self.prev_ticks.get(&pid) {
                    Some(&p) if total_delta > 0 => used.saturating_sub(p) as f64 / total_delta as f64 * ncpu,
                    _ => 0.0,
                };
                ticks.insert(pid, used);
                procs.push(Proc { pid, name, cpu, mem: rss });
            }
        }
        self.prev_ticks = ticks;
        let keep = self.procs.get(self.sel).map(|p| p.pid);
        match self.sort {
            Sort::Cpu => procs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.mem.cmp(&a.mem))),
            Sort::Mem => procs.sort_by(|a, b| b.mem.cmp(&a.mem)),
        }
        procs.truncate(200);
        self.procs = procs;
        self.sel = keep
            .and_then(|pid| self.procs.iter().position(|p| p.pid == pid))
            .unwrap_or(self.sel.min(self.procs.len().saturating_sub(1)));
    }

    fn spark(hist: &[f64], w: usize, lv: &[char]) -> String {
        let start = hist.len().saturating_sub(w);
        let s: String = hist[start..]
            .iter()
            .map(|v| lv[((v.clamp(0.0, 1.0)) * (lv.len() - 1) as f64).round() as usize])
            .collect();
        format!("{}{s}", " ".repeat(w.saturating_sub(hist.len() - start)))
    }
}

impl Tab for System {
    fn name(&self) -> &'static str {
        "system"
    }

    fn tick(&mut self, _ui: &mut Ui) {
        if self.last.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            self.sample();
            self.last = Some(Instant::now());
        }
    }

    fn draw(&mut self, f: &mut Frame, area: Rect, ui: &Ui) {
        let look = &ui.look;
        let t = look.theme;
        let ncores = self.cpu.len().saturating_sub(1);
        let core_rows = ncores.div_ceil(2) as u16;
        let top_h = (core_rows + 4).max(9);
        let [top, bottom] = Layout::vertical([Constraint::Length(top_h), Constraint::Fill(1)]).areas(area);
        let [cpu_a, side_a] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(top);

        // CPU
        let block = look.block(false).title(Span::styled(format!(" {} ", look.tx("cpu")), Style::new().fg(t.primary).bold()));
        let inner = block.inner(cpu_a);
        f.render_widget(block, cpu_a);
        let w = inner.width as usize;
        let mut lines = Vec::new();
        let total = self.cpu.first().copied().unwrap_or(0.0);
        let temp = self.temps.iter().find(|x| x.0 == "cpu").map(|x| format!("{:.0}°", x.1)).unwrap_or_default();
        let (d, r) = look.bar(total, w.saturating_sub(16));
        lines.push(Line::from(vec![
            Span::styled(format!("{:>4.0}% ", total * 100.0), Style::new().fg(t.fg).bold()),
            Span::styled(d, Style::new().fg(t.primary)),
            Span::styled(r, Style::new().fg(t.border)),
            Span::styled(format!(" {temp:>5}"), Style::new().fg(t.accent)),
        ]));
        lines.push(Line::styled(Self::spark(&self.cpu_hist, w, look.levels()), Style::new().fg(t.primary)));
        let col = w / 2;
        for row in 0..core_rows as usize {
            let mut spans = Vec::new();
            for c in [row, row + core_rows as usize] {
                if let Some(v) = self.cpu.get(c + 1) {
                    let (d, r) = look.bar(*v, col.saturating_sub(10));
                    spans.push(Span::styled(format!("{c:>3} "), Style::new().fg(t.muted)));
                    spans.push(Span::styled(d, Style::new().fg(if *v > 0.85 { t.accent } else { t.primary })));
                    spans.push(Span::styled(r, Style::new().fg(t.border)));
                    spans.push(Span::styled(format!("{:>4.0}% ", v * 100.0), Style::new().fg(t.fg)));
                }
            }
            lines.push(Line::from(spans));
        }
        f.render_widget(Paragraph::new(lines), inner);

        // GPU, memoria, red, discos
        let block = look.block(false);
        let inner = block.inner(side_a);
        f.render_widget(block, side_a);
        let w = inner.width as usize;
        let meter = |label: &str, frac: f64, right: String| -> Line<'static> {
            let bw = w.saturating_sub(8 + right.chars().count() + 1);
            let (d, r) = look.bar(frac, bw);
            Line::from(vec![
                Span::styled(format!("{:<7} ", look.tx(label)), Style::new().fg(t.muted).bold()),
                Span::styled(d, Style::new().fg(if frac > 0.85 { t.accent } else { t.primary })),
                Span::styled(r, Style::new().fg(t.border)),
                Span::styled(format!(" {right}"), Style::new().fg(t.fg)),
            ])
        };
        let mut lines = Vec::new();
        if let Some((busy, used, total)) = self.gpu {
            let temp = self.temps.iter().find(|x| x.0 == "gpu").map(|x| format!(" {:.0}°", x.1)).unwrap_or_default();
            lines.push(meter("gpu", busy, format!("{:>3.0}%{temp}", busy * 100.0)));
            if total > 0 {
                lines.push(meter("vram", used as f64 / total as f64, format!("{}/{}", human(used as f64), human(total as f64))));
            }
        }
        let (mt, ma, st, sf) = self.mem;
        if mt > 0 {
            lines.push(meter("ram", (mt - ma) as f64 / mt as f64, format!("{}/{}", human((mt - ma) as f64), human(mt as f64))));
        }
        if st > 0 {
            lines.push(meter("swap", (st - sf) as f64 / st as f64, format!("{}/{}", human((st - sf) as f64), human(st as f64))));
        }
        for (mnt, used, total) in &self.disks {
            let name = if mnt == "/" { "/".to_string() } else { mnt.rsplit('/').next().unwrap_or(mnt).to_string() };
            let name: String = name.chars().take(7).collect();
            lines.push(meter(&name, *used as f64 / *total as f64, format!("{}/{}", human(*used as f64), human(*total as f64))));
        }
        lines.push(Line::from(vec![
            Span::styled(format!("{:<7} ", look.tx(l("red", "net"))), Style::new().fg(t.muted).bold()),
            Span::styled(format!("↓ {}/s  ↑ {}/s", human(self.net.0), human(self.net.1)), Style::new().fg(t.fg)),
        ]));
        let up = self.uptime;
        let temps: String = self.temps.iter().map(|(n, v)| format!("{n} {v:.0}°  ")).collect();
        lines.push(Line::from(vec![
            Span::styled(format!("{:<7} ", look.tx(l("activo", "up"))), Style::new().fg(t.muted).bold()),
            Span::styled(format!("{}h {:02}m   {} {}", up / 3600, up / 60 % 60, l("carga", "load"), self.load), Style::new().fg(t.fg)),
        ]));
        if !temps.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(format!("{:<7} ", look.tx("temp")), Style::new().fg(t.muted).bold()),
                Span::styled(temps, Style::new().fg(t.accent)),
            ]));
        }
        if !self.gpu_hist.is_empty() && lines.len() + 1 < inner.height as usize {
            lines.push(Line::styled(Self::spark(&self.gpu_hist, w, look.levels()), Style::new().fg(t.accent)));
        }
        f.render_widget(Paragraph::new(lines), inner);

        // procesos
        let title = format!(" {} · {} ", look.tx(l("procesos", "processes")), look.tx(match self.sort {
            Sort::Cpu => l("por cpu", "by cpu"),
            Sort::Mem => l("por memoria", "by memory"),
        }));
        let block = look.block(true).title(Span::styled(title, Style::new().fg(t.primary).bold()));
        let inner = block.inner(bottom);
        f.render_widget(block, bottom);
        let h = inner.height as usize;
        let w = inner.width as usize;
        let mut lines = vec![Line::styled(
            format!(" {:>7}  {:<width$} {:>6} {:>8}", "pid", look.tx(l("nombre", "name")), "cpu", look.tx("mem"), width = w.saturating_sub(28)),
            Style::new().fg(t.muted).bold(),
        )];
        let top = self.sel.saturating_sub(h.saturating_sub(3));
        let (sel_style, mark) = look.sel(true);
        for (i, p) in self.procs.iter().enumerate().skip(top).take(h.saturating_sub(1)) {
            let selected = i == self.sel;
            let name: String = p.name.chars().take(w.saturating_sub(28)).collect();
            let st = if selected { sel_style } else { Style::new().fg(t.fg) };
            lines.push(Line::from(vec![
                Span::styled(if selected { mark } else { " " }, Style::new().fg(t.primary)),
                Span::styled(
                    format!("{:>7}  {:<width$} {:>5.1}% {:>8}", p.pid, name, p.cpu * 100.0, human(p.mem as f64), width = w.saturating_sub(28)),
                    st,
                ),
            ]));
        }
        f.render_widget(Paragraph::new(lines), inner);
        if let Some((pid, name)) = &self.kill {
            let msg = format!(" {} {name} ({pid})? k {} · esc {} ", l("¿Cerrar", "Kill"), l("sí", "yes"), l("no", "no"));
            let r = Rect { x: bottom.x + 2, y: bottom.y + bottom.height.saturating_sub(1), width: (msg.chars().count() as u16).min(bottom.width.saturating_sub(4)), height: 1 };
            f.render_widget(Paragraph::new(Line::styled(msg, Style::new().fg(t.base).bg(t.red).bold())), r);
        }
    }

    fn key(&mut self, k: KeyEvent, ui: &mut Ui) -> bool {
        if let Some((pid, name)) = self.kill.take() {
            if k.code == KeyCode::Char('k') {
                if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
                    ui.say(format!("{name}: {}", l("cerrado", "killed")));
                } else {
                    ui.warn(format!("{name}: {}", l("no se pudo cerrar (¿es de otro usuario?)", "couldn't kill (another user's?)")));
                }
            }
            return true;
        }
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => self.sel = (self.sel + 1).min(self.procs.len().saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::PageDown => self.sel = (self.sel + 15).min(self.procs.len().saturating_sub(1)),
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(15),
            KeyCode::Char('c') => {
                self.sort = Sort::Cpu;
                self.last = None;
            }
            KeyCode::Char('m') => {
                self.sort = Sort::Mem;
                self.last = None;
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                if let Some(p) = self.procs.get(self.sel) {
                    self.kill = Some((p.pid, p.name.clone()));
                }
            }
            _ => return false,
        }
        true
    }

    fn scroll(&mut self, down: bool, _ui: &mut Ui) {
        self.sel = if down { (self.sel + 3).min(self.procs.len().saturating_sub(1)) } else { self.sel.saturating_sub(3) };
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("j/k", l("mover", "move")), ("c/m", l("ordenar cpu/mem", "sort cpu/mem")), ("x", l("cerrar proceso", "kill"))]
    }
}
