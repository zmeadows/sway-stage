# sway-stage

A lightweight declarative workspace launcher for the Sway window manager on Linux.

The planned layout engine will read YAML layouts and launch applications into nested,
proportionally sized tiles on the currently focused, empty workspace. It will leave
existing windows alone and stop if workspace focus changes during construction.

**Status:** `load NAME` safely launches one root window on the focused empty Sway
workspace, verifies its identity and placement, and exits without layout manipulation.
`list` discovers named layouts and `check` validates version 1 YAML without Sway or
application launches. Nested/multi-window execution, sizing, final focus commands,
capture, the picker, and precompiled releases remain planned. The user has manually
verified a single Foot launch; broader compositor behavior remains unverified.
Automated tests use isolated IPC fixtures only.

- [Specification](docs/SPECIFICATION.md): behavioral contract and YAML format.
- [Roadmap](docs/PLAN.md): milestones and dependency review.
- [Four-window example](examples/four-windows.yaml): a valid layout to check and adapt.
- [Single Foot example](examples/single-foot.yaml): the supported single-window load shape.
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

The first command prints `four-windows` and `single-foot`; the second prints
`four-windows: valid`. Loading `four-windows` fails before contacting Sway or launching
anything because nested layout execution is not implemented.
Without `--layouts-dir`, layouts come from `$XDG_CONFIG_HOME/sway-stage/layouts`,
falling back to `$HOME/.config/sway-stage/layouts`. Discovery never creates directories,
and `list` does not parse file contents. See the specification for discovery and validation
rules. `check` verifies working directories but does not check whether executables are
installed or whether they will create matching windows. It writes no logs or notifications.

For your first **manual** launch, build and validate first:

```sh
cargo build --release --locked
./target/release/sway-stage --layouts-dir examples check single-foot
```

Use an isolated disposable Sway session with Foot installed and `SWAYSOCK` set. From a
terminal in this repository, run the following, then manually switch to a completely
empty disposable workspace during the five-second delay. Stay on that workspace until
the launcher exits. The delay lets the launching terminal remain on another workspace:

```sh
sleep 5; /home/zac/Code/sway-stage/target/release/sway-stage --layouts-dir /home/zac/Code/sway-stage/examples load single-foot
```

Adjust the absolute repository paths if needed. Success exits 0 without stdout; the Foot
window remains open. An occupied workspace, focus switch (even away and back), unexpected
window, wrong placement, or uncertain observation stops the run. Applications are left
in place on failure; inspect them before retrying. Do not run another launch with the
same app ID concurrently: exact selectors and new container IDs cannot prove causality.
The loader also conservatively aborts on any window move event, even elsewhere, because
window events lack workspace ancestry. New-window close events and structural prelaunch
activity also abort when their placement history cannot be established.

Direct `std::process::Command` launching preserves literal arguments, child cwd and
environment inheritance/overrides. Application standard streams go to `/dev/null`;
the loader does not wait for application termination. Unlike Sway `exec`, it creates no
workspace-associated activation token. Sway normally falls back to current focus at map
time, but assignment rules, inherited activation/ancestor launch contexts, delegation,
and application behavior can affect placement. Wrong placement is a fatal error; the
loader never moves the window. See the [source review and limitations](docs/PLAN.md#2b-safe-single-window-launching-and-diagnostics).
Manual verification on your Sway version is still required.

Each `load` overwrites a private (0600) `last-run.log` in
`$XDG_STATE_HOME/sway-stage`, falling back to `$HOME/.local/state/sway-stage`.
The log records timestamps, stages, workspace/window IDs, identities, placement, and
elapsed time, omitting titles, full argv, and environment values. Fatal errors go to
stderr with the log path and attempt `notify-send` with a two-second wait. Logging and
notification failures remain secondary. `list`, `check`, help, and version remain
independent of IPC, logging, and notification tools.

To inspect the currently focused workspace, run this manually from a Sway session with
`SWAYSOCK` set:

```sh
cargo run --example inspect_sway
```

The example prints workspace identity, layout, occupancy, and nested tiled/floating
containers with available app IDs, Xwayland classes/IDs, and parent-area percentages.
It omits window titles, issues only workspace/tree queries, and launches no processes
for socket discovery. It is a snapshot utility, not the planned `capture` command.

`cargo run --example observe_sway` provides ten seconds of read-only event observation
using the same production observer as `load`. It sends workspace/tree queries and tick
fences only; the fences are visible to other tick subscribers.
