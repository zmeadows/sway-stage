# Development roadmap

The [specification](SPECIFICATION.md) owns behavior. This document owns implementation
status, remaining milestones, design decisions, Sway and `swayipc` findings, open questions,
and the manual verification procedure. Keep one binary crate, synchronous control flow,
and a small set of concrete types; add modules only as implemented responsibilities
justify them.

## Status

Implemented through milestone 3C (combined menu awaits manual desktop verification):

- Layout discovery, `list`, and `check` with strict YAML validation.
- `load` for a root window and arbitrarily nested splits, with relative weights applied by
  bounded sizing and verified final focus.
- Launch diagnostics: a private `last-run.log`, stderr errors, and bounded `notify-send`
  notifications for fatal errors and sizing warnings.
- Read-only `capture`, printing an editable draft with `command: null` (and, without an
  identifier, `match: null`) placeholders, plus opt-in `--infer-commands` process arguments.
- Combined `menu`: Fuzzel layout loading and named draft capture, with opt-in inference
  and private, non-overwriting saves into the layouts directory.
- Development tools in `tools/`, run as Cargo examples: `inspect_sway` prints a read-only
  snapshot of the focused workspace, and `observe_sway` runs the production observer for
  ten seconds.
- Development-only integration harness in `tools/integration/` (the `integration` Cargo
  example): six Foot scenarios plus four optional extended scenarios, with independent
  raw-tree verification, capture checks, guarded cleanup, workspace restoration, and
  SIGINT handling. Its offline tests are the separate `integration_harness` test target.
  See [3B](#3b-self-contained-sway-integration-harness).

Next remaining milestone: public release preparation (4).

Automated tests are offline. Configuration and CLI tests use temporary directories and a
fake `notify-send`; IPC tests replay recorded trees and the window events Sway emits for
them over disposable `UnixStream::pair` sockets, with harmless child scripts as
applications. They never use the user's `SWAYSOCK` and do not simulate Sway's window
manager. CLI capture/menu tests use fake Fuzzel and Sway sockets bound in temporary
directories; sandboxes that forbid binding Unix sockets require an outside-sandbox run.

The user has manually verified all ten integration scenarios, including raw-tree
sizing/focus, read-only capture, and command inference checks, as well as construction of
the single-Foot, two-Foot, and four-window Firefox/Foot examples. Manual interference runs
(SIGINT during cleanup, a workspace focus change, and an unexpected window) showed the
harness's own guards stopping conservatively. Still unverified live: the combined menu,
the loader's abort behavior under real interference, keybinding launch contexts,
multi-seat behavior, and a supported Sway version.

## Completed milestones

- **Preliminary inspection.** Read-only `inspect_sway` tool over `GET_WORKSPACES` and
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
  one retained `GET_TREE` response and refuses unrepresentable arrangements when rendered.
- **3B. Integration harness.** All six standard and four extended scenarios passed the
  user's manual runs, along with the harness's manual interference checks.
- **3A.1. Optional command inference and integration coverage.** Implemented
  `capture --infer-commands` with bounded Linux process-argument reads and per-window
  null fallback. Offline fixtures cover malformed data, argument preservation, PID
  validation, partial success, and unchanged structure. The existing `single-window`
  and `horizontal-pair` scenarios compare normal and inferred captures, require each
  Foot's run-specific `--app-id`, and recheck workspace ownership/state after each.
  The user has manually verified these observational checks; scenario definitions,
  Firefox/Xterm assertions, and cleanup behavior are unchanged.
- **3C. Combined Fuzzel menu.** `menu [--infer-commands]` loads exact offered selections
  through `load_named` or captures before a separate name prompt. Offline process and IPC
  fixtures cover ordering, cancellation, diagnostics, private saves, and failures.

## 3B. Self-contained Sway integration harness

Implemented as the development-only `integration` Cargo example in `tools/integration/`.
It deliberately shares no code with the loader: it drives Sway through bounded `swaymsg`
subprocesses, decodes their output with its own types rather than `swayipc`, and describes
scenarios with its own layout types, so a defect in the production parser, IPC decoding,
observer, or construction cannot also hide itself in the verification.

It generates temporary YAML outside normal layout discovery and exercises single-window,
horizontal-pair, vertical-pair, uneven-sibling, mixed-nesting, and deep-nesting layouts,
including a meaningful horizontal group inside another horizontal group. Each Foot
gets a random run/scenario/window app ID and runs `sleep 300`, with no shell or hold option.

`--extended` appends Firefox with two Foot windows, Xterm with Foot, nested Firefox with
three Foot windows (44:32:24 widths and 77:23 right-column heights), and two Foot windows
sharing an app ID. All select an explicit non-first Foot focus target. Unavailable optional
applications/subsystems produce `SKIP`; option, launch, identity, geometry, or capture
failures with available applications produce `FAIL` and stop the suite.

The independent verifier checks exact identities and leaf count, visual order, split axes,
meaningful hierarchy, final focus, and rectangle-based normalized sibling shares at every
split. Deviations above 1 percentage point are diagnostics; above 5 fail the scenario.
Only transparent single-child wrappers collapse. Capture is secondary: a narrow comparison
with the known draft format checks fields, nesting, identifiers, focus, placeholders, and
weights against the raw measurements. A filled copy must pass strict `check`; raw workspace
state before and after capture must agree. This intentionally fails visibly if capture's
serialization format changes rather than implementing a YAML parser. In `single-window`
and `horizontal-pair`, a second capture uses `--infer-commands`; its block command arrays
must contain each corresponding Foot's unique `--app-id` token (either combined or paired).
Ignoring commands and comments, both captures must agree exactly. Recovered arguments
are kept in memory, never printed in reports or saved to the temporary configurations.
The ordinary capture checks and independent ownership guards still run unchanged.

Commands and queries are bounded `swaymsg` subprocesses. The only persistent child is the
`swaymsg` workspace/window/tick subscription, with bounded reads and shutdown, needed to
retain away-and-back focus history. Snapshot tick fences and fresh guards reduce races;
they cannot provide transactional isolation. Any focus switch, unexpected structural
activity, IPC failure, or uncertainty aborts. No scenario follows a failure. Ordered
`window::new` events associate sequential launches with layout IDs; selectors alone never
establish cleanup ownership. Cleanup requires globally new container IDs, exact app IDs or
Xwayland classes, the designated workspace, safe tiled state, and the verified arrangement.
Firefox and repeated-ID Foot windows additionally require Sway's PID to expose the expected
literal launch arguments through `/proc/PID/cmdline`: the private browser profile or a
distinct per-leaf Foot `--title` argument. Titles from the tree are not ownership evidence.
Existing Firefox windows elsewhere are ignored; extra new matches anywhere, missing events,
or uncertain process evidence abort without closing anything. Each exact-ID close is
guarded by a new snapshot; disappearance and the remaining hierarchy are checked
afterwards. Failure paths never attempt further closes, moves, process termination,
repairs, or relaunches, and report observed test IDs/app IDs for manual inspection. The
harness terminates only its own direct children (see the process decisions below).

Firefox uses a fresh private profile, HOME, and XDG directories per scenario, with local
first-run preferences, `--no-remote --new-instance --profile`, and
`about:blank`. Native Wayland is required via child-only environment settings.
The expected app ID comes from its adjacent `application.ini`
`App.RemotingName` (including `firefox-esr`), or an explicit `--firefox-app-id` override;
the observed native identity must agree. Both remoting flags are deliberate: newer
[Mozilla startup code](https://github.com/mozilla/gecko-dev/blob/master/toolkit/xre/nsAppRunner.cpp)
ignores `--no-remote`, whereas `--new-instance` disables remote-client delegation.
Browser layouts allow 60 seconds per window; other layouts retain 20 seconds. The
configuration has one timeout shared by all leaves, so the Foot waits in browser layouts
also receive that bound. Sizing tolerances are unchanged; minimum-size violations fail.

Browser files are deliberately retained outside the automatically removed YAML directory,
on success and failure, and their paths are printed. Closing a window cannot prove that
all Firefox processes have exited; no process scanning, killing, or profile deletion is
attempted. The user must remove these directories only after all associated processes
exit. Firefox has no automatic lifetime limit. Xterm uses a unique `-class`, verified as
`window_properties.class` with no native app ID, and `+hold -e sleep 300` (the final
`-e` consumes the remaining command arguments). See the
[Xterm manual](https://invisible-island.net/xterm/manpage/xterm.html).

Restoration is independent of cleanup: it requires reliable focus history, the disposable
workspace still focused, and the original workspace's recorded identity and name still
present. Otherwise it is skipped; an empty original workspace destroyed by Sway is not
recreated. A concurrent user action can still occur after the last guard and before a
command, so run only in a disposable session and leave input idle.

All ten scenarios, including inference checks, have passed the user's manual runs. The
user's manual interference runs (SIGINT during cleanup, a workspace focus change, and an
unexpected window) each stopped conservatively. Those runs exercised the harness's own
interruption, focus-history, and ownership guards; they do not establish how
`sway-stage load` itself handles interference, and no single run proves the absence of
races. No live execution by an agent is permitted, including help, partial runs, or trial
runs.

### Implementation decisions

- Scenarios use harness-owned layout types, independent of `LayoutNode` and the
  production parser. Expected YAML has a fixed format, including JSON flow commands and
  environments, and capture is compared line by line.
- Failures are `Failure::Check` (an expectation failed while ownership and control remain
  established) or `Failure::Uncertain` (ownership, placement, identity, or control is
  uncertain). `Failure::report` distrusts focus history for uncertain failures, after
  which every fence is refused, so neither cleanup nor restoration can follow. In layout
  verification, unusable geometry and sizing beyond 5 points are ordinary failures;
  hierarchy, identity, order, focus, history, and ownership failures are uncertain.
- Foot, `sleep`, Firefox, and Xterm keep their unresolved `PATH` names so wrapper or
  multi-call links such as BusyBox still work; `sway-stage` and `swaymsg` are
  canonicalized. A Firefox root is
  `sway-stage-firefox-RUN_ID-SCENARIO` in the temporary directory, with a random run ID,
  created exclusively with mode 0700.
- `lifecycle.rs` keeps preparation, observation, load/capture verification, cleanup, and
  restoration in synchronous execution order. The initial preflight precedes six standard
  configuration checks; a second preflight immediately precedes the designated switch.
  With `--extended`, Firefox and Xterm are found on `PATH` once at the start of the run;
  each application is prepared only just before its scenario. Standard
  configurations are checked again per scenario, with an additional fresh empty-workspace
  fence and global ID snapshot immediately before every load. YAML directories are 0700
  and files 0600, automatically removed separately from retained Firefox roots.
- The only command interfaces switch to the validated positive decimal target, restore
  the validated original name, or issue `[con_id=EXACT_ID] kill`. Each requires exactly one
  explicit successful reply. Cleanup refreshes event, ownership, signature, and complete
  hierarchy evidence before each close; bounded observations then verify disappearance,
  ownership, and the predicted remaining hierarchy before advancing the baseline. Close
  order is launch order even though accepted IDs are stored in a sorted map.
- No failure path enters window cleanup. Uncertain verification failures pass through
  `Failure::report` before any restoration decision. Ordinary failures may restore only
  with fresh quiet observations, trusted disposable focus, and the exact original ID/name
  still present. Restoration also verifies the destination ID after switching. An
  unestablished disposable identity never permits restoration.
- `signal-hook` with default features disabled installs only an atomic SIGINT flag action;
  no signal handler performs IPC or cleanup. Subprocess and event reads poll interruption,
  and every desktop mutation checks it. Interruption invalidates history and skips
  restoration. Every helper and directly launched CLI child uses
  `CommandExt::process_group(0)` to isolate terminal Ctrl-C. Timeout, interruption, and
  capture failures kill and reap only the direct child by PID, never its descendants or
  process group. The subscription helper is stopped on every exit, including readiness
  failure. Stopping uses direct SIGKILL with a two-second reap bound.
- Failure diagnostics retain the last associated IDs and query the tree read-only, even
  after interruption; candidate IDs, selectors, PIDs, workspaces, and unsafe state do not
  imply ownership. Inferred arguments stay in memory. Layout failure context omits launch
  arguments and environment values, including Firefox details. Counts and retained browser
  roots are reported on success and failure. Typed JSON decoding, bounded child output,
  and ignoring relative `PATH` entries deliberately fail closed on unusual input.
- `integration_harness` is a separate crate root over the harness modules without
  `main.rs`. Its tests use constructed Sway trees and events, temporary files, and the
  test process's own `/proc/PID/cmdline` for process evidence. CLI parsing, preflight,
  exact command construction/replies, cleanup transitions/refusals, restoration eligibility,
  permanent history invalidation, interruption decisions, and private temporary YAML are
  covered without reading `SWAYSOCK`, connecting to Sway, or starting processes.

## 3C. Combined Fuzzel menu — complete

`menu [--infer-commands]` uses an explicit label/action mapping: lexical `Load: NAME`
entries followed by `Capture current layout…`. It discovers names without parsing YAML
and offers Capture even when there are no layouts. Fuzzel is synchronous, with literal
arguments and no timeout for user input. A scoped writer feeds options while stdout is
read, then both process and writer are joined. Output must be exactly one UTF-8 line;
arbitrary typed selections fail closed. Fuzzel must use its default dmenu text output;
index output is rejected with a useful error.

Fuzzel 1.11 introduced both `--prompt-only=PROMPT` and dmenu cancellation status 2
(previously 1), per the upstream manual and changelog shipped with Fuzzel and
[upstream sources](https://codeberg.org/dnkl/fuzzel). Require 1.11 or newer: only status 2
with empty stdout cancels; status 1, other failures, and malformed output remain errors.
Fuzzel is optional for all other commands.

After a load selection and Fuzzel exit, `main.rs` starts `Diagnostics` under `menu` and
calls the unchanged `load_named` implementation, including rediscovery and safe target
observation, selecting the workspace focused at load time independently of the pre-menu
tree. Before the first prompt, `main.rs` retains `capture::snapshot()` as a `Result`;
`Snapshot::output_name` reads the original workspace's containing output from that tree.
Both prompts receive the same literal `--output=OUTPUT` when known. Acquisition errors
are deferred until Capture; Load and cancellation ignore them. Empty and unsupported
layouts are validated only if Capture renders the saved tree after the action prompt exits.
No later tree query or fallback capture occurs. Inference runs only on Capture and uses
saved PIDs, allowing stale-process null fallback. Cancellation discards any draft in
memory. Selection/capture/save failures notify under `menu` without launch logs.

Drafts are saved into `discovery::Directory::resolve`'s path, the same directory the
action menu discovered, so `--layouts-dir` controls loading and saving alike. A missing
explicit directory fails discovery before either prompt; a missing default directory is
created only on save. A separate drafts directory was dropped to remove the manual move
step: the strict validation in `check`/`load` already rejects placeholders before any IPC,
so listing an unfinished draft is harmless. Old `drafts/` directories are neither migrated
nor touched. Directory creation uses 0700 without changing existing permissions. Saving creates a 0600 `.partial` staging file
exclusively, writes and syncs the unchanged draft, then publishes with `hard_link` so an
existing `.yaml` destination cannot be overwritten. Both `.yaml` and `.yml` collisions
are checked; names are ASCII layout names limited to 250 bytes. Write/publication failure
retains the staging file and reports its path; no partial YAML is published and no failure
rollback removes uncertain files. Successful publication removes its staging link and
attempts a best-effort path notification. The filesystem must support hard links within
the layouts directory. External concurrent changes across two suffixes are not atomic.
The `.partial` staging name is outside discovery's extensions.

Offline tests use fake Fuzzel/notify-send and single-tree capture IPC fixtures. They verify
the tree response precedes the first Fuzzel launch, both prompts use the original output,
Capture renders saved state, and Load independently observes its target after Fuzzel exit.
They also cover deferred snapshot errors, empty/unsupported initial layouts, exact
selections, cancellation, subprocess/protocol errors, opt-in inference, default/HOME and
explicit (relative) save destinations, permissions, collisions, exclusive publication,
write and directory-creation failures, discovery of saved drafts, placeholder rejection
by `check`, `load`, and menu loading, and launch-log isolation.
The ten-scenario integration harness is unchanged. Live menu acceptance is user-only;
see the manual procedure below.

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
| `menu.rs` | Fuzzel actions/name prompts and private, exclusive saving into the layouts directory. |
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

**Capture.** `capture::snapshot` queries exactly one complete tree through `SWAYSOCK`,
using a disposable query thread and monotonic deadline with socket shutdown to cancel
stalled or fragmented reads. It does not use or change the loader's `Observer`, subscribe,
or send ticks. The owned `capture::Snapshot` retains the entire tree, including output
ancestry and PIDs, and identifies the original workspace by the uniquely focused node's
workspace ancestry and container ID. An empty workspace may itself be focused. Invalid
root, duplicate IDs, absent focus, or ambiguous focus fails acquisition; representability
and occupancy are checked only by `Snapshot::render`. Later desktop activity cannot
redirect or invalidate rendering. Menu retains this snapshot before its first prompt
and renders it only after Capture is selected.

One recursive pass validates and converts the saved target subtree directly, since it
needs the structure that `locate_windows` flattens:
windows are `con` nodes with `shell`, `app_id`, or `window`; a one-child container,
including the workspace, is replaced by its child after its layout is checked; extents
come from `sibling_dimensions`. Weights are `proportions` as percentages rounded to
hundredths with a 0.01 floor, which shifts reconstructed shares by under 3 percentage
points even across 255 siblings. Capture-specific Serde types serialize through
serde-saphyr with single-line scalars and indented sequences, so ambiguous scalars are
quoted and control characters escaped; a fixed comment header is prepended. The strict
`config` types are not reused, and `config` exports its depth and node limits so filled
drafts stay loadable. Optional inference extends the same draft window's command field;
its reader boundary takes a PID, opens only `/proc/PID/cmdline`, and silently rejects
unavailable or invalid data. Reads are bounded to 64 KiB plus a detection byte and preserve
UTF-8 argv boundaries, including empty arguments. Reads use positive leaf PIDs recorded
in the saved tree, with no second PID observation. Process exit, PID reuse, or argv
changes after the snapshot remain possible and cannot alter the captured structure.
No inferred arguments reach error diagnostics; only an explicit menu save or stdout
redirection persists the generated draft. Capture failures report through
`diagnostics::notify_failure`, never `Diagnostics::start`, so `last-run.log` is untouched.

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

Reviewed against upstream documentation on 2026-11-09; harness signal handling on 2026-11-10:

| Need | Decision |
| --- | --- |
| CLI | [Clap 4.6.7](https://docs.rs/clap/4.6.7/clap/) with derive, std, help, usage, and error-context; default features disabled. |
| Typed YAML | [Serde](https://serde.rs/) with derive and [serde-saphyr 2.1.1](https://docs.rs/serde-saphyr/2.1.1/serde_saphyr/) with `deserialize` and `serialize` (for capture drafts); it declares Rust 1.89. |
| Field diagnostics | [serde_path_to_error 0.1.20](https://docs.rs/serde_path_to_error/0.1.20/serde_path_to_error/) for nested field paths. |
| Native synchronous IPC | [swayipc 4.0.0](https://docs.rs/swayipc/4.0.0/swayipc/struct.Connection.html) and swayipc-types 2.0.3 (Serde, serde_json, thiserror 2); no async runtime. |
| Processes | `std::process::Command` for applications, Fuzzel, and `notify-send`; no shell wrapper, Fuzzel crate, or supervision system. |
| Errors and logging | Standard-library I/O and contextual `String` errors; add a logging or error crate only for a concrete need. |
| Tests | `serde_json` as a development dependency, already locked through `swayipc`. |
| Harness SIGINT | Development-only [signal-hook 0.4.6](https://docs.rs/signal-hook/0.4.6/signal_hook/), default features disabled; the flag API avoids handwritten unsafe signal handling. |

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

The harness signal dependency adds `signal-hook-registry` 1.4.8, `libc` 0.2.191, and
`errno` 0.3.14 on Linux; the lockfile also records errno's Windows-only `windows-sys`
0.61.2 and `windows-link` 0.2.1. All declare MIT OR Apache-2.0 licensing.
Default signal iterator/channel features and optional C compilation are disabled.
Cached crate manifests and the flag implementation were
reviewed; the RustSec advisory index contained no matching advisory for these added
crates. The locked graph is checked on Rust 1.89.0. These are development dependencies
and do not enter the production binary's dependency graph.

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
- Combined-menu desktop behavior and the loader's live abort behavior await manual
  verification.
- Capture fixes Sway state at one tree response and ignores subsequent desktop activity.
  Process arguments may already have changed by inference time; there is no second PID
  consistency check. Inference is opt-in and best-effort. Capture refuses structural
  container focus and never approximates tabbed, stacked, floating, or fullscreen state.

## Manual verification

For the user only, in an isolated, disposable Sway session with `SWAYSOCK` pointing to it.
Agents must never run these commands; see [AGENTS.md](../AGENTS.md).

### Combined menu (user only)

With Fuzzel 1.11 or newer and the built binary on the disposable session's PATH, use a
Sway keybinding such as `bindsym $mod+Shift+m exec sway-stage menu`. Alternatively, from
a terminal in the repository root, allow five seconds to focus the intended workspace:

```sh
sleep 5; ./target/release/sway-stage --layouts-dir examples menu
sleep 5; ./target/release/sway-stage --layouts-dir examples menu --infer-commands
```

Run these separately. For loading, focus a completely empty workspace and select
`Load: two-foot`; verify the normal sizes/final focus and a new launch log. Repeat with
an occupied workspace to confirm refusal. For capture, focus a supported tiled arrangement,
note the original focused window and proportions, invoke the menu, and move the pointer
between windows while the action menu is open before selecting Capture. Move it again
while entering a fresh name such as `captured-development`. Verify a path
notification and the complete draft in the `--layouts-dir` directory (`examples` for the
commands above; use a fresh name and delete the test file afterwards). Without
`--layouts-dir`, the draft goes to `$XDG_CONFIG_HOME/sway-stage/layouts` (or
`$HOME/.config/sway-stage/layouts`). The normal command must leave null commands; the
inference command may recover process argv, which needs review. Neither capture changes
the arrangement or launch log. The saved name appears in `list` and the next menu.

On two monitors, invoke the menu from a supported layout on output A. While each prompt
is open, move the pointer to output B and change workspace focus, then return to complete
the prompt. Both prompts must remain on A; the draft must preserve A's original hierarchy,
weights, selectors, and focused window. Repeat with `--infer-commands`; arguments remain
best-effort if a process exits during the wait. On one monitor, change window focus while
the action prompt is open and change workspace while the name prompt is open, then verify
the same original snapshot. Compare `sha256sum` of `last-run.log` before and after capture.

For loading, invoke from an empty workspace; after the action menu opens, manually switch
to a different empty workspace before choosing `Load: two-foot`. Verify construction uses
the workspace focused after Fuzzel exits, with the normal safety checks and a new log.
Snapshot errors must not prevent cancellation or reaching ordinary load validation.
Output-removal behavior remains a user-only check: any Fuzzel failure should be reported
without focus manipulation or automatic repair. No claim of live acceptance is made for
these corrected interactions until the user performs them.

Press Escape in the action menu, then separately choose Capture and press Escape at the
name prompt. Both should exit successfully without files, errors, or a replaced launch
log. Try an invalid name and an existing layout name: expect a menu error and preserved
files. `check NAME` must reject an unedited null-command draft; selecting it from the menu
must fail validation without launching anything. Edit it in place, then `check` it.
Agents must never run these desktop checks.

### Integration harness (user only)

Prerequisites: `swaymsg`, Foot, and `sleep` on `PATH`; built `target/release/sway-stage`
and `target/release/examples/integration`; and a valid `SWAYSOCK` for the isolated
disposable Sway session. Only one Sway seat is supported. Start on an existing workspace
other than the explicit target, keep input idle until the harness exits, and choose a
disposable positive numeric workspace that is absent or completely empty (including
floating windows and empty containers). An existing workspace with that number but a
different name is refused. The starting name must contain only ASCII letters/digits,
spaces, or `_ . : + -`, beginning with a letter/digit, so restoration needs no command
escaping. Agents must never execute the harness, even with `--help`; only compilation,
static review, and the offline `integration_harness` tests are allowed.

From the repository root, build both executables, then run the six standard scenarios:

```sh
cargo build --release --locked
cargo build --release --locked --example integration
./target/release/examples/integration --workspace 9
```

Use `--binary /absolute/path/to/sway-stage` to select another build. The harness preflights
and switches to the requested empty workspace, runs all six scenarios with guarded cleanup
between them, and restores the original workspace when safe. It never chooses an alternate
target. A successful run reports six passes, each with verified capture and cleanup. A
failure stops the run and leaves remaining windows alone; their bounded commands normally
expire within five minutes of launch. Read the reported container IDs, app IDs,
classes, and PIDs before any manual intervention. Workspace/window rules, minimum sizes,
gaps, activity, or version differences may cause a conservative failure.

The optional extended run, after those six scenarios, is:

```sh
./target/release/examples/integration --workspace 9 --extended
```

A successful extended run reports ten passes when all optional prerequisites below are
available, or explicit SKIPs for unavailable applications/subsystems.

Additional prerequisites: native `firefox-esr` (preferred) or `firefox` on `PATH`, a valid
`WAYLAND_DISPLAY`, and readable `/proc/PID/cmdline` for browser and repeated-ID Foot
ownership checks. Firefox must support the configured options and keep its literal launch
arguments visible; packaging wrappers or sandboxed builds that obscure this evidence fail
conservatively. If adjacent application metadata is unavailable or the distribution uses
another app ID, add `--firefox-app-id EXACT_ID` after verifying it for that installation.
No normal Firefox profile is used. Retained temporary browser directories need manual
removal after all associated processes exit, including after successful runs.

Xterm coverage additionally requires `xterm`, `Xwayland`, and `xdpyinfo` on `PATH`, with
`DISPLAY` pointing to the disposable session's reachable Xwayland server advertising the
`XWAYLAND` extension. The bounded read-only `xdpyinfo -queryExtensions` probe skips an
unavailable server (or missing probe); a later Xterm launch or class mismatch fails.
Keep the disposable session idle throughout.

To check the harness's own safety mechanisms after changing them, in additional, separate
disposable-session runs, interrupt during a load or capture and during cleanup, using
Ctrl-C in the invoking terminal or SIGINT directed only to the harness PID from another
terminal. Expect a failure, remaining-window diagnostics, retained-profile paths, no
further closes or scenarios, and no restoration after the interrupt is observed. If a
close was already dispatched, it may still complete. Check that the harness's direct child
is reaped and that interruption did not signal application processes. Separately switch
away and back, introduce a foreign window, or change an owned window's state: expect
conservative failure without further cleanup or automatic focus restoration. These checks
exercise the harness's guards, not the loader's interference handling, and do not imply
transactional protection against races. Agents must never execute any of these harness
commands.

### Individual examples

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
