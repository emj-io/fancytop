#!/usr/bin/env python3
"""fancytop - a flashy, fast terminal monitor for CPU, GPU and memory on Apple Silicon Macs.

Pure standard library: per-core CPU and memory come straight from Mach, GPU
utilisation from IOKit, so no sudo and no pip installs are needed.
"""

import argparse
import ctypes
import os
import plistlib
import select
import shutil
import signal
import subprocess
import sys
import termios
import threading
import time
import tty
from collections import deque
from ctypes import (POINTER, Structure, byref, c_bool, c_char_p, c_int, c_int64, c_long, c_size_t,
                    c_uint, c_uint32, c_uint64, c_ulong, c_void_p)

if sys.platform != "darwin":
    sys.exit("fancytop only supports macOS right now.")

libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib")

# --------------------------------------------------------------------------- sysctl

libc.sysctlbyname.argtypes = [c_char_p, c_void_p, POINTER(c_size_t), c_void_p, c_size_t]


def sysctl_raw(name, size=256):
    buf = ctypes.create_string_buffer(size)
    n = c_size_t(size)
    if libc.sysctlbyname(name.encode(), buf, byref(n), None, 0) != 0:
        return None
    return buf.raw[:n.value]


def sysctl_int(name):
    raw = sysctl_raw(name, 8)
    return int.from_bytes(raw, "little") if raw else None


def sysctl_str(name):
    raw = sysctl_raw(name)
    return raw.rstrip(b"\x00").decode(errors="replace") if raw else None


class XswUsage(Structure):
    _fields_ = [("total", c_uint64), ("avail", c_uint64), ("used", c_uint64),
                ("pagesize", c_uint32), ("encrypted", c_int)]


def swap_usage():
    raw = sysctl_raw("vm.swapusage", ctypes.sizeof(XswUsage))
    if not raw or len(raw) < ctypes.sizeof(XswUsage):
        return 0, 0
    x = XswUsage.from_buffer_copy(raw)
    return x.used, x.total


def boot_time():
    raw = sysctl_raw("kern.boottime", 16)
    return int.from_bytes(raw[:8], "little") if raw else time.time()

# --------------------------------------------------------------------------- Mach: CPU + VM

libc.mach_host_self.restype = c_uint
HOST = libc.mach_host_self()
TASK = c_uint.in_dll(libc, "mach_task_self_").value
libc.host_processor_info.argtypes = [c_uint, c_int, POINTER(c_uint), POINTER(POINTER(c_uint)), POINTER(c_uint)]
libc.vm_deallocate.argtypes = [c_uint, c_size_t, c_size_t]
libc.host_statistics64.argtypes = [c_uint, c_int, c_void_p, POINTER(c_uint)]
libc.host_page_size.argtypes = [c_uint, POINTER(c_size_t)]

PROCESSOR_CPU_LOAD_INFO = 2
HOST_VM_INFO64 = 4


def cpu_ticks():
    """Return a flat list of [user, system, idle, nice] ticks for every logical CPU."""
    n, info, cnt = c_uint(), POINTER(c_uint)(), c_uint()
    if libc.host_processor_info(HOST, PROCESSOR_CPU_LOAD_INFO, byref(n), byref(info), byref(cnt)) != 0:
        raise OSError("host_processor_info failed")
    vals = info[:n.value * 4]
    libc.vm_deallocate(TASK, ctypes.addressof(info.contents), cnt.value * 4)
    return vals


class VmStats(Structure):
    _fields_ = [
        ("free_count", c_uint32), ("active_count", c_uint32), ("inactive_count", c_uint32), ("wire_count", c_uint32),
        ("zero_fill_count", c_uint64), ("reactivations", c_uint64), ("pageins", c_uint64), ("pageouts", c_uint64),
        ("faults", c_uint64), ("cow_faults", c_uint64), ("lookups", c_uint64), ("hits", c_uint64), ("purges", c_uint64),
        ("purgeable_count", c_uint32), ("speculative_count", c_uint32),
        ("decompressions", c_uint64), ("compressions", c_uint64), ("swapins", c_uint64), ("swapouts", c_uint64),
        ("compressor_page_count", c_uint32), ("throttled_count", c_uint32),
        ("external_page_count", c_uint32), ("internal_page_count", c_uint32),
        ("total_uncompressed_pages_in_compressor", c_uint64),
    ]


