//! CPU, memory and process readers built on Mach, sysctl and IOKit.

use std::collections::{HashMap, HashSet};
use std::ffi::{CString, c_void};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};
use std::{ptr, thread};

use crate::ffi::{self, cf_to_bytes, cf_to_i64, cfstr, registry_property};

// ---------------------------------------------------------------------------- sysctl

pub fn sysctl_raw(name: &str, size: usize) -> Option<Vec<u8>> {
    let c = CString::new(name).ok()?;
    let mut buf = vec![0u8; size];
    let mut len = size;
    let r = unsafe { libc::sysctlbyname(c.as_ptr(), buf.as_mut_ptr() as *mut c_void, &mut len, ptr::null_mut(), 0) };
    if r != 0 {
        return None;
    }
    buf.truncate(len);
    Some(buf)
}

pub fn sysctl_u64(name: &str) -> Option<u64> {
    let raw = sysctl_raw(name, 8)?;
    match raw.len() {
        4 => Some(u32::from_ne_bytes(raw[..4].try_into().ok()?) as u64),
        8 => Some(u64::from_ne_bytes(raw[..8].try_into().ok()?)),
        _ => None,
    }
}

/// Lock a mutex, carrying on with the data inside if a sampler thread panicked while holding it,
/// so one dead sampler shows stale numbers instead of taking the UI down with it.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn sysctl_string(name: &str) -> Option<String> {
    let raw = sysctl_raw(name, 256)?;
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    Some(String::from_utf8_lossy(&raw[..end]).into_owned())
}

/// Swap (used, total) in bytes, from `vm.swapusage` (`struct xsw_usage`).
pub fn swap_usage() -> (f64, f64) {
    match sysctl_raw("vm.swapusage", 32) {
        Some(raw) => {
            let word = |at: usize| Some(u64::from_ne_bytes(raw.get(at..at + 8)?.try_into().ok()?));
            match (word(0), word(16)) {
                (Some(total), Some(used)) => (used as f64, total as f64),
                _ => (0.0, 0.0),
            }
        }
        None => (0.0, 0.0),
    }
}

/// Seconds since the epoch at boot, from `kern.boottime` (`struct timeval`).
pub fn boot_time() -> Option<i64> {
    let raw = sysctl_raw("kern.boottime", 16)?;
    Some(i64::from_ne_bytes(raw.get(0..8)?.try_into().ok()?))
}

// ---------------------------------------------------------------------------- Mach

fn host() -> ffi::mach_port_t {
    static HOST: OnceLock<ffi::mach_port_t> = OnceLock::new();
    *HOST.get_or_init(|| unsafe { ffi::mach_host_self() })
}

/// Flat `[user, system, idle, nice]` tick counters for every logical CPU.
pub fn cpu_ticks() -> Vec<u32> {
    let (mut n, mut info, mut cnt) = (0u32, ptr::null_mut::<i32>(), 0u32);
    unsafe {
        if ffi::host_processor_info(host(), ffi::PROCESSOR_CPU_LOAD_INFO, &mut n, &mut info, &mut cnt) != 0 || info.is_null() {
            return Vec::new();
        }
        let ticks = std::slice::from_raw_parts(info as *const u32, n as usize * 4).to_vec();
        ffi::vm_deallocate(ffi::mach_task_self_, info as usize, cnt as usize * 4);
        ticks
    }
}

pub fn vm_stats() -> Option<ffi::vm_statistics64> {
    let mut s = ffi::vm_statistics64::default();
    let mut count = (std::mem::size_of::<ffi::vm_statistics64>() / 4) as u32;
    let r = unsafe { ffi::host_statistics64(host(), ffi::HOST_VM_INFO64, &mut s as *mut _ as *mut i32, &mut count) };
    (r == 0).then_some(s)
}

pub fn page_size() -> f64 {
    let mut ps = 0usize;
    unsafe { ffi::host_page_size(host(), &mut ps) };
    if ps == 0 { 16384.0 } else { ps as f64 }
}

