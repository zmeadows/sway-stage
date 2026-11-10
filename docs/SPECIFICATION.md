# sway-stage specification

This is the authoritative behavioral contract. It specifies `pick` before it is
implemented; current status, design decisions, and the manual verification procedure
are in the [roadmap](PLAN.md). The YAML format may be revised before its first release,
with this document and the examples updated together.

## Purpose and scope

`sway-stage` is a small standalone Linux executable that reads a named YAML layout and
constructs it on the currently focused, empty Sway workspace. It uses synchronous native
Sway IPC, launches applications sequentially, and exits after construction. Precompiled
Linux releases are a delivery goal; the supported build targets will be documented when
release validation is available.

Layouts describe ordered, nested horizontal and vertical tiled splits with relative
sibling weights. They specify application commands, arguments, optional working
directories and environment overrides, and explicit window identification.

Non-goals: session restoration, existing-window reuse, persistent daemons, automatic
layout enforcement, cross-compositor support, graphical configuration editors, workspace
or monitor assignment, and management of application lifetimes after launch.

## Commands and discovery

The command surface (`pick` is not yet implemented):

| Command | Behavior |
| --- | --- |
| `load NAME` | Validate the named layout, then construct it on the focused empty workspace. |
| `check NAME` | Validate a layout's syntax and semantics without IPC or launching anything. |
| `capture [--infer-commands]` | Read the focused workspace and write an editable YAML draft to stdout. No discovery. |
| `pick` | Offer named layouts through `fuzzel --dmenu`, then perform `load` for the selection. |
| `list` | Print available layout names, one per line in lexical order. No IPC. |

`--layouts-dir PATH` overrides discovery for `load`, `check`, `list`, and `pick`. Otherwise
use `$XDG_CONFIG_HOME/sway-stage/layouts`, or `$HOME/.config/sway-stage/layouts` if
`XDG_CONFIG_HOME` is unset or empty. A nonempty XDG path must be absolute; if neither
base is usable, report an error. An explicit relative directory is relative to the CLI's
working directory. Discovery reads only immediate regular `*.yaml` and `*.yml` files;
the stem is the case-sensitive name. Names must match `[A-Za-z0-9][A-Za-z0-9_-]*`.
Reject invalid names and duplicate stems rather than choosing an arbitrary file.
Never create a discovery directory on read. A missing default directory produces an
empty list; loading a missing name and a missing explicit directory are errors.

`list` does not parse layout contents. `check` performs all static validation required by
`load`, including cwd existence, but does not run commands, probe applications, contact
Sway, create logs, or send notifications. It cannot promise that a command will create
a matching window or that an application is installed. Help and version output also
require no configuration, compositor, or optional tools. No subcommand shows help and
performs no action.

Fuzzel is an optional external executable needed only by `pick`; no shell is involved.
Offer names as lines and accept only an exact member of that set. No layouts or picker
cancellation is a successful no-op. An unavailable picker, execution failure other than
cancellation, or invalid selection is an error. Resolve the target workspace only after
selection; picker window focus must not select a target early. `pick` shares `load`'s
validation and safety behavior.

Successful operations and picker cancellation exit 0; validation or operational failure
exits 1; invalid CLI usage exits 2. Data goes to stdout and errors, warnings, and diagnostics
to stderr. A failed capture emits no partial YAML. Captures never overwrite a layout file;
users choose where to save stdout.

## YAML format, version 1

See [`examples/four-windows.yaml`](../examples/four-windows.yaml) for a complete Firefox
and three-terminal layout. Each file contains one UTF-8 YAML document with these
top-level keys:

| Key | Contract |
| --- | --- |
| `version` | Required integer `1`; reject other versions. |
| `layout` | Required root node: a split or a window. |
| `focus` | Optional window `id` to focus at completion; default is the first leaf. |
| `window_timeout_ms` | Optional integer from `1` to `300000` (five minutes), default `20000`, for each window wait. The user must keep the target focused while waiting, so longer waits are rejected. |

A split node has `split: horizontal` (left to right, Sway `splith`) or `split: vertical`
(top to bottom, `splitv`), and an ordered `children` sequence with at least two nodes.
Children may be splits or windows, allowing arbitrary nesting within validation limits.
A window node has the following fields:

