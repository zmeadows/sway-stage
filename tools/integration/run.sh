#!/usr/bin/env bash
# Builds sway-stage and runs its headless integration checks in a private, disposable
# headless Sway compositor. For now, that is a temporary two-Foot-window smoke test.
#
# User-run only: agents must never execute this script. See docs/DEVELOPMENT.md.
set -euo pipefail

if (($# != 0)); then
    echo "usage: tools/integration/run.sh" >&2
    exit 2
fi

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

cargo build --release --locked --bin sway-stage --example headless_smoke
exec ./target/release/examples/headless_smoke
