# sway-stage

A lightweight declarative workspace launcher for the Sway window manager on Linux.

It reads YAML layouts and launches applications into ordered, nested horizontal and
vertical tiles on the currently focused, empty workspace. It leaves existing windows
alone and stops if workspace focus changes during construction.

**Status:** `load NAME` constructs single-window and nested layouts using Sway's default
sizing. YAML weights and final `focus` remain valid but are not applied yet (Milestone
2C.2). `list` discovers layouts and `check` validates version 1 YAML without Sway or
application launches. Capture, the picker, and precompiled releases remain planned.
Automated verification uses isolated IPC fixtures only. The user has manually verified
the single-Foot, two-Foot, and four-window Firefox/Foot layouts on Sway. These checks
confirm those layouts; broader compositor behavior remains unverified.

- [Specification](docs/SPECIFICATION.md): behavioral contract and YAML format.
- [Roadmap](docs/PLAN.md): milestones and dependency review.
- [Four-window example](examples/four-windows.yaml): a valid layout to check and adapt.
- [Two Foot example](examples/two-foot.yaml): a minimal horizontal split.
- [Single Foot example](examples/single-foot.yaml): a root window.
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

The first command prints `four-windows`, `single-foot`, and `two-foot`; the second prints
`four-windows: valid`.
Without `--layouts-dir`, layouts come from `$XDG_CONFIG_HOME/sway-stage/layouts`,
falling back to `$HOME/.config/sway-stage/layouts`. Discovery never creates directories,
and `list` does not parse file contents. See the specification for discovery and validation
rules. `check` verifies working directories but does not check whether executables are
installed or whether they will create matching windows. It writes no logs or notifications.

For **manual verification only**, build and validate first:

```sh
cargo build --release --locked
./target/release/sway-stage --layouts-dir examples check two-foot
./target/release/sway-stage --layouts-dir examples check four-windows
```

Use an isolated disposable Sway session with `SWAYSOCK` pointing to that session.
Install/verify Foot and, for the four-window example, `firefox-esr` with the exact app ID
in the YAML; adapt the example to your installation before checking it. From a terminal
in this repository, run **one** command below, then manually switch to a completely empty
disposable workspace during the five-second delay. The launching terminal must remain
on another workspace. Stay on the target until the launcher exits.

```sh
sleep 5; /home/zac/Code/sway-stage/target/release/sway-stage --layouts-dir /home/zac/Code/sway-stage/examples load two-foot
```

Expect two Foot windows, left then right. Repeat on a **different empty workspace**:

```sh
sleep 5; /home/zac/Code/sway-stage/target/release/sway-stage --layouts-dir /home/zac/Code/sway-stage/examples load four-windows
```

Expect Firefox, a middle Foot, and a right vertical group with upper/lower Foot windows.
Default sizing will differ from the configured weights, and browser final focus is not
applied. Adjust the absolute repository paths if needed. Success exits 0 without stdout;
inspect the hierarchy manually with `cargo run --example inspect_sway` if useful.
Fixture success does not establish compatibility with your Sway version.

Occupied workspaces, focus switches (even away and back), unexpected windows or hierarchy,
wrong placement, and uncertain observation stop the run. Applications remain in place
on failure; inspect them before retrying. Avoid concurrent launches with the same app ID:
selectors and new IDs cannot prove causality. Any window move event still aborts, even
elsewhere. Split reparenting is checked as an exact tree transition, with new structural
IDs read back from Sway; the reviewed split path emits no window move events. Uncertain
structural activity between launches or during command verification also aborts.

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
The log records timestamps, stages, workspace/window IDs, identities, placement, elapsed
time, structural commands/replies, and expected/observed hierarchies. It omits titles,
full argv, and environment values. Fatal errors go to stderr with the log path and attempt `notify-send` with a two-second wait. Logging and
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
