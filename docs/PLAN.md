# Development roadmap

The [specification](SPECIFICATION.md) owns behavior. This document owns implementation
status, remaining milestones, design decisions, Sway and `swayipc` findings, open questions,
and the manual verification procedure. Keep one binary crate, synchronous control flow,
and a small set of concrete types; add modules only as implemented responsibilities
justify them.

## Status

Implemented through milestone 3A:

- Layout discovery, `list`, and `check` with strict YAML validation.
- `load` for a root window and arbitrarily nested splits, with relative weights applied by
  bounded sizing and verified final focus.
- Launch diagnostics: a private `last-run.log`, stderr errors, and bounded `notify-send`
  notifications for fatal errors and sizing warnings.
- Read-only `capture`, printing an editable draft with `command: null` (and, without an
  identifier, `match: null`) placeholders.
- Development examples: `inspect_sway` prints a read-only snapshot of the focused workspace,
  and `observe_sway` runs the production observer for ten seconds.

Remaining: the integration harness (3B), the Fuzzel picker (3C), then public release
preparation (4).

Automated tests are offline. Configuration and CLI tests use temporary directories and a
fake `notify-send`; IPC tests replay recorded trees and the window events Sway emits for
them over disposable `UnixStream::pair` sockets, with harmless child scripts as
applications. They never use the user's `SWAYSOCK` and do not simulate Sway's window
manager. One CLI test runs `capture` end to end against a fake Sway socket bound in its
temporary directory; sandboxes that forbid binding Unix sockets fail it.

The user has manually verified, on Sway, construction of the single-Foot, two-Foot, and
four-window Firefox/Foot examples, and Firefox focus after the four-window load with
parent-area values consistent with the sizing tolerance. Still unverified live: `capture`,
precise normalized axis shares, abort behavior under real interference, keybinding launch
contexts, multi-seat behavior, and a supported Sway version.

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
- **3A. Read-only capture.** `capture` (`src/capture.rs`) drafts the focused workspace from
  two agreeing fenced observations and refuses unrepresentable arrangements.

## 3B. Self-contained Sway integration harness (next)

A development-only `scripts/integration.py` that the user runs manually inside a Sway
session to exercise `load` and `capture` end to end. When the user runs it, it may
manipulate its designated workspace; agents must never run it against a live desktop.

- Start from an existing workspace and use one designated, disposable workspace that must
  be empty at the start; refuse to run otherwise.
- Load predefined layouts there sequentially: single window, horizontal pair, vertical
  pair, uneven siblings, mixed-orientation nesting, and deeper and same-axis nesting.
  Prefer Foot with a distinct app ID per window; Firefox may be an optional compatibility
  scenario.
- After each load, inspect raw Sway IPC trees independently of `sway-stage` to verify
  hierarchy, child order, app IDs, axis shares within the sizing tolerance, and final
  focus. Then run `capture` and compare its draft with the scenario as an additional
  check, never the sole oracle.
- Between scenarios, close only windows positively identified as the current scenario's:
  containers that appeared during it, with its expected app IDs, on the designated
  workspace. Whenever ownership is uncertain, abort without destructive cleanup and
  report what remains.
- Return to the original workspace and print a concise pass/fail summary per scenario.

Complete when the scenarios run unattended in an isolated session, each failure names its
scenario and check, and cleanup never touches a window it cannot attribute.

## 3C. Fuzzel picker

Add an optional `pick` command through a `fuzzel --dmenu` subprocess
(`std::process::Command`, no shell), reusing configuration discovery and the `load` path.
Only after an exact selection may it start launch diagnostics and call `load_named` in
`src/main.rs`; cancellation and preselection errors leave the log alone and report through
`diagnostics::notify_failure`.

Complete when fake picker tests cover exact selection, cancellation, missing executable,
and invalid output, and Fuzzel is not required by unrelated commands.

## 4. Public release preparation

Packaging, distribution, documentation, and release readiness. Review actual behavior
against the specification, update the README with working usage, and document
demonstrated limitations. The MIT license and repository URL are set. Add a small
tagged-release workflow after the behavior is validated, reusing the CI checks.

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
| `capture.rs` | Read-only conversion of the focused workspace into a YAML draft. |
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
each child more than 1 percentage point off with `resize set <width|height> N px`,
remeasuring after every command. Stop within 1 percentage point per child, after three
passes, or when a pass improves the maximum error by less than 0.1 percentage point.
Stopping at 5 points instead left results near that edge, and capture round trips drifted
by almost 5 points per load; the extra commands cost a few milliseconds each. Residual
deviation beyond 5 percentage points or unusable geometry becomes a warning, logged,
printed, and sent as one normal-urgency notification after final focus succeeds.

