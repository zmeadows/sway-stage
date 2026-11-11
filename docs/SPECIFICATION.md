# User reference and specification

This is the authoritative contract for implemented behavior. Start with the
[README](../README.md) for installation; contributors should also read
[development](DEVELOPMENT.md) and [AGENTS.md](../AGENTS.md).

## Commands and layout discovery

| Command | Behavior |
| --- | --- |
| `load NAME` | Validate and launch a layout on the focused, empty workspace. Success is silent. |
| `check NAME` | Validate syntax, semantics, and cwd existence; print `NAME: valid`. |
| `list` | Print discovered names in lexical order without parsing contents. |
| `capture [--infer-commands]` | Print an editable draft of the focused workspace. |
| `menu [--infer-commands]` | Load a layout or save a named capture through Fuzzel. |

`check` and `list` never contact Sway, launch applications, write logs, or notify.
`check` does not probe executables or promise that a command creates a matching window.
Help, version, and invocation without a subcommand need no configuration or optional tools;
no subcommand prints help and does nothing else.

Global `--layouts-dir PATH` controls discovery and menu saves; standalone capture ignores
it. Otherwise use `$XDG_CONFIG_HOME/sway-stage/layouts`, falling back to
`$HOME/.config/sway-stage/layouts` when XDG_CONFIG_HOME is unset or empty. Nonempty default
bases must be absolute. Explicit relative paths resolve from the CLI's working directory.

Discovery reads immediate regular `.yaml` and `.yml` files only, ignoring symlinks and
subdirectories. The case-sensitive filename stem is the name and must match
`[A-Za-z0-9][A-Za-z0-9_-]*`. Invalid names or duplicate stems are errors. Reads never create
directories. A missing default directory lists as empty; a missing explicit directory or
requested layout is an error.

Success and menu cancellation exit 0, operational or validation errors exit 1, and invalid
CLI usage exits 2. Data goes to stdout; errors, warnings, and diagnostics go to stderr.
Capture prepares a complete draft before writing; acquisition or rendering failure emits
no YAML. An output-stream failure can still interrupt delivery.

## YAML format, version 1

See [`examples/four-windows.yaml`](../examples/four-windows.yaml) for a complete Firefox
and three-terminal layout. Each file contains one UTF-8 YAML document with these
top-level keys:

| Key | Contract |
| --- | --- |
| `version` | Required integer `1`; reject other versions. |
| `layout` | Required root node: a split or a window. |
| `focus` | Optional window `id` to focus at completion; default is the first leaf. |
| `window_timeout_ms` | Optional integer from `1` to `300000` (five minutes), default `20000`, for each window wait. |

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
parser resource limits also bound pathological input.
Errors include the filename, node/field path, and line/column where available.
String fields require quotes around numeric/boolean scalars; numeric fields reject quoted
numbers. Explicit nulls never trigger optional defaults.

## Loading, sizing, and safety

Layouts describe new applications in ordered horizontal and vertical tiles. There is no
session restoration, existing-window reuse, daemon, ongoing enforcement, workspace/output
assignment, or management of application lifetimes after launch. Tabbed, stacked, floating,
and fullscreen arrangements are outside the layout format.

The following safeguards apply to standalone and menu loading:

1. Validate the complete file before launching applications or mutating Sway. Standalone
   Load validates before contacting Sway; menu takes its read-only snapshot first. Select the
   currently focused workspace by container identity, and retain its containing output
   identity. Both tiled and floating child lists must be empty, including empty containers.
   Never create, select, rename, or assign a workspace for the user.
2. Observe focus throughout construction, sizing, and final focus. Switching away, even
   briefly and back, destroys the safety precondition. Moving the target to another output,
   including away and back, also stops construction. Missing targets, changed output
   identity, lost observations, and IPC failures are fatal. An unrelated workspace move or
   output geometry change alone is allowed if the target stays on its original output.
3. Launch once per leaf in depth-first visual order, waiting for each window before
   continuing. Only a newly observed, unclaimed window absent from all pre-launch snapshots
   can match. Matching is global: a matching new window elsewhere, including the scratchpad,
   multiple matches, a floating/fullscreen match, or an unexpected window entering the
   target is fatal. Delayed identifiers may resolve within the window timeout; uncertain
   ownership is never guessed. An existing matching window cannot substitute for a new one.
