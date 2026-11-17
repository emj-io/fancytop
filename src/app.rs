//! Application state, sampling, and the panel layout.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ratatui::crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::gpu::{Bandwidth, BwState, Gpu, GpuStats, spawn_bandwidth_sampler};
use crate::sys::{self, Core, Proc};
use crate::theme::THEMES;
use crate::throughput::{IoRates, spawn_io_sampler};
use crate::ui::*;
use crate::wm::{Dir, PANELS, Panel, Side, Wm};

const HIST_LEN: usize = 2000;
/// Seconds between graph samples.
const HIST_DT: f64 = 0.1;
/// Columns between CPU core cells.
const CORE_GAP: i32 = 2;

/// Eases a displayed value toward its target so bars glide instead of jump, and keeps a
/// slowly decaying peak for the peak-hold marker.
#[derive(Default, Clone, Copy)]
struct Smooth {
    v: f64,
    peak: f64,
}

impl Smooth {
    fn step(&mut self, target: f64, dt: f64) -> f64 {
        self.v += (target - self.v) * (dt * 14.0).min(1.0);
        self.peak = (self.peak - 0.35 * dt).max(target);
        self.v
    }
}

fn push(q: &mut VecDeque<f64>, v: f64) {
    if q.len() == HIST_LEN {
        q.pop_front();
    }
    q.push_back(v);
}

#[derive(Default)]
struct Mem {
    wired: f64,
    app: f64,
    comp: f64,
    cached: f64,
    used: f64,
    free: f64,
}

/// Paging activity, bytes/s.
#[derive(Default)]
struct Rates {
    pagein: f64,
    pageout: f64,
    swapin: f64,
    swapout: f64,
    comp: f64,
    decomp: f64,
}

pub struct App {
    pub fps: u32,
    pub render_ms: f64,
    pub truecolor: bool,
    /// Whether the terminal is reporting the mouse to us (`m` toggles it).
    pub mouse: bool,
    /// Index into `THEMES` (`c` / `C` cycle it).
    pub theme: usize,
    pub wm: Wm,
    window: f64,
    start: Instant,

    ncpu: usize,
    cores: Vec<Core>,
    clusters: Vec<String>,
    model: String,
    mem_total: f64,
    page: f64,
    boot: i64,

    gpu: Gpu,
    procs: Arc<Mutex<Vec<Proc>>>,
    bw: Arc<Mutex<Bandwidth>>,
    io: Arc<Mutex<IoRates>>,

    tick_hist: VecDeque<(f64, Vec<u32>)>,
    core_val: Vec<f64>,
    core_smooth: Vec<Smooth>,
    core_spark: Vec<VecDeque<f64>>,
    user_frac: f64,
    sys_frac: f64,

    gpu_stats: Option<GpuStats>,
    gpu_smooth: [Smooth; 3],

    mem: Mem,
    vm_hist: VecDeque<(f64, [u64; 6])>,
    rates: Rates,
    swap: (f64, f64),
    pressure: f64,

    bw_now: Bandwidth,
    bw_smooth: Smooth,
    bw_peak: f64,

    io_now: IoRates,
    io_smooth: [Smooth; 4],
    /// Busiest combined throughput in the graph history, bytes/s: the scale for the I/O bars and graphs.
    /// Unlike DRAM bandwidth this decays, so one burst doesn't flatten the bars for the whole session.
    disk_peak: f64,
    net_peak: f64,

    cpu_hist: VecDeque<f64>,
    gpu_hist: VecDeque<f64>,
    mem_hist: VecDeque<f64>,
    bw_hist: VecDeque<f64>,
    disk_hist: VecDeque<f64>,
    net_hist: VecDeque<f64>,
    last_hist: f64,
    last_spark: f64,
    last_slow: f64,
}