| Key | Contract |
| --- | --- |
| `id` | Required unique, nonempty identifier matching the layout-name grammar. |
| `command` | Required nonempty string sequence: nonempty executable first, then literal arguments. |
| `match` | Required mapping with exactly one nonempty string: `app_id` or `class`. |
| `cwd` | Optional nonempty directory path; relative paths resolve against the YAML file. |
| `env` | Optional string-to-string map of child environment overrides; default empty. |

Any node may have `weight`, a finite positive number defaulting to `1`. Its share is
`weight / sum(sibling weights)` along the parent's split axis. Weights need not sum to
100. The root has no siblings and its weight has no effect. Every sibling sum must be
finite. Tree order is visual order and depth-first launch order. Do not flatten nested
splits merely because their orientation matches their parent.

Window matching uses exact, case-sensitive Wayland `app_id` or Xwayland
`window_properties.class` equality, never a title, substring, regex, or PID alone. Repeated
selectors are allowed for separate sequential launches; IDs must still be unique.
Users should supply distinct application IDs in both arguments and selectors when the
application supports them. The launcher never invents IDs or modifies commands.

Commands are launched with preserved argument boundaries, without implicit shell parsing,
variable expansion, tilde expansion, or globbing. A bare executable uses the child's
effective `PATH`. Executable paths containing `/` resolve relative to the effective child
cwd; the default cwd is the launcher's working directory. `env` overrides the inherited
environment only in that child. Keys must be nonempty and contain neither `=` nor NUL;
arguments, paths, and environment values cannot contain NUL. An empty argument or
environment value is allowed. Users may explicitly invoke a shell in `command`; layout
files are executable instructions and should be trusted accordingly.

Validation rejects unknown or duplicate keys, mixed split/window fields, wrong types,
missing required values, empty children/commands/selectors, invalid weights, duplicate
IDs, and a `focus` that does not identify a leaf. Do not silently coerce or repair input.
Reject multiple documents, custom tags, merge keys, and aliases. Bound input size (1 MiB),
tree depth (32 nodes along a path), and total layout nodes (256) before side effects;
parser resource limits must also prevent pathological input before semantic validation.
Report the filename, node/field path, and line/column where available.

Default capture drafts use `command: null` as an explicit editing placeholder.
With `--infer-commands`, successfully recovered process arguments replace that placeholder.
If an observed window has no usable identifier, emit `match: null` too. These are permitted in capture
output only: `check` and `load` reject them with instructions to fill them in. A draft
otherwise follows the same format. There is no executable fallback.

## Loading and safety

`load` supports a root window and ordered, arbitrarily nested splits within the validation
limits. See [two Foot windows](../examples/two-foot.yaml) and the
[four-window reference](../examples/four-windows.yaml). One observation session retains
the original workspace and every accepted window throughout the run.

Construction completes the first child of a split, then establishes its enclosing split
and launches subsequent children as explicitly focused siblings. If that first completed
group fills the workspace, it is wrapped using the populated workspace's split operation;
the workspace parent is selected explicitly through an owned direct child and verified
before splitting. This does not change workspaces. Equal-axis meaningful groups remain
separate. No empty placeholder hierarchy or window move commands are used.

Every command is guarded by a fresh fenced tree and ownership checks. A launch or command
may proceed only from a settled fence: a verified tree whose event history contains no
window events. Harmless window events, such as focus or title changes, require another
fence rather than aborting. Every command reply must succeed, and subsequent trees must
match the expected leaf order, grouping, orientation, and retained identities. New wrapper
identities are obtained from Sway. Only the exact structural transition requested by the
command is allowed. The reviewed split path reparents without emitting window move events;
every move event remains fatal because its payload omits workspace ancestry. New-window close events and uncertain structural
activity between launches or during command verification also abort conservatively.

Construction has no transaction or rollback. Safety takes precedence over completing a
layout. The target is the focused workspace observed after validation, identified by its
Sway workspace identity rather than a configured number or monitor. Keep that target
fixed; never create, select, rename, or assign a workspace on the user's behalf.

1. Validate the whole layout before launching or mutating Sway. Connect and subscribe to
   workspace/window events before the initial target snapshot. Establish an empty target:
   both tiled and floating child lists must be empty, including otherwise empty containers.
   Snapshot existing window container IDs across the whole tree, including other workspaces.
