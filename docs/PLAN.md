# Development roadmap

The [specification](SPECIFICATION.md) owns behavior. This document owns implementation
status, remaining milestones, design decisions, Sway and `swayipc` findings, open questions,
and the manual verification procedure. Keep one binary crate, synchronous control flow,
and a small set of concrete types; add modules only as implemented responsibilities
justify them.

## Status

Implemented through milestone 2:

- Layout discovery, `list`, and `check` with strict YAML validation.
- `load` for a root window and arbitrarily nested splits, with relative weights applied by
  bounded sizing and verified final focus.
- Launch diagnostics: a private `last-run.log`, stderr errors, and bounded `notify-send`
  notifications for fatal errors and sizing warnings.
- Development examples: `inspect_sway` prints a read-only snapshot of the focused workspace,
  and `observe_sway` runs the production observer for ten seconds.

Remaining: `capture` and `pick` (milestone 3), then the first Linux release (milestone 4).

Automated tests are offline. Configuration and CLI tests use temporary directories and a
fake `notify-send`; IPC tests replay recorded trees and the window events Sway emits for
them over disposable `UnixStream::pair` sockets, with harmless child scripts as
applications. They never use `SWAYSOCK` and do not simulate Sway's window manager.

The user has manually verified, on Sway, construction of the single-Foot, two-Foot, and
four-window Firefox/Foot examples, and Firefox focus after the four-window load with
parent-area values consistent with the sizing tolerance. Still unverified live: precise
normalized axis shares, abort behavior under real interference, keybinding launch contexts,
multi-seat behavior, and a supported Sway version.

## Completed milestones

- **Preliminary inspection.** Read-only `inspect_sway` example over `GET_WORKSPACES` and
  `GET_TREE`.
- **1. Validation and discovery.** Typed split/window model, strict YAML, immediate-file
  discovery, `check`, and `list`; no IPC, launches, logs, or notifications.
- **2A. Event observation.** Subscribed reader thread, bounded event history, tick fences,
  and monotonic deadlines (`src/observation.rs`).
- **2B. Single-window launching.** Direct process launch, global new-window matching,
  provisional then confirmed acceptance, and launch diagnostics (`src/launch.rs`,
  `src/diagnostics.rs`).
- **2C.1. Nested construction.** Recursive guarded construction of arbitrary split trees.
- **2C.2. Sizing and focus.** Weighted sizing from outer splits inward, then verified final
  focus.

## 3. Capture and optional picker

Implement read-only `capture`, preserving nested hierarchy and proportions while emitting
missing-command placeholders. Reuse the observer's snapshot validation, `locate_windows`,
`sibling_dimensions`, and `proportions`; capture sends no commands. Report capture errors
through `diagnostics::notify_failure`, never `Diagnostics::start`, so `last-run.log` is
untouched. Decide whether to enable serde-saphyr serialization or emit the small document
by hand; placeholder comments may favor a hand-written emitter.

Add `pick` through an optional `fuzzel --dmenu` subprocess (`std::process::Command`, no
shell). Only after an exact selection may it start launch diagnostics and call the same
path as `load` (`load_named` in `src/main.rs`); cancellation and preselection errors leave
the log alone.

Complete when fixture-based captures preserve order, focus, and proportions; unsupported
floating/tabbed/stacked/fullscreen layouts fail before output; missing match metadata
remains an explicit draft; and filling placeholders yields a checkable layout. Fake picker
tests cover exact selection, cancellation, missing executable, and invalid output. Confirm
neither tool is required by unrelated commands and capture never issues mutations.

## 4. First Linux release

Review actual behavior against the specification, update the README with working usage,
and document demonstrated limitations. The MIT license and repository URL are set. Add a
small tagged-release workflow after the behavior is validated, reusing the CI checks.

Complete when precompiled Linux archives and checksums can be reproduced from a tag,
installation is documented and smoke-tested, the supported Sway version and Linux
architecture/libc baseline are recorded, and optional-tool failures are understandable.
Start with x86_64 Linux; add ARM64 only with a validated build/run path. Do not claim
static linking, other platforms, or untested compositor versions.

