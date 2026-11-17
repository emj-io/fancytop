# fancytop

Terminal monitor for Apple Silicon Macs (Rust, ratatui). It reads Mach, IOKit and the private IOReport API directly, with no root and no per-frame subprocesses. macOS-only on purpose: `main.rs` has a `compile_error!` for other targets.

## Commands

- `scripts/check.sh`: fmt, clippy (`-D warnings`), tests, `cargo deny`, gitleaks. This is what the pre-push hook and CI run. Run it before calling anything done.
- `scripts/install-hooks.sh`: enable the pre-push hook (`core.hooksPath = .githooks`). Needs `cargo install cargo-deny --locked` and `brew install gitleaks`.
- `cargo run --release -- --snapshot 160x50`: render one frame as text without a TTY. The quickest way to see a UI change.
- `scripts/pty-smoke.py target/release/fancytop`: checks the app quits and restores the terminal on `q`, Ctrl-C, SIGINT, SIGTERM and SIGHUP.

## Layout

- `main.rs`: args, terminal setup/restore, signal handling, the frame loop and global keys.
- `app.rs`: app state, sampling, panel drawing, per-panel keys.
- `wm.rs`: tiling window manager (split tree, drag/drop, focus). Has the unit tests.
- `ui.rs`: drawing primitives (gradients, bars, graphs, sparklines).
- `theme.rs`: colour themes. `ffi.rs`: raw bindings. `sys.rs`: CPU, memory, processes, sysctl. `gpu.rs`: GPU and DRAM bandwidth. `throughput.rs`: disk and network.

## Conventions

- Match the surrounding code: terse `//!` module docs, comments only for why, no new dependencies without a reason (`deny.toml` restricts licenses to what is already allowed).
- `rustfmt.toml` sets `max_width = 130`; run `cargo fmt`.
- Sampler threads share data through `Arc<Mutex<_>>`; always lock with `sys::lock()` so a panicked sampler can't take the UI down.
- Every `unsafe` call is an FFI call. Check buffer lengths and null pointers before indexing or dereferencing, and release CF/IOKit objects (see `ffi::CfOwned`).
- IOReport is private API: any new use must degrade to an "unavailable" state rather than panic.
- README and `--help` text in `main.rs` both list keys and flags; change both together. Add user-visible changes to `CHANGELOG.md`.
- The license is GPL-3.0-or-later. New dependencies must pass `cargo deny check`.

## Don't

- Don't add Linux/Windows or Intel-Mac code paths without discussing it first.
- Don't force-push or retag a published release.
