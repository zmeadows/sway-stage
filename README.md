# sway-stage

A lightweight declarative workspace launcher for the Sway window manager on Linux.

The planned layout engine will read YAML layouts and launch applications into nested,
proportionally sized tiles on the currently focused, empty workspace. It will leave
existing windows alone and stop if workspace focus changes during construction.

**Status:** `list` discovers named layouts and `check` validates version 1 YAML layouts.
Both work without Sway and do not launch applications. Layout construction (`load`),
capture, the picker, and precompiled releases remain planned. A development-only
read-only Sway IPC example is also available.

- [Specification](docs/SPECIFICATION.md): behavioral contract and YAML format.
- [Roadmap](docs/PLAN.md): milestones and dependency review.
- [Four-window example](examples/four-windows.yaml): a valid layout to check and adapt.
- [Engineering conventions](AGENTS.md): contributor guidance and safety invariants.

Building requires Rust 1.89 or newer (edition 2024):

```sh
git clone https://github.com/zmeadows/sway-stage.git
cd sway-stage
cargo build --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Try discovery and validation against the included example:

```sh
cargo run --locked -- --layouts-dir examples list
cargo run --locked -- --layouts-dir examples check four-windows
cargo run --locked -- --help
```

The first command prints `four-windows`; the second prints `four-windows: valid`.
Without `--layouts-dir`, layouts come from `$XDG_CONFIG_HOME/sway-stage/layouts`,
falling back to `$HOME/.config/sway-stage/layouts`. Discovery never creates directories,
and `list` does not parse file contents. See the specification for discovery and validation
rules. `check` verifies working directories but does not check whether executables are
installed or whether they will create matching windows. It writes no logs or notifications.

To inspect the currently focused workspace, run this manually from a Sway session with
`SWAYSOCK` set:

```sh
cargo run --example inspect_sway
```

The example prints workspace identity, layout, occupancy, and nested tiled/floating
containers with available app IDs, Xwayland classes/IDs, and parent-area percentages.
It omits window titles, issues only workspace/tree queries, and launches no processes
for socket discovery. It is a snapshot utility, not the planned `capture` command.