// ---------------------------------------------------------------------------- CPU topology

pub struct Core {
    /// Index into the per-CPU tick array.
    pub id: usize,
    /// Index into the cluster name list (fastest cluster first).
    pub cluster: usize,
    pub label: String,
}

/// Cores ordered fastest cluster first, plus the cluster names (e.g. "Super", "Performance").
pub fn cpu_topology(ncpu: usize) -> (Vec<Core>, Vec<String>) {
    let levels: Vec<String> = (0..sysctl_u64("hw.nperflevels").unwrap_or(0))
        .map(|i| sysctl_string(&format!("hw.perflevel{i}.name")).unwrap_or_else(|| format!("Cluster {i}")))
        .collect();

    // Each CPU's IORegistry node carries its logical id and cluster type (P, M or E).
    let mut kinds = vec![None::<char>; ncpu];
    let (k_id, k_type) = (cfstr("logical-cpu-id"), cfstr("cluster-type"));
    for entry in ffi::matching_services("IOPlatformDevice") {
        let id = cf_to_i64(registry_property(entry, k_id).0);
        let kind = cf_to_bytes(registry_property(entry, k_type).0).and_then(|b| b.first().map(|&c| c as char));
        if let (Some(id), Some(kind)) = (id, kind)
            && (id as usize) < ncpu
        {
            kinds[id as usize] = Some(kind);
        }
        unsafe { ffi::IOObjectRelease(entry) };
    }

    let rank = |k: Option<char>| match k {
        Some('P') => 0,
        Some('M') => 1,
        Some('E') => 2,
        _ => 9,
    };
    let mut types: Vec<Option<char>> = kinds.clone();
    types.sort_by_key(|&k| rank(k));
    types.dedup();
    let names: Vec<String> = types
        .iter()
        .enumerate()
        .map(|(i, k)| match k {
            None => "CPU".to_string(),
            Some(c) => levels.get(i).cloned().unwrap_or_else(|| c.to_string()),
        })
        .collect();

    let mut order: Vec<usize> = (0..ncpu).collect();
    order.sort_by_key(|&c| (rank(kinds[c]), c));
    let mut counts = vec![0usize; names.len()];
    let mut cores: Vec<(usize, usize, usize)> = Vec::with_capacity(ncpu);
    for c in order {
        let cluster = types.iter().position(|&t| t == kinds[c]).unwrap_or(0);
        cores.push((c, cluster, counts[cluster]));
        counts[cluster] += 1;
    }
    let width = counts
        .iter()
        .map(|&n| n.saturating_sub(1).to_string().len())
        .max()
        .unwrap_or(1);
    let cores = cores
        .into_iter()
        .map(|(id, cluster, idx)| {
            let initial = names[cluster].chars().next().unwrap_or('C');
            Core {
                id,
                cluster,
                label: format!("{initial}{idx:0width$}"),
            }
        })
        .collect();
    (cores, names)
}

// ---------------------------------------------------------------------------- processes

#[derive(Clone)]
pub struct Proc {
    pub pid: u32,
    pub cpu: f64,
    pub rss: f64,
    pub name: String,
}

