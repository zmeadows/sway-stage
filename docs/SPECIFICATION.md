# User reference and specification

This is the authoritative contract for implemented behavior. Start with the
[README](../README.md) for installation; contributors should also read
[development](DEVELOPMENT.md) and [AGENTS.md](../AGENTS.md).

## Commands and layout discovery

| Command | Behavior |
| --- | --- |
| `load NAME [--cwd PATH]` | Validate and launch a layout on the focused, empty workspace. Success is silent. |
| `check NAME` | Validate syntax, semantics, and cwd existence; print `NAME: valid`. |
| `list` | Print discovered names in lexical order without parsing contents. |
| `capture [--no-infer-commands]` | Print an editable draft of the focused workspace. |
| `menu [--no-infer-commands]` | Load a layout or save a named capture through Fuzzel. |

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
`[A-Za-z0-9][A-Za-z0-9_-]*`. Invalid filenames are skipped. Duplicate stems such as
`foo.yaml` and `foo.yml` are ambiguous: neither file is listed or selectable in the menu.
Other valid layouts remain usable. `list` reports these discovery warnings on stderr,
keeping stdout to sorted, unambiguous names. The menu omits these entries without filename
warnings or notifications; unambiguous files with invalid YAML retain their `[invalid]` labels.
`check NAME` and `load NAME` report an ambiguity error identifying the conflicting filenames
when that name is requested, before loader IPC or application launches. Unrelated filename
problems do not affect their resolution. Directory read, enumeration, and inspection failures
remain fatal. Reads never create directories. A missing default directory lists as empty;
a missing explicit directory or requested layout is an error.

Success and menu cancellation exit 0, operational or validation errors exit 1, and invalid
CLI usage exits 2. Data goes to stdout; errors, warnings, and diagnostics go to stderr.
Capture prepares a complete draft before writing; acquisition or rendering failure emits
no YAML. An output-stream failure can still interrupt delivery.

## YAML format, version 1

See [`examples/four-windows.yaml`](../examples/four-windows.yaml) for nested splits,
[`mixed-groups.yaml`](../examples/mixed-groups.yaml) for tabbed and stacked groups, and
[`working-directory.yaml`](../examples/working-directory.yaml) for directory selection.
Each file contains one UTF-8 YAML document with these top-level keys:

| Key | Contract |
| --- | --- |
| `version` | Required integer `1`; reject other versions. |
| `layout` | Required root node: a group or a window. |
| `cwd` | Optional default working directory for all windows; a window’s own `cwd` overrides it. |
| `cwd_select` | Optional boolean, default `false`; `true` requires top-level `cwd`. In the menu, select the base directory or an immediate child as the layout default. |
| `focus` | Optional window `id` to focus at completion; default is the first leaf. |
| `window_timeout_ms` | Optional integer from `1` to `300000` (five minutes), default `20000`, for each window wait. |

A group node has `group` and an ordered `children` sequence. The four values use Sway's
IPC layout names: `splith` (left to right) and `splitv` (top to bottom) require at least
two children; `tabbed` and `stacked` require at least one. Groups and windows can nest
arbitrarily within validation limits. The old `split` field and `horizontal`/`vertical`
values are rejected; the configuration version remains `1`.

`check`, loading, and capture support all four group types, including singleton
tabbed/stacked groups and mixed nesting.

A window node has the following fields:

| Key | Contract |
| --- | --- |
| `id` | Required unique, nonempty identifier matching the layout-name grammar. |
| `command` | Required nonempty string sequence: nonempty executable first, then literal arguments. |
| `match` | Required mapping with exactly one nonempty string: `app_id` or `class`. |
| `cwd` | Optional working directory overriding the layout default. |
| `env` | Optional string-to-string map of child environment overrides; default empty. |

Children of `splith` and `splitv` may have `weight`, a finite positive number defaulting
to `1`. This includes groups of any type. Its share is `weight / sum(sibling weights)`
along the parent's split axis; weights need not sum to 100 and every sibling sum must be
finite. Children of `tabbed` and `stacked` must omit `weight`, even when it would be `1`.
The root may still have a valid weight, which has no effect. Tree order is depth-first
launch order and sibling order, including tab/stack header order. Do not flatten nested
splits merely because their orientation matches their parent. Global `focus` selects the
final window and its visible tab along its ancestry; it defaults to the first leaf. After
sizing, every tabbed/stacked group selects its first child, including inactive and nested
groups. Global focus is applied last and may override those selections along its ancestry.
There is no per-group selection setting.

