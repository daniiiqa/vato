//! Sistema: CPU (total, por núcleo y gráfica), GPU (uso, reloj, consumo,
//! ventilador y qué procesos la están usando), memoria, temperaturas, discos,
//! red y los procesos que más gastan. Todo sale de /proc y /sys, una vez por
//! segundo; lo de la GPU por proceso, del fdinfo de DRM (AMD e Intel).

use crate::{Tab, Ui, l};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::collections::{HashMap, HashSet};
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
    mem: u64,       // bytes
    pids: Vec<i32>, // agrupados: todos los de ese nombre
}

/// La tarjeta: uso y VRAM, y de hwmon reloj, consumo, ventilador y temperaturas.
#[derive(Clone, Default)]
struct Gpu {
    busy: f64,
    vram: (u64, u64),  // usada, total
    sclk: u64,         // MHz
    mclk: u64,         // MHz
    power: (f64, f64), // W, tope W
    fan: Option<u64>,  // rpm
    temps: Vec<(&'static str, f64)>,
}

/// Un proceso usando la GPU: cuánto de gráficos/cómputo y de video (codificar
/// o decodificar) en el último segundo, y su VRAM.
#[derive(Clone)]
struct GpuProc {
    pid: i32,
    name: String,
    gfx: f64,
    video: f64,
    vram: u64,
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
    gpu: Option<Gpu>,
    gpu_dir: Option<String>,
    gpu_pdev: String, // "0000:03:00.0": con esto se reconocen sus clientes en fdinfo
    gpu_name: String,
    gpu_prev: HashMap<String, (u64, u64, Instant)>, // cliente DRM → ns de gráficos y video
    gpu_procs: Vec<GpuProc>,
    gpu_top: usize,
    gpu_view: bool, // pantalla angosta: la lista de abajo muestra la GPU en vez de los procesos
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
    group: bool, // juntar los procesos del mismo programa
    kill: Option<(Vec<i32>, String)>,
    uptime: u64,
    load: String,
    hits: Hits,
}

/// Dónde quedó cada cosa al dibujar (para el ratón).
#[derive(Default)]
struct Hits {
    procs: Rect,
    rows: Vec<(u16, usize)>, // fila de pantalla → proceso de la lista
    head: u16,               // la fila de los títulos de columna
    cpu_col: (u16, u16),     // x donde empieza y termina la columna cpu
    mem_col: (u16, u16),
    gpu: Rect,
    gpu_rows: Vec<(u16, i32)>, // fila de pantalla → pid
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

/// El modelo exacto: la tabla de AMD que trae libdrm (la que usa Mesa) lo da
/// por chip y revisión ("7590, C0" → "AMD Radeon RX 9060 XT"). lspci no
/// sirve de primera: junta todo lo que lleva el mismo chip ("RX 9050 / 9060 XT").
fn gpu_name(dir: &str, pdev: &str) -> String {
    let hex = |f: &str| read(&format!("{dir}/{f}")).trim().trim_start_matches("0x").to_uppercase();
    let (dev, rev) = (hex("device"), hex("revision"));
    let ids = ["/usr/share/libdrm/amdgpu.ids", "/usr/local/share/libdrm/amdgpu.ids"].iter().map(|p| read(p)).find(|t| !t.is_empty());
    let found = ids.as_deref().and_then(|t| {
        t.lines().find_map(|line| {
            let mut f = line.splitn(3, ',').map(str::trim);
            let (d, r, name) = (f.next()?, f.next()?, f.next()?);
            (d.eq_ignore_ascii_case(&dev) && r.eq_ignore_ascii_case(&rev)).then(|| name.trim_start_matches("AMD ").to_string())
        })
    });
    if let Some(name) = found {
        return name;
    }
    // si no: lspci, "Navi 44 [Radeon RX 9050 / 9060 XT]" → lo de los corchetes
    let Ok(out) = std::process::Command::new("lspci").args(["-s", pdev, "-mm"]).output() else { return String::new() };
    let out = String::from_utf8_lossy(&out.stdout);
    let dev = out.split('"').nth(5).unwrap_or("");
    match (dev.rfind('['), dev.rfind(']')) {
        (Some(a), Some(b)) if a < b => dev[a + 1..b].to_string(),
        _ => dev.to_string(),
    }
}

/// "12 KiB" → bytes
fn size(v: &str) -> u64 {
    let mut it = v.split_whitespace();
    let n: u64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    n * match it.next() {
        Some("KiB") => 1 << 10,
        Some("MiB") => 1 << 20,
        Some("GiB") => 1 << 30,
        _ => 1,
    }
}

/// Los clientes DRM de un proceso en la GPU `pdev`: (id, ns de gráficos y
/// cómputo, ns de video, VRAM). Un mismo cliente puede estar en varios fd.
fn drm_clients(pid: i32, pdev: &str) -> Vec<(String, u64, u64, u64)> {
    let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else { return Vec::new() };
    let mut out: Vec<(String, u64, u64, u64)> = Vec::new();
    for fd in fds.flatten() {
        if !fs::read_link(fd.path()).is_ok_and(|t| t.starts_with("/dev/dri")) {
            continue;
        }
        let info = read(&format!("/proc/{pid}/fdinfo/{}", fd.file_name().to_string_lossy()));
        let (mut id, mut ours, mut gfx, mut video, mut vram, mut vram_old) = (None, false, 0, 0, None, None);
        for line in info.lines() {
            let Some((k, v)) = line.split_once(':') else { continue };
            let v = v.trim();
            match k {
                "drm-client-id" => id = Some(v.to_string()),
                "drm-pdev" => ours = v == pdev,
                "drm-resident-vram" => vram = Some(size(v)),
                "drm-memory-vram" => vram_old = Some(size(v)),
                _ => {
                    let Some(e) = k.strip_prefix("drm-engine-") else { continue };
                    let ns: u64 = v.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0);
                    if matches!(e, "gfx" | "render" | "compute" | "ccs") {
                        gfx += ns;
                    } else if ["enc", "dec", "jpeg", "video", "vcn"].iter().any(|x| e.contains(x)) && !e.starts_with("capacity") {
                        video += ns;
                    }
                }
            }
        }
        if let (Some(id), true) = (id, ours) {
            if !out.iter().any(|c| c.0 == id) {
                out.push((id, gfx, video, vram.or(vram_old).unwrap_or(0)));
            }
        }
    }
    out
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
        let dir = gpu_dir();
        let mut s = System {
            last: None,
            model: cpu_model(),
            freq: 0.0,
            prev_cpu: Vec::new(),
            cpu: Vec::new(),
            cpu_hist: Vec::new(),
            gpu: None,
            gpu_pdev: dir.as_ref().and_then(|d| fs::canonicalize(d).ok()).and_then(|p| Some(p.file_name()?.to_string_lossy().to_string())).unwrap_or_default(),
            gpu_name: String::new(),
            gpu_dir: dir,
            gpu_prev: HashMap::new(),
            gpu_procs: Vec::new(),
            gpu_top: 0,
            gpu_view: false,
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
            group: false,
            kill: None,
            uptime: 0,
            load: String::new(),
            hits: Hits::default(),
        };
        if !s.gpu_pdev.is_empty() {
            s.gpu_name = gpu_name(s.gpu_dir.as_deref().unwrap_or(""), &s.gpu_pdev);
        }
        s
    }

