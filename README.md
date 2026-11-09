# sway-stage

A lightweight declarative workspace launcher for the Sway window manager on Linux.

The planned executable will read YAML layouts and launch applications into nested,
proportionally sized tiles on the currently focused, empty workspace. It will leave
existing windows alone and stop if workspace focus changes during construction.

**Status:** specification, engineering baseline, and a development-only read-only Sway IPC
example. No CLI commands, YAML parsing, layout construction, or precompiled releases are
implemented. The main binary remains a placeholder that exits unsuccessfully.

- [Specification](docs/SPECIFICATION.md): planned behavior and YAML format.
- [Roadmap](docs/PLAN.md): milestones and dependency review.
- [Four-window example](examples/four-windows.yaml): illustration of the planned format.
- [Engineering conventions](AGENTS.md): contributor guidance and safety invariants.

Building requires Rust 1.89 or newer (edition 2024):

```sh
cargo build --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

To inspect the currently focused workspace, run this manually from a Sway session with
`SWAYSOCK` set:

```sh
cargo run --example inspect_sway
```

The example prints workspace identity, layout, occupancy, and nested tiled/floating
containers with available app IDs, Xwayland classes/IDs, and parent-area percentages.
It omits window titles, issues only workspace/tree queries, and launches no processes
for socket discovery. It is a snapshot utility, not the planned `capture` command.
