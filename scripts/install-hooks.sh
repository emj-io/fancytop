#!/bin/sh
# Point git at the versioned hooks in .githooks.
cd "$(dirname "$0")/.." && git config core.hooksPath .githooks && echo "hooks installed (.githooks/pre-push runs scripts/check.sh)"