    fn press(&mut self, code: KeyCode, ui: &mut Ui) {
        self.key(KeyEvent::new(code, KeyModifiers::NONE), ui);
    }

    /// Ir a un proceso en la lista (desde la de la GPU): sin búsqueda que lo esconda.
    fn select_pid(&mut self, pid: i32) {
        if !self.view().iter().any(|p| p.pids.contains(&pid)) {
            self.filter.clear();
        }
        if let Some(i) = self.view().iter().position(|p| p.pids.contains(&pid)) {
            self.sel = i;
            self.follow = true;
            self.gpu_view = false;
        }
    }

    /// Los procesos que pasan el filtro (nombre o línea de comando); con
    /// `group`, uno por programa con su CPU y memoria sumadas.
    fn view(&self) -> Vec<Proc> {
        let f = self.filter.to_lowercase();
        let shown = self
            .procs
            .iter()
            .filter(|p| f.is_empty() || p.name.to_lowercase().contains(&f) || p.args.to_lowercase().contains(&f) || p.pid.to_string() == f);
        if !self.group {
            return shown.cloned().collect();
        }
        let mut groups: Vec<Proc> = Vec::new();
        let mut at: HashMap<&str, usize> = HashMap::new();
        for p in shown {
            match at.get(p.name.as_str()) {
                Some(&i) => {
                    let g = &mut groups[i];
                    g.cpu += p.cpu;
                    g.mem += p.mem;
                    g.pids.push(p.pid);
                    if p.pid < g.pid {
                        g.pid = p.pid;
                        g.args = p.args.clone();
                    }
                }
                None => {
                    at.insert(&p.name, groups.len());
                    groups.push(p.clone());
                }
            }
        }
        self.sort_procs(&mut groups);
        groups
    }