Window matching uses exact, case-sensitive Wayland `app_id` or Xwayland
`window_properties.class` equality, never a title, substring, regex, or PID alone. Repeated
selectors are allowed for separate sequential launches; IDs must still be unique.
Users should supply distinct application IDs in both arguments and selectors when the
application supports them. The launcher never invents IDs or modifies commands.

Both layout-level and window-level `cwd` must be nonempty absolute paths (such as
`/home/user/Projects/gon`) or home-relative paths beginning with `~/` (such as
`~/Projects/gon`). Only a leading `~/` expands, using the current user's `$HOME`, which
must be set, nonempty, and absolute when expansion is needed. Other YAML-relative paths,
including `.` and `../project`, are rejected. Paths never resolve against the YAML file.
There is no `$HOME` substitution, `~otheruser` lookup, globbing, or command substitution.
Resolved paths must exist and identify directories; symlinks to directories are accepted.
Explicit `null`, empty paths, and paths containing NUL are invalid. Both levels are
validated independently before loader IPC or launches, even when an override would hide
one of them. Groups do not accept `cwd`; groups and windows do not accept `cwd_select`.

`load NAME --cwd PATH` overrides the layout default with any valid directory, including
one outside the configured root. CLI-relative paths resolve from the invoking process's
working directory: `--cwd .` uses that directory and `--cwd ../another-project` uses its
sibling. Absolute paths and literal leading `~/` paths are also supported. The override is
validated before connecting to Sway or launching applications. This option belongs only
to `load`, and does not bypass validation of configured paths.

For each child, precedence is window `cwd`, then CLI `--cwd` or the menu-selected
directory, then layout `cwd`, then the launcher's existing working directory. The parent process's directory never changes.
`cwd_select` accepts only booleans (explicit `null` is invalid). A valid layout `cwd` may
be used with `cwd_select` omitted or `false`. With `cwd_select: true`, direct loading
still uses the configured root normally unless `--cwd` overrides it. Only `menu` offers
interactive project directory selection; direct `load` never prompts. Capture and command
inference do not attempt to reconstruct working-directory settings.

Commands are launched with preserved argument boundaries, without implicit shell parsing,
variable expansion, tilde expansion, or globbing. A bare executable uses the child's
effective `PATH`. Executable paths containing `/` resolve relative to the effective child
cwd selected above. `env` overrides the inherited
environment only in that child. Keys must be nonempty and contain neither `=` nor NUL;
arguments, paths, and environment values cannot contain NUL. An empty argument or
environment value is allowed. Users may explicitly invoke a shell in `command`; layout
files are executable instructions and should be trusted accordingly.

Validation rejects unknown or duplicate keys, mixed group/window fields, wrong types,
missing required values, empty children/commands/selectors, invalid weights, duplicate
IDs, and a `focus` that does not identify a leaf. Do not silently coerce or repair input.
Reject multiple documents, custom tags, merge keys, and aliases. Bound input size (1 MiB),
tree depth (32 nodes along a path), and total layout nodes (256) before side effects;
parser resource limits also bound pathological input.
Errors include the filename, node/field path, and line/column where available.
String fields require quotes around numeric/boolean scalars; numeric fields reject quoted
numbers. Explicit nulls never trigger optional defaults.

## Loading, sizing, and safety

Layouts describe new applications in ordered split, tabbed, and stacked groups. There is no
session restoration, existing-window reuse, daemon, ongoing enforcement, workspace/output
assignment, or management of application lifetimes after launch. Floating and fullscreen
arrangements are outside the layout format.

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
4. Preserve group layouts, nesting, and order, including meaningful same-axis nested groups
   and singleton tabbed/stacked groups.
   Recheck workspace, output, focus, ownership, and hierarchy before every launch or mutation;
   verify command replies and resulting structure. Accepted windows must retain their
   identity, selector, tiled state, and location. Closing one or changing its state stops
   construction rather than triggering repair or relaunch.