impl App {
    pub fn new(fps: u32, window: f64) -> App {
        let ncpu = sys::cpu_ticks().len() / 4;
        let (cores, clusters) = sys::cpu_topology(ncpu);
        let colorterm = std::env::var("COLORTERM").unwrap_or_default().to_lowercase();
        let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
        App {
            fps,
            render_ms: 0.0,
            truecolor: colorterm == "truecolor"
                || colorterm == "24bit"
                || ["iTerm.app", "WezTerm", "ghostty", "vscode"].contains(&term_program.as_str()),
            mouse: true,
            theme: 0,
            wm: Wm::new(Box::new(move |p, w| min_size(ncpu as i32, p, w))),
            window,
            start: Instant::now(),
            ncpu,
            cores,
            clusters,
            model: sys::sysctl_string("machdep.cpu.brand_string").unwrap_or_else(|| "Mac".into()),
            mem_total: sys::sysctl_u64("hw.memsize").unwrap_or(1) as f64,
            page: sys::page_size(),
            boot: sys::boot_time().unwrap_or(0),
            gpu: Gpu::new(),
            procs: sys::spawn_process_watcher(),
            bw: spawn_bandwidth_sampler(),
            io: spawn_io_sampler(),
            tick_hist: VecDeque::new(),
            core_val: vec![0.0; ncpu],
            core_smooth: vec![Smooth::default(); ncpu],
            core_spark: (0..ncpu).map(|_| VecDeque::from(vec![0.0; 8])).collect(),
            user_frac: 0.0,
            sys_frac: 0.0,
            gpu_stats: None,
            gpu_smooth: [Smooth::default(); 3],
            mem: Mem::default(),
            vm_hist: VecDeque::new(),
            rates: Rates::default(),
            swap: (0.0, 0.0),
            pressure: 0.0,
            bw_now: Bandwidth {
                state: BwState::Starting,
                total: 0.0,
                cpu: 0.0,
                cpu_saturated: false,
            },
            bw_smooth: Smooth::default(),
            bw_peak: 100e9,
            io_now: IoRates::default(),
            io_smooth: [Smooth::default(); 4],
            disk_peak: 100e6,
            net_peak: 1e6,
            cpu_hist: VecDeque::new(),
            gpu_hist: VecDeque::new(),
            mem_hist: VecDeque::new(),
            bw_hist: VecDeque::new(),
            disk_hist: VecDeque::new(),
            net_hist: VecDeque::new(),
            last_hist: f64::NEG_INFINITY,
            last_spark: f64::NEG_INFINITY,
            last_slow: f64::NEG_INFINITY,
        }
    }

    // ------------------------------------------------------------------------ sampling

    pub fn sample(&mut self) {
        let now = self.start.elapsed().as_secs_f64();
        self.sample_cpu(now);
        self.gpu_stats = self.gpu.read().or(self.gpu_stats);
        self.sample_mem(now);
        self.bw_now = *crate::sys::lock(&self.bw);
        self.bw_peak = self.bw_peak.max(self.bw_now.total);
        self.io_now = *crate::sys::lock(&self.io);

        if now - self.last_slow >= 1.0 {
            self.last_slow = now;
            self.swap = sys::swap_usage();
            self.pressure = sys::sysctl_u64("kern.memorystatus_level").map_or(0.0, |l| 100.0 - l as f64);
        }
        if now - self.last_hist >= HIST_DT {
            self.last_hist = now;
            push(&mut self.cpu_hist, self.user_frac + self.sys_frac);
            push(&mut self.gpu_hist, self.gpu_stats.map_or(0.0, |g| g.device));
            push(&mut self.mem_hist, self.mem.used / self.mem_total);
            push(&mut self.bw_hist, self.bw_now.total);
            push(&mut self.disk_hist, self.io_now.disk_read + self.io_now.disk_write);
            push(&mut self.net_hist, self.io_now.net_in + self.io_now.net_out);
            self.disk_peak = self.disk_hist.iter().fold(100e6, |a, &b| a.max(b));
            self.net_peak = self.net_hist.iter().fold(1e6, |a, &b| a.max(b));
        }
        if now - self.last_spark >= 0.25 {
            self.last_spark = now;
            for (spark, &v) in self.core_spark.iter_mut().zip(&self.core_val) {
                spark.pop_front();
                spark.push_back(v);
            }
        }
    }

    /// Per-core load over the last `window` seconds. Ticks only advance 100 times a second,
    /// so diffing against an older snapshot keeps fast refreshes from looking quantised.
    fn sample_cpu(&mut self, now: f64) {
        let ticks = sys::cpu_ticks();
        if ticks.len() < self.ncpu * 4 {
            return;
        }
        self.tick_hist.push_back((now, ticks));
        while self.tick_hist.len() > 2 && self.tick_hist[1].0 <= now - self.window {
            self.tick_hist.pop_front();
        }
        let (old, new) = (&self.tick_hist[0].1, &self.tick_hist[self.tick_hist.len() - 1].1);
        let (mut tu, mut ts, mut tt) = (0u64, 0u64, 0u64);
        for c in 0..self.ncpu {
            let d = |k: usize| new[c * 4 + k].wrapping_sub(old[c * 4 + k]) as u64;
            let (u, s, idle, n) = (d(0), d(1), d(2), d(3));
            let total = u + s + idle + n;
            if total > 0 {
                self.core_val[c] = (u + s + n) as f64 / total as f64;
            }
            tu += u + n;
            ts += s;
            tt += total;
        }
        if tt > 0 {
            self.user_frac = tu as f64 / tt as f64;
            self.sys_frac = ts as f64 / tt as f64;
        }
    }