4. Preserve grouping, orientation, and order, including meaningful same-axis nested groups.
   Recheck workspace, output, focus, ownership, and hierarchy before every launch or mutation;
   verify command replies and resulting structure. Accepted windows must retain their
   identity, selector, tiled state, and location. Closing one or changing its state stops
   construction rather than triggering repair or relaunch.
5. Mutate only accepted windows and containers whose window descendants all belong to the
   run. Never close, move, resize, or reuse preexisting windows, use broad app-ID/class
   criteria for mutations, or manipulate the global scratchpad. Construction uses neither
   window moves nor placeholder windows.
6. Apply weights from outer splits inward, then focus the configured leaf (the first
   depth-first leaf by default), verify it, and exit.

Any window move event, even elsewhere, stops the run because its workspace ancestry is
unknown. Other uncertain structural activity between launches or during command verification
also stops it. Harmless title/focus activity can settle through another observation; a close
of a window proven foreign by the initial baseline may also settle if no contradictory
history exists. Continuous activity can time out a run.

On failure, stop further launches and layout mutations. Leave applications and partial
layouts in place: no cleanup, killing, rollback, focus restoration, retries, or automatic
relaunch. Process exit does not prove that no window will appear; delegating applications
are still allowed the window deadline. Failure to spawn is reported immediately.

Sizing compares each child's extent along its parent's split axis with the sum of sibling
extents. It aims within **1 percentage point** of the requested share, using at most three
passes per split. Residual differences up to **5 percentage points** are accepted; larger
differences or unusable sizing geometry produce a warning rather than failing an otherwise
correct hierarchy. Gaps, borders, integer rounding, and application minimum sizes remain
in effect. Structural and ownership errors are always fatal.

App IDs/classes identify a window type, not which process created it. Concurrent launches
with the same selector can be indistinguishable; distinct IDs reduce ambiguity. Sway rules,
inherited activation context, and single-instance applications can place a window elsewhere;
the loader refuses rather than moving it. IPC observations and commands are not atomic:
these safeguards do not provide isolation from concurrent desktop activity.

## Capture workflow

Capture records an arrangement as a draft, not a restorable session. From a terminal in the
arrangement, or a keybinding, run `sway-stage capture > draft.yaml`. To exclude your terminal,
run the following from another workspace and switch to the arranged workspace during the
delay:

```sh
sleep 5; sway-stage capture > draft.yaml
# Edit draft.yaml: fill command and match placeholders, and review any inferred commands.
sway-stage --layouts-dir . check draft
```

Move the edited file into your layouts directory, then load it from a keybinding on a new,
empty workspace. The [examples](../examples) show complete commands and selectors.

Capture reads one Sway tree through `SWAYSOCK`, with a bounded response wait. It never
launches applications or sends layout/focus commands. Later desktop changes cannot redirect
or invalidate the saved structure; no freeze or ongoing event observation is attempted.
Standalone capture writes YAML exclusively to stdout; the shell controls the destination
(and may create or truncate the file even if capture fails). After the complete draft is
written and flushed, a concise success summary goes to stderr without a destination filename.
It reports disabled inference and required manual editing, or the inferred and unresolved
command counts. Unresolved commands require manual editing; fully inferred commands still
need review and are best-effort, with no promise of recreating the original windows.
These counts cover launch commands only, independently of `match: null` placeholders.

The draft preserves split hierarchy, orientations, visual order, and focused window. IDs
are `window1`, `window2`, … in depth-first order. Child weights are percentages of summed
sibling extents on the split axis, rounded to hundredths with a minimum of `0.01`; the root
has no weight. A single window becomes the root. Single-child wrappers collapse after
validation; multi-child groups remain even if their orientation matches their parent's.
Selectors prefer nonempty Wayland app IDs, then Xwayland classes, otherwise `match: null`.
Commands default to `command: null`. Workspace names, outputs, titles, container IDs, PIDs,
cwd, and environment are not serialized.

Capture refuses empty workspaces, floating containers, fullscreen windows/groups,
scratchpad content (even shown), tabbed/stacked containers (even with one child), unsupported
or malformed nodes, nonpositive split-axis extents, ambiguous/absent focus, focus on a group
rather than a window, and layouts exceeding the format's depth/node limits. The workspace's
own fullscreen flag is not treated as a fullscreen window.