2. Observe workspace focus throughout construction, including while waiting and resizing.
   A focus event away from the target aborts even if the user switches back before the
   next snapshot. Recheck focus, target existence, and ownership before every launch or
   mutating IPC command. Lost event observation or IPC connectivity is fatal.
3. Launch leaves in depth-first order, establishing split orientations as groups are built.
   Snapshot global window IDs immediately before each launch. Launch exactly once and wait for
   exactly one new, unclaimed window with the leaf's explicit selector; never match an
   ID in any pre-launch snapshot or already accepted for another leaf. Observe events and
   fresh trees with a monotonic deadline. Match properties may arrive after window creation;
   do not mutate the layout while a new target window's identity is unresolved. Resolve
   it within the current deadline or fail rather than guessing its ownership.
4. Inspect candidates globally. A matching new window on the wrong workspace (or in the
   scratchpad), multiple new matches, a floating/fullscreen match, an unrelated new window entering
   the target, or a preexisting window entering it is fatal. Do not move, tile, close,
   resize, or adopt such a window. Unrelated windows elsewhere remain untouched.
5. Accept the expected window by container ID only after confirming its target and tiled
   state. Build the split hierarchy using the accepted windows and their containers,
   verifying observed structure as construction proceeds. Any accepted window closing,
   leaving the target, becoming floating/fullscreen, or unexpected hierarchy changes, aborts.
   Do not relaunch or repair around interference.
6. Verify final leaf order and split hierarchy. Apply weights from outer
   splits inward. Bound adjustment to at most three passes per split; read the actual tree
   after sizing. Each IPC command must have replies and every reply must succeed.
7. Recheck all safety conditions and focus `focus`, or the first depth-first
   window by default, using its accepted container ID. Then exit; there is no ongoing enforcement.

Direct mutations must target only accepted windows and containers whose window descendants
are all accepted by this run. Before any ancestor resize or split change, verify it cannot
affect foreign descendants. Never use broad selector criteria for mutations. Never close,
move, resize, or reuse preexisting windows anywhere, and never manipulate the global Sway
scratchpad. Do not use scratchpad staging, placeholders that swallow existing windows,
or workspace moves to construct the hierarchy.

On error or cancellation, stop further launches and layout mutations; leave the partial
layout and processes in place. Do not kill applications, undo splits, restore focus by
switching workspaces, or retry an application. A process may delegate to an existing
instance; process exit alone does not establish window creation. Wait for the new window
or deadline, and report a spawn error immediately if the process could not be started.
An existing matching window is never an alternative to a missing new window.

An app ID/class identifies a window type, not causality. A concurrent unrelated launch
with the same selector can be indistinguishable; unique IDs reduce this ambiguity.
Sway commands and event/tree reads are not atomic together. Minimize race windows with
fresh guards and stop on observed interference; do not promise transaction-like isolation.

## Sizing and focus

Compute desired proportions independently for each sibling set using its weights. Compare
observed sibling dimensions on that axis, normalized by their sum, with desired shares.
Borders, gaps, rounding, and application minimum sizes can prevent exact proportions.
Adjustment aims for an absolute difference of at most **1 percentage point** per child,
and an absolute difference of at most **5 percentage points** is accepted. A larger
difference after bounded adjustment produces a warning identifying the split, requested share, and
observed share; it does not turn an otherwise correct hierarchy into failure. Structural
or ownership errors are always fatal. Do not disable user gaps, borders, or application
constraints to force a result.

The [four-window reference](../examples/four-windows.yaml) launches Firefox, middle Foot,
upper-right Foot, then lower-right Foot, constructs a horizontal root with a vertical right
column, applies root weights **44:32:24** and right-column weights **77:23**, and focuses
Firefox last. Sizing verifies the resulting proportions; it does not replay a fixed sequence
of percentage commands.

## Capture

`capture` accepts only the optional `--infer-commands` flag and uses read-only IPC:
workspace and tree queries and tick fences only. It never sends commands, changes focus, launches applications, or writes
files. On success, stdout holds exactly one draft document.