**Capture.** `capture` subscribes like `load` and takes two fenced snapshots through one
`Observer`, so a workspace focus change at any point fails it. Each snapshot becomes a
comparison value of container IDs, split axes, child extents, selectors, and the focused
container; the two must be equal, and neither history may hold a window event other than
a title, urgency, or mark change. Window payloads lack workspace ancestry and identity
updates emit no event, hence both rules. One recursive pass validates and converts the
target subtree directly, since it needs the structure that `locate_windows` flattens:
windows are `con` nodes with `shell`, `app_id`, or `window`; a one-child container,
including the workspace, is replaced by its child after its layout is checked; extents
come from `sibling_dimensions`. Weights are `proportions` as percentages rounded to
hundredths with a 0.01 floor, which shifts reconstructed shares by under 3 percentage
points even across 255 siblings. Capture-specific Serde types serialize through
serde-saphyr with single-line scalars and indented sequences, so ambiguous scalars are
quoted and control characters escaped; a fixed comment header is prepended. The strict
`config` types are not reused, and `config` exports its depth and node limits so filled
drafts stay loadable. Failures report through `diagnostics::notify_failure`, never
`Diagnostics::start`, so `last-run.log` is untouched.

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
  or the workspace. On the user's Sway (2026-11-09 log), `resize set width` on a first child
  took the whole delta from the following sibling rather than spreading it over all
  siblings; resizing children in order therefore converges in one pass. The loop does not
  depend on either behavior because it remeasures after every command. A resize can
  succeed without changing geometry at minimum sizes. Setting each
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
| Typed YAML | [Serde](https://serde.rs/) with derive and [serde-saphyr 2.1.1](https://docs.rs/serde-saphyr/2.1.1/serde_saphyr/) with `deserialize` and `serialize` (for capture drafts); it declares Rust 1.89. |
| Field diagnostics | [serde_path_to_error 0.1.20](https://docs.rs/serde_path_to_error/0.1.20/serde_path_to_error/) for nested field paths. |
| Native synchronous IPC | [swayipc 4.0.0](https://docs.rs/swayipc/4.0.0/swayipc/struct.Connection.html) and swayipc-types 2.0.3 (Serde, serde_json, thiserror 2); no async runtime. |
| Processes | `std::process::Command` for applications, Fuzzel, and `notify-send`; no shell wrapper, Fuzzel crate, or supervision system. |
| Errors and logging | Standard-library I/O and contextual `String` errors; add a logging or error crate only for a concrete need. |
| Tests | `serde_json` as a development dependency, already locked through `swayipc`. |

serde-saphyr brings granit-parser, encoding_rs/encoding_rs_io, smallvec, num-traits, and
annotate-snippets for parsing, and base64 0.23, nohash-hasher 0.2, and zmij (already locked
through serde_json) for serialization; none of those three has further dependencies. Clap
adds its builder, lexer, and derive crates. No declared compiler baseline exceeds 1.89,
and the locked graph compiles on Rust 1.89.0. Licenses are permissive: MIT/Apache-2.0,
with Unlicense/MIT for memchr, Unicode-3.0 for unicode-ident, BSD-3-Clause for
encoding_rs, and MIT for zmij. A manual review of the RustSec advisory index found no
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
- Precise axis-share measurement, live abort behavior, and live capture await manual
  verification.
- Capture is deliberately sensitive to activity: any window event other than a title,
  urgency, or mark change fails it, as does focus on a container. It never infers
  commands or approximates tabbed, stacked, floating, or fullscreen state.
- Milestone 3C must settle how `fuzzel --dmenu` reports cancellation (exit status and
  output) as distinct from failure.

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
  Shares are sibling dimensions normalized by their sum, excluding gaps, and should be
  within about 1 percentage point; up to 5 is accepted without a warning.

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

### Capture

Capture only reads Sway. In the same isolated session, load `four-windows` on an empty
workspace as above. From the repository root in a terminal on another workspace, run each
`sleep 5` command and switch to the loaded workspace during its delay:

```sh
mkdir -p /tmp/sway-stage-capture
sleep 5; ./target/release/sway-stage capture > /tmp/sway-stage-capture/draft.yaml
cp /tmp/sway-stage-capture/draft.yaml /tmp/sway-stage-capture/original.yml
./target/release/sway-stage --layouts-dir /tmp/sway-stage-capture check draft
```

Expect a horizontal root of `window1`, `window2`, and a vertical split of `window3` and
`window4`; root weights near 44/32/24 and column weights near 77/23; `focus: window1`; the
example's app IDs; `command: null` on every window; and an unchanged workspace. `check`
fails with placeholder instructions. Replace each `command: null` in `draft.yaml` with the
corresponding command from `examples/four-windows.yaml`, then check, load the draft on a
new empty workspace, and capture that workspace:

```sh
./target/release/sway-stage --layouts-dir /tmp/sway-stage-capture check draft
sleep 5; ./target/release/sway-stage --layouts-dir /tmp/sway-stage-capture load draft
sleep 5; ./target/release/sway-stage capture > /tmp/sway-stage-capture/recaptured.yml
diff /tmp/sway-stage-capture/original.yml /tmp/sway-stage-capture/recaptured.yml
```

The two captures should differ at most in weights, by about 1 percentage point. Then, on
an arranged workspace, try each refusal in turn, restoring the arrangement between
attempts: one window floating (`floating toggle`), one fullscreen, `layout tabbed`, and
`focus parent`. Each capture exits 1 with an explanation on stderr and a notification,
and leaves its output file empty, since the shell creates it before capture runs.
`last-run.log` is unchanged throughout (compare `sha256sum` before and after).
