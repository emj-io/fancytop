//! Raw bindings to the macOS APIs fancytop reads from: Mach (CPU + VM), CoreFoundation,
//! IOKit (GPU, CPU topology) and the private IOReport library (DRAM bandwidth).

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFMutableDictionaryRef = *mut c_void;
pub type CFArrayRef = *const c_void;
pub type CFIndex = isize;
pub type Boolean = u8;
pub type mach_port_t = u32;
pub type kern_return_t = i32;
pub type io_object_t = u32;

pub const UTF8: u32 = 0x0800_0100;
pub const kCFNumberSInt64Type: CFIndex = 4;

// ---------------------------------------------------------------------------- Mach

pub const PROCESSOR_CPU_LOAD_INFO: i32 = 2;
pub const HOST_VM_INFO64: i32 = 4;

/// `struct vm_statistics64` from <mach/vm_statistics.h>.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct vm_statistics64 {
    pub free_count: u32,
    pub active_count: u32,
    pub inactive_count: u32,
    pub wire_count: u32,
    pub zero_fill_count: u64,
    pub reactivations: u64,
    pub pageins: u64,
    pub pageouts: u64,
    pub faults: u64,
    pub cow_faults: u64,
    pub lookups: u64,
    pub hits: u64,
    pub purges: u64,
    pub purgeable_count: u32,
    pub speculative_count: u32,
    pub decompressions: u64,
    pub compressions: u64,
    pub swapins: u64,
    pub swapouts: u64,
    pub compressor_page_count: u32,
    pub throttled_count: u32,
    pub external_page_count: u32,
    pub internal_page_count: u32,
    pub total_uncompressed_pages_in_compressor: u64,
}

unsafe extern "C" {
    pub static mach_task_self_: mach_port_t;
    pub fn mach_host_self() -> mach_port_t;
    pub fn host_processor_info(
        host: mach_port_t,
        flavor: i32,
        out_processor_count: *mut u32,
        out_processor_info: *mut *mut i32,
        out_processor_info_cnt: *mut u32,
    ) -> kern_return_t;
    pub fn host_statistics64(host: mach_port_t, flavor: i32, info: *mut i32, count: *mut u32) -> kern_return_t;
    pub fn host_page_size(host: mach_port_t, out: *mut usize) -> kern_return_t;
    pub fn vm_deallocate(task: mach_port_t, address: usize, size: usize) -> kern_return_t;
}

// ---------------------------------------------------------------------------- CoreFoundation

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, encoding: u32) -> CFStringRef;
    pub fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: CFIndex, encoding: u32) -> Boolean;
    pub fn CFDictionaryGetValue(d: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
    pub fn CFDictionaryGetCount(d: CFDictionaryRef) -> CFIndex;
    pub fn CFDictionaryCreateMutableCopy(alloc: CFTypeRef, capacity: CFIndex, d: CFDictionaryRef) -> CFMutableDictionaryRef;
    pub fn CFArrayGetCount(a: CFArrayRef) -> CFIndex;
    pub fn CFArrayGetValueAtIndex(a: CFArrayRef, i: CFIndex) -> CFTypeRef;
    pub fn CFNumberGetValue(n: CFTypeRef, ty: CFIndex, out: *mut c_void) -> Boolean;
    pub fn CFGetTypeID(t: CFTypeRef) -> usize;
    pub fn CFNumberGetTypeID() -> usize;
    pub fn CFStringGetTypeID() -> usize;
    pub fn CFDataGetTypeID() -> usize;
    pub fn CFDataGetLength(d: CFTypeRef) -> CFIndex;
    pub fn CFDataGetBytePtr(d: CFTypeRef) -> *const u8;
    pub fn CFRelease(t: CFTypeRef);
}

// ---------------------------------------------------------------------------- IOKit

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    pub fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
    pub fn IOServiceGetMatchingServices(
        main_port: mach_port_t,
        matching: CFDictionaryRef,
        iter: *mut io_object_t,
    ) -> kern_return_t;
    pub fn IOIteratorNext(iter: io_object_t) -> io_object_t;
    pub fn IOObjectRelease(obj: io_object_t) -> kern_return_t;
    pub fn IORegistryEntryCreateCFProperty(entry: io_object_t, key: CFStringRef, alloc: CFTypeRef, options: u32) -> CFTypeRef;
}

// ---------------------------------------------------------------------------- IOReport (private)

