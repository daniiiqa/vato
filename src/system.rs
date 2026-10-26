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

const HIST: usize = 300;
const PF_KTHREAD: u64 = 0x0020_0000;

#[derive(Clone)]
struct Proc {
    pid: i32,
    name: String,
    args: String, // la línea de comando sin el programa
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
    model: String,
    freq: f64,                 // GHz, promedio de los núcleos
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
    procs: Vec<Proc>, // todos, ya ordenados; la lista muestra los que pasan el filtro
    sort: Sort,
    sel: usize,
    follow: bool, // la selección sigue a su proceso (ya te moviste) o se queda en su fila
    filter: String,
    typing: bool,
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

/// "AMD Ryzen 5 8400F 6-Core Processor" → "Ryzen 5 8400F"
fn cpu_model() -> String {
    let mi = read("/proc/cpuinfo");
    let Some(name) = mi.lines().find_map(|l| l.strip_prefix("model name")?.split_once(':').map(|x| x.1)) else {
        return String::new();
    };
    name.replace("(R)", "")
        .replace("(TM)", "")
        .split_whitespace()
        .take_while(|w| !matches!(*w, "@" | "with" | "CPU" | "Processor") && !w.ends_with("-Core"))
        .filter(|w| !matches!(*w, "AMD" | "Intel" | "Core"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn uptime(s: u64) -> String {
    if s >= 86400 { format!("{}d {}h", s / 86400, s / 3600 % 24) } else { format!("{}h {:02}m", s / 3600, s / 60 % 60) }
}

impl System {
    pub fn new() -> System {
        System {
            last: None,
            model: cpu_model(),
            freq: 0.0,
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
            follow: false,
            filter: String::new(),
            typing: false,
            kill: None,
            uptime: 0,
            load: String::new(),
        }
    }

    /// Los procesos que pasan el filtro (nombre o línea de comando).
    fn view(&self) -> Vec<&Proc> {
        if self.filter.is_empty() {
            return self.procs.iter().collect();
        }
        let f = self.filter.to_lowercase();
        self.procs
            .iter()
            .filter(|p| p.name.to_lowercase().contains(&f) || p.args.to_lowercase().contains(&f) || p.pid.to_string() == f)
            .collect()
    }

    fn move_sel(&mut self, by: isize) {
        let n = self.view().len();
        self.sel = self.sel.saturating_add_signed(by).min(n.saturating_sub(1));
        self.follow = true;
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
        let ncpu = self.prev_cpu.len().saturating_sub(1).max(1);
        let khz: Vec<f64> = (0..ncpu)
            .filter_map(|i| read(&format!("/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq")).trim().parse().ok())
            .collect();
        self.freq = if khz.is_empty() { 0.0 } else { khz.iter().sum::<f64>() / khz.len() as f64 / 1e6 };

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

        // procesos (sin los hilos del kernel)
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        let mut ticks = HashMap::new();
        let mut procs = Vec::new();
        if let Ok(dir) = fs::read_dir("/proc") {
            for e in dir.flatten() {
                let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
                let s = read(&format!("/proc/{pid}/stat"));
                let (Some(a), Some(b)) = (s.find('('), s.rfind(')')) else { continue };
                let rest: Vec<&str> = s[b + 2..].split_whitespace().collect();
                // después del nombre: estado [0], flags [6], utime [11], stime [12], rss [21]
                let num = |i: usize| rest.get(i).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
                if num(6) & PF_KTHREAD != 0 {
                    continue;
                }
                let used = num(11) + num(12);
                let rss = num(21) * page;
                let cpu = match self.prev_ticks.get(&pid) {
                    Some(&p) if total_delta > 0 => used.saturating_sub(p) as f64 / total_delta as f64 * ncpu as f64,
                    _ => 0.0,
                };
                ticks.insert(pid, used);
                // el nombre del kernel se corta a 15; si el programa empieza igual, mejor ese
                let comm = &s[a + 1..b];
                let cmdline = read(&format!("/proc/{pid}/cmdline"));
                let mut argv = cmdline.split('\0').filter(|x| !x.is_empty());
                let prog = argv.next().map(|x| x.rsplit('/').next().unwrap_or(x)).unwrap_or("");
                let name = if comm.len() >= 15 && prog.starts_with(comm) { prog } else { comm };
                let args = argv.collect::<Vec<_>>().join(" ");
                procs.push(Proc { pid, name: name.to_string(), args, cpu, mem: rss });
            }
        }
        self.prev_ticks = ticks;
        let keep = self.view().get(self.sel).map(|p| p.pid);
        match self.sort {
            Sort::Cpu => procs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.mem.cmp(&a.mem))),
            Sort::Mem => procs.sort_by(|a, b| b.mem.cmp(&a.mem)),
        }
        self.procs = procs;
        let view = self.view();
        self.sel = keep
            .filter(|_| self.follow)
            .and_then(|pid| view.iter().position(|p| p.pid == pid))
            .unwrap_or(self.sel.min(view.len().saturating_sub(1)));
    }

    /// Gráfica de `h` filas por `w` columnas con lo último de `hist` (0..1).
    fn graph(hist: &[f64], w: usize, h: usize, lv: &[char]) -> Vec<String> {
        let start = hist.len().saturating_sub(w);
        let pad = w - (hist.len() - start);
        let n = lv.len();
        (0..h)
            .map(|row| {
                let floor = (h - 1 - row) as f64;
                let mut s = " ".repeat(pad);
                for v in &hist[start..] {
                    let fill = v.clamp(0.0, 1.0) * h as f64 - floor;
                    s.push(if fill <= 0.0 {
                        if row == h - 1 { lv[0] } else { ' ' }
                    } else {
                        lv[((fill.min(1.0) * n as f64).ceil() as usize).clamp(1, n) - 1]
                    });
                }
                s
            })
            .collect()
    }
}

impl Tab for System {
    fn name(&self) -> &'static str {
        "system"
    }

    fn typing(&self) -> bool {
        self.typing
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

        // alto de arriba: lo que pidan los núcleos (en 2–4 columnas) o el panel de al lado
        let cpu_w = (area.width as usize * 55 / 100).saturating_sub(2);
        let ncores = self.cpu.len().saturating_sub(1);
        let ncols = (cpu_w / 26).clamp(2, 4);
        let core_rows = ncores.div_ceil(ncols);
        let (mt, ma, st, sf) = self.mem;
        let side_rows = self.gpu.map_or(0, |g| if g.2 > 0 { 2 } else { 1 })
            + (mt > 0) as usize
            + (st > 0) as usize
            + self.disks.len()
            + 2
            + !self.temps.is_empty() as usize;
        let want = (core_rows + 1 + 3).max(side_rows) as u16 + 2;
        let top_h = want.min(area.height * 3 / 5);
        let [top, bottom] = Layout::vertical([Constraint::Length(top_h), Constraint::Fill(1)]).areas(area);
        let [cpu_a, side_a] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(top);

        // CPU
        let mut title = vec![Span::styled(format!(" {} ", look.tx("cpu")), Style::new().fg(t.primary).bold())];
        if !self.model.is_empty() {
            title.push(Span::styled(format!("{} ", self.model), Style::new().fg(t.muted)));
        }
        if self.freq > 0.0 {
            title.push(Span::styled(format!("· {:.1} GHz ", self.freq), Style::new().fg(t.muted)));
        }
        let block = look.block(false).title(Line::from(title));
        let inner = block.inner(cpu_a);
        f.render_widget(block, cpu_a);
        let w = inner.width as usize;
        let mut lines = Vec::new();
        let total = self.cpu.first().copied().unwrap_or(0.0);
        let temp = self.temps.iter().find(|x| x.0 == "cpu").map(|x| format!("{:.0}°", x.1)).unwrap_or_default();
        let (d, r) = look.bar(total, w.saturating_sub(13));
        lines.push(Line::from(vec![
            Span::styled(format!("{:>4.0}% ", total * 100.0), Style::new().fg(t.fg).bold()),
            Span::styled(d, Style::new().fg(t.primary)),
            Span::styled(r, Style::new().fg(t.border)),
            Span::styled(format!(" {temp:>5}"), Style::new().fg(t.accent)),
        ]));
        let graph_h = (inner.height as usize).saturating_sub(1 + core_rows).max(1);
        for g in Self::graph(&self.cpu_hist, w, graph_h, look.levels()) {
            lines.push(Line::styled(g, Style::new().fg(t.primary)));
        }
        let col = w / ncols;
        for row in 0..core_rows {
            let mut spans = Vec::new();
            for c in (0..ncols).map(|k| row + k * core_rows) {
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

        // GPU, memoria, discos, red, temperaturas
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
        lines.push(Line::from(vec![
            Span::styled(format!("{:<7} ", look.tx(l("activo", "up"))), Style::new().fg(t.muted).bold()),
            Span::styled(
                format!(
                    "{}   {} {}",
                    uptime(self.uptime),
                    l("carga", "load"),
                    if w < 40 { self.load.split(' ').next().unwrap_or("") } else { &self.load }
                ),
                Style::new().fg(t.fg),
            ),
        ]));
        if !self.temps.is_empty() {
            let temps: String = self.temps.iter().map(|(n, v)| format!("{n} {v:.0}°  ")).collect();
            lines.push(Line::from(vec![
                Span::styled(format!("{:<7} ", look.tx("temp")), Style::new().fg(t.muted).bold()),
                Span::styled(temps, Style::new().fg(t.accent)),
            ]));
        }
        let spare = (inner.height as usize).saturating_sub(lines.len());
        if !self.gpu_hist.is_empty() && spare > 0 {
            for g in Self::graph(&self.gpu_hist, w, spare, look.levels()) {
                lines.push(Line::styled(g, Style::new().fg(t.accent)));
            }
        }
        f.render_widget(Paragraph::new(lines), inner);

        // procesos
        let view = self.view();
        let mut title = format!(" {} {} · {} ", look.tx(l("procesos", "processes")), view.len(), look.tx(match self.sort {
            Sort::Cpu => l("por cpu", "by cpu"),
            Sort::Mem => l("por memoria", "by memory"),
        }));
        if !self.filter.is_empty() && !self.typing {
            title.push_str(&format!("· /{} ", self.filter));
        }
        let block = look.block(true).title(Span::styled(title, Style::new().fg(t.primary).bold()));
        let inner = block.inner(bottom);
        f.render_widget(block, bottom);
        let h = inner.height as usize;
        let nw = (inner.width as usize).saturating_sub(28);
        let mut lines = vec![Line::styled(
            format!(" {:>7}  {:<nw$} {:>6} {:>8}", "pid", look.tx(l("nombre", "name")), "cpu", look.tx("mem")),
            Style::new().fg(t.muted).bold(),
        )];
        let first = self.sel.saturating_sub(h.saturating_sub(3));
        let (sel_style, mark) = look.sel(true);
        for (i, p) in view.iter().enumerate().skip(first).take(h.saturating_sub(1)) {
            let selected = i == self.sel;
            let name: String = p.name.chars().take(nw).collect();
            let room = nw.saturating_sub(name.chars().count() + 2);
            let args: String = if room > 0 { p.args.chars().take(room).collect() } else { String::new() };
            let pad = nw.saturating_sub(name.chars().count() + if args.is_empty() { 0 } else { 2 + args.chars().count() });
            let (st, dim) = if selected { (sel_style, sel_style) } else { (Style::new().fg(t.fg), Style::new().fg(t.muted)) };
            let mut spans = vec![
                Span::styled(if selected { mark } else { " " }, Style::new().fg(t.primary)),
                Span::styled(format!("{:>7}  ", p.pid), st),
                Span::styled(name, st),
            ];
            if !args.is_empty() {
                spans.push(Span::styled(format!("  {args}"), dim));
            }
            spans.push(Span::styled(format!("{} {:>5.1}% {:>8}", " ".repeat(pad), p.cpu * 100.0, human(p.mem as f64)), st));
            lines.push(Line::from(spans));
        }
        f.render_widget(Paragraph::new(lines), inner);

        // abajo del recuadro: confirmar cierre o lo que vas buscando
        let prompt = if let Some((pid, name)) = &self.kill {
            Some((
                format!(
                    " {} {name} ({pid})? k {} · K {} · esc {} ",
                    l("¿Cerrar", "Kill"),
                    l("sí", "yes"),
                    l("forzar", "force"),
                    l("no", "no")
                ),
                Style::new().fg(t.base).bg(t.red).bold(),
            ))
        } else if self.typing {
            Some((format!(" / {}▏ ", self.filter), Style::new().fg(t.base).bg(t.primary).bold()))
        } else {
            None
        };
        if let Some((msg, style)) = prompt {
            let r = Rect {
                x: bottom.x + 2,
                y: bottom.y + bottom.height.saturating_sub(1),
                width: (msg.chars().count() as u16).min(bottom.width.saturating_sub(4)),
                height: 1,
            };
            f.render_widget(Paragraph::new(Line::styled(msg, style)), r);
        }
    }

    fn key(&mut self, k: KeyEvent, ui: &mut Ui) -> bool {
        if let Some((pid, name)) = self.kill.take() {
            let sig = match k.code {
                KeyCode::Char('k') | KeyCode::Char('y') | KeyCode::Char('s') => Some(libc::SIGTERM),
                KeyCode::Char('K') => Some(libc::SIGKILL),
                _ => None,
            };
            if let Some(sig) = sig {
                if unsafe { libc::kill(pid, sig) } == 0 {
                    ui.say(format!("{name}: {}", l("cerrado", "killed")));
                    self.last = None;
                } else {
                    ui.warn(format!("{name}: {}", l("no se pudo cerrar (¿es de otro usuario?)", "couldn't kill (another user's?)")));
                }
            }
            return true;
        }
        if self.typing {
            match k.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.typing = false;
                }
                KeyCode::Enter | KeyCode::Down | KeyCode::Up => self.typing = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => {}
            }
            self.sel = 0;
            self.follow = false;
            return true;
        }
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(15),
            KeyCode::PageUp => self.move_sel(-15),
            KeyCode::Home | KeyCode::Char('g') => {
                self.sel = 0;
                self.follow = false;
            }
            KeyCode::End | KeyCode::Char('G') => self.move_sel(isize::MAX),
            KeyCode::Char('c') => {
                self.sort = Sort::Cpu;
                self.sel = 0;
                self.follow = false;
                self.last = None;
            }
            KeyCode::Char('m') => {
                self.sort = Sort::Mem;
                self.sel = 0;
                self.follow = false;
                self.last = None;
            }
            KeyCode::Char('/') => self.typing = true,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.sel = 0;
                self.follow = false;
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                if let Some(p) = self.view().get(self.sel) {
                    self.kill = Some((p.pid, p.name.clone()));
                }
            }
            _ => return false,
        }
        true
    }

    fn scroll(&mut self, down: bool, _ui: &mut Ui) {
        self.move_sel(if down { 3 } else { -3 });
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.typing {
            return vec![("enter", l("listo", "done")), ("esc", l("borrar", "clear"))];
        }
        let mut v = vec![
            ("j/k", l("mover", "move")),
            ("c/m", l("ordenar cpu/mem", "sort cpu/mem")),
            ("/", l("buscar", "search")),
            ("x", l("cerrar proceso", "kill")),
        ];
        if !self.filter.is_empty() {
            v.push(("esc", l("quitar búsqueda", "clear search")));
        }
        v
    }
}