5. Mutate only accepted windows and containers whose window descendants all belong to the
   run. Never close, move, resize, or reuse preexisting windows, use broad app-ID/class
   criteria for mutations, or manipulate the global scratchpad. Construction uses neither
   window moves nor placeholder windows.
6. Apply weights from outer splits inward, skipping sibling sizing at tabbed/stacked levels
   while still sizing nested splits. Then focus the configured leaf (the first depth-first
   leaf by default), verify it, and exit.

Construction verifies intermediate wrappers as well as the completed hierarchy. Unexpected
wrapping, flattening, or reparenting is fatal, even when the result looks similar. An empty
workspace may initially report any of the four layouts. Sway rules or `workspace_layout`
settings that introduce additional containers can still cause a safe hierarchy refusal;
the loader does not remove unexpected wrappers.

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
(and may create or truncate the file even if capture fails). Capture does not control the
permissions of a redirected file. After the complete draft is written and flushed, a concise
success summary goes to stderr without a destination filename.
It reports disabled inference and required manual editing, or the inferred and unresolved
command counts. Unresolved commands require manual editing; fully inferred commands still
need review and are best-effort, with no promise of recreating the original windows.
These counts cover launch commands only, independently of `match: null` placeholders.

The draft preserves `splith`, `splitv`, `tabbed`, and `stacked` groups, their nesting,
child order, and the globally focused window. IDs are `window1`, `window2`, … in
depth-first order. Children of splits receive weights from their observed widths or
heights, including when the child is a tabbed/stacked group. Weights are percentages of
summed sibling extents on the split axis, rounded to hundredths with a minimum of `0.01`.
Children directly inside tabbed/stacked groups have no weights; nested splits still receive
weights on their own children. The root has no weight. Single-child split wrappers collapse
after validation, including at the workspace root; singleton tabbed/stacked groups remain.
Multi-child splits remain even if their orientation matches their parent's. A window with
only split wrappers becomes the root.
Selectors prefer nonempty Wayland app IDs, then Xwayland classes, otherwise `match: null`.
Launch commands are inferred automatically on a best-effort basis; unresolved commands remain
`command: null`. `--no-infer-commands` disables inference and leaves all commands null.
Workspace names, outputs, titles, container IDs, PIDs, cwd, and environment are not serialized.

Capture records only global focus, not independently selected tabs in inactive groups.
Reloading uses the first-child defaults described above, then applies global focus along
its ancestry. Capture does not query additional snapshots or infer inactive selections.

Inferred commands use compact flow sequences such as `command: [foot, --app-id=term]`
when the YAML-escaped line, including indentation, fits within 120 characters. Longer
commands use block sequences. Layout `children` remain block sequences, and unresolved
commands remain `command: null`. Both command styles are accepted by `check` and `load`.

Capture refuses empty workspaces, floating containers, fullscreen windows/groups,
scratchpad content (even shown), unsupported layouts or malformed nodes (including empty
groups), nonpositive sibling extents in multi-child splits, ambiguous/absent focus, focus
on a group rather than a window, and layouts exceeding the format's depth/node limits.
Tabbed/stacked children need no split-axis geometry. The workspace's own fullscreen flag
is not treated as a fullscreen window.

Command inference uses each saved window's actual PID. Before and after inference, it
compares the process start time from `/proc/PID/stat` (bounded to 4 KiB) and the executable's
path, device, and inode. Missing/malformed identity observations or observed changes leave
`command: null`. This is not atomic verification: Sway's saved PID may already be stale,
and same-executable execs or changes made and undone between observations can escape it.

Launcher selection first reads `/proc/PID/root/.flatpak-info` (at most 64 KiB, valid UTF-8).
A valid `[Application]` `name` produces `[flatpak, run, APPLICATION_ID]`, with separate `--branch=...` and
`--arch=...` arguments before the ID when valid fields are present in `[Instance]`.
These select the running reference rather than Flatpak's current default. Window matchers
still reflect Sway's actual app ID or Xwayland class, independently of the launch command.
Runtime-only sandboxes, malformed/ambiguous metadata, invalid identity fields, and metadata
access errors leave `command: null`; sandbox-internal argv is never used in these cases.
Identity fields use a conservative ASCII subset; unsupported key-file escapes are rejected.