/// Process list, refreshed on a background thread.
///
/// Processes you own are read straight from libproc twice a second, with CPU% measured over that
/// interval (a full sweep costs well under a millisecond). Without root, libproc can't read
/// root-owned processes at all, so those come from `ps`, which is setuid root, every 2 seconds. A `ps`
/// call costs ~15ms of CPU, and its %CPU is macOS's short decaying average rather than a live reading.
pub fn spawn_process_watcher() -> Arc<Mutex<Vec<Proc>>> {
    let shared = Arc::new(Mutex::new(Vec::new()));
    let out = shared.clone();
    thread::spawn(move || {
        let mut tb = ffi::mach_timebase_info::default();
        unsafe { ffi::mach_timebase_info(&mut tb) };
        let ns_per_tick = if tb.denom == 0 {
            1.0
        } else {
            tb.numer as f64 / tb.denom as f64
        };

        // Keyed by (pid, start time) so a recycled pid isn't mistaken for the old process.
        let mut prev: HashMap<(i32, u64), u64> = HashMap::new();
        let mut names: HashMap<(i32, u64), String> = HashMap::new();
        let mut privileged: Vec<Proc> = Vec::new();
        let mut last_ps: Option<Instant> = None;
        let mut pids: Vec<i32> = vec![0; 4096];
        let mut prev_t = Instant::now();
        loop {
            let now = Instant::now();
            let dt = (now - prev_t).as_secs_f64();
            prev_t = now;

            let mut n = unsafe { ffi::proc_listallpids(pids.as_mut_ptr() as *mut c_void, (pids.len() * 4) as i32) };
            if n > 0 && n as usize >= pids.len() {
                pids.resize(n as usize * 2, 0);
                n = unsafe { ffi::proc_listallpids(pids.as_mut_ptr() as *mut c_void, (pids.len() * 4) as i32) };
            }
            let mut rows = Vec::new();
            let mut cur = HashMap::with_capacity(prev.len());
            let mut readable = HashSet::new();
            for &pid in &pids[..n.max(0) as usize] {
                let mut ri = ffi::rusage_info_v0::default();
                if pid <= 0 || unsafe { ffi::proc_pid_rusage(pid, ffi::RUSAGE_INFO_V0, &mut ri) } != 0 {
                    continue;
                }
                readable.insert(pid as u32);
                let key = (pid, ri.proc_start_abstime);
                let ticks = ri.user_time + ri.system_time;
                cur.insert(key, ticks);
                if let Some(&before) = prev.get(&key) {
                    let cpu = ticks.saturating_sub(before) as f64 * ns_per_tick / (dt * 1e9) * 100.0;
                    let name = names.entry(key).or_insert_with(|| process_name(pid)).clone();
                    rows.push(Proc {
                        pid: pid as u32,
                        cpu,
                        rss: ri.resident_size as f64,
                        name,
                    });
                }
            }
            let first_sweep = prev.is_empty();
            names.retain(|k, _| cur.contains_key(k));
            prev = cur;

            if last_ps.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) {
                privileged = ps_snapshot();
                last_ps = Some(now);
            }
            rows.extend(privileged.iter().filter(|p| !readable.contains(&p.pid)).cloned());
            rows.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
            rows.truncate(80);
            if !first_sweep {
                *lock(&out) = rows;
            }
            thread::sleep(Duration::from_millis(500));
        }
    });
    shared
}

/// Executable name for a process we own: the basename of its path, falling back to the short name.
fn process_name(pid: i32) -> String {
    let mut buf = vec![0u8; ffi::PROC_PIDPATHINFO_MAXSIZE];
    let len = unsafe { ffi::proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    if len > 0 {
        let path = String::from_utf8_lossy(&buf[..len as usize]);
        if let Some(base) = path.rsplit('/').next().filter(|b| !b.is_empty()) {
            return base.to_string();
        }
    }
    let len = unsafe { ffi::proc_name(pid, buf.as_mut_ptr() as *mut c_void, 64) };
    String::from_utf8_lossy(&buf[..len.max(0) as usize]).into_owned()
}

/// Every process as `ps` sees it (pid, %CPU, resident size, name).
fn ps_snapshot() -> Vec<Proc> {
    let Ok(o) = Command::new("ps").args(["-Aceo", "pid=,pcpu=,rss=,comm="]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let cpu = it.next()?.parse().ok()?;
            let rss: f64 = it.next()?.parse().ok()?;
            let name = it.collect::<Vec<_>>().join(" ");
            Some(Proc {
                pid,
                cpu,
                rss: rss * 1024.0,
                name,
            })
        })
        .collect()
}