`--infer-commands` uses each saved window's actual PID. It first reads
`/proc/PID/root/.flatpak-info` (at most 64 KiB, valid UTF-8). A valid `[Application]`
`name` produces `[flatpak, run, APPLICATION_ID]`, with separate `--branch=...` and
`--arch=...` arguments before the ID when valid fields are present in `[Instance]`.
These select the running reference rather than Flatpak's current default. Window matchers
still reflect Sway's actual app ID or Xwayland class, independently of the launch command.
Runtime-only sandboxes, malformed/ambiguous metadata, invalid identity fields, and metadata
access errors leave `command: null`; sandbox-internal argv is never used in these cases.
Identity fields use a conservative ASCII subset; unsupported key-file escapes are rejected.

Only when `.flatpak-info` is missing does inference inspect `/proc/PID/environ` for GLib's
`GIO_LAUNCHED_DESKTOP_FILE` and `GIO_LAUNCHED_DESKTOP_FILE_PID`. Reads are bounded to 64 KiB
and require complete NUL-separated entries. Only these two variables are extracted;
unrelated values, including non-UTF-8 bytes, are ignored and no environment mapping is copied into
the draft or diagnostics. The recorded positive process PID must exactly match the window
PID supplied by Sway. The launcher path must be valid UTF-8, absolute, end in `.desktop`,
and refer to a readable, nonempty regular file. Verified metadata produces the host command
`[gio, launch, /absolute/path/to/application.desktop]`, preserving the path as one argument,
including spaces and private launchers outside XDG application directories. `gio` must be
available on the host `PATH`. GLib interprets the launcher; sway-stage does not parse `Exec`,
expand field codes, or reproduce terminal, working-directory, or D-Bus activation semantics.

A different valid recorded PID indicates inherited metadata, which is ignored. Absent
launcher fields or inherited metadata permit ordinary `/proc/PID/cmdline` inference.
Duplicate identity fields, missing/malformed PIDs, and matching-PID metadata with a missing
or unusable launcher leave `command: null`; they never silently fall back to argv.
Inaccessible, oversized, or incomplete environment reads also leave the command unresolved.
Valid Flatpak metadata takes precedence over GLib metadata, including inherited fields;
unusable Flatpak metadata still suppresses both launcher and argv fallback.

Ordinary cmdline reads are bounded to 64 KiB per window, requiring valid UTF-8, a nonempty
executable, and NUL-terminated arguments. Argument boundaries, embedded spaces, and empty
arguments are preserved. Unavailable or malformed process data leaves that window's command
null without failing capture or extra warnings. No ancestor searches or application-name,
executable-name, title, or app-ID heuristics identify launchers. Flatpak inference recovers
a conventional host launch, not the original installation, permission overrides, application
arguments, or session. Metadata `extra-args` describes sandbox options, not the application's
argv, and is ignored. Flatpak must be available on host `PATH`.