Only when `.flatpak-info` is missing does inference read `/proc/PID/environ`, once per
window, bounded to 64 KiB with complete NUL-separated entries. It extracts only the GLib,
AppImage, and unsupported Snap markers described below. Duplicate or malformed GLib/AppImage
entries and inaccessible, oversized, or incomplete reads leave `command: null`. Unrelated values,
including non-UTF-8 bytes, are ignored; no environment mapping or values enter diagnostics.
Linux exposes the [initial environment of the current executable](https://man7.org/linux/man-pages/man5/proc_pid_environ.5.html),
which need not reflect later environment changes; access restrictions can also prevent
inference. Missing launcher fields therefore cannot establish the application's full launch
history.

GLib inference uses `GIO_LAUNCHED_DESKTOP_FILE` and `GIO_LAUNCHED_DESKTOP_FILE_PID`.
The recorded positive process PID must exactly match the window PID supplied by Sway.
The launcher path must be valid UTF-8, absolute, end in `.desktop`, and refer to a readable, nonempty regular file. Verified metadata produces the host command
`[gio, launch, /absolute/path/to/application.desktop]`, preserving the path as one argument,
including spaces and private launchers outside XDG application directories. `gio` must be
available on the host `PATH`. GLib interprets the launcher; sway-stage does not parse `Exec`,
expand field codes, or reproduce terminal, working-directory, or D-Bus activation semantics.

A different valid recorded PID indicates inherited GLib metadata, which is ignored.
Missing/malformed PIDs and matching-PID metadata with a missing or unusable launcher leave
`command: null`; they never fall back to package inference or argv.

AppImage inference requires `APPIMAGE` and `APPDIR`. `APPIMAGE` must be a resolved absolute
path to a readable, nonempty regular file with executable permission bits; no `.AppImage`
extension is required. `APPDIR` must resolve to an existing absolute directory, and
`/proc/PID/exe` must resolve to an executable path inside it. A host descendant outside that
directory is insufficient evidence even if it inherits the environment. The image must be
outside `APPDIR` and outside paths with `.mount_` components. A conventional image produces
one argument, `[/absolute/path/to/application]`, preserving spaces and other literal content.
No executable inside the image mount is copied into the command.

When present, `ARGV0` must retain the image's basename and be either that bare name,
`./name`, or the resolved absolute `APPIMAGE` path. Different symlink names can select a
different program in a multi-binary AppImage; these and invocation paths requiring the
original cwd are left unresolved. Missing `ARGV0` permits a conventional image launch but
cannot establish whether unrecorded invocation-name behavior was involved. `ARGV0` alone
is not AppImage identity. Missing/inconsistent supporting metadata, vanished images,
nonexecutable files, and unavailable process executables leave `command: null`. These are
modest filesystem checks, not AppImage format, mount, or wrapper validation.

After Flatpak detection, a verified exact-PID GLib launcher wins; unresolved GLib evidence
blocks all fallback. With GLib absent or inherited, any `SNAP`, `SNAP_NAME`,
`SNAP_INSTANCE_NAME`, `SNAP_APP_NAME`, or `SNAP_INSTANCE_KEY` marker leaves `command: null`.
Snap environments are unsupported: marker presence alone blocks both AppImage reconstruction
and ordinary argv, even with empty, malformed, duplicate, or non-UTF-8 values. An independently
verified GLib launcher may still supply a command. Otherwise, AppImage evidence is resolved
as described above; unusable evidence blocks argv. Only absence of AppImage and unsupported
Snap evidence permits ordinary `/proc/PID/cmdline` inference.

Deleted process executables cannot corroborate AppImage membership, even if a new
file exists at the former path. Filesystem checks describe only the inspected files: a
launcher may have been updated since launch or change again before replay. No original
launcher contents or executable format, access policy, or future launch success is verified.

Package membership cannot distinguish every helper or descendant within the same package.
Interpreted applications whose executable is a host/base-runtime interpreter, unsupported
mount locations, and wrappers without corroborating package membership remain unresolved.
Reconstruction does not discover original arguments, cwd, environment overrides, wrapper
behavior, or invocation-name semantics that were not recorded. It recovers a conventional
host launch, not an exact window or application/session restoration.

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
accepts a desktop-file path from any location. AppImage identity and invocation-name
constraints follow its [environment documentation](https://docs.appimage.org/packaging-guide/environment-variables.html).

GLib 2.84.4's [`gio launch` implementation](https://github.com/GNOME/glib/blob/2.84.4/gio/gio-tool-launch.c)
loses the source filename while loading the desktop entry. Replaying an inferred launcher
command still starts the application, but its PID-only metadata makes a subsequent inferred
capture leave `command: null`, even if AppImage evidence is also available. This
preserves the unresolved launcher boundary rather than bypassing missing provenance.
Fill that placeholder before another load, or retain the previously reviewed launcher command. The
[newer implementation](https://github.com/GNOME/glib/blob/main/gio/gio-tool-launch.c)
preserves the filename, allowing repeated launcher inference. sway-stage never invents
missing launcher provenance or copies environment fields into the draft.

**Review inferred commands before executing or sharing.** They may contain secrets and may
not reproduce the application. Process exit, PID reuse, or argument changes can race with
inference; it is best-effort, with no second process-identity check. Launcher and package
files may change or disappear, and checking the path does not establish their validity or
behavior.
Process metadata and filesystem checks are separate observations, not an atomic identity
or executability guarantee (for example, mount execution policy is not checked).
Launcher reconstruction does not recover original file/URL arguments or application state.
Review the referenced launcher as well as the recovered command. Inferred arguments stay
out of diagnostics and logs, appearing only in the draft sent to stdout or explicitly saved
through the menu. Fill all remaining null placeholders and run `check` before loading.

## Fuzzel menu

`menu` requires Fuzzel 1.11+ on `PATH`, using default dmenu text output. One read-only
snapshot is saved **before the first prompt**. Choices depend on the actual focused
workspace in that snapshot, never its name or number:

- **Empty:** both tiled and floating child lists must be empty, exactly as for loading.
  Even an empty structural container counts as occupancy. Validate discovered files using
  the same syntax, semantics, and filesystem checks as `check`. Show names in lexical
  order by actual name, with the prompt `Load layout: `. Valid names appear normally;
  invalid or incomplete layouts have an `[invalid]` prefix. Their presence alone does
  not notify. Selecting one reports a concise error directing the user to
  `sway-stage check NAME`, without loading or replacing the launch log. Validation does
  not guarantee application launch success. If no layouts exist, report the missing-layout
  error without opening Fuzzel. If all layouts are invalid, report “No valid layouts found”
  with guidance to run `check`, without opening Fuzzel or replacing the launch log.
- **Populated and capturable:** immediately ask for a draft name with the prompt
  `Capture layout as: `, using Fuzzel's prompt-only mode, after validating the saved
  arrangement through capture rendering with command inference disabled.
  Do not discover layouts; capture remains available with no layouts or invalid discovery
  entries. Inference does not change eligibility when commands cannot be inferred.
- **Populated and noncapturable:** report the capture validation reason without opening
  Fuzzel, for example guidance to tile floating windows, exit fullscreen, or focus a window
  instead of its parent. Load is unavailable on any occupied workspace.

Initial snapshot failures report an error without opening Fuzzel. No unavailable-action
case writes the launch log. Layout choices are fixed while the prompt is open; eligibility
is a convenience and does not replace operation-specific validation after selection.

Only exact offered layout labels are accepted; arbitrary typed selection text is an error.
All prompts use the original output when known and disable automatic exit on keyboard-focus loss,
without changing Fuzzel's configured focus mode. Interactive input has no deadline. Escape
at any offered prompt cancels successfully without saving/loading or replacing the launch
log. Fuzzel cancellation requires exit status 2 and empty output; other failures, unexpected
output, control characters, or anything other than one UTF-8 newline-terminated line are errors.

- **Load:** after Fuzzel exits, rediscover and validate the selected file, then require the
  original workspace identity to be focused and empty. A recreated workspace with the same
  name/number is refused. Focus is never restored automatically. Intermediate away-and-back
  changes while the menu is open need not be detected; construction then uses the normal
  continuous focus/output safeguards.
  With `cwd_select: true`, validation is followed by a second Fuzzel prompt,
  `Launch in: `, before loading. The first option, `Base directory (PATH)`, uses the
  configured top-level `cwd`, preserving its configured spelling (such as `~/projects`).
  Immediate child directories follow, using filesystem directory reads without recursive scanning.
  Names beginning with `.`, files, non-directory entries, non-UTF-8 names, and names
  containing control characters are excluded. Directory symlinks are included even when
  their targets are outside the root; broken or unusable symlinks are ignored. Basenames
  appear in deterministic lexical order. Only offered choices are accepted, even when
  a child name resembles the base-directory label. Arbitrary typed paths and parent traversal
  are unavailable. With no eligible children, the base directory is the sole option.
  An enumeration failure is an error and launches nothing.
  Escape cancels successfully and preserves the previous launch log. Selection errors
  also leave that log untouched.
  Selecting the base keeps the configured default; selecting a child replaces it.
  Window `cwd` overrides still win. A selected child path is validated again before loader IPC
  or application launches, so a removed or invalid directory fails without launching.
  Filesystem checks do not lock paths against subsequent changes. The parent process's
  directory never changes.
  Both prompts retain the original output and workspace baseline; directory selection
  never captures a replacement baseline or bypasses fresh emptiness, focus, output,
  and ownership checks. With `cwd_select` omitted or false, Load uses one prompt.
- **Capture:** render only the pre-menu snapshot, then ask for a draft name.
  Invalid names and existing-name conflicts reopen the same prompt with an error placeholder
  identifying the rejected input (abbreviated when long). Type another name or Escape to cancel;
  input is never prepopulated. Retries reuse the original rendered draft without further Sway
  queries or command inference. Operational save errors terminate instead of retrying.
  Later workspace, window, output, or focus changes do not alter the draft.
  Unsupported capture state fails before the name prompt. Default command inference runs after
  eligibility validation and before the prompt, using the saved PIDs; process arguments
  may have changed since the snapshot.

After a successful save, the bounded, best-effort notification says only `Saved draft 'NAME'.`.
Notification failure does not fail the save. Cancellation and failed captures do not report
success. `menu --no-infer-commands` disables inference for saved captures.

Save `NAME.yaml` into the discovery directory. Names follow the layout-name grammar, at
most 250 ASCII characters, without an extension; input is not trimmed or normalized.
Reject collisions at either `.yaml` or `.yml`, including symlinks. Create missing default
directories only on save (0700); preserve existing directory permissions. Capture also creates
a missing explicit directory on save; Load reports its discovery error before prompting.
New drafts are 0600.

Saving requires hard-link support in that directory. Publish only complete drafts,
exclusively, without overwriting an existing `.yaml`. Cross-suffix concurrent external
changes are not atomic. On write/publication failure, retain the reported `.partial` file
for inspection; it is outside discovery. Do not automatically remove uncertain files.
A successful save attempts a path notification; notification failure does not fail the save.

Drafts appear immediately in `list`; the menu marks incomplete drafts `[invalid]` when
at least one valid layout is available. They are saved without executable-layout validation; `check` and `load` reject remaining placeholders
before loader IPC or application launches. Edit them in place and check them. There is no automatic repair, execution, or deletion.

## Diagnostics and troubleshooting

`load`, `menu`, and `capture` report errors to stderr and attempt bounded desktop
notifications through `notify-send`. Sizing warnings also notify. Missing notification
tools or delivery failures never hide the primary result. Error notifications give concise
summaries; detailed causes remain on stderr and in the launch log when applicable. A failure
notification refers to `last-run.log` only when logging remains available for that attempt.
Pre-load menu failures identify missing layouts, unavailable capture, Fuzzel problems,
save failures, or working-directory selection problems without exposing paths or technical
error chains. Successful loads do not notify; saved menu captures retain their save
confirmation. Cancellation does not notify.

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
