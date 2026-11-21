#!/bin/sh
# Everything CI runs that works locally: format, lint, tests, dependency audit and a secrets scan.
set -eu
cd "$(dirname "$0")/.."

need() {
    command -v "$1" >/dev/null 2>&1 || { echo "missing $1: $2" >&2; exit 1; }
}
need cargo-deny "brew bundle"
need gitleaks "brew bundle"

cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo deny check
gitleaks git --no-banner --redact .
