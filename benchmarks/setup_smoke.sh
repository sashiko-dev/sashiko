#!/bin/bash
# Clone the cJSON repository for use as a lightweight smoke-test target.
# cJSON is a small C project (~80KB source, MIT licensed) with real bugs
# in its git history — ideal for validating the review pipeline quickly
# without needing a full Linux kernel clone.
#
# Usage:
#   ./benchmarks/setup_smoke.sh
#
# Then run:
#   cargo run --release --bin ingest_benchmark -- \
#     --file ./benchmarks/benchmark_smoke.json \
#     --repo ./benchmarks/smoke-repo

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SMOKE_DIR="$SCRIPT_DIR/smoke-repo"

if [ -d "$SMOKE_DIR/.git" ]; then
    echo "smoke-repo already exists at $SMOKE_DIR"
    echo "To re-clone, remove it first: rm -rf $SMOKE_DIR"
    exit 0
fi

echo "Cloning cJSON into $SMOKE_DIR..."
git clone --depth=500 https://github.com/DaveGamble/cJSON.git "$SMOKE_DIR"
echo "Done. $(cd "$SMOKE_DIR" && git log --oneline | wc -l) commits available."