    fn sample_mem(&mut self, now: f64) {
        let Some(vm) = sys::vm_stats() else { return };
        let p = self.page;
        let wired = vm.wire_count as f64 * p;
        let app = vm.internal_page_count.saturating_sub(vm.purgeable_count) as f64 * p;
        // Under heavy pressure the compressor count overlaps other buckets, so cap it to the
        // RAM not already accounted for by free/active/inactive/wired pages.
        let accounted = (vm.free_count as f64
            + vm.speculative_count as f64
            + vm.active_count as f64
            + vm.inactive_count as f64
            + vm.throttled_count as f64
            + vm.wire_count as f64)
            * p;
        let comp = (vm.compressor_page_count as f64 * p).min(self.mem_total - accounted).max(0.0);
        let cached = (vm.external_page_count + vm.purgeable_count) as f64 * p;
        let used = wired + app + comp;
        self.mem = Mem {
            wired,
            app,
            comp,
            cached,
            used,
            free: (self.mem_total - used - cached).max(0.0),
        };

        let counters = [
            vm.pageins,
            vm.pageouts,
            vm.swapins,
            vm.swapouts,
            vm.compressions,
            vm.decompressions,
        ];
        self.vm_hist.push_back((now, counters));
        while self.vm_hist.len() > 2 && self.vm_hist[1].0 <= now - 1.0 {
            self.vm_hist.pop_front();
        }
        let (t0, c0) = self.vm_hist[0];
        let dt = now - t0;
        if dt > 0.0 {
            let r = |i: usize| counters[i].saturating_sub(c0[i]) as f64 / dt * p;
            self.rates = Rates {
                pagein: r(0),
                pageout: r(1),
                swapin: r(2),
                swapout: r(3),
                comp: r(4),
                decomp: r(5),
            };
        }
    }

    // ------------------------------------------------------------------------ windows