def vm_stats():
    s = VmStats()
    cnt = c_uint(ctypes.sizeof(VmStats) // 4)
    if libc.host_statistics64(HOST, HOST_VM_INFO64, byref(s), byref(cnt)) != 0:
        raise OSError("host_statistics64 failed")
    return s


def page_size():
    ps = c_size_t()
    libc.host_page_size(HOST, byref(ps))
    return ps.value or 16384

# --------------------------------------------------------------------------- CoreFoundation

UTF8 = 0x08000100
_cf = None


def corefoundation():
    global _cf
    if _cf is None:
        cf = ctypes.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
        for fn, res, args in [
            (cf.CFStringCreateWithCString, c_void_p, [c_void_p, c_char_p, c_uint]),
            (cf.CFStringGetCString, c_bool, [c_void_p, c_char_p, c_long, c_uint]),
            (cf.CFDictionaryGetValue, c_void_p, [c_void_p, c_void_p]),
            (cf.CFDictionaryGetCount, c_long, [c_void_p]),
            (cf.CFDictionaryCreateMutableCopy, c_void_p, [c_void_p, c_long, c_void_p]),
            (cf.CFArrayGetCount, c_long, [c_void_p]),
            (cf.CFArrayGetValueAtIndex, c_void_p, [c_void_p, c_long]),
            (cf.CFNumberGetValue, c_bool, [c_void_p, c_int, c_void_p]),
            (cf.CFGetTypeID, c_ulong, [c_void_p]),
            (cf.CFNumberGetTypeID, c_ulong, []),
            (cf.CFRelease, None, [c_void_p]),
        ]:
            fn.restype, fn.argtypes = res, args
        _cf = cf
    return _cf


def cfstr(s):
    return corefoundation().CFStringCreateWithCString(None, s.encode(), UTF8)


def pystr(ref, buf=ctypes.create_string_buffer(256)):
    if not ref or not corefoundation().CFStringGetCString(ref, buf, len(buf), UTF8):
        return ""
    return buf.value.decode(errors="replace")

# --------------------------------------------------------------------------- IOKit: GPU


class Gpu:
    KEYS = ["Device Utilization %", "Renderer Utilization %", "Tiler Utilization %",
            "In use system memory", "Alloc system memory"]

    def __init__(self):
        self.model, self.cores, self.ok = "GPU", 0, False
        self._fallback_at, self._fallback_val = 0.0, None
        try:
            entries = plistlib.loads(subprocess.run(
                ["ioreg", "-r", "-d", "1", "-c", "IOAccelerator", "-a"], capture_output=True, timeout=5).stdout)
            if entries:
                self.model = entries[0].get("model", self.model)
                self.cores = entries[0].get("gpu-core-count", 0)
        except Exception:
            pass
        try:
            self._init_iokit()
        except (OSError, AttributeError):
            self.ok = False

    def _init_iokit(self):
        io = ctypes.CDLL("/System/Library/Frameworks/IOKit.framework/IOKit")
        io.IOServiceMatching.restype = c_void_p
        io.IOServiceMatching.argtypes = [c_char_p]
        io.IOServiceGetMatchingServices.argtypes = [c_uint, c_void_p, POINTER(c_uint)]
        io.IOIteratorNext.restype = c_uint
        io.IOIteratorNext.argtypes = [c_uint]
        io.IOObjectRelease.argtypes = [c_uint]
        io.IORegistryEntryCreateCFProperty.restype = c_void_p
        io.IORegistryEntryCreateCFProperty.argtypes = [c_uint, c_void_p, c_void_p, c_uint]
        cf = corefoundation()
        self.io, self.cf = io, cf

        it = c_uint()
        if io.IOServiceGetMatchingServices(0, io.IOServiceMatching(b"IOAccelerator"), byref(it)) != 0:
            return
        self.entry = io.IOIteratorNext(it.value)
        io.IOObjectRelease(it.value)
        if not self.entry:
            return

        self.stats_key = cfstr("PerformanceStatistics")
        self.keys = {k: cfstr(k) for k in self.KEYS}
        self.number_type = cf.CFNumberGetTypeID()
        self.ok = self._read_iokit() is not None

    def read(self):
        if not self.ok:
            return self._read_ioreg()
        return self._read_iokit()

    def _read_iokit(self):
        d = self.io.IORegistryEntryCreateCFProperty(self.entry, self.stats_key, None, 0)
        if not d:
            return None
        out = {}
        for name, key in self.keys.items():
            v = self.cf.CFDictionaryGetValue(d, key)
            if v and self.cf.CFGetTypeID(v) == self.number_type:
                x = c_int64()
                if self.cf.CFNumberGetValue(v, 4, byref(x)):  # kCFNumberSInt64Type
                    out[name] = x.value
        self.cf.CFRelease(d)
        return out

    def _read_ioreg(self):
        # Slow path (~25ms a call), so rate-limit it.
        now = time.monotonic()
        if now - self._fallback_at >= 1.0:
            self._fallback_at = now
            try:
                entries = plistlib.loads(subprocess.run(
                    ["ioreg", "-r", "-d", "1", "-c", "IOAccelerator", "-a"], capture_output=True, timeout=5).stdout)
                self._fallback_val = entries[0].get("PerformanceStatistics") if entries else None
            except Exception:
                self._fallback_val = None
        return self._fallback_val

# --------------------------------------------------------------------------- IOReport: DRAM bandwidth


class Bandwidth:
    """DRAM bandwidth from the memory controllers' IOReport counters (private API, no root needed).

    Each "DCS BW" channel is a histogram: thousands of times a second the controller bins the traffic
    it saw into fixed-width buckets (e.g. 64 GB/s for the AMCC aggregate, 1 GB/s per client). Weighting
    each bucket by its lower edge matched measured copy loads to within ~5%.

    The AMCC aggregate sees everything, GPU included, but its coarse buckets read low under light load.
    Per-client counters (CPU clusters, display, media) are fine-grained but saturate at 32 GB/s each, and
    the GPU's own counter stays at zero, so: total = max(aggregate, sum of clients), and whatever the CPU
    clusters don't account for is reported as GPU & other.

    Taking a sample blocks ~8ms per die waiting on the power-management coprocessor, so it runs on a
    background thread (ctypes drops the GIL while it waits)."""

    PERIOD = 0.25

    def __init__(self):
        self.ok, self.total, self.cpu = False, 0.0, 0.0
        self.prev, self.widths = None, {}
        try:
            self._init()
        except (OSError, AttributeError):
            self.ok = False

    def _init(self):
        ior = ctypes.CDLL("/usr/lib/libIOReport.dylib")
        for fn, res, args in [
            (ior.IOReportCopyChannelsInGroup, c_void_p, [c_void_p, c_void_p, c_uint64, c_uint64, c_uint64]),
            (ior.IOReportCreateSubscription, c_void_p, [c_void_p, c_void_p, POINTER(c_void_p), c_uint64, c_void_p]),
            (ior.IOReportCreateSamples, c_void_p, [c_void_p, c_void_p, c_void_p]),
            (ior.IOReportCreateSamplesDelta, c_void_p, [c_void_p, c_void_p, c_void_p]),
            (ior.IOReportChannelGetChannelName, c_void_p, [c_void_p]),
            (ior.IOReportStateGetCount, c_int, [c_void_p]),
            (ior.IOReportStateGetNameForIndex, c_void_p, [c_void_p, c_int]),
            (ior.IOReportStateGetResidency, c_int64, [c_void_p, c_int]),
        ]:
            fn.restype, fn.argtypes = res, args
        cf = corefoundation()
        self.ior, self.cf, self.key = ior, cf, cfstr("IOReportChannels")

        # One power-management processor per die: "PMP" on single-die chips, PMP0/PMP1/... on Ultras.
        self.subs = []
        for group in ("PMP", "PMP0", "PMP1", "PMP2", "PMP3"):
            chans = ior.IOReportCopyChannelsInGroup(cfstr(group), cfstr("DCS BW"), 0, 0, 0)
            if not chans:
                continue
            arr = cf.CFDictionaryGetValue(chans, self.key)
            if not arr or cf.CFArrayGetCount(arr) == 0:
                cf.CFRelease(chans)
                continue
            mutable = cf.CFDictionaryCreateMutableCopy(None, cf.CFDictionaryGetCount(chans), chans)
            cf.CFRelease(chans)
            subbed = c_void_p()
            sub = ior.IOReportCreateSubscription(None, mutable, byref(subbed), 0, None)
            if sub and subbed:
                self.subs.append((sub, subbed))
        self.ok = bool(self.subs)

    def _mean(self, item, name):
        """Average bandwidth (bytes/s) of one histogram channel, using each bucket's lower edge."""
        ior = self.ior
        n = ior.IOReportStateGetCount(item)
        if n <= 0:
            return 0.0
        width = self.widths.get(name)
        if width is None:
            label = pystr(ior.IOReportStateGetNameForIndex(item, 0)).strip()
            num = "".join(ch for ch in label if ch.isdigit() or ch == ".")
            unit = {"K": 1e3, "M": 1e6, "G": 1e9, "T": 1e12}.get(label[len(num):].strip()[:1], 1)
            width = self.widths[name] = float(num or 0) * unit
        events = weighted = 0
        for k in range(n):
            c = ior.IOReportStateGetResidency(item, k)
            events += c
            weighted += c * k
        return weighted * width / events if events else 0.0

    def start(self):
        if self.ok:
            threading.Thread(target=self._run, daemon=True).start()

    def _run(self):
        while True:
            try:
                self.sample()
            except Exception:
                self.ok = False
                return
            time.sleep(self.PERIOD)

    def sample(self):
        ior, cf = self.ior, self.cf
        cur = [ior.IOReportCreateSamples(sub, subbed, None) for sub, subbed in self.subs]
        if self.prev and all(cur):
            aggregate = clients = cpu = 0.0
            for old, new in zip(self.prev, cur):
                delta = ior.IOReportCreateSamplesDelta(old, new, None)
                if not delta:
                    continue
                arr = cf.CFDictionaryGetValue(delta, self.key)
                for i in range(cf.CFArrayGetCount(arr) if arr else 0):
                    item = cf.CFArrayGetValueAtIndex(arr, i)
                    name = pystr(ior.IOReportChannelGetChannelName(item))
                    if not name.endswith(" RD+WR"):
                        continue
                    v = self._mean(item, name)
                    if name.startswith("AMCC"):
                        aggregate += v
                    else:
                        clients += v
                        if name[1:4] == "ACC":  # CPU clusters: PACC0, MACC1, EACC0, ...
                            cpu += v
                cf.CFRelease(delta)
            self.total = max(aggregate, clients)
            self.cpu = min(cpu, self.total)
        for s in self.prev or ():
            if s:
                cf.CFRelease(s)
        self.prev = cur

# --------------------------------------------------------------------------- CPU topology


def cpu_topology(ncpu):
    """Return [(logical_id, cluster_name, label)] ordered fastest cluster first."""
    levels = []
    for i in range(sysctl_int("hw.nperflevels") or 0):
        levels.append(sysctl_str("hw.perflevel%d.name" % i) or "Cluster %d" % i)

    kinds = {}
    try:
        entries = plistlib.loads(subprocess.run(
            ["ioreg", "-r", "-d", "1", "-c", "IOPlatformDevice", "-a"], capture_output=True, timeout=5).stdout)
        for e in entries:
            if "logical-cpu-id" in e and "cluster-type" in e:
                kinds[e["logical-cpu-id"]] = e["cluster-type"].rstrip(b"\x00").decode()
    except Exception:
        pass

    rank = {"P": 0, "M": 1, "E": 2}
    types = sorted(set(kinds.values()), key=lambda t: rank.get(t, 9))
    names = {t: (levels[i] if i < len(levels) else t) for i, t in enumerate(types)}

    cores, seen = [], {}
    for cid in sorted(range(ncpu), key=lambda c: (rank.get(kinds.get(c), 9), c)):
        name = names.get(kinds.get(cid), "CPU")
        idx = seen.get(name, 0)
        seen[name] = idx + 1
        cores.append((cid, name, idx))
    width = len(str(max(seen.values()) - 1)) if seen else 1
    return [(cid, name, "%s%0*d" % (name[0], width, idx)) for cid, name, idx in cores]

# --------------------------------------------------------------------------- processes


class Processes(threading.Thread):
    """Polls ps in the background; it's too slow to run every frame."""

    def __init__(self, period=1.0):
        super().__init__(daemon=True)
        self.period, self.rows = period, []

    def run(self):
        while True:
            try:
                out = subprocess.run(["ps", "-Aceo", "pid=,pcpu=,rss=,comm=", "-r"],
                                     capture_output=True, text=True, timeout=5).stdout
                rows = []
                for line in out.splitlines()[:80]:
                    parts = line.split(None, 3)
                    if len(parts) == 4:
                        rows.append((int(parts[0]), float(parts[1]), int(parts[2]) * 1024, parts[3]))
                self.rows = rows
            except Exception:
                pass
            time.sleep(self.period)

# --------------------------------------------------------------------------- colour


def lerp(a, b, t):
    return a + (b - a) * t


def gradient(stops):
    def f(t):
        t = 0.0 if t < 0 else 1.0 if t > 1 else t
        for (t0, c0), (t1, c1) in zip(stops, stops[1:]):
            if t <= t1:
                u = (t - t0) / (t1 - t0) if t1 > t0 else 0
                return (int(lerp(c0[0], c1[0], u)), int(lerp(c0[1], c1[1], u)), int(lerp(c0[2], c1[2], u)))
        return stops[-1][1]
    cache = [f(i / 64) for i in range(65)]
    return lambda t: cache[0 if t <= 0 else 64 if t >= 1 else int(t * 64)]


HEAT = gradient([(0.0, (0, 220, 140)), (0.4, (180, 230, 40)), (0.6, (255, 200, 0)),
                 (0.8, (255, 110, 30)), (1.0, (255, 30, 90))])
PLASMA = gradient([(0.0, (70, 40, 255)), (0.45, (190, 40, 230)), (0.75, (255, 60, 140)), (1.0, (255, 200, 60))])
OCEAN = gradient([(0.0, (0, 120, 255)), (0.5, (0, 210, 255)), (1.0, (150, 255, 230))])
TITLE = gradient([(0.0, (0, 230, 255)), (0.5, (190, 80, 255)), (1.0, (255, 60, 150))])

FG = (220, 222, 235)
DIM = (110, 112, 135)
FAINT = (55, 57, 75)
BORDER = (80, 85, 120)
HEADER_BG = (24, 24, 40)
WHITE = (255, 255, 255)

MEM_WIRED = (255, 90, 120)
MEM_APP = (255, 190, 60)
MEM_COMP = (190, 90, 255)
MEM_CACHE = (0, 170, 255)
MEM_FREE = (45, 48, 66)

# --------------------------------------------------------------------------- canvas


def _cube(v):
    return 0 if v < 48 else 1 if v < 115 else (v - 35) // 40


def rgb256(c):
    return 16 + 36 * _cube(c[0]) + 6 * _cube(c[1]) + _cube(c[2])


class Canvas:
    def __init__(self, w, h, truecolor):
        self.w, self.h, self.truecolor = w, h, truecolor
        n = w * h
        self.ch, self.fg, self.bg, self.bold = [" "] * n, [None] * n, [None] * n, [False] * n
        self.top, self.bottom = 0, h - 1

    def clip(self, top=0, bottom=None):
        """Restrict drawing to rows top..bottom so panel content never spills out of its box."""
        self.top, self.bottom = max(0, top), min(self.h - 1, self.h - 1 if bottom is None else bottom)

    def put(self, x, y, text, fg=FG, bg=None, bold=False):
        if not (self.top <= y <= self.bottom):
            return
        row = y * self.w
        for c in text:
            if 0 <= x < self.w:
                i = row + x
                self.ch[i], self.fg[i], self.bold[i] = c, fg, bold
                if bg is not None:
                    self.bg[i] = bg
            x += 1

    def fill_bg(self, x, y, w, bg):
        if 0 <= y < self.h:
            for xx in range(max(0, x), min(self.w, x + w)):
                self.bg[y * self.w + xx] = bg

    def _sgr(self, c, base):
        if self.truecolor:
            return "%d;2;%d;%d;%d" % (base, c[0], c[1], c[2])
        return "%d;5;%d" % (base, rgb256(c))

    def line(self, y):
        out, prev, row = [], None, y * self.w
        for i in range(row, row + self.w):
            style = (self.fg[i], self.bg[i], self.bold[i])
            if style != prev:
                codes = ["0"]
                if style[2]:
                    codes.append("1")
                if style[0]:
                    codes.append(self._sgr(style[0], 38))
                if style[1]:
                    codes.append(self._sgr(style[1], 48))
                out.append("\x1b[" + ";".join(codes) + "m")
                prev = style
            out.append(self.ch[i])
        out.append("\x1b[0m")
        return "".join(out)

# --------------------------------------------------------------------------- widgets


EIGHTHS = " ▏▎▍▌▋▊▉█"
SPARK = "▁▂▃▄▅▆▇█"
BRAILLE_L = (0, 0x40, 0x44, 0x46, 0x47)
BRAILLE_R = (0, 0x80, 0xA0, 0xB0, 0xB8)


def draw_box(cv, x, y, w, h, title):
    """Draw a rounded box, then clip the canvas to its interior.

    title is a list of (text, colour, bold) segments, or lists of them that should only be shown
    together; trailing ones that don't fit are dropped."""
    cv.clip(y, y + h - 1)
    if w < 4 or h < 2:
        cv.clip(0, -1)
        return
    cv.put(x, y, "╭" + "─" * (w - 2) + "╮", BORDER)
    for yy in range(y + 1, y + h - 1):
        cv.put(x, yy, "│", BORDER)
        cv.put(x + w - 1, yy, "│", BORDER)
    cv.put(x, y + h - 1, "╰" + "─" * (w - 2) + "╯", BORDER)
    tx, limit = x + 3, x + w - 3
    for i, group in enumerate(title):
        if isinstance(group, tuple):
            group = [group]
        if tx + sum(len(t) for t, _, _ in group) > limit:
            if i == 0:
                text, color, bold = group[0]
                cv.put(tx, y, text[:max(0, limit - tx)], color, bold=bold)
                tx = limit
            break
        for text, color, bold in group:
            cv.put(tx, y, text, color, bold=bold)
            tx += len(text)
    cv.put(x + 2, y, " ", BORDER)
    cv.put(tx, y, " ", BORDER)
    cv.clip(y + 1, y + h - 2)


def draw_bar(cv, x, y, w, frac, grad, peak=None):
    if w <= 0:
        return
    frac = 0.0 if frac < 0 else 1.0 if frac > 1 else frac
    filled = frac * w
    peak_cell = None if peak is None else min(w - 1, int(peak * w))
    for i in range(w):
        f = filled - i
        if f >= 1:
            cv.put(x + i, y, "█", grad((i + 0.5) / w))
        elif f > 0.0625:
            cv.put(x + i, y, EIGHTHS[max(1, int(f * 8))], grad((i + 0.5) / w))
        elif i == peak_cell:
            cv.put(x + i, y, "▏", WHITE)
        else:
            cv.put(x + i, y, "─", FAINT)


def draw_graph(cv, x, y, w, h, values, grad, baseline=True):
    """Braille area chart: 2 samples per column, 4 dots per row, vertical colour gradient."""
    if w <= 0 or h <= 0:
        return
    need = w * 2
    vals = list(values)[-need:]
    vals = [None] * (need - len(vals)) + vals
    dots = h * 4
    colors = [grad((r + 0.5) / h) for r in range(h)]
    for col in range(w):
        a, b = vals[2 * col], vals[2 * col + 1]
        ha = 0 if a is None else (max(1, round(a * dots)) if a > 0.004 else 0)
        hb = 0 if b is None else (max(1, round(b * dots)) if b > 0.004 else 0)
        for r in range(h):
            la = min(4, max(0, ha - r * 4))
            lb = min(4, max(0, hb - r * 4))
            if la or lb:
                cv.put(x + col, y + h - 1 - r, chr(0x2800 | BRAILLE_L[la] | BRAILLE_R[lb]), colors[r])
            elif r == 0 and baseline:
                cv.put(x + col, y + h - 1, "⣀", FAINT)


def spark(cv, x, y, values, grad):
    for i, v in enumerate(values):
        v = 0.0 if v < 0 else 1.0 if v > 1 else v
        cv.put(x + i, y, SPARK[min(7, int(v * 8))], grad(v))


def fmt_bytes(n, prec=1):
    n = float(n)
    for unit in ("B", "K", "M", "G", "T"):
        if abs(n) < 1024 or unit == "T":
            return "%.0f%s" % (n, unit) if unit in "BK" else "%.*f%s" % (prec, n, unit)
        n /= 1024


def fmt_bw(n):
    """Bandwidth in decimal GB/s, the way memory bandwidth is usually quoted."""
    return "%.0f GB/s" % (n / 1e9) if n >= 99.5e9 else "%.1f GB/s" % (n / 1e9)


def fmt_rate(n):
    return fmt_bytes(n) + "/s" if n >= 1 else "0"


def fmt_uptime(sec):
    d, rem = divmod(int(sec), 86400)
    h, rem = divmod(rem, 3600)
    m = rem // 60
    return "%dd %dh" % (d, h) if d else "%dh %dm" % (h, m)

# --------------------------------------------------------------------------- app


class Smooth:
    """Eases a displayed value toward its target so bars glide instead of jump."""

    __slots__ = ("v", "peak")

    def __init__(self):
        self.v, self.peak = 0.0, 0.0

    def step(self, target, dt, speed=14.0, peak_decay=0.35):
        self.v += (target - self.v) * min(1.0, dt * speed)
        self.peak = max(self.peak - peak_decay * dt, target)
        return self.v


class App:
    HIST_DT = 0.1  # seconds between graph samples

    def __init__(self, fps, window):
        self.fps, self.window = fps, window
        self.truecolor = os.environ.get("COLORTERM", "").lower() in ("truecolor", "24bit") or \
            os.environ.get("TERM_PROGRAM") in ("iTerm.app", "WezTerm", "ghostty", "vscode")
        self.ncpu = len(cpu_ticks()) // 4
        self.cores = cpu_topology(self.ncpu)
        self.cluster_names = []
        for _, name, _ in self.cores:
            if name not in self.cluster_names:
                self.cluster_names.append(name)
        self.model = sysctl_str("machdep.cpu.brand_string") or "Mac"
        self.mem_total = sysctl_int("hw.memsize") or 1
        self.page = page_size()
        self.boot = boot_time()
        self.gpu = Gpu()
        self.bw = Bandwidth()
        self.bw.start()
        self.bw_smooth = Smooth()
        self.bw_peak = 100e9
        self.procs = Processes()
        self.procs.start()

        self.tick_hist = deque()
        self.core_val = [0.0] * self.ncpu
        self.core_smooth = [Smooth() for _ in range(self.ncpu)]
        self.core_spark = [deque([0.0] * 8, maxlen=8) for _ in range(self.ncpu)]
        self.user_frac = self.sys_frac = 0.0
        self.cpu_hist = deque(maxlen=2000)
        self.gpu_hist = deque(maxlen=2000)
        self.mem_hist = deque(maxlen=2000)
        self.bw_hist = deque(maxlen=2000)
        self.gpu_smooth = {k: Smooth() for k in ("dev", "ren", "til")}
        self.gpu_stats = {}
        self.mem = {}
        self.vm_hist = deque()
        self.mem_rates = {}
        self.swap = (0, 0)
        self.pressure = 0
        self.last_hist = self.last_spark = self.last_slow = 0.0
        self.render_ms = 0.0

    # ---- sampling

    def sample(self, now):
        ticks = cpu_ticks()
        self.tick_hist.append((now, ticks))
        while len(self.tick_hist) > 2 and self.tick_hist[1][0] <= now - self.window:
            self.tick_hist.popleft()
        _, old = self.tick_hist[0]
        tu = ts = tt = 0
        for c in range(self.ncpu):
            i = c * 4
            u = (ticks[i] - old[i]) & 0xFFFFFFFF
            s = (ticks[i + 1] - old[i + 1]) & 0xFFFFFFFF
            idle = (ticks[i + 2] - old[i + 2]) & 0xFFFFFFFF
            n = (ticks[i + 3] - old[i + 3]) & 0xFFFFFFFF
            total = u + s + idle + n
            if total:
                self.core_val[c] = (u + s + n) / total
            tu += u + n
            ts += s
            tt += total
        if tt:
            self.user_frac, self.sys_frac = tu / tt, ts / tt

        self.bw_peak = max(self.bw_peak, self.bw.total)

        g = self.gpu.read()
        if g:
            self.gpu_stats = g

        vm = vm_stats()
        p = self.page
        wired = vm.wire_count * p
        app = max(0, vm.internal_page_count - vm.purgeable_count) * p
        # Under heavy pressure the compressor count overlaps other buckets, so cap it to the
        # RAM not already accounted for by free/active/inactive/wired pages.
        accounted = (vm.free_count + vm.speculative_count + vm.active_count + vm.inactive_count +
                     vm.throttled_count + vm.wire_count) * p
        comp = max(0, min(vm.compressor_page_count * p, self.mem_total - accounted))
        cached = (vm.external_page_count + vm.purgeable_count) * p
        used = wired + app + comp
        self.mem = dict(wired=wired, app=app, comp=comp, cached=cached, used=used,
                        free=max(0, self.mem_total - used - cached))
        counters = (vm.pageins, vm.pageouts, vm.swapins, vm.swapouts, vm.compressions, vm.decompressions, vm.faults)
        self.vm_hist.append((now, counters))
        while len(self.vm_hist) > 2 and self.vm_hist[1][0] <= now - 1.0:
            self.vm_hist.popleft()
        t0, c0 = self.vm_hist[0]
        dt = now - t0
        if dt > 0:
            r = [(b - a) / dt for a, b in zip(c0, counters)]
            self.mem_rates = dict(pagein=r[0] * p, pageout=r[1] * p, swapin=r[2] * p, swapout=r[3] * p,
                                  comp=r[4] * p, decomp=r[5] * p, faults=r[6])

        if now - self.last_slow >= 1.0:
            self.last_slow = now
            self.swap = swap_usage()
            level = sysctl_int("kern.memorystatus_level")
            self.pressure = 100 - level if level is not None else 0

        if now - self.last_hist >= self.HIST_DT:
            self.last_hist = now
            self.cpu_hist.append(self.user_frac + self.sys_frac)
            self.gpu_hist.append(self.gpu_stats.get("Device Utilization %", 0) / 100)
            self.mem_hist.append(used / self.mem_total)
            self.bw_hist.append(self.bw.total)
        if now - self.last_spark >= 0.25:
            self.last_spark = now
            for c in range(self.ncpu):
                self.core_spark[c].append(self.core_val[c])

    # ---- drawing

    def draw(self, cv, dt):
        W, H = cv.w, cv.h
        self.draw_header(cv)

        avail = H - 2
        side_by_side = W >= 110
        mid_min = 9 if side_by_side else 13
        inner, gap = W - 4, 2
        # Roomy cells (with sparklines) when they fit, otherwise pack the cores tighter.
        for min_cell in (34, 16):
            cols = max(1, (inner + gap) // (min_cell + gap))
            core_rows = -(-self.ncpu // cols)
            if core_rows + 2 + mid_min <= avail:
                break

        extra = avail - (core_rows + 2) - mid_min
        if extra >= 0:
            cpu_graph = min(6, extra // 3)
            extra -= cpu_graph
            mid_extra = min(8 if side_by_side else 10, extra // 2)
            extra -= mid_extra
            proc_h = extra if extra >= 5 else 0
            mid_h = mid_min + mid_extra + (0 if proc_h else extra)
        else:
            cpu_graph = proc_h = 0
            mid_h = max(0, avail - (core_rows + 2))

        y = 1
        cpu_h = core_rows + 2 + cpu_graph
        self.draw_cpu(cv, 0, y, W, cpu_h, cols, gap, cpu_graph, dt)
        y += cpu_h
        if side_by_side:
            gw = W // 2
            self.draw_gpu(cv, 0, y, gw, mid_h, dt)
            self.draw_mem(cv, gw, y, W - gw, mid_h, dt)
        else:
            gh = mid_h // 2
            self.draw_gpu(cv, 0, y, W, gh, dt)
            self.draw_mem(cv, 0, y + gh, W, mid_h - gh, dt)
        y += mid_h
        if proc_h:
            self.draw_procs(cv, 0, y, W, proc_h)
        cv.clip()
        self.draw_footer(cv)

    def draw_header(self, cv):
        W = cv.w
        cv.clip()
        cv.fill_bg(0, 0, W, HEADER_BG)
        title = " ◆ fancytop "
        for i, c in enumerate(title):
            cv.put(i, 0, c, TITLE(i / len(title)), HEADER_BG, bold=True)
        la = os.getloadavg()
        right = "load %.2f %.2f %.2f │ up %s │ %s " % (
            la[0], la[1], la[2], fmt_uptime(time.time() - self.boot), time.strftime("%H:%M:%S"))
        if len(title) + len(right) > W:
            right = time.strftime("%H:%M:%S ")
        cv.put(W - len(right), 0, right, FG, HEADER_BG)
        room = W - len(title) - len(right)
        for info in (" %s · %d cores · %d-core GPU · %s " % (self.model, self.ncpu, self.gpu.cores,
                                                            fmt_bytes(self.mem_total, 0)),
                     " %s " % self.model):
            if len(info) <= room:
                cv.put(len(title), 0, info, DIM, HEADER_BG)
                break

    def draw_footer(self, cv):
        y, W = cv.h - 1, cv.w
        cv.put(0, y, " " * W)
        keys = [("q", "quit"), ("+/-", "speed")]
        x = 1
        for k, label in keys:
            cv.put(x, y, k, (0, 0, 0), (0, 200, 255), bold=True)
            x += len(k)
            cv.put(x, y, " " + label + "  ", DIM)
            x += len(label) + 3
        stat = "%d fps · %.1fms/frame · %s" % (self.fps, self.render_ms,
                                                "truecolor" if self.truecolor else "256 colour")
        if x + len(stat) + 1 <= W:
            cv.put(W - len(stat) - 1, y, stat, FAINT)

    def draw_cpu(self, cv, x, y, w, h, cols, gap, graph_h, dt):
        total = self.user_frac + self.sys_frac
        title = [("CPU ", WHITE, True), ("%3.0f%%" % (total * 100), HEAT(total), True),
                 ("  usr %.0f%%  sys %.0f%%" % (self.user_frac * 100, self.sys_frac * 100), DIM, False)]
        for name in self.cluster_names:
            ids = [cid for cid, n, _ in self.cores if n == name]
            avg = sum(self.core_val[c] for c in ids) / len(ids)
            title.append([("  │  ", FAINT, False), ("%s×%d " % (name, len(ids)), FG, False),
                          ("%.0f%%" % (avg * 100), HEAT(avg), True)])
        draw_box(cv, x, y, w, h, title)

        inner_x, inner_w = x + 2, w - 4
        cell_w = (inner_w - gap * (cols - 1)) // cols
        show_spark = cell_w >= 28
        label_w = max(len(lbl) for _, _, lbl in self.cores)
        bar_w = cell_w - label_w - 1 - 5 - (9 if show_spark else 0)
        rows = -(-self.ncpu // cols)
        for k, (cid, name, label) in enumerate(self.cores):
            r, c = k % rows, k // rows
            cx, cy = inner_x + c * (cell_w + gap), y + 1 + r
            sm = self.core_smooth[cid]
            v = sm.step(self.core_val[cid], dt)
            lc = TITLE(self.cluster_names.index(name) / max(1, len(self.cluster_names)))
            cv.put(cx, cy, label.ljust(label_w), lc, bold=True)
            bx = cx + label_w + 1
            if show_spark:
                spark(cv, bx, cy, self.core_spark[cid], HEAT)
                bx += 9
            draw_bar(cv, bx, cy, bar_w, v, HEAT, sm.peak)
            cv.put(bx + bar_w + 1, cy, "%3.0f%%" % (v * 100), HEAT(v) if v > 0.02 else DIM)
        if graph_h:
            draw_graph(cv, inner_x, y + 1 + rows, inner_w, graph_h, self.cpu_hist, HEAT)

    def draw_gpu(self, cv, x, y, w, h, dt):
        g = self.gpu_stats
        dev = self.gpu_smooth["dev"].step(g.get("Device Utilization %", 0) / 100, dt)
        ren = self.gpu_smooth["ren"].step(g.get("Renderer Utilization %", 0) / 100, dt)
        til = self.gpu_smooth["til"].step(g.get("Tiler Utilization %", 0) / 100, dt)
        title = [("GPU ", WHITE, True), ("%3.0f%%" % (dev * 100), PLASMA(dev), True),
                 ("  %s · %d cores" % (self.gpu.model, self.gpu.cores), DIM, False)]
        draw_box(cv, x, y, w, h, title)
        ix, iw = x + 2, w - 4
        bar_w = iw - 9 - 5
        rows = [("Device", dev, self.gpu_smooth["dev"]), ("Render", ren, self.gpu_smooth["ren"]),
                ("Tiler", til, self.gpu_smooth["til"])]
        for i, (label, v, sm) in enumerate(rows):
            cv.put(ix, y + 1 + i, label, FG)
            draw_bar(cv, ix + 9, y + 1 + i, bar_w, v, PLASMA, sm.peak)
            cv.put(ix + 9 + bar_w + 1, y + 1 + i, "%3.0f%%" % (v * 100), PLASMA(v) if v > 0.02 else DIM)
        if "In use system memory" in g:
            cv.put(ix, y + 4, "Memory", FG)
            in_use = "%s in use" % fmt_bytes(g["In use system memory"])
            cv.put(ix + 9, y + 4, in_use, PLASMA(0.8))
            cv.put(ix + 10 + len(in_use), y + 4, "· %s allocated" % fmt_bytes(g.get("Alloc system memory", 0)), DIM)
        if not self.gpu_stats:
            cv.put(ix, y + 4, "GPU statistics unavailable", DIM)
        gh = h - 7
        if gh > 0:
            draw_graph(cv, ix, y + 6, iw, gh, self.gpu_hist, PLASMA)

    def draw_mem(self, cv, x, y, w, h, dt):
        m, T = self.mem, self.mem_total
        used_frac = m.get("used", 0) / T
        title = [("MEMORY ", WHITE, True), ("%s" % fmt_bytes(m.get("used", 0)), OCEAN(used_frac), True),
                 (" / %s  (%.0f%%)" % (fmt_bytes(T, 0), used_frac * 100), DIM, False)]
        draw_box(cv, x, y, w, h, title)
        ix, iw = x + 2, w - 4
        segs = [("Wired", m.get("wired", 0), MEM_WIRED), ("App", m.get("app", 0), MEM_APP),
                ("Compressed", m.get("comp", 0), MEM_COMP), ("Cached", m.get("cached", 0), MEM_CACHE),
                ("Free", m.get("free", 0), MEM_FREE)]
        acc, px = 0, 0
        for _, size, color in segs:
            acc += size
            end = min(iw, round(acc / T * iw))
            cv.put(ix + px, y + 1, "█" * max(0, end - px), color)
            px = max(px, end)
        lx = ix
        for name, size, color in segs:
            text = "%s %s" % (name, fmt_bytes(size))
            if lx + len(text) + 2 > ix + iw:
                break
            cv.put(lx, y + 2, "■", color)
            cv.put(lx + 2, y + 2, text, FG)
            lx += len(text) + 4

        su, st = self.swap
        half = (iw - 2) // 2
        cv.put(ix, y + 3, "Swap", FG)
        swap_txt = "%s/%s" % (fmt_bytes(su), fmt_bytes(st))
        sbw = max(4, half - 6 - len(swap_txt))
        draw_bar(cv, ix + 5, y + 3, sbw, su / st if st else 0, OCEAN)
        cv.put(ix + 6 + sbw, y + 3, swap_txt, DIM)
        px0 = ix + half + 2
        pbw = max(4, iw - (px0 - ix) - 9 - 5)
        cv.put(px0, y + 3, "Pressure", FG)
        draw_bar(cv, px0 + 9, y + 3, pbw, self.pressure / 100, HEAT)
        cv.put(px0 + 9 + pbw + 1, y + 3, "%3d%%" % self.pressure, HEAT(self.pressure / 100))

        r = self.mem_rates
        items = [("in", r.get("pagein", 0)), ("out", r.get("pageout", 0)), ("swap↓", r.get("swapin", 0)),
                 ("swap↑", r.get("swapout", 0)), ("zip", r.get("comp", 0)), ("unzip", r.get("decomp", 0))]
        lx = ix
        cv.put(lx, y + 4, "Paging", FG)
        lx += 7
        for label, val in items:
            val_txt = fmt_rate(val)
            if lx + len(label) + len(val_txt) + 3 > ix + iw:
                break
            cv.put(lx, y + 4, label, DIM)
            cv.put(lx + len(label) + 1, y + 4, val_txt, OCEAN(min(1, val / (64 << 20))) if val >= 1 else FAINT)
            lx += len(label) + len(val_txt) + 3

        cv.put(ix, y + 5, "Bandwidth", FG)
        if self.bw.ok:
            total = self.bw_smooth.step(self.bw.total, dt)
            frac = total / self.bw_peak
            cpu = min(self.bw.cpu, total)
            split = "  CPU %s · GPU & other %s" % (fmt_bw(cpu), fmt_bw(total - cpu))
            val = "%9s" % fmt_bw(total)
            bar_w = max(4, iw - 10 - len(val) - 1 - (len(split) if iw >= 70 else 0))
            draw_bar(cv, ix + 10, y + 5, bar_w, frac, OCEAN, self.bw_smooth.peak / self.bw_peak)
            cv.put(ix + 11 + bar_w, y + 5, val, OCEAN(0.4 + 0.6 * frac), bold=True)
            if iw >= 70:
                cv.put(ix + 11 + bar_w + len(val), y + 5, split, DIM)
        else:
            cv.put(ix + 10, y + 5, "unavailable on this Mac", FAINT)

        gh = h - 8
        if gh > 0:
            if self.bw.ok:
                scale = self.bw_peak
                draw_graph(cv, ix, y + 7, iw, gh, [v / scale for v in list(self.bw_hist)[-iw * 2:]], OCEAN)
                cv.put(ix, y + 7, "DRAM bandwidth · peak %s " % fmt_bw(self.bw_peak), DIM)
            else:
                draw_graph(cv, ix, y + 7, iw, gh, self.mem_hist, OCEAN)

    def draw_procs(self, cv, x, y, w, h):
        rows = self.procs.rows
        ix, iw = x + 2, w - 4
        ncols = 2 if iw >= 130 else 1
        per_col = max(0, h - 3)
        shown = rows[:per_col * ncols]
        draw_box(cv, x, y, w, h, [("PROCESSES ", WHITE, True), ("top %d by CPU" % len(shown), DIM, False)])
        col_gap = 4
        col_w = (iw - col_gap * (ncols - 1)) // ncols
        # columns: pid(7) _ name _ cpu%(6) _ bar _ mem(7)
        bar_w = max(6, min(24, col_w // 5))
        name_w = max(4, col_w - 7 - 1 - 1 - 6 - 1 - bar_w - 1 - 7)
        top = max([r[1] for r in shown] + [100.0])
        for c in range(ncols):
            cx = ix + c * (col_w + col_gap)
            x_name, x_cpu = cx + 8, cx + 8 + name_w + 1
            x_bar = x_cpu + 7
            x_mem = x_bar + bar_w + 1
            cv.put(cx, y + 1, "%7s" % "PID", DIM, bold=True)
            cv.put(x_name, y + 1, "NAME", DIM, bold=True)
            cv.put(x_cpu, y + 1, "%6s" % "CPU%", DIM, bold=True)
            cv.put(x_mem, y + 1, "%7s" % "MEM", DIM, bold=True)
            for i, (pid, pcpu, rss, name) in enumerate(shown[c * per_col:(c + 1) * per_col]):
                ry = y + 2 + i
                cv.put(cx, ry, "%7d" % pid, DIM)
                cv.put(x_name, ry, name[:name_w], FG if pcpu >= 1 else DIM)
                cv.put(x_cpu, ry, "%6.1f" % pcpu, HEAT(min(1, pcpu / 100)) if pcpu >= 1 else DIM)
                draw_bar(cv, x_bar, ry, bar_w, pcpu / top, HEAT)
                cv.put(x_mem, ry, "%7s" % fmt_bytes(rss), OCEAN(0.3))

    # ---- main loop

    def run(self):
        fd_in, out = sys.stdin.fileno(), sys.stdout
        old_attrs = termios.tcgetattr(fd_in)
        tty.setcbreak(fd_in)
        out.write("\x1b[?1049h\x1b[?25l\x1b[2J")
        out.flush()
        prev_lines, prev_size = [], None
        last = time.monotonic()
        try:
            while True:
                frame_start = time.monotonic()
                dt, last = frame_start - last, frame_start
                self.sample(frame_start)

                size = shutil.get_terminal_size((100, 40))
                W, H = size.columns, size.lines
                if (W, H) != prev_size:
                    prev_size, prev_lines = (W, H), []
                    out.write("\x1b[2J")
                cv = Canvas(W, H, self.truecolor)
                self.draw(cv, dt)

                buf = ["\x1b[?2026h"]
                for yy in range(H):
                    line = cv.line(yy)
                    if yy >= len(prev_lines) or prev_lines[yy] != line:
                        buf.append("\x1b[%d;1H" % (yy + 1))
                        buf.append(line)
                    if yy < len(prev_lines):
                        prev_lines[yy] = line
                    else:
                        prev_lines.append(line)
                buf.append("\x1b[?2026l")
                out.write("".join(buf))
                out.flush()
                self.render_ms = self.render_ms * 0.9 + (time.monotonic() - frame_start) * 1000 * 0.1

                timeout = max(0.0, 1.0 / self.fps - (time.monotonic() - frame_start))
                if select.select([fd_in], [], [], timeout)[0]:
                    key = os.read(fd_in, 32).decode(errors="ignore")
                    if any(k in key for k in ("q", "Q", "\x1b", "\x03")):
                        break
                    if "+" in key or "=" in key:
                        self.fps = min(60, self.fps + 5)
                    if "-" in key or "_" in key:
                        self.fps = max(1, self.fps - 5)
        finally:
            out.write("\x1b[0m\x1b[?25h\x1b[?1049l")
            out.flush()
            termios.tcsetattr(fd_in, termios.TCSADRAIN, old_attrs)


def main():
    ap = argparse.ArgumentParser(description="Flashy real-time CPU / GPU / memory monitor for macOS.")
    ap.add_argument("-f", "--fps", type=int, default=20, help="frames per second (default 20)")
    ap.add_argument("-w", "--window", type=float, default=0.5,
                    help="seconds of CPU history each core reading averages over (default 0.5)")
    args = ap.parse_args()
    if not sys.stdin.isatty() or not sys.stdout.isatty():
        sys.exit("fancytop needs an interactive terminal.")
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    try:
        App(max(1, min(60, args.fps)), max(0.1, args.window)).run()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
