#!/bin/sh
# Everything CI runs that works locally: format, lint, tests, dependency audit and a secrets scan.
set -eu
cd "$(dirname "$0")/.."

need() {
    command -v "$1" >/dev/null 2>&1 || { echo "missing $1: $2" >&2; exit 1; }
}
cargo deny --version >/dev/null 2>&1 || { echo "missing cargo-deny: cargo install cargo-deny --locked" >&2; exit 1; }
need gitleaks "brew install gitleaks"

cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check
gitleaks git --no-banner --redact .