    fn sort_procs(&self, v: &mut [Proc]) {
        match self.sort {
            Sort::Cpu => v.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.mem.cmp(&a.mem))),
            Sort::Mem => v.sort_by_key(|p| std::cmp::Reverse(p.mem)),
        }
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

        // GPU (AMD): uso y VRAM; de su hwmon, reloj, consumo, ventilador, temperaturas
        self.gpu = self.gpu_dir.as_ref().map(|d| {
            let n = |f: &str| read(&format!("{d}/{f}")).trim().parse::<f64>().ok();
            let hw = fs::read_dir(format!("{d}/hwmon"))
                .ok()
                .and_then(|mut r| r.next())
                .and_then(|e| e.ok())
                .map(|e| e.path().to_string_lossy().to_string())
                .unwrap_or_default();
            let h = |f: &str| read(&format!("{hw}/{f}")).trim().parse::<f64>().ok();
            let temps = (1..=3)
                .filter_map(|i| {
                    let v = h(&format!("temp{i}_input"))?;
                    let label = match read(&format!("{hw}/temp{i}_label")).trim() {
                        "edge" => l("borde", "edge"),
                        "junction" => l("unión", "hotspot"),
                        "mem" => "mem",
                        _ => "temp",
                    };
                    Some((label, v / 1000.0))
                })
                .collect();
            Gpu {
                busy: n("gpu_busy_percent").unwrap_or(0.0) / 100.0,
                vram: (n("mem_info_vram_used").unwrap_or(0.0) as u64, n("mem_info_vram_total").unwrap_or(0.0) as u64),
                sclk: (h("freq1_input").unwrap_or(0.0) / 1e6) as u64,
                mclk: (h("freq2_input").unwrap_or(0.0) / 1e6) as u64,
                power: (h("power1_average").or_else(|| h("power1_input")).unwrap_or(0.0) / 1e6, h("power1_cap").unwrap_or(0.0) / 1e6),
                fan: h("fan1_input").map(|v| v as u64),
                temps,
            }
        });
        if let Some(g) = &self.gpu {
            self.gpu_hist.push(g.busy);
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
        let gnow = Instant::now();
        let mut gseen = HashSet::new();
        let mut gprev = HashMap::new();
        let mut gprocs = Vec::new();
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
                // en la GPU: lo que gastó cada cliente desde la vuelta pasada
                if !self.gpu_pdev.is_empty() {
                    let clients = drm_clients(pid, &self.gpu_pdev);
                    let (mut g, mut v, mut vram, mut any) = (0.0, 0.0, 0, false);
                    for (id, gfx, video, mem) in clients {
                        if !gseen.insert(id.clone()) {
                            continue;
                        }
                        any = true;
                        if let Some(&(pg, pv, pt)) = self.gpu_prev.get(&id) {
                            let dt = gnow.duration_since(pt).as_nanos().max(1) as f64;
                            g += gfx.saturating_sub(pg) as f64 / dt;
                            v += video.saturating_sub(pv) as f64 / dt;
                        }
                        vram += mem;
                        gprev.insert(id, (gfx, video, gnow));
                    }
                    if any && (vram > 0 || g > 0.0 || v > 0.0) {
                        gprocs.push(GpuProc { pid, name: name.to_string(), gfx: g.min(1.0), video: v.min(1.0), vram });
                    }
                }
                procs.push(Proc { pid, name: name.to_string(), args, cpu, mem: rss, pids: vec![pid] });
            }
        }
        self.prev_ticks = ticks;
        gprocs.sort_by(|a: &GpuProc, b: &GpuProc| (b.gfx + b.video).total_cmp(&(a.gfx + a.video)).then(b.vram.cmp(&a.vram)));
        self.gpu_procs = gprocs;
        self.gpu_prev = gprev;
        let keep = self.view().get(self.sel).map(|p| p.pid);
        self.sort_procs(&mut procs);
        self.procs = procs;
        let view = self.view();
        self.sel = keep
            .filter(|_| self.follow)
            .and_then(|pid| view.iter().position(|p| p.pid == pid))
            .unwrap_or(self.sel.min(view.len().saturating_sub(1)));
    }

    /// La GPU: reloj, consumo, ventilador, temperaturas y quién la usa.
    fn draw_gpu(&mut self, f: &mut Frame, area: Rect, ui: &Ui) {
        let look = &ui.look;
        let t = look.theme;
        let Some(g) = self.gpu.clone() else { return };
        let mut title = vec![Span::styled(format!(" {} ", look.tx("gpu")), Style::new().fg(t.primary).bold())];
        if !self.gpu_name.is_empty() {
            title.push(Span::styled(format!("{} ", self.gpu_name), Style::new().fg(t.muted)));
        }
        let block = look.block(false).title(Line::from(title));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let w = inner.width as usize;
        let label = |s: &str| Span::styled(format!("{:<10} ", look.tx(s)), Style::new().fg(t.muted).bold());
        let mut lines = Vec::new();
        let (d, r) = look.bar(g.busy, w.saturating_sub(17));
        lines.push(Line::from(vec![
            label(l("uso", "load")),
            Span::styled(d, Style::new().fg(if g.busy > 0.85 { t.accent } else { t.primary })),
            Span::styled(r, Style::new().fg(t.border)),
            Span::styled(format!(" {:>4.0}%", g.busy * 100.0), Style::new().fg(t.fg)),
        ]));
        if g.sclk > 0 {
            lines.push(Line::from(vec![
                label(l("reloj", "clock")),
                Span::styled(format!("{} MHz", g.sclk), Style::new().fg(t.fg)),
                Span::styled(format!("   mem {} MHz", g.mclk), Style::new().fg(t.muted)),
            ]));
        }
        if g.power.0 > 0.0 {
            let right = if g.power.1 > 0.0 { format!(" {:>3.0}/{:.0} W", g.power.0, g.power.1) } else { format!(" {:.0} W", g.power.0) };
            let frac = if g.power.1 > 0.0 { g.power.0 / g.power.1 } else { 0.0 };
            let (d, r) = look.bar(frac, w.saturating_sub(11 + right.chars().count()));
            lines.push(Line::from(vec![
                label(l("consumo", "power")),
                Span::styled(d, Style::new().fg(if frac > 0.85 { t.accent } else { t.primary })),
                Span::styled(r, Style::new().fg(t.border)),
                Span::styled(right, Style::new().fg(t.fg)),
            ]));
        }
        if let Some(rpm) = g.fan {
            let fan = if rpm == 0 { l("parado", "stopped").to_string() } else { format!("{rpm} rpm") };
            lines.push(Line::from(vec![label(l("ventilador", "fan")), Span::styled(fan, Style::new().fg(t.fg))]));
        }
        if !g.temps.is_empty() {
            let temps: String = g.temps.iter().map(|(n, v)| format!("{n} {v:.0}°  ")).collect();
            lines.push(Line::from(vec![label("temp"), Span::styled(temps, Style::new().fg(t.accent))]));
        }
        lines.push(Line::raw(""));
        let nw = w.saturating_sub(23);
        lines.push(Line::styled(
            format!(" {:<nw$} {:>5} {:>6} {:>8}", look.tx(l("usándola", "using it")), "gfx", look.tx("video"), "vram"),
            Style::new().fg(t.muted).bold(),
        ));
        let list_y = inner.y + lines.len() as u16;
        let room = (inner.height as usize).saturating_sub(lines.len());
        self.gpu_top = self.gpu_top.min(self.gpu_procs.len().saturating_sub(room.max(1)));
        let pct = |v: f64| if v >= 0.0005 { format!("{:.0}%", (v * 100.0).max(1.0)) } else { "·".to_string() };
        for (k, p) in self.gpu_procs.iter().skip(self.gpu_top).take(room).enumerate() {
            self.hits.gpu_rows.push((list_y + k as u16, p.pid));
            let name: String = p.name.chars().take(nw).collect();
            let busy = p.gfx + p.video > 0.0005;
            lines.push(Line::from(vec![
                Span::styled(format!(" {name:<nw$}"), Style::new().fg(if busy { t.fg } else { t.muted })),
                Span::styled(format!(" {:>5} {:>6}", pct(p.gfx), pct(p.video)), Style::new().fg(if busy { t.primary } else { t.muted })),
                Span::styled(format!(" {:>8}", human(p.vram as f64)), Style::new().fg(t.fg)),
            ]));
        }
        if self.gpu_procs.is_empty() {
            lines.push(Line::styled(format!(" {}", l("nadie (o no se puede ver)", "nobody (or can't tell)")), Style::new().fg(t.muted)));
        }
        f.render_widget(Paragraph::new(lines), inner);
        self.hits.gpu = inner;
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
        let side_rows = self.gpu.as_ref().map_or(0, |g| if g.vram.1 > 0 { 2 } else { 1 })
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
        if let Some(Gpu { busy, vram: (used, total), .. }) = self.gpu.clone() {
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

        // abajo: procesos y, si cabe, la GPU al lado (si no, `v` cambia entre las dos)
        let wide = bottom.width >= 110;
        let (bottom, gpu_a) = match (&self.gpu, wide) {
            (Some(_), true) => {
                let [a, b] = Layout::horizontal([Constraint::Fill(1), Constraint::Length((bottom.width * 2 / 5).clamp(46, 64))]).areas(bottom);
                (a, Some(b))
            }
            (Some(_), false) if self.gpu_view => (Rect::default(), Some(bottom)),
            _ => (bottom, None),
        };
        self.hits = Hits::default();
        if let Some(a) = gpu_a {
            self.draw_gpu(f, a, ui);
        }
        if bottom.area() == 0 {
            return;
        }

        // procesos
        let view = self.view();
        let mut title = format!(" {} {} · {} ", look.tx(l("procesos", "processes")), view.len(), look.tx(match self.sort {
            Sort::Cpu => l("por cpu", "by cpu"),
            Sort::Mem => l("por memoria", "by memory"),
        }));
        if self.group {
            title.push_str(&format!("· {} ", look.tx(l("por programa", "by program"))));
        }
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
        let cx = inner.x + 1 + 7 + 2 + nw as u16 + 1;
        self.hits.procs = inner;
        self.hits.head = inner.y;
        self.hits.cpu_col = (cx, cx + 6);
        self.hits.mem_col = (cx + 7, cx + 15);
        for (i, p) in view.iter().enumerate().skip(first).take(h.saturating_sub(1)) {
            let selected = i == self.sel;
            self.hits.rows.push((inner.y + 1 + (i - first) as u16, i));
            let name: String = p.name.chars().take(nw).collect();
            let room = nw.saturating_sub(name.chars().count() + 2);
            let args = if p.pids.len() > 1 { format!("×{}", p.pids.len()) } else { p.args.clone() };
            let args: String = if room > 0 { args.chars().take(room).collect() } else { String::new() };
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
        let prompt = if let Some((pids, name)) = &self.kill {
            let what = match pids.as_slice() {
                [pid] => format!("{} {name} ({pid})?", l("¿Cerrar", "Kill")),
                _ => format!("{} {} {name}?", l("¿Cerrar los", "Kill all"), pids.len()),
            };
            Some((
                format!(" {what} k {} · K {} · esc {} ", l("sí", "yes"), l("forzar", "force"), l("no", "no")),
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
        if let Some((pids, name)) = self.kill.take() {
            if !matches!(k.code, KeyCode::Char('k' | 'y' | 's' | 'K') | KeyCode::Esc | KeyCode::Char('n')) {
                self.kill = Some((pids, name)); // otra tecla no cuenta como respuesta
                return true;
            }
            let sig = match k.code {
                KeyCode::Char('k') | KeyCode::Char('y') | KeyCode::Char('s') => Some(libc::SIGTERM),
                KeyCode::Char('K') => Some(libc::SIGKILL),
                _ => None,
            };
            if let Some(sig) = sig {
                let ok = pids.iter().filter(|&&pid| unsafe { libc::kill(pid, sig) } == 0).count();
                if ok > 0 {
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
            KeyCode::Char('a') => {
                self.group = !self.group;
                self.sel = 0;
                self.follow = false;
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.sel = 0;
                self.follow = false;
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                if let Some(p) = self.view().get(self.sel) {
                    self.kill = Some((p.pids.clone(), p.name.clone()));
                }
            }
            // pkill: los que se llaman como la búsqueda (solo el nombre, como
            // pkill: por los argumentos caería hasta la terminal que lo lanzó), menos vato
            KeyCode::Char('X') => {
                if self.filter.is_empty() {
                    ui.say(l("Primero busca con /; X cierra los que se llamen así", "Search with / first; X kills the ones with that name"));
                } else {
                    let me = std::process::id() as i32;
                    let f = self.filter.to_lowercase();
                    let pids: Vec<i32> =
                        self.procs.iter().filter(|p| p.pid != me && p.name.to_lowercase().contains(&f)).map(|p| p.pid).collect();
                    if pids.is_empty() {
                        ui.say(format!("{} «{}»", l("Ningún proceso se llama", "No process is named"), self.filter));
                    } else {
                        self.kill = Some((pids, format!("«{}»", self.filter)));
                    }
                }
            }
            KeyCode::Char('v') if self.gpu.is_some() => self.gpu_view = !self.gpu_view,
            _ => return false,
        }
        true
    }

    fn scroll(&mut self, x: u16, y: u16, down: bool, _ui: &mut Ui) {
        if self.hits.gpu.contains(Position { x, y }) {
            let max = self.gpu_procs.len().saturating_sub(1);
            self.gpu_top = if down { (self.gpu_top + 1).min(max) } else { self.gpu_top.saturating_sub(1) };
        } else {
            self.move_sel(if down { 3 } else { -3 });
        }
    }

    fn click(&mut self, x: u16, y: u16, b: MouseButton, double: bool, ui: &mut Ui) {
        // con la pregunta de cerrar abierta, un clic en otro lado la cancela
        if self.kill.take().is_some() {
            return;
        }
        self.typing = false;
        if let Some(&(_, pid)) = self.hits.gpu_rows.iter().find(|r| r.0 == y && self.hits.gpu.contains(Position { x, y })) {
            self.select_pid(pid);
            if double || b == MouseButton::Right {
                self.press(KeyCode::Char('x'), ui);
            }
            return;
        }
        if !self.hits.procs.contains(Position { x, y }) {
            return;
        }
        if y == self.hits.head {
            if (self.hits.cpu_col.0..self.hits.cpu_col.1).contains(&x) {
                self.press(KeyCode::Char('c'), ui);
            } else if (self.hits.mem_col.0..self.hits.mem_col.1).contains(&x) {
                self.press(KeyCode::Char('m'), ui);
            }
            return;
        }
        let Some(&(_, i)) = self.hits.rows.iter().find(|r| r.0 == y) else { return };
        self.sel = i;
        self.follow = true;
        if double || b == MouseButton::Right {
            self.press(KeyCode::Char('x'), ui);
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.typing {
            return vec![("enter", l("listo", "done")), ("esc", l("borrar", "clear"))];
        }
        if self.kill.is_some() {
            return vec![("k", l("cerrar", "kill")), ("K", l("forzar", "force")), ("esc", l("no", "no"))];
        }
        let mut v = vec![
            ("j/k", l("mover", "move")),
            ("c/m", l("ordenar cpu/mem", "sort cpu/mem")),
            ("a", if self.group { l("desagrupar", "ungroup") } else { l("agrupar", "group") }),
            ("/", l("buscar", "search")),
            ("x", l("cerrar proceso", "kill")),
        ];
        if !self.filter.is_empty() {
            v.push(("X", l("cerrar por nombre", "pkill name")));
            v.push(("esc", l("quitar búsqueda", "clear search")));
        }
        if self.gpu.is_some() && !self.hits.procs.is_empty() && self.hits.gpu.is_empty() || self.gpu_view {
            v.push(("v", if self.gpu_view { l("procesos", "processes") } else { l("gpu", "gpu") }));
        }
        v
    }
}
