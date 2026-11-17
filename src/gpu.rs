//! GPU utilisation (IOKit) and DRAM bandwidth (IOReport).

use std::collections::HashMap;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::ffi::{self, CFDictionaryRef, CFMutableDictionaryRef, CFStringRef, cf_to_i64, cf_to_string, cfstr, registry_property};

// ---------------------------------------------------------------------------- GPU

#[derive(Default, Clone, Copy)]
pub struct GpuStats {
    pub device: f64,
    pub renderer: f64,
    pub tiler: f64,
    pub mem_in_use: f64,
    pub mem_alloc: f64,
}

pub struct Gpu {
    entry: ffi::io_object_t,
    stats_key: CFStringRef,
    keys: [CFStringRef; 5],
    pub model: String,
    pub cores: i64,
}

impl Gpu {
    pub fn new() -> Gpu {
        let mut gpu = Gpu {
            entry: 0,
            stats_key: cfstr("PerformanceStatistics"),
            keys: [ptr::null(); 5],
            model: "GPU".into(),
            cores: 0,
        };
        gpu.keys = [
            cfstr("Device Utilization %"),
            cfstr("Renderer Utilization %"),
            cfstr("Tiler Utilization %"),
            cfstr("In use system memory"),
            cfstr("Alloc system memory"),
        ];
        let entries = ffi::matching_services("IOAccelerator");
        if let Some(&entry) = entries.first() {
            gpu.entry = entry;
            let model = cf_to_string(registry_property(entry, cfstr("model")).0);
            if !model.is_empty() {
                gpu.model = model;
            }
            gpu.cores = cf_to_i64(registry_property(entry, cfstr("gpu-core-count")).0).unwrap_or(0);
        }
        for &e in entries.iter().skip(1) {
            unsafe { ffi::IOObjectRelease(e) };
        }
        gpu
    }

    pub fn read(&self) -> Option<GpuStats> {
        if self.entry == 0 {
            return None;
        }
        let dict = registry_property(self.entry, self.stats_key);
        if dict.0.is_null() {
            return None;
        }
        let get = |k: CFStringRef| cf_to_i64(unsafe { ffi::CFDictionaryGetValue(dict.0, k) }).unwrap_or(0) as f64;
        Some(GpuStats {
            device: get(self.keys[0]) / 100.0,
            renderer: get(self.keys[1]) / 100.0,
            tiler: get(self.keys[2]) / 100.0,
            mem_in_use: get(self.keys[3]),
            mem_alloc: get(self.keys[4]),
        })
    }
}

// ---------------------------------------------------------------------------- DRAM bandwidth

#[derive(Clone, Copy, PartialEq)]
pub enum BwState {
    Starting,
    Ok,
    Unavailable,
}

#[derive(Clone, Copy)]
pub struct Bandwidth {
    pub state: BwState,
    /// Total DRAM traffic, bytes/s.
    pub total: f64,
    /// Portion attributed to the CPU clusters, bytes/s.
    pub cpu: f64,
    /// A CPU cluster's counter is pinned in its top bucket, so `cpu` is only a lower bound.
    pub cpu_saturated: bool,
}

/// DRAM bandwidth from the memory controllers' IOReport counters (private API, no root needed).
///
/// Each "DCS BW" channel is a histogram: thousands of times a second the controller bins the traffic
/// it saw into fixed-width buckets (64 GB/s for the AMCC aggregate, 1-2 GB/s per client). Weighting
/// each bucket by its lower edge matched measured copy loads to within ~5%.
///
/// The AMCC aggregate sees everything, GPU included, but its coarse buckets read low under light load.
/// Per-client counters (CPU clusters, display, media) are fine-grained but saturate at 32 GB/s each, and
/// the GPU's own counter stays at zero, so: total = max(aggregate, sum of clients), and whatever the CPU
/// clusters don't account for is reported as GPU & other. When a cluster's counter is pinned in its top
/// bucket, the CPU figure is flagged as a lower bound rather than silently pushing traffic into "other".
///
/// Taking a sample blocks ~8ms per die waiting on the power-management coprocessor, so it runs on
/// its own thread.
pub fn spawn_bandwidth_sampler() -> Arc<Mutex<Bandwidth>> {
    let shared = Arc::new(Mutex::new(Bandwidth {
        state: BwState::Starting,
        total: 0.0,
        cpu: 0.0,
        cpu_saturated: false,
    }));
    let out = shared.clone();
    thread::spawn(move || {
        let Some(mut sampler) = Sampler::new() else {
            crate::sys::lock(&out).state = BwState::Unavailable;
            return;
        };
        loop {
            if let Some(reading) = sampler.sample() {
                *crate::sys::lock(&out) = reading;
            }
            thread::sleep(Duration::from_millis(250));
        }
    });
    shared
}

struct Sampler {
    subs: Vec<(*const std::ffi::c_void, CFMutableDictionaryRef)>,
    prev: Vec<CFDictionaryRef>,
    key: CFStringRef,
    widths: HashMap<String, f64>,
}

