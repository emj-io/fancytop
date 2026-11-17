//! Disk throughput (IOKit block-storage statistics) and network throughput (per-interface
//! counters from the routing socket sysctl).

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{ptr, thread};

use crate::ffi::{self, CFStringRef, cf_to_i64, cfstr, registry_property};

/// `IFT_ETHER` from <net/if_types.h>: Ethernet, Wi-Fi and Thunderbolt ports.
const IFT_ETHER: u8 = 0x06;

/// Throughput in bytes/s.
#[derive(Default, Clone, Copy)]
pub struct IoRates {
    pub disk_read: f64,
    pub disk_write: f64,
    pub net_in: f64,
    pub net_out: f64,
}

/// Disk and network rates, refreshed 4× a second on a background thread and averaged over the
/// last second so they don't flicker.
pub fn spawn_io_sampler() -> Arc<Mutex<IoRates>> {
    let shared = Arc::new(Mutex::new(IoRates::default()));
    let out = shared.clone();
    thread::spawn(move || {
        let keys = [cfstr("Statistics"), cfstr("Bytes (Read)"), cfstr("Bytes (Write)")];
        let mut hist: VecDeque<(Instant, [u64; 4])> = VecDeque::new();
        loop {
            let now = Instant::now();
            let [read, write] = disk_bytes(&keys);
            let [rx, tx] = net_bytes();
            let counters = [read, write, rx, tx];
            hist.push_back((now, counters));
            while hist.len() > 2 && now - hist[1].0 >= Duration::from_secs(1) {
                hist.pop_front();
            }
            let (t0, c0) = hist[0];
            let dt = (now - t0).as_secs_f64();
            if dt > 0.0 {
                // A disk or interface going away makes a counter drop; read that as idle.
                let r = |i: usize| counters[i].saturating_sub(c0[i]) as f64 / dt;
                *crate::sys::lock(&out) = IoRates {
                    disk_read: r(0),
                    disk_write: r(1),
                    net_in: r(2),
                    net_out: r(3),
                };
            }
            thread::sleep(Duration::from_millis(250));
        }
    });
    shared
}

/// Total bytes (read, written) across every block storage driver: internal SSD, external
/// drives and mounted disk images.
fn disk_bytes(keys: &[CFStringRef; 3]) -> [u64; 2] {
    let mut total = [0u64; 2];
    for entry in ffi::matching_services("IOBlockStorageDriver") {
        let stats = registry_property(entry, keys[0]);
        if !stats.0.is_null() {
            for (t, &k) in total.iter_mut().zip(&keys[1..]) {
                *t += cf_to_i64(unsafe { ffi::CFDictionaryGetValue(stats.0, k) })
                    .unwrap_or(0)
                    .max(0) as u64;
            }
        }
        unsafe { ffi::IOObjectRelease(entry) };
    }
    total
}

/// Total bytes (in, out) across physical network interfaces. Loopback, bridges and tunnels are
/// left out because their traffic is already counted on the ports underneath.
fn net_bytes() -> [u64; 2] {
    let mut mib = [libc::CTL_NET, libc::PF_ROUTE, 0, 0, libc::NET_RT_IFLIST2, 0];
    let mut len = 0usize;
    let mut sysctl = |buf: *mut c_void, len: &mut usize| unsafe {
        libc::sysctl(mib.as_mut_ptr(), mib.len() as u32, buf, len, ptr::null_mut(), 0)
    };
    if sysctl(ptr::null_mut(), &mut len) != 0 {
        return [0, 0];
    }
    // Leave room for interfaces that appear between the two calls.
    len += 4096;
    let mut buf = vec![0u8; len];
    if sysctl(buf.as_mut_ptr() as *mut c_void, &mut len) != 0 {
        return [0, 0];
    }
    buf.truncate(len);

    let mut total = [0u64; 2];
    let mut off = 0;
    while off + 4 <= buf.len() {
        let msglen = u16::from_ne_bytes([buf[off], buf[off + 1]]) as usize;
        if msglen == 0 {
            break;
        }
        if buf[off + 3] as i32 == libc::RTM_IFINFO2 && off + size_of::<libc::if_msghdr2>() <= buf.len() {
            let m = unsafe { ptr::read_unaligned(buf[off..].as_ptr() as *const libc::if_msghdr2) };
            let data = m.ifm_data;
            if m.ifm_flags & libc::IFF_LOOPBACK == 0 && data.ifi_type == IFT_ETHER {
                total[0] += data.ifi_ibytes;
                total[1] += data.ifi_obytes;
            }
        }
        off += msglen;
    }
    total
}