## Architecture and decisions

| Module | Responsibility |
| --- | --- |
| `main.rs` | Clap CLI, command dispatch, and the single `load_named` path. |
| `discovery.rs` | Layout directory resolution and immediate-file discovery. |
| `config.rs` | Strict YAML parsing into `LayoutNode` trees and semantic validation. |
| `observation.rs` | Event reader, fenced snapshots, and read-only tree inspection. |
| `launch.rs` | Launching, window acceptance, guarded construction, sizing, and focus. |
| `diagnostics.rs` | `last-run.log`, stderr reporting, and bounded notifications. |

**Observation.** One reader thread owns the blocking `swayipc` event stream and forwards
workspace, window, and tick events through a 256-message channel; overflow is fatal, and
the reader never blocks on a send. Each snapshot uses a disposable query connection on a
temporary thread: `GET_WORKSPACES`, `GET_TREE`, then `SEND_TICK` with a unique payload.
Consuming events through that tick proves earlier events were read; initial ticks and
other clients' ticks cannot satisfy the fence. The first snapshot freezes the target
workspace ID; a focus event away from it aborts even if focus returns before the next
snapshot. A workspace reload or the target's destruction also aborts. Snapshot history is
capped at 256 events. A cloned event socket provides `shutdown(Both)` cancellation, and
dropping the observer joins the reader even during an idle or partial read.

**Guarded command path.** Every launch and Sway command starts from a settled fence: a
fenced snapshot whose tree verifies the complete expected hierarchy and every accepted
window, and whose history holds no window events. Structural events for unaccepted windows
(new, close, move, floating, fullscreen) abort; harmless window events trigger another
fence within the same two-second deadline. A command runs on its own connection, every
reply must report success, and a settled fence afterwards must show the exact expected
structural transition. New wrapper IDs are bound from that tree, never predicted. The path
exists because IPC is not transactional, an accepted command is not proof of its effect,
and `swayipc` discards the reply's success flag.

**Launch acceptance.** Each launch snapshots global container IDs, spawns once, and polls
fenced trees every 100 ms within the leaf's monotonic deadline (IPC waits are bounded to
two seconds). `std::process::Command` preserves argv and applies cwd and environment
overrides to the child only; explicit relative executables (containing `/`) resolve
against the effective cwd. Children get null standard streams and their own process group,
so a terminal interrupt aimed at the launcher does not reach them. A new window must be
the only new, unclaimed window with the exact selector, on the target, tiled, and not
fullscreen. A first matching tree makes it provisional; a second fresh tree without window
events confirms it. Delayed identities may wait; any contradictory history fails.
New-window diagnostic history is capped at 256 IDs. Process exit is only diagnostic, since
applications may delegate; exited children are reaped, and live ones are never waited on,
killed, or relaunched.

**Construction.** Build the first child, enclose it in the split, then focus each
completed child by ID before launching the next sibling. A singleton parent changes axis
in place (`splith`/`splitv`, or `layout splith`/`layout splitv` when the parent is tabbed
or stacked). When the first completed child is a split occupying the whole workspace,
focus an owned direct child, `focus parent`, and run the criteria-free workspace split
only after the settled pre-command fence shows the workspace focused. All other commands
use numeric `con_id` criteria. No placeholders, scratchpad staging, moves, or extra splits
are used; equal-axis groups stay separate.

**Sizing.** For each split, outer before inner, compute `weight / sibling sum` and compare
it with sibling container dimensions along the split axis, normalized by their sum. Set
each out-of-tolerance child with `resize set <width|height> N px`, remeasuring after every
command. Stop within 5 percentage points per child, after three passes, or when a pass
improves the maximum error by less than 0.1 percentage point. Residual deviation or
unusable geometry becomes a warning, logged, printed, and sent as one normal-urgency
notification after final focus succeeds.