impl Sampler {
    fn new() -> Option<Sampler> {
        let key = cfstr("IOReportChannels");
        let subgroup = cfstr("DCS BW");
        let mut subs = Vec::new();
        // One power-management processor per die: "PMP" on single-die chips, PMP0/PMP1/... on Ultras.
        for group in ["PMP", "PMP0", "PMP1", "PMP2", "PMP3"] {
            unsafe {
                let chans = ffi::IOReportCopyChannelsInGroup(cfstr(group), subgroup, 0, 0, 0);
                if chans.is_null() {
                    continue;
                }
                let arr = ffi::CFDictionaryGetValue(chans, key);
                if arr.is_null() || ffi::CFArrayGetCount(arr) == 0 {
                    ffi::CFRelease(chans);
                    continue;
                }
                let mutable = ffi::CFDictionaryCreateMutableCopy(ptr::null(), ffi::CFDictionaryGetCount(chans), chans);
                ffi::CFRelease(chans);
                let mut subscribed: CFMutableDictionaryRef = ptr::null_mut();
                let sub = ffi::IOReportCreateSubscription(ptr::null(), mutable, &mut subscribed, 0, ptr::null());
                if !sub.is_null() && !subscribed.is_null() {
                    subs.push((sub, subscribed));
                }
            }
        }
        (!subs.is_empty()).then(|| Sampler {
            subs,
            prev: Vec::new(),
            key,
            widths: HashMap::new(),
        })
    }

    /// Average bandwidth (bytes/s) of one histogram channel, using each bucket's lower edge, and the
    /// share of time spent in the top (saturated) bucket.
    fn mean(&mut self, item: CFDictionaryRef, name: &str) -> (f64, f64) {
        let n = unsafe { ffi::IOReportStateGetCount(item) };
        if n <= 0 {
            return (0.0, 0.0);
        }
        let width = *self.widths.entry(name.to_string()).or_insert_with(|| {
            let label = cf_to_string(unsafe { ffi::IOReportStateGetNameForIndex(item, 0) });
            parse_rate(label.trim())
        });
        let (mut events, mut weighted, mut top) = (0i64, 0i64, 0i64);
        for k in 0..n {
            let c = unsafe { ffi::IOReportStateGetResidency(item, k) };
            events += c;
            weighted += c * k as i64;
            top = c;
        }
        if events == 0 {
            return (0.0, 0.0);
        }
        (weighted as f64 * width / events as f64, top as f64 / events as f64)
    }

    fn sample(&mut self) -> Option<Bandwidth> {
        let cur: Vec<CFDictionaryRef> = self
            .subs
            .iter()
            .map(|&(s, c)| unsafe { ffi::IOReportCreateSamples(s, c, ptr::null()) })
            .collect();
        let prev = std::mem::replace(&mut self.prev, cur.clone());
        let mut result = None;
        if !prev.is_empty() && cur.iter().all(|s| !s.is_null()) {
            let (mut aggregate, mut clients, mut cpu, mut saturated) = (0.0, 0.0, 0.0, false);
            for (&old, &new) in prev.iter().zip(&cur) {
                let delta = ffi::CfOwned(unsafe { ffi::IOReportCreateSamplesDelta(old, new, ptr::null()) });
                if delta.0.is_null() {
                    continue;
                }
                let arr = unsafe { ffi::CFDictionaryGetValue(delta.0, self.key) };
                let count = if arr.is_null() {
                    0
                } else {
                    unsafe { ffi::CFArrayGetCount(arr) }
                };
                for i in 0..count {
                    let item = unsafe { ffi::CFArrayGetValueAtIndex(arr, i) };
                    let name = cf_to_string(unsafe { ffi::IOReportChannelGetChannelName(item) });
                    if !name.ends_with(" RD+WR") {
                        continue;
                    }
                    let (v, top_share) = self.mean(item, &name);
                    if name.starts_with("AMCC") {
                        aggregate += v;
                    } else {
                        clients += v;
                        // CPU clusters: PACC0, MACC1, EACC0, ...
                        if name.get(1..4) == Some("ACC") {
                            cpu += v;
                            saturated |= top_share > 0.2;
                        }
                    }
                }
            }
            let total = f64::max(aggregate, clients);
            result = Some(Bandwidth {
                state: BwState::Ok,
                total,
                cpu: f64::min(cpu, total),
                cpu_saturated: saturated,
            });
        }
        for s in prev {
            if !s.is_null() {
                unsafe { ffi::CFRelease(s) };
            }
        }
        result
    }
}

/// Parse a bucket label like "64GB/s" or "1.5 MB/s" into bytes/s.
fn parse_rate(label: &str) -> f64 {
    let num: String = label.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let unit = match label[num.len()..].trim_start().chars().next() {
        Some('K') => 1e3,
        Some('M') => 1e6,
        Some('G') => 1e9,
        Some('T') => 1e12,
        _ => 1.0,
    };
    num.parse::<f64>().unwrap_or(0.0) * unit
}

#[cfg(test)]
mod tests {
    use super::parse_rate;

    #[test]
    fn parses_bucket_labels() {
        assert_eq!(parse_rate("64GB/s"), 64e9);
        assert_eq!(parse_rate("1GB/s"), 1e9);
        assert_eq!(parse_rate("1.5 MB/s"), 1.5e6);
        assert_eq!(parse_rate("garbage"), 0.0);
    }
}
