---
name: check
description: Run fancytop's full verification (format, clippy, tests, dependency audit, secrets scan, terminal-restore smoke test) and fix what fails. Use before finishing a change, committing, or pushing.
---

1. Run `scripts/check.sh`. It stops at the first failure.
2. Fix the cause, not the lint: don't add `#[allow]` or loosen `deny.toml` to get green unless there is a reason you can state.
3. If the change touched `main.rs`, signal handling or terminal setup, also run `cargo build --release && scripts/pty-smoke.py target/release/fancytop`.
4. If the change touched drawing, run `cargo run --release -- --snapshot 160x50` and look at the output.
5. Report what ran and what failed. Don't claim a check passed that you didn't run.