**Diagnostics.** Each launch replaces `last-run.log` by unlinking it and creating a new
0600 file with `create_new` (0700 directories), so an existing symlink or hard link is never
followed or reused. Records carry wall-clock and elapsed times, stage, and escaped text;
they omit titles, full argv, and environment values. `notify-send` runs with literal
arguments and null streams and is killed after two seconds; it is the only process the tool
ever terminates. Partial-layout advice appears only after this run spawned an application.

## Sway and swayipc findings

Upstream sources reviewed on 2026-11-09 against Sway master (and the 1.10 tag for
`resize.c`). These are source-level conclusions, not compatibility claims for an installed
Sway version.

- **Tree.** `Node::id` identifies every container; `Node::window` is an optional X11 ID.
  App IDs and Xwayland classes are optional. A workspace's `focused` flag is false when a
  descendant has focus. Traverse `nodes` and `floating_nodes` explicitly. `percent` is
  parent-area share, may be absent, and is never used for sizing.
  [`ipc-json.c`](https://github.com/swaywm/sway/blob/master/sway/ipc-json.c) serializes
  `fullscreen_mode: 1` on every workspace, so only `con`/`floating_con` values carry
  fullscreen state to descendants.
- **Socket.** Use `SWAYSOCK` with `Connection::from(UnixStream)`. `Connection::new()` prefers
  `I3SOCK` and can spawn socket-discovery processes.
- **Event framing.** `swayipc` 4.0.0 `receive_from_stream` uses separate `read_exact` calls
  for the 14-byte header and payload with local buffers, so a read timeout can lose partial
  progress and corrupt framing. Never use socket timeouts or retries on the event stream;
  the blocking reader plus channel deadlines avoids this without an async runtime or a
  second IPC client. `EventStream` has no cancellation method, and its iterator returns
  `Some` even on EOF or errors; stop at the first error.
- **Fences.** Sway's [IPC server](https://github.com/swaywm/sway/blob/master/sway/ipc-server.c)
  queues tick events before replying to `SEND_TICK` and sends each client's messages in
  order. A fence covers events emitted through a known point, not later changes, and the
  tree may predate some returned events; snapshots and events are not an atomic view.
- **Identity updates.** The [app-ID](https://github.com/swaywm/sway/blob/master/sway/desktop/xdg_shell.c)
  and [Xwayland class](https://github.com/swaywm/sway/blob/master/sway/desktop/xwayland.c)
  handlers emit no dedicated IPC event, so waits poll fresh trees even when idle.
- **Window events.** Payloads omit workspace ancestry, so any move event (even a foreign
  one) and any new-window close event abort. A window's map emits `window::new`, then
  `window::focus` if it takes focus. Sway 1.10.1's `seat_set_workspace_focus` emits
  `window::focus` only when focus moves to a different view, never for workspace or
  structural-container focus. A command's events precede its reply, so they arrive in the
  next fence.
- **Splits.** [`container.c`](https://github.com/swaywm/sway/blob/master/sway/tree/container.c)
  (`container_split`, `container_replace`, `container_flatten`, `container_squash`) explains
  singleton orientation changes, wrapper creation, and simplification; the split path
  reparents without move events.
  [`workspace.c`](https://github.com/swaywm/sway/blob/master/sway/tree/workspace.c)
  (`workspace_split`, `workspace_wrap_children`) wraps populated children, keeping their
  old axis. [`view.c`](https://github.com/swaywm/sway/blob/master/sway/tree/view.c)
  (`view_map`) inserts a new tiled view after the focused container, including a focused
  group. Command semantics come from
  [`split`](https://github.com/swaywm/sway/blob/master/sway/commands/split.c),
  [`focus`](https://github.com/swaywm/sway/blob/master/sway/commands/focus.c), and
  [`layout`](https://github.com/swaywm/sway/blob/master/sway/commands/layout.c).
- **Resizing.** In [`resize.c`](https://github.com/swaywm/sway/blob/master/sway/commands/resize.c),
  tiled `resize set ... ppt` converts against the nearest ancestor with the requested axis,
  or the workspace, and width/height changes redistribute the delta equally among the other
  siblings. A resize can succeed without changing geometry at minimum sizes. Setting each
  child's percentage once therefore cannot establish a multi-child ratio; use pixel targets
  from fresh rectangles. [`arrange.c`](https://github.com/swaywm/sway/blob/master/sway/tree/arrange.c)
  accounts for gaps and integer rounding.
- **Command replies.** `swayipc-types` 2.0.3 `CommandOutcome::decode` ignores `success` and
  can lose an incomplete error object. `run_command` in `src/launch.rs` therefore encodes
  with swayipc, then reads one reply frame itself, checking the magic, reply type, a 1 MiB
  bound, and the deadline. An uncertain command is never retried.
- **Placement.** Sway's [`cmd_exec_process`](https://github.com/swaywm/sway/blob/master/sway/commands/exec_always.c)
  creates a launcher context, exports `XDG_ACTIVATION_TOKEN` (and usually
  `DESKTOP_STARTUP_ID`), and runs `sh -c`.
  [`launcher.c`](https://github.com/swaywm/sway/blob/master/sway/desktop/launcher.c) ties that
  context to the focused workspace and finds it through process ancestry; `view_map` checks
  assignments and launcher context before falling back to seat focus, and fullscreen output
  requests can also influence placement. Direct `Command` launching creates no token, so a
  simple new client should map on the focused workspace, but inherited activation
  variables, ancestor launch contexts, delegating applications, and assignment rules can
  place it elsewhere. Environment is inherited unchanged; no token is fabricated or
  stripped, and wrong placement fails without moving anything.
- **Decoder limits.** `swayipc` is not a hardened protocol validator: it allocates the
  declared payload, disables JSON recursion limits, accepts defaulted fields, ignores
  trailing JSON, and masks the event high bit. The observer validates decoded workspace,
  container, and focus invariants and assumes a normal local Sway peer. Strict wire
  validation would need a targeted upstream patch, not a second IPC implementation.
  Deadlines cover established sockets and I/O waits, not `UnixStream::connect`, scheduling,
  or decoding time.

## Dependencies

Reviewed against upstream documentation on 2026-11-09:

| Need | Decision |
| --- | --- |
| CLI | [Clap 4.6.7](https://docs.rs/clap/4.6.7/clap/) with derive, std, help, usage, and error-context; default features disabled. |
| Typed YAML | [Serde](https://serde.rs/) with derive and [serde-saphyr 2.1.1](https://docs.rs/serde-saphyr/2.1.1/serde_saphyr/) with only `deserialize`; the parser declares Rust 1.89. |
| Field diagnostics | [serde_path_to_error 0.1.20](https://docs.rs/serde_path_to_error/0.1.20/serde_path_to_error/) for nested field paths. |
| Native synchronous IPC | [swayipc 4.0.0](https://docs.rs/swayipc/4.0.0/swayipc/struct.Connection.html) and swayipc-types 2.0.3 (Serde, serde_json, thiserror 2); no async runtime. |
| Processes | `std::process::Command` for applications, Fuzzel, and `notify-send`; no shell wrapper, Fuzzel crate, or supervision system. |
| Errors and logging | Standard-library I/O and contextual `String` errors; add a logging or error crate only for a concrete need. |
| Tests | `serde_json` as a development dependency, already locked through `swayipc`. |

serde-saphyr brings granit-parser, encoding_rs/encoding_rs_io, smallvec, and
annotate-snippets; Clap adds its builder, lexer, and derive crates. No declared compiler
baseline exceeds 1.89, and the locked graph compiles on Rust 1.89.0. Licenses are
permissive: MIT/Apache-2.0, with Unlicense/MIT for memchr, Unicode-3.0 for unicode-ident,
and BSD-3-Clause for encoding_rs. A manual review of the RustSec advisory index found no
applicable advisory; this is not a security guarantee. Do not use the archived
`serde_yaml`. Before adding a dependency, inspect releases, advisories, license, needed
features, the transitive graph, and actual MSRV compilation; keep the lockfile committed
and never silently raise the compiler baseline.

serde-saphyr is configured strictly: duplicate and merge keys are errors, aliases have a
zero budget, unsupported tags are rejected on values and field names, and only one
document is accepted. String fields require quotes around numeric or boolean scalars, and
numeric fields reject quoted numbers; explicit nulls never trigger defaults. Input is read
up to 1 MiB plus one detection byte and must be UTF-8. Parser depth is capped at 68
(children sequences plus window field collections at layout depth 32), with bounded
events, scalar bytes, and nodes, before semantic validation enforces 32 layout levels and
256 layout nodes. Source snippets are disabled so errors cannot echo commands or
environment values.

## Known limitations and open questions

- Sway IPC is not transactional. Fences narrow races but cannot isolate construction from
  concurrent activity. Conservative aborts are intentional: any move event, foreign
  structural activity during a fence, or continuous window activity can stop or time out
  a run.
- App IDs and classes identify a window type, not causality; an indistinguishable
  concurrent launch cannot be detected.
- Placement depends on Sway's rules and inherited launch context. Keybinding and terminal
  launch contexts still need manual verification.
- `swayipc` decoding and socket establishment limits are listed under the findings above.
- No supported Sway version or Linux ABI baseline is set; Cargo publishing stays disabled
  until release behavior is ready.
- Precise axis-share measurement and live abort behavior await manual verification.
- Milestone 3 must settle the capture serialization approach and how `fuzzel --dmenu`
  reports cancellation (exit status and output) as distinct from failure.

## Manual verification

For the user only, in an isolated, disposable Sway session with `SWAYSOCK` pointing to it.
Agents must never run these commands; see [AGENTS.md](../AGENTS.md).

Build and validate from the repository root. Install Foot and, for the four-window example,
`firefox-esr` with the app ID used in the YAML; adapt the example to your installation
before checking it.

```sh
cargo build --release --locked
./target/release/sway-stage --layouts-dir examples check two-foot
./target/release/sway-stage --layouts-dir examples check four-windows
```

From a terminal in the repository root, run **one** load, then switch to a completely empty
disposable workspace during the five-second delay. The terminal must stay on another
workspace; stay on the target until the command exits. Use a different empty workspace
for each run.

```sh
sleep 5; ./target/release/sway-stage --layouts-dir examples load two-foot
sleep 5; ./target/release/sway-stage --layouts-dir examples load four-windows
```

- `two-foot`: two Foot windows of equal width, left then right; the left one focused.
- `four-windows`: Firefox at 44% width, middle Foot at 32%, and the right column at 24%;
  inside the column, the upper Foot at 77% height and the lower at 23%; Firefox focused.
  Shares are sibling dimensions normalized by their sum, excluding gaps, and may differ by
  up to 5 percentage points.

Success exits 0 without stdout. Inspect `last-run.log` after failures. To inspect the
loaded workspace, run either read-only command below from the terminal and switch back to
the loaded workspace during the delay. The inspector prints the hierarchy, focus marker,
app IDs or classes, and Sway's `parent_area`, which is not an axis share.

```sh
sleep 5; cargo run --locked --example inspect_sway
```

The following query (requires `jq`) prints every sibling group with normalized axis
shares and focus flags:

```sh
sleep 5; swaymsg -t get_tree | jq '
  recurse(.nodes[]) | select(.type == "workspace")
  | select(any(recurse(.nodes[]); .focused))
  | recurse(.nodes[]) | select((.nodes | length) > 1)
  | (if .layout == "splith" then "width" else "height" end) as $axis
  | (.nodes | map(.rect[$axis]) | add) as $total
  | {group: .id, layout, children: [.nodes[] |
      {id, app_id, focused, share_pct:
        (if $total > 0 then 100 * .rect[$axis] / $total else null end)}]}'
```

`cargo run --locked --example observe_sway` prints ten seconds of events through the
production observer. It sends only workspace/tree queries and tick fences, which other
tick subscribers can see.