    /// Mouse input for moving and resizing windows. Returns whether anything changed.
    pub fn on_mouse(&mut self, ev: MouseEvent) -> bool {
        let (x, y) = (ev.column as i32, ev.row as i32);
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => self.wm.press(x, y),
            MouseEventKind::Drag(MouseButton::Left) => self.wm.drag_to(x, y),
            MouseEventKind::Up(MouseButton::Left) => self.wm.release(),
            _ => false,
        }
    }

    /// Window keys: 1-5 show or hide panels, arrows move focus and Shift+arrows move the focused
    /// window, Tab cycles focus, [ ] { } resize, t flips a split, z zooms, r resets the layout,
    /// c / C cycle the colour theme. Returns whether anything changed.
    pub fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        let side = match code {
            KeyCode::Left => Some(Side::Left),
            KeyCode::Right => Some(Side::Right),
            KeyCode::Up => Some(Side::Up),
            KeyCode::Down => Some(Side::Down),
            _ => None,
        };
        if let Some(side) = side {
            return if mods.contains(KeyModifiers::SHIFT) {
                self.wm.move_toward(side)
            } else {
                self.wm.focus_toward(side)
            };
        }
        match code {
            KeyCode::Char(c @ '1'..='5') => self.wm.toggle(PANELS[c as usize - '1' as usize]),
            KeyCode::Char('r' | 'R') => self.wm.reset(),
            KeyCode::Char('t' | 'T') => self.wm.flip(),
            KeyCode::Char('z' | 'Z') => self.wm.toggle_zoom(),
            KeyCode::Char('[') => self.wm.resize(Dir::Row, false),
            KeyCode::Char(']') => self.wm.resize(Dir::Row, true),
            KeyCode::Char('{') => self.wm.resize(Dir::Col, false),
            KeyCode::Char('}') => self.wm.resize(Dir::Col, true),
            KeyCode::Tab => self.wm.cycle(false),
            KeyCode::BackTab => self.wm.cycle(true),
            KeyCode::Char('c') => {
                self.theme = (self.theme + 1) % THEMES.len();
                true
            }
            KeyCode::Char('C') => {
                self.theme = (self.theme + THEMES.len() - 1) % THEMES.len();
                true
            }
            _ => false,
        }
    }

    // ------------------------------------------------------------------------ layout

    pub fn draw(&mut self, cv: &mut Canvas, dt: f64) {
        let t = &THEMES[self.theme];
        cv.theme = t;
        cv.clear(Area::new(0, 0, cv.w, cv.h));
        self.draw_header(cv);
        let body = Area::new(0, 1, cv.w, cv.h - 2);
        let auto = self.auto_layout(body);
        self.wm.layout(body, &auto);

        let focus = self.wm.highlight();
        for (panel, a) in self.wm.tiles() {
            cv.unclip();
            cv.clear(a);
            cv.border = if Some(panel) == focus { t.accent } else { t.border };
            let Area { x, y, w, h } = a;
            match panel {
                Panel::Cpu => self.draw_cpu(cv, x, y, w, h, dt),
                Panel::Gpu => self.draw_gpu(cv, x, y, w, h, dt),
                Panel::Mem => self.draw_mem(cv, x, y, w, h, dt),
                Panel::Io => self.draw_io(cv, x, y, w, h, dt),
                Panel::Procs => self.draw_procs(cv, x, y, w, h),
            }
        }
        cv.border = t.border;
        cv.unclip();
        if self.wm.resizing() {
            for (_, a) in self.wm.tiles() {
                let size = format!(" {}×{} ", a.w, a.h);
                cv.bold(a.right() - size.chars().count() as i32 - 2, a.bottom() - 1, &size, t.accent);
            }
        }
        if let Some((a, label)) = self.wm.preview() {
            cv.border = t.accent;
            draw_box(cv, a.x, a.y, a.w, a.h, &[vec![seg(label, t.accent, true)]]);
            let shade = "░".repeat((a.w - 2).max(0) as usize);
            for y in a.y + 1..a.bottom() - 1 {
                cv.put(a.x + 1, y, &shade, t.shade);
            }
            cv.border = t.border;
            cv.unclip();
        }
        self.draw_footer(cv);
    }

    /// The responsive layout, indexed by panel: CPU across the top, GPU and memory side by side
    /// (or stacked when narrow), then I/O and processes, each growing as the terminal allows.
    /// Hidden panels give their room to the others; a panel with no room gets an empty area.
    fn auto_layout(&self, body: Area) -> [Area; 5] {
        let shown = |p| self.wm.visible(p);
        let (cpu, gpu, mem, io, procs) = PANELS.map(shown).into();
        let (w, avail) = (body.w, body.h);
        let side_by_side = w >= 110 && gpu && mem;
        let mid_min = match (gpu || mem, gpu && mem && !side_by_side) {
            (false, _) => 0,
            (true, true) => 13,
            (true, false) => 9,
        };
        let core_rows = if cpu {
            core_grid(self.ncpu as i32, w, avail - 2 - mid_min).1
        } else {
            0
        };
        let cpu_min = if cpu { core_rows + 2 } else { 0 };

        let mut extra = avail - cpu_min - mid_min;
        let (mut cpu_graph, mut io_h, mut proc_h) = (0, 0, 0);
        let mut mid_h;
        if extra >= 0 {
            if cpu {
                cpu_graph = (extra / 3).min(6);
                extra -= cpu_graph;
            }
            // I/O's two rate rows come before GPU/memory graphs; its own graph only grows once
            // the process list is sure of at least 5 rows.
            let io_rows = if io && extra >= 4 { 4 } else { 0 };
            extra -= io_rows;
            let mid_extra = if gpu || mem {
                (extra / 2).min(if side_by_side { 8 } else { 10 })
            } else {
                0
            };
            extra -= mid_extra;
            let reserve = if procs { 5 } else { 0 };
            let io_graph = if io_rows > 0 { ((extra - reserve) / 2).clamp(0, 5) } else { 0 };
            io_h = io_rows + io_graph;
            extra -= io_graph;
            mid_h = mid_min + mid_extra;
            if procs && extra >= 5 {
                proc_h = extra;
            } else if gpu || mem {
                mid_h += extra;
            } else if io_h > 0 {
                io_h += extra;
            } else {
                cpu_graph += extra;
            }
        } else {
            mid_h = (avail - cpu_min).max(0);
        }

        let mut out = [Area::default(); 5];
        let mut y = body.y;
        let cpu_h = cpu_min + cpu_graph;
        out[Panel::Cpu as usize] = Area::new(0, y, w, cpu_h);
        y += cpu_h;
        if side_by_side {
            let gw = w / 2;
            out[Panel::Gpu as usize] = Area::new(0, y, gw, mid_h);
            out[Panel::Mem as usize] = Area::new(gw, y, w - gw, mid_h);
        } else if gpu && mem {
            // Memory needs 7 rows to reach its bandwidth line; GPU gives up its memory line first.
            let mem_h = (mid_h - mid_h / 2).max(mid_h.min(7));
            out[Panel::Gpu as usize] = Area::new(0, y, w, mid_h - mem_h);
            out[Panel::Mem as usize] = Area::new(0, y + mid_h - mem_h, w, mem_h);
        } else {
            let p = if gpu { Panel::Gpu } else { Panel::Mem };
            out[p as usize] = Area::new(0, y, w, mid_h);
        }
        y += mid_h;
        out[Panel::Io as usize] = Area::new(0, y, w, io_h);
        y += io_h;
        out[Panel::Procs as usize] = Area::new(0, y, w, proc_h);
        out
    }

    fn draw_header(&self, cv: &mut Canvas) {
        let t = cv.theme;
        let w = cv.w;
        cv.unclip();
        cv.fill_bg(0, t.header_bg);
        let title = " ◆ fancytop ";
        let tlen = title.chars().count() as i32;
        for (i, ch) in title.chars().enumerate() {
            let mut b = [0u8; 4];
            cv.styled(
                i as i32,
                0,
                ch.encode_utf8(&mut b),
                t.title.at(i as f64 / tlen as f64),
                Some(t.header_bg),
                true,
            );
        }
        let mut la = [0f64; 3];
        unsafe { libc::getloadavg(la.as_mut_ptr(), 3) };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let clock = local_hms(now);
        let mut right = format!(
            "load {:.2} {:.2} {:.2} │ up {} │ {} ",
            la[0],
            la[1],
            la[2],
            fmt_uptime(now - self.boot),
            clock
        );
        if tlen + right.chars().count() as i32 > w {
            right = format!("{clock} ");
        }
        let rlen = right.chars().count() as i32;
        cv.styled(w - rlen, 0, &right, t.fg, Some(t.header_bg), false);
        let room = w - tlen - rlen;
        let full = format!(
            " {} · {} cores · {}-core GPU · {} ",
            self.model,
            self.ncpu,
            self.gpu.cores,
            fmt_bytes(self.mem_total, 0)
        );
        for info in [full, format!(" {} ", self.model)] {
            if info.chars().count() as i32 <= room {
                cv.styled(tlen, 0, &info, t.dim, Some(t.header_bg), false);
                break;
            }
        }
    }

    fn draw_footer(&self, cv: &mut Canvas) {
        let t = cv.theme;
        let (y, w) = (cv.h - 1, cv.w);
        cv.put(0, y, &" ".repeat(w.max(0) as usize), t.fg);
        let mut keys = vec![("q", "quit"), ("+/-", "speed"), ("1-5", "panels")];
        if self.wm.zoom {
            keys.push(("z", "unzoom"));
        }
        if self.wm.manual {
            keys.push(("r", "reset layout"));
        }
        keys.push(("m", if self.mouse { "mouse" } else { "mouse (off)" }));
        let theme = format!("theme: {}", t.name);
        keys.push(("c", &theme));
        let mut x = 1;
        for (key, label) in keys {
            cv.styled(x, y, key, t.on_accent, Some(t.accent), true);
            x += key.len() as i32;
            cv.put(x, y, &format!(" {label}  "), t.dim);
            x += label.len() as i32 + 3;
        }
        let stat = format!(
            "{} fps · {:.2}ms/frame · {}",
            self.fps,
            self.render_ms,
            if self.truecolor { "truecolor" } else { "256 colour" }
        );
        let slen = stat.chars().count() as i32;
        if x + slen < w {
            cv.put(w - slen - 1, y, &stat, t.faint);
        }
    }

    fn draw_cpu(&mut self, cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, dt: f64) {
        let t = cv.theme;
        let total = self.user_frac + self.sys_frac;
        let mut title = vec![vec![
            seg("CPU ", t.hi, true),
            seg(format!("{:3.0}%", total * 100.0), t.heat.at(total), true),
            seg(
                format!("  usr {:.0}%  sys {:.0}%", self.user_frac * 100.0, self.sys_frac * 100.0),
                t.dim,
                false,
            ),
        ]];
        for (ci, name) in self.clusters.iter().enumerate() {
            let ids: Vec<usize> = self.cores.iter().filter(|c| c.cluster == ci).map(|c| c.id).collect();
            let avg = ids.iter().map(|&c| self.core_val[c]).sum::<f64>() / ids.len().max(1) as f64;
            title.push(vec![
                seg("  │  ", t.faint, false),
                seg(format!("{name}×{} ", ids.len()), t.fg, false),
                seg(format!("{:.0}%", avg * 100.0), t.heat.at(avg), true),
            ]);
        }
        draw_box(cv, x, y, w, h, &title);

        let (cols, rows) = core_grid(self.ncpu as i32, w, h - 2);
        let (graph_h, gap) = (h - 2 - rows, CORE_GAP);
        let (ix, iw) = (x + 2, w - 4);
        let cell_w = (iw - gap * (cols - 1)) / cols;
        let show_spark = cell_w >= 28;
        let label_w = self.cores.iter().map(|c| c.label.len()).max().unwrap_or(3) as i32;
        let bar_w = cell_w - label_w - 1 - 5 - if show_spark { 9 } else { 0 };
        let nclusters = self.clusters.len().max(1) as f64;
        for (k, core) in self.cores.iter().enumerate() {
            let (r, c) = (k as i32 % rows, k as i32 / rows);
            let (cx, cy) = (ix + c * (cell_w + gap), y + 1 + r);
            let sm = &mut self.core_smooth[core.id];
            let v = sm.step(self.core_val[core.id], dt);
            cv.bold(
                cx,
                cy,
                &format!("{:<width$}", core.label, width = label_w as usize),
                t.title.at(core.cluster as f64 / nclusters),
            );
            let mut bx = cx + label_w + 1;
            if show_spark {
                draw_spark(cv, bx, cy, self.core_spark[core.id].iter(), &t.heat);
                bx += 9;
            }
            draw_bar(cv, bx, cy, bar_w, v, &t.heat, Some(sm.peak));
            cv.put(
                bx + bar_w + 1,
                cy,
                &format!("{:3.0}%", v * 100.0),
                if v > 0.02 { t.heat.at(v) } else { t.dim },
            );
        }
        if graph_h > 0 {
            draw_graph(cv, ix, y + 1 + rows, iw, graph_h, self.cpu_hist.iter(), &t.heat);
        }
    }

    fn draw_gpu(&mut self, cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, dt: f64) {
        let t = cv.theme;
        let g = self.gpu_stats.unwrap_or_default();
        let dev = self.gpu_smooth[0].step(g.device, dt);
        let ren = self.gpu_smooth[1].step(g.renderer, dt);
        let til = self.gpu_smooth[2].step(g.tiler, dt);
        let title = [vec![
            seg("GPU ", t.hi, true),
            seg(format!("{:3.0}%", dev * 100.0), t.gpu.at(dev), true),
            seg(format!("  {} · {} cores", self.gpu.model, self.gpu.cores), t.dim, false),
        ]];
        draw_box(cv, x, y, w, h, &title);
        let (ix, iw) = (x + 2, w - 4);
        let bar_w = iw - 9 - 5;
        for (i, (label, v, sm)) in [("Device", dev, 0), ("Render", ren, 1), ("Tiler", til, 2)]
            .into_iter()
            .enumerate()
        {
            let ry = y + 1 + i as i32;
            cv.put(ix, ry, label, t.fg);
            draw_bar(cv, ix + 9, ry, bar_w, v, &t.gpu, Some(self.gpu_smooth[sm].peak));
            cv.put(
                ix + 9 + bar_w + 1,
                ry,
                &format!("{:3.0}%", v * 100.0),
                if v > 0.02 { t.gpu.at(v) } else { t.dim },
            );
        }
        if self.gpu_stats.is_some() {
            cv.put(ix, y + 4, "Memory", t.fg);
            let in_use = format!("{} in use", fmt_bytes(g.mem_in_use, 1));
            cv.put(ix + 9, y + 4, &in_use, t.gpu.at(0.8));
            cv.put(
                ix + 10 + in_use.len() as i32,
                y + 4,
                &format!("· {} allocated", fmt_bytes(g.mem_alloc, 1)),
                t.dim,
            );
        } else {
            cv.put(ix, y + 4, "GPU statistics unavailable", t.dim);
        }
        let gh = h - 7;
        if gh > 0 {
            draw_graph(cv, ix, y + 6, iw, gh, self.gpu_hist.iter(), &t.gpu);
        }
    }

    fn draw_mem(&mut self, cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, dt: f64) {
        let t = cv.theme;
        let (m, total_mem) = (&self.mem, self.mem_total);
        let used_frac = m.used / total_mem;
        let title = [vec![
            seg("MEMORY ", t.hi, true),
            seg(fmt_bytes(m.used, 1), t.mem.at(used_frac), true),
            seg(
                format!(" / {}  ({:.0}%)", fmt_bytes(total_mem, 0), used_frac * 100.0),
                t.dim,
                false,
            ),
        ]];
        draw_box(cv, x, y, w, h, &title);
        let (ix, iw) = (x + 2, w - 4);

        // Stacked breakdown bar + legend.
        let segs = [
            ("Wired", m.wired, t.mem_wired),
            ("App", m.app, t.mem_app),
            ("Compressed", m.comp, t.mem_comp),
            ("Cached", m.cached, t.mem_cache),
            ("Free", m.free, t.mem_free),
        ];
        let (mut acc, mut px) = (0.0, 0);
        for &(_, size, color) in &segs {
            acc += size;
            let end = ((acc / total_mem * iw as f64).round() as i32).min(iw);
            if end > px {
                cv.put(ix + px, y + 1, &"█".repeat((end - px) as usize), color);
            }
            px = px.max(end);
        }
        let mut lx = ix;
        for &(name, size, color) in &segs {
            let text = format!("{name} {}", fmt_bytes(size, 1));
            let tlen = text.chars().count() as i32;
            if lx + tlen + 2 > ix + iw {
                break;
            }
            cv.put(lx, y + 2, "■", color);
            cv.put(lx + 2, y + 2, &text, t.fg);
            lx += tlen + 4;
        }

        // Swap + pressure.
        let (su, st) = self.swap;
        let half = (iw - 2) / 2;
        cv.put(ix, y + 3, "Swap", t.fg);
        let swap_txt = format!("{}/{}", fmt_bytes(su, 1), fmt_bytes(st, 1));
        let sbw = (half - 6 - swap_txt.len() as i32).max(4);
        draw_bar(cv, ix + 5, y + 3, sbw, if st > 0.0 { su / st } else { 0.0 }, &t.mem, None);
        cv.put(ix + 6 + sbw, y + 3, &swap_txt, t.dim);
        let px0 = ix + half + 2;
        let pbw = (iw - (px0 - ix) - 9 - 5).max(4);
        let pressure = self.pressure / 100.0;
        cv.put(px0, y + 3, "Pressure", t.fg);
        draw_bar(cv, px0 + 9, y + 3, pbw, pressure, &t.heat, None);
        cv.put(
            px0 + 9 + pbw + 1,
            y + 3,
            &format!("{:3.0}%", self.pressure),
            t.heat.at(pressure),
        );

        // Paging activity.
        let r = &self.rates;
        let items = [
            ("in", r.pagein),
            ("out", r.pageout),
            ("swap↓", r.swapin),
            ("swap↑", r.swapout),
            ("zip", r.comp),
            ("unzip", r.decomp),
        ];
        cv.put(ix, y + 4, "Paging", t.fg);
        let mut lx = ix + 7;
        for (label, val) in items {
            let val_txt = fmt_rate(val);
            let (ll, vl) = (label.chars().count() as i32, val_txt.len() as i32);
            if lx + ll + vl + 3 > ix + iw {
                break;
            }
            cv.put(lx, y + 4, label, t.dim);
            cv.put(
                lx + ll + 1,
                y + 4,
                &val_txt,
                if val >= 1.0 {
                    t.mem.at((val / (64u64 << 20) as f64).min(1.0))
                } else {
                    t.faint
                },
            );
            lx += ll + vl + 3;
        }

        // DRAM bandwidth.
        cv.put(ix, y + 5, "Bandwidth", t.fg);
        let bw = self.bw_now;
        match bw.state {
            BwState::Ok => {
                let total = self.bw_smooth.step(bw.total, dt);
                let frac = total / self.bw_peak;
                let cpu = bw.cpu.min(total);
                // Saturated CPU counters only give a floor, so say so instead of overstating "other".
                let (ge, le) = if bw.cpu_saturated { ("≥", "≤") } else { ("", "") };
                let split = format!("  CPU {ge}{} · GPU & other {le}{}", fmt_bw(cpu), fmt_bw(total - cpu));
                let val = format!("{:>9}", fmt_bw(total));
                let show_split = iw >= 70;
                let bar_w = (iw - 10 - val.len() as i32 - 1 - if show_split { split.chars().count() as i32 } else { 0 }).max(4);
                draw_bar(
                    cv,
                    ix + 10,
                    y + 5,
                    bar_w,
                    frac,
                    &t.mem,
                    Some(self.bw_smooth.peak / self.bw_peak),
                );
                cv.bold(ix + 11 + bar_w, y + 5, &val, t.mem.at(0.4 + 0.6 * frac));
                if show_split {
                    cv.put(ix + 11 + bar_w + val.len() as i32, y + 5, &split, t.dim);
                }
            }
            BwState::Starting => cv.put(ix + 10, y + 5, "…", t.faint),
            BwState::Unavailable => cv.put(ix + 10, y + 5, "unavailable on this Mac", t.faint),
        }

        let gh = h - 8;
        if gh > 0 {
            if bw.state == BwState::Unavailable {
                draw_graph(cv, ix, y + 7, iw, gh, self.mem_hist.iter(), &t.mem);
            } else {
                let scaled: Vec<f64> = self.bw_hist.iter().map(|v| v / self.bw_peak).collect();
                draw_graph(cv, ix, y + 7, iw, gh, scaled.iter(), &t.mem);
                cv.put(ix, y + 7, &format!("DRAM bandwidth · peak {} ", fmt_bw(self.bw_peak)), t.dim);
            }
        }
    }

    fn draw_io(&mut self, cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, dt: f64) {
        let t = cv.theme;
        let r = self.io_now;
        let (disk, net) = (r.disk_read + r.disk_write, r.net_in + r.net_out);
        let title = [
            vec![seg("I/O", t.hi, true)],
            vec![
                seg("  │  ", t.faint, false),
                seg("Disk ", t.fg, false),
                seg(fmt_rate(disk), t.disk.at(disk / self.disk_peak), true),
            ],
            vec![
                seg("  │  ", t.faint, false),
                seg("Network ", t.fg, false),
                seg(fmt_rate(net), t.net.at(net / self.net_peak), true),
            ],
        ];
        draw_box(cv, x, y, w, h, &title);

        let (ix, iw) = (x + 2, w - 4);
        let col_gap = 4;
        let half = (iw - col_gap) / 2;
        let gh = h - 4;
        // Disk on the left, network on the right: label(6) bar value(9), then a graph.
        for side in 0..2 {
            let (name, rows, peak, hist, grad) = if side == 0 {
                (
                    "disk",
                    [("Read", r.disk_read), ("Write", r.disk_write)],
                    self.disk_peak,
                    &self.disk_hist,
                    &t.disk,
                )
            } else {
                (
                    "network",
                    [("In", r.net_in), ("Out", r.net_out)],
                    self.net_peak,
                    &self.net_hist,
                    &t.net,
                )
            };
            let sx = ix + side * (half + col_gap);
            let bar_w = half - 6 - 10;
            for (i, (label, rate)) in rows.into_iter().enumerate() {
                let ry = y + 1 + i as i32;
                let smooth = &mut self.io_smooth[side as usize * 2 + i];
                // Smoothed as a fraction of the peak, so the peak-hold marker decays at the same pace as other bars.
                let frac = smooth.step(rate / peak, dt);
                cv.put(sx, ry, label, t.fg);
                draw_bar(cv, sx + 6, ry, bar_w, frac, grad, Some(smooth.peak));
                cv.put(
                    sx + 6 + bar_w.max(0) + 1,
                    ry,
                    &format!("{:>9}", fmt_rate(frac * peak)),
                    if frac * peak >= 1.0 {
                        grad.at(0.4 + 0.6 * frac)
                    } else {
                        t.faint
                    },
                );
            }
            if gh > 0 {
                let scaled: Vec<f64> = hist.iter().map(|v| v / peak).collect();
                draw_graph(cv, sx, y + 3, half, gh, scaled.iter(), grad);
                cv.put(sx, y + 3, &format!("{name} · peak {} ", fmt_rate(peak)), t.dim);
            }
        }
    }

    fn draw_procs(&self, cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32) {
        let t = cv.theme;
        let rows = crate::sys::lock(&self.procs).clone();
        let (ix, iw) = (x + 2, w - 4);
        let ncols = if iw >= 130 { 2 } else { 1 };
        let per_col = (h - 3).max(0) as usize;
        let shown = &rows[..rows.len().min(per_col * ncols as usize)];
        draw_box(
            cv,
            x,
            y,
            w,
            h,
            &[vec![
                seg("PROCESSES ", t.hi, true),
                seg(format!("top {} by CPU", shown.len()), t.dim, false),
            ]],
        );
        let col_gap = 4;
        let col_w = (iw - col_gap * (ncols - 1)) / ncols;
        // columns: pid(7) _ name _ cpu%(6) _ bar _ mem(7)
        let bar_w = (col_w / 5).clamp(6, 24);
        let name_w = (col_w - 24 - bar_w).max(4);
        let top = shown.iter().map(|p| p.cpu).fold(100.0, f64::max);
        for c in 0..ncols {
            let cx = ix + c * (col_w + col_gap);
            let (x_name, x_cpu) = (cx + 8, cx + 8 + name_w + 1);
            let x_bar = x_cpu + 7;
            let x_mem = x_bar + bar_w + 1;
            cv.bold(cx, y + 1, &format!("{:>7}", "PID"), t.dim);
            cv.bold(x_name, y + 1, "NAME", t.dim);
            cv.bold(x_cpu, y + 1, &format!("{:>6}", "CPU%"), t.dim);
            cv.bold(x_mem, y + 1, &format!("{:>7}", "MEM"), t.dim);
            let start = c as usize * per_col;
            for (i, p) in shown.iter().skip(start).take(per_col).enumerate() {
                let ry = y + 2 + i as i32;
                cv.put(cx, ry, &format!("{:>7}", p.pid), t.dim);
                cv.put(
                    x_name,
                    ry,
                    &p.name.chars().take(name_w as usize).collect::<String>(),
                    if p.cpu >= 1.0 { t.fg } else { t.dim },
                );
                cv.put(
                    x_cpu,
                    ry,
                    &format!("{:6.1}", p.cpu),
                    if p.cpu >= 1.0 {
                        t.heat.at((p.cpu / 100.0).min(1.0))
                    } else {
                        t.dim
                    },
                );
                draw_bar(cv, x_bar, ry, bar_w, p.cpu / top, &t.heat, None);
                cv.put(x_mem, ry, &format!("{:>7}", fmt_bytes(p.rss, 1)), t.mem.at(0.3));
            }
        }
    }
}

/// Columns and rows of the CPU core grid in a box `w` wide: roomy cells (with sparklines) when
/// that takes no more than `max_rows`, otherwise packed tighter.
fn core_grid(ncpu: i32, w: i32, max_rows: i32) -> (i32, i32) {
    let inner = w - 4;
    let (mut cols, mut rows) = (1, ncpu);
    for min_cell in [34, 16] {
        cols = ((inner + CORE_GAP) / (min_cell + CORE_GAP)).max(1);
        rows = (ncpu + cols - 1) / cols;
        if rows <= max_rows {
            break;
        }
    }
    (cols, rows)
}

/// Smallest (width, height) a panel can be dragged to at a given width: room for its bars and
/// readouts, without graphs.
fn min_size(ncpu: i32, p: Panel, w: i32) -> (i32, i32) {
    let h = match p {
        Panel::Cpu => core_grid(ncpu, w, 0).1 + 2,
        Panel::Gpu => 6,
        Panel::Mem => 7,
        Panel::Io => 4,
        Panel::Procs => 5,
    };
    (24, h)
}

/// Local wall-clock time as HH:MM:SS.
fn local_hms(epoch: i64) -> String {
    let t = epoch as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}