The metadata layout and reference selection follow Flatpak's
[metadata documentation](https://github.com/flatpak/flatpak/blob/main/doc/flatpak-metadata.xml),
[metadata writer](https://github.com/flatpak/flatpak/blob/main/common/flatpak-run.c), and
[run command documentation](https://github.com/flatpak/flatpak/blob/main/doc/flatpak-run.xml).
GLib launcher identity follows its
[launch documentation](https://docs.gtk.org/gio/vfunc.AppInfo.launch.html);
[`gio launch`](https://manpages.debian.org/testing/libglib2.0-bin/gio.1.en.html)
accepts a desktop-file path from any location.

**Review inferred commands before executing or sharing.** They may contain secrets and may
not reproduce the application. Process exit, PID reuse, or argument changes can race with
inference; it is best-effort, with no second process-identity check. Launcher files may change
or disappear, and their validity and behavior are not established by checking the path.
Launcher reconstruction does not recover original file/URL arguments or application state.
Review the referenced launcher as well as the recovered command. Inferred arguments stay
out of diagnostics and logs, appearing only in the draft sent to stdout or explicitly saved
through the menu. Fill all remaining null placeholders and run `check` before loading.

## Fuzzel menu

`menu` requires Fuzzel 1.11+ on `PATH`, using default dmenu text output. It offers sorted
`Load: NAME` entries followed by `Capture current layout…`, even with no layouts. Only exact
offered actions are accepted; arbitrary typed action text is an error. Both prompts disable
automatic exit on keyboard-focus loss, without changing Fuzzel's configured focus mode. Interactive
input has no deadline. Escape at either prompt cancels successfully without saving/loading.
Fuzzel cancellation requires exit status 2 and empty output; other failures, index output,
control characters, or anything other than one UTF-8 newline-terminated line are errors.

One read-only snapshot is saved **before the first prompt**. Both prompts use its original
output when known. Snapshot errors are deferred until an action needs the snapshot;
cancellation remains available, and empty/unsupported capture layouts do not block Load.

- **Load:** after Fuzzel exits, rediscover and validate the selected file, then require the
  original workspace identity to be focused and empty. A recreated workspace with the same
  name/number is refused. Focus is never restored automatically. Intermediate away-and-back
  changes while the menu is open need not be detected; construction then uses the normal
  continuous focus/output safeguards. If the original identity is unavailable, loading
  refuses after ordinary file validation.
- **Capture:** render only the pre-menu snapshot, then ask for a draft name. Later workspace,
  window, output, or focus changes during either prompt do not alter the draft. Unsupported
  capture state fails before the name prompt. `--infer-commands` runs only after Capture is
  selected, using the saved PIDs; process arguments may have changed during the wait.

After a successful save, the bounded, best-effort notification includes the saved YAML path,
inference status, and unresolved-command count. It calls for manual editing when commands
remain unresolved, or review of best-effort commands when all were inferred. Notification
failure does not fail the save. Cancellation and failed captures do not report success.

Save `NAME.yaml` into the discovery directory. Names follow the layout-name grammar, at
most 250 ASCII characters, without an extension; input is not trimmed or normalized.
Reject collisions at either `.yaml` or `.yml`, including symlinks. Create missing default
directories only on save (0700); preserve existing directory permissions. A missing explicit
directory fails discovery before prompting. New drafts are 0600.

Saving requires hard-link support in that directory. Publish only complete drafts,
exclusively, without overwriting an existing `.yaml`. Cross-suffix concurrent external
changes are not atomic. On write/publication failure, retain the reported `.partial` file
for inspection; it is outside discovery. Do not automatically remove uncertain files.
A successful save attempts a path notification; notification failure does not fail the save.

Drafts appear immediately in `list` and the menu. They are saved without executable-layout
validation; `check` and `load` reject remaining placeholders before loader IPC or application launches. Edit
them in place and check them. There is no automatic repair, execution, or deletion.

## Diagnostics and troubleshooting

`load`, `menu`, and `capture` report errors to stderr and attempt bounded desktop
notifications through `notify-send`. Sizing warnings also notify. Missing notification
tools or delivery failures never hide the primary result. Success requires no notification.

Each load attempt, including a menu Load selection, replaces the private launch log at
`$XDG_STATE_HOME/sway-stage/last-run.log`, falling back to
`$HOME/.local/state/sway-stage/last-run.log`; state bases must be absolute. New log files are
0600 and new directories 0700. Existing log links are not followed or reused. Log failure
falls back to stderr. Records include timestamps, stages, waits, accepted IDs, commands and
replies, hierarchy checks, and requested/observed sizes; titles, full argv, and environment
values are omitted by default. No telemetry or background service is involved.

Menu cancellation, selection errors, capture, and saves leave the launch log untouched.
Capture failures report through stderr and notifications only. After a failed load that
started applications, inspect remaining windows before retrying; failure messages identify
the stage and log path when available. Timeouts distinguish no new windows from new windows
that failed to match, with observed IDs, selectors, locations, and floating state.

| Symptom | What to check |
| --- | --- |
| Workspace occupied | Invoke from a Sway keybinding on an empty workspace; the calling terminal counts as an occupant. |
| No matching window / wrong workspace | Check executable, exact app ID/class, Sway assignment rules, and single-instance behavior. |
| Focus/output refusal | Keep the target focused on its original output until completion; inspect partial results before retrying. |
| Draft rejected | Replace null commands/selectors and review inferred arguments, then run `check NAME`. |
| Menu fails | Install Fuzzel 1.11+, use dmenu text output, and check Fuzzel's stderr/configuration. |
| Proportions differ | Check gaps and application minimum sizes; substantial deviations are warnings. |

No minimum supported Sway version or Linux architecture/libc baseline has been established.
Release packaging and compatibility verification remain in the [roadmap](PLAN.md).