The draft follows the version 1 format. It preserves the focused workspace's ordered tiled
hierarchy and orientations. Each split child's `weight` is its percentage of its siblings'
summed extent along the split axis, rounded to hundredths with a minimum of `0.01`, which
reconstructs proportions well within the sizing tolerance; the root has no weight. Leaf
IDs are `window1`, `window2`, ... in depth-first order, and `focus` names the focused
window. `match` uses the window's nonempty Wayland app ID, otherwise its nonempty Xwayland
class, otherwise `null`. Without the flag, `command` is always `null`. A leading comment
explains the placeholders. Do not serialize Sway container IDs, workspace names/numbers,
outputs, titles, PIDs, cwd, or environment from the running session. The result records
arrangement and selectors, with optional process arguments, rather than session restoration.

With `--infer-commands`, attempt each leaf's Linux `/proc/PID/cmdline` only when its
container ID and positive PID agree across both observations. Read at most 64 KiB plus
one oversize detection byte. Require a nonempty executable, a final NUL terminator, and
valid UTF-8 arguments; preserve argument boundaries and empty arguments. Unavailable,
inconsistent, inaccessible, empty, malformed, or oversized process information leaves
`command: null` for that window without failing capture. No process-name searches,
parent traversal, command rewriting, environment capture, or application-specific recovery
is performed. Never execute or probe inferred commands.

Recovered arrays represent current process arguments, which may differ from the original
launch command and may not reproduce the application. They may contain passwords or
tokens. An additional brief YAML comment asks users to review commands before executing
or sharing. Arguments appear only in the opted-in stdout draft, never diagnostics,
notifications, logs, caches, or automatically written configuration files. Process exit,
PID reuse, and argument changes can race with observation; inference is best-effort and
cannot promise transactional process identity. Null command and match placeholders still
require editing before `check` or `load` accepts the draft.

A single-window workspace is a root leaf. A container with one tiled child, including the
workspace itself, is replaced by that child, which takes the container's extent among its
siblings. Every container with two or more children remains a split, even when it shares
its parent's orientation.

Refuse an empty workspace; any floating container; a tabbed or stacked container, even
with a single child; a fullscreen window or container (the workspace's own
`fullscreen_mode` is ignored); a scratchpad window or container, even when shown;
unsupported node types and malformed containers;
nonpositive extents along a split axis; focus on anything but a window, such as a group
after `focus parent`; and an arrangement exceeding the depth or node limits.

Capture observes twice through fenced snapshots and emits a draft only if both yield the
same arrangement: hierarchy, order, container identities, extents, selectors, and focus.
Any workspace focus change since subscribing, even away and back, and any window event
other than a title, urgency, or mark change, fails the capture. A failed capture writes
no YAML.

## Errors, notifications, and diagnostics

Errors identify the operation/stage, layout and leaf where relevant, target workspace,
underlying failure, and the next useful action. A timeout includes observed new container
IDs, app IDs/classes, workspace locations, and floating states; it must distinguish no
new window from new windows that failed to match. Log stages, elapsed waits, accepted
container IDs, IPC commands/replies, hierarchy checks, and requested/observed sizing.

`load`, `pick`, and `capture` attempt a desktop notification for fatal errors, titled for
the invoked operation (`sway-stage load failed`, `sway-stage pick failed`, or
`sway-stage capture failed`), using `notify-send` with a bounded wait. Missing notification
tools or delivery failures are secondary diagnostics; stderr remains sufficient and the
original failure remains primary. Advise that a partial layout or applications may remain,
and to inspect windows before retrying, only after this invocation has successfully
spawned an application; a spawn that fails to start does not count. Send a warning
notification for substantial sizing deviations so keybinding launches can surface them.
`list`, `check`, help, and version are side-effect-free apart from stdout/stderr.

`last-run.log` is reserved for layout launches. Each `load`, and each `pick` once a layout
has been selected, replaces it. Picker cancellation or failure before a selection and
every `capture` leave the previous log untouched; their errors go only to stderr and a
best-effort notification. Write the log to
`$XDG_STATE_HOME/sway-stage/last-run.log`, falling back to
`$HOME/.local/state/sway-stage/last-run.log`. Require absolute state bases. Keep only this
last-run log, create it with private permissions, and include its path in failure output
when available. Logging failure falls back to stderr without hiding the operation result.
Use timestamps and stages; omit titles, environment values, and complete argv by default.
No telemetry or background logging service is involved.
