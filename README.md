# fancytop

[![CI](https://github.com/emj-io/fancytop/actions/workflows/ci.yml/badge.svg)](https://github.com/emj-io/fancytop/actions/workflows/ci.yml)

A flashy, fast terminal monitor for Apple Silicon Macs: every CPU core, the GPU, memory, live DRAM bandwidth, and disk and network I/O.

## Build and run

Needs an Apple Silicon Mac and Rust 1.89 or newer ([rustup](https://rustup.rs)). No root or sudo.

```
cargo install --git https://github.com/emj-io/fancytop   # or grab a binary from the Releases page
cargo build --release
./target/release/fancytop             # 30 fps by default
./target/release/fancytop --fps 60    # up to 120
cargo install --path .                # or install it as `fancytop` in ~/.cargo/bin
```

Options:

| flag | meaning |
|---|---|
| `-f`, `--fps <N>` | frames per second, 1–120 (default 30) |
| `-w`, `--window <SECS>` | seconds of history each per-core reading averages over (default 0.5) |
| `-t`, `--theme <NAME>` | colour theme to start with (default Neon) |
| `--snapshot <WxH>` | print one frame as plain text at that size (e.g. `160x50`) and exit |

Keys: `q` / `Esc` / `Ctrl-C` quit, `+` / `-` raise or lower the frame rate by 1 fps (1–120).

### Arranging windows

Panels tile the screen like a tiling window manager (i3, say): rows and columns of windows that never overlap.
They start in a layout that adapts to the terminal size; the first change switches to your own tiling, which
then scales with the terminal.

With the mouse:

- **Resize**: drag the border between two windows. Only those two change size, and the border snaps into line
  with other borders and to the halfway point. Drag a corner where two borders meet to move both.
- **Move**: drag a window's title bar onto another window. Dropped near an edge it tiles beside that window on
  that side; dropped in the middle the two swap. Dropped on the header, the footer or a side of the screen, it
  spans that whole edge. A shaded outline shows where it will land.

| key | action |
|---|---|
| `1`–`5` | show / hide CPU, GPU, memory, I/O, processes |
| arrows | focus the window in that direction |
| `Shift` + arrows | swap the focused window with the next one that way, or move it to that edge of the screen |
| `Tab` / `Shift-Tab` | focus the next / previous window |
| `[` / `]` | make the focused window narrower / wider |
| `{` / `}` | make the focused window shorter / taller |
| `t` | switch the focused window's split between side by side and stacked |
| `z` | zoom the focused window to fill the screen, or unzoom |
| `r` | back to the automatic layout with every panel shown |
| `m` | turn mouse capture off / on; with it off, the terminal can select text again |
| `c` / `C` | next / previous colour theme |

### Colour themes

Press `c` to cycle through eleven themes: **Neon** (the original, on your terminal's own
background), **Synthwave**, **Dracula**, **Nord**, **Gruvbox**, **Tokyo Night**, **Matrix**,
**Phosphor** (amber CRT), **Lava**, **Abyss** (deep-sea bioluminescence) and **Sakura** (light).
All but Neon paint their own background.

## What it shows

- **CPU**: one row per logical core, grouped by cluster (for example Super / Performance on M5), each with a
  sparkline of the last 2 seconds, a gradient bar with peak hold, and a percentage. Also total, user and system
  load, per-cluster averages, and a history graph of total load.
- **GPU**: device, renderer and tiler utilisation, GPU memory in use and allocated, and a history graph.
- **Memory**: wired / app / compressed / cached / free breakdown, swap, memory pressure, paging and swap rates,
  and live **DRAM bandwidth** split into CPU vs GPU & other, with a bandwidth history graph.
- **I/O**: disk read / write and network in / out throughput, each with a history graph. The bars and graphs
  are scaled to the busiest moment in the last few minutes.
- **Processes**: top processes by CPU (two columns on wide terminals).

The layout adapts to the terminal size. Panels shrink or drop out on small windows.

## How it gets the data

No root needed and no helper processes per frame:

- **CPU and memory**: Mach (`host_processor_info`, `host_statistics64`) and `sysctl`.
- **GPU**: the GPU's IOKit `PerformanceStatistics`. Core topology and GPU model also come from IOKit.
- **DRAM bandwidth**: the memory controllers' counters in IOReport, sampled on a background thread 4× a second.
- **Disk**: the byte counters in each IOKit `IOBlockStorageDriver`'s statistics (internal SSD, external drives
  and mounted disk images).
- **Network**: per-interface byte counters from the `NET_RT_IFLIST2` sysctl, for physical ports (Ethernet, Wi-Fi,
  Thunderbolt). Loopback, bridges and VPN tunnels are skipped because their traffic is already counted on the
  ports underneath. Disk and network are sampled on a background thread 4× a second.
- **Processes**: processes you own are read directly from libproc twice a second, with CPU% measured over that
  interval. macOS doesn't let unprivileged programs read root-owned processes at all (`ps` and `top` can because
  they're setuid root), so those come from `ps` every 2 seconds.

Colours are 24-bit when the terminal advertises it (`COLORTERM=truecolor`, iTerm2, WezTerm, Ghostty, VS Code),
and 256-colour otherwise.

## Notes

- **Overhead**: about 2% of one core for the UI at 30 fps, plus about 3–5% for the bandwidth sampler. Each
  IOReport snapshot costs ~5ms of kernel CPU however many counters it covers. The process list adds under 1%.
- **Memory bandwidth** comes from IOReport, a private macOS API, so a future macOS could change or remove it.
  The row then shows "unavailable" and the graph falls back to memory used. The counters are histograms of
  traffic per short time slice. The total uses the memory-controller aggregate, which matched measured copy loads
  within about 5–10%. It's coarse below about 64 GB/s, so the finer per-client counters fill in at the low end.
  - The GPU's own counter reads zero, so GPU traffic shows up as "GPU & other": everything the CPU clusters don't
    account for.
  - Each CPU cluster's counter tops out at 32 GB/s. When one is pinned there, the split shows `CPU ≥… · GPU &
    other ≤…`, because the true CPU share is higher than the counters can say.
  - The bar is scaled to the highest bandwidth seen this session (at least 100 GB/s), not a theoretical peak.
- CPU% for root-owned processes is macOS's short decaying average (from `ps`), so those rows react more slowly
  than your own processes.
- macOS rounds the network counters it gives unprivileged programs to 4 KB, so very light traffic reads in
  4 KB/s steps.
- CPU/GPU frequency and power aren't shown yet. IOReport exposes them without root too.
- `legacy/fancytop.py` is the original dependency-free Python version.

## Contributing

`scripts/install-hooks.sh` runs `scripts/check.sh` (fmt, clippy, tests, `cargo deny`, gitleaks) before every push; it needs
`cargo install cargo-deny --locked` and `brew install gitleaks`. See [AGENTS.md](AGENTS.md) for how the code is organised,
and [SECURITY.md](SECURITY.md) to report a vulnerability.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). Changes are listed in [CHANGELOG.md](CHANGELOG.md).