#[link(name = "IOReport", kind = "dylib")]
unsafe extern "C" {
    pub fn IOReportCopyChannelsInGroup(group: CFStringRef, subgroup: CFStringRef, a: u64, b: u64, c: u64) -> CFDictionaryRef;
    pub fn IOReportCreateSubscription(
        a: *const c_void,
        channels: CFMutableDictionaryRef,
        subscribed: *mut CFMutableDictionaryRef,
        d: u64,
        e: CFTypeRef,
    ) -> *const c_void;
    pub fn IOReportCreateSamples(sub: *const c_void, channels: CFMutableDictionaryRef, e: CFTypeRef) -> CFDictionaryRef;
    pub fn IOReportCreateSamplesDelta(a: CFDictionaryRef, b: CFDictionaryRef, e: CFTypeRef) -> CFDictionaryRef;
    pub fn IOReportChannelGetChannelName(item: CFDictionaryRef) -> CFStringRef;
    pub fn IOReportStateGetCount(item: CFDictionaryRef) -> i32;
    pub fn IOReportStateGetNameForIndex(item: CFDictionaryRef, index: i32) -> CFStringRef;
    pub fn IOReportStateGetResidency(item: CFDictionaryRef, index: i32) -> i64;
}

// ---------------------------------------------------------------------------- libproc

/// `struct rusage_info_v0` from <sys/resource.h>. Times are in Mach absolute-time ticks.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct rusage_info_v0 {
    pub uuid: [u8; 16],
    pub user_time: u64,
    pub system_time: u64,
    pub pkg_idle_wkups: u64,
    pub interrupt_wkups: u64,
    pub pageins: u64,
    pub wired_size: u64,
    pub resident_size: u64,
    pub phys_footprint: u64,
    pub proc_start_abstime: u64,
    pub proc_exit_abstime: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct mach_timebase_info {
    pub numer: u32,
    pub denom: u32,
}

pub const RUSAGE_INFO_V0: i32 = 0;
pub const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;

unsafe extern "C" {
    pub fn proc_listallpids(buffer: *mut c_void, buffersize: i32) -> i32;
    pub fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut rusage_info_v0) -> i32;
    pub fn proc_pidpath(pid: i32, buffer: *mut c_void, buffersize: u32) -> i32;
    pub fn proc_name(pid: i32, buffer: *mut c_void, buffersize: u32) -> i32;
    pub fn mach_timebase_info(info: *mut mach_timebase_info) -> kern_return_t;
}

// ---------------------------------------------------------------------------- helpers

/// Create a CFString. The caller owns it (most are created once and kept for the process lifetime).
pub fn cfstr(s: &str) -> CFStringRef {
    let c = CString::new(s).expect("no interior NUL");
    unsafe { CFStringCreateWithCString(ptr::null(), c.as_ptr(), UTF8) }
}

pub fn cf_to_string(s: CFStringRef) -> String {
    if s.is_null() {
        return String::new();
    }
    let mut buf = [0 as c_char; 256];
    unsafe {
        if CFGetTypeID(s) != CFStringGetTypeID() || CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as CFIndex, UTF8) == 0 {
            return String::new();
        }
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

pub fn cf_to_i64(n: CFTypeRef) -> Option<i64> {
    if n.is_null() {
        return None;
    }
    let mut v: i64 = 0;
    unsafe {
        if CFGetTypeID(n) != CFNumberGetTypeID() {
            return None;
        }
        (CFNumberGetValue(n, kCFNumberSInt64Type, &mut v as *mut i64 as *mut c_void) != 0).then_some(v)
    }
}

pub fn cf_to_bytes(d: CFTypeRef) -> Option<Vec<u8>> {
    if d.is_null() {
        return None;
    }
    unsafe {
        if CFGetTypeID(d) != CFDataGetTypeID() {
            return None;
        }
        let len = CFDataGetLength(d) as usize;
        Some(std::slice::from_raw_parts(CFDataGetBytePtr(d), len).to_vec())
    }
}

/// A CoreFoundation object we own, released on drop.
pub struct CfOwned(pub CFTypeRef);

impl Drop for CfOwned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}

/// Read one property of an IORegistry entry.
pub fn registry_property(entry: io_object_t, key: CFStringRef) -> CfOwned {
    CfOwned(unsafe { IORegistryEntryCreateCFProperty(entry, key, ptr::null(), 0) })
}

/// Every IORegistry entry whose class is (or inherits from) `class`. The caller releases them.
pub fn matching_services(class: &str) -> Vec<io_object_t> {
    let name = CString::new(class).expect("no interior NUL");
    let mut iter: io_object_t = 0;
    let mut out = Vec::new();
    unsafe {
        if IOServiceGetMatchingServices(0, IOServiceMatching(name.as_ptr()), &mut iter) != 0 {
            return out;
        }
        loop {
            let entry = IOIteratorNext(iter);
            if entry == 0 {
                break;
            }
            out.push(entry);
        }
        IOObjectRelease(iter);
    }
    out
}
