#!/usr/bin/env bash
# Builds sway-stage and the integration harness, then runs the integration scenarios in
# private, disposable headless Sway compositors that exist only for this run: one output for
# most scenarios, and a separate two-output compositor only for the cross-output scenario.
#
#   tools/integration/run.sh                 run every scenario
#   tools/integration/run.sh --filter TEXT   run the scenarios whose names contain TEXT
#
# The harness parses and checks these arguments itself.
# The harness's only supported entry point. Contributors and agents may run it, agents under
# the restrictions in AGENTS.md. See docs/DEVELOPMENT.md.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

# Explicit flags override CARGO_TARGET_DIR, CARGO_BUILD_TARGET, and Cargo configuration, so
# both binaries are always built fresh in this one place. The harness then runs the
# sway-stage built beside it.
host_target=$(rustc --print host-tuple)
cargo build --release --locked --target-dir target --target "$host_target" \
    --bin sway-stage --example integration
exec "target/$host_target/release/examples/integration" "$@"
