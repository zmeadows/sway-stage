# User reference

This document describes the implemented CLI, configuration, and observable safety
behavior. Start with the [README](../README.md) for installation and a small layout.
For implementation rationale see [INTERNALS.md](INTERNALS.md); for verification see
[DEVELOPMENT.md](DEVELOPMENT.md).

## Commands and configuration files

| Command | Behavior |
| --- | --- |
| `list` / `profile list` | List unambiguous names in lexical order; do not parse file contents. |
| `check NAME` | Validate a layout without contacting Sway or launching an application. |
| `load NAME [--cwd PATH]` | Load a layout onto the currently focused, empty workspace. |
| `profile show NAME` | Display mappings in file order; does not read referenced layouts. |
| `profile check NAME` | Validate the profile and *all* referenced layouts offline. |
| `profile load NAME` | Load layouts on explicit workspace targets in profile order. |
| `capture [--no-infer-commands]` | Print an editable draft of the focused workspace to stdout. |
| `profile capture NAME WORKSPACE...` | Save one draft per selected workspace and a profile. |
| `profile capture NAME --all` | Capture every populated ordinary workspace. |
| `menu [--no-infer-commands]` | Use one Fuzzel selector for loading or capture. |

Prefix commands with `sway-stage`. Global options `--layouts-dir PATH` and
`--profiles-dir PATH` independently override the two configuration directories.
The defaults are:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/sway-stage/layouts/
${XDG_CONFIG_HOME:-$HOME/.config}/sway-stage/profiles/
```

`XDG_CONFIG_HOME` and the fallback `HOME` must be absolute. Explicit directory
arguments may be relative to the invoking process's cwd. Standalone `capture` writes
stdout and does not use `--layouts-dir`; the menu and profile capture use it to save drafts.

Discovery considers only immediate, regular `.yaml`/`.yml` files (not symlinks or nested
files). Names are case-sensitive filename stems matching
`[A-Za-z0-9][A-Za-z0-9_-]*`. Two files with the same stem are ambiguous: neither can
be listed or loaded until renamed. Invalid YAML may still appear in the menu as
`[invalid]`. A missing *default* directory is treated as empty for listing; a missing
explicit directory is an error. Reads do not create directories.

`list`, `check`, `profile list`, `profile show`, and `profile check` never contact Sway,
launch applications, notify, or replace the load log. `check` validates configuration,
not whether executables exist or windows will match. Successful checks print
`NAME: valid`. CLI success and menu cancellation return exit status 0; operational
errors return 1 and invalid CLI syntax returns 2. Running without a subcommand prints
help and does nothing else.

## Layout YAML

A layout is one UTF-8 YAML document. For example:

```yaml
version: 1
cwd: ~/Projects
cwd_select: true
window_timeout_ms: 20000
focus: editor
layout:
  group: splith
  children:
    - id: editor
      weight: 3
      command: [foot, --app-id=stage-editor]
      match: {app_id: stage-editor}
    - group: stacked
      weight: 2
      active: 2
      children:
        - id: status
          command: [foot, --app-id=stage-status]
          match: {app_id: stage-status}
        - id: logs
          command: [foot, --app-id=stage-logs]
          match: {app_id: stage-logs}
```

Top-level fields:

| Field | Meaning |
| --- | --- |
| `version` | **Required**, unquoted integer `1`. |
| `layout` | **Required** root window or group. |
| `cwd` | Optional default cwd for launched windows. |
| `cwd_select` | Optional boolean (default `false`); `true` requires top-level `cwd` and enables menu selection of its immediate subdirectories. |
| `window_timeout_ms` | Optional per-window wait, integer `1`–`300000`; default `20000`. |
| `focus` | Optional leaf window `id` selected at completion. |

**Groups** specify `group` and ordered `children`. The group types are:

| Type | Children | Selection / sizing |
| --- | --- | --- |
| `splith` | At least 2 | Side by side, left to right. |
| `splitv` | At least 2 | Top to bottom. |
| `tabbed` | At least 1 | One selected direct child; may set `active`. |
| `stacked` | At least 1 | One selected direct child; may set `active`. |

`active` is a **one-based, unquoted integer** selecting a direct child of a tabbed or
stacked group; it defaults to `1`. It cannot appear on split groups or windows.
A child of `splith`/`splitv` (including a subgroup) may set `weight` to a finite
positive number, default `1`. The share is `weight / sum(sibling weights)` on that
split's axis. Tabbed/stacked children and the root must omit `weight` entirely.
Groups can nest, including inside tabs/stacks; child order is launch order and
visual order. Do not flatten nested splits.

**Windows** specify:

| Field | Meaning |
| --- | --- |
| `id` | **Required** unique layout-local identifier, using the file-name grammar. |
| `command` | **Required** nonempty string array, executable then literal arguments. |
| `match` | **Required** map with exactly one nonempty `app_id` (Wayland) or `class` (Xwayland) string. |
| `cwd` | Optional per-window cwd overriding all layout/menu/CLI defaults. |
| `env` | Optional map of string environment overrides for that child alone. |

`match` is exact and case-sensitive. Titles, regexes, PID-only matches, and substrings
are not supported. The launcher does not invent app IDs; using distinct IDs where
applications permit them reduces matching ambiguity.

The default final focus descends through the first child of each split and the
`active` child of each tabbed/stacked group. Explicit top-level `focus` wins over
conflicting selections on the path to that leaf; unrelated tab/stack selections remain.
All tab/stack selections, including inactive nested groups, are applied and checked.

**Working directories.** YAML `cwd` paths must be absolute or start with `~/` (expanded
from `HOME`); they must exist and be directories. Paths relative to the YAML file,
`$HOME` expansion, globs, and shell expansion are not supported. `load NAME --cwd PATH`
accepts absolute paths, `~/`, and paths relative to the invoking cwd, but does not
override a window-level `cwd`. Effective order: window `cwd` > CLI/menu selection >
layout `cwd` > inherited launcher cwd. A CLI/menu override does not excuse an
invalid configured `cwd`: all configured paths are validated. The menu offers the
configured base directory and its immediate non-hidden child directories when
`cwd_select: true`; direct `load`
never prompts. Capture does not infer working directories.

**Commands and validation.** Each `command` is executed as an argv array with no
implicit shell or variable substitution. A bare executable uses the child's effective
`PATH`; an executable with `/` resolves relative to the effective cwd. Environment
values are child-only. `XDG_ACTIVATION_TOKEN` and `DESKTOP_STARTUP_ID` cannot be
specified in `env`. A layout is an instruction to execute programs: use only trusted
files. Validation rejects unknown/duplicate keys, incorrect types, explicit nulls,
invalid selectors, weights, focus, paths, IDs, or node mixtures. No coercion is done.
Each layout is limited to 1 MiB input, 32 nodes of nesting depth, and 256 nodes total;
YAML aliases, merge keys, custom tags, and multiple documents are rejected. The
legacy `split: horizontal/vertical` syntax is unsupported; use `group: splith/splitv`.

See the [two-window](../examples/two-foot.yaml), [nested](../examples/four-windows.yaml),
[mixed-group](../examples/mixed-groups.yaml), and
[working-directory](../examples/working-directory.yaml) examples.

## Profiles

Profiles are YAML files under the profiles directory; layouts referenced by name
resolve independently in the layouts directory. Example `work.yaml`:

```yaml
version: 1
workspaces:
  - workspace: 1
    layout: development
    output: DP-4
  - workspace: music
    layout: monitoring
  - workspace: "music player"
    layout: browser
```

`version` must be the unquoted integer `1`. `workspaces` is an ordered list of 1–256
entries. Every entry requires `workspace` and `layout` and may specify `output`.
`layout` uses the normal layout-name grammar. Targets must be unique:

- **Number:** unquoted integer `0`–`999999999`. Matches Sway's numeric workspace
  identity, including a descriptive name such as `1:code`.
- **Name:** exact, case-sensitive *non-numeric* Sway workspace (`num = -1`). Must start
  with an ASCII letter; may contain ASCII letters, digits, internal spaces, `_`, `-`,
  `.`, `:`, `+`; maximum 128 bytes, with no trailing space. Numeric-looking names and
  Sway workspace command keywords (such as `next`, `number`, `output`) are forbidden.
  Quoting names with spaces in YAML is recommended for clarity.

`output` is an optional exact, case-sensitive Sway output name (1–1024 UTF-8 bytes,
no control characters). It is a **placement expectation, not a placement request**.
Configure workspace placement in Sway itself (`workspace 1 output DP-4`).
`profile check` validates syntax, names, referenced layouts, and cwd paths offline;
it cannot verify actual output availability. `profile show` displays ordered mappings
without parsing referenced layouts; use `profile check` for full validation.

**Loading a profile:**

1. Validate and retain the profile and every referenced layout before connecting to
   Sway. Preflight **every** target before the first workspace switch: existing targets
   must be completely empty (including floating windows and empty structural containers),
   identities unambiguous, and any output expectations satisfied. A nonexistent target
   requires its specified output to be active.
2. Select or create target workspaces in YAML order, checking identity, emptiness,
   focus, and output before and after selection. Sway's rules determine output placement;
   Sway Stage does not move workspaces or assign outputs. A newly created target that
   lands on an unexpected output fails before its layout launches.
3. Load each layout with the same safety rules as standalone `load`. On success, leave
   the final profile workspace focused.

A later failure stops the entire profile. Earlier and partially built layouts remain;
there is no rollback, cleanup, focus restoration, or continuation. A failed preflight
starts **nothing**. Concurrent workspace changes, including away-and-back events,
are grounds for refusal.

## Loading and safety

Standalone `load` operates only on the *currently focused*, completely empty
workspace. It does not switch, create, rename, or move workspaces. Before construction,
it verifies emptiness, workspace identity, and containing output, and observes the
Sway event stream while building.

- Launch one configured window at a time in depth-first order. Accept only an
  unclaimed **new** window with the expected exact app ID/class, anywhere in the
  observed tree. Existing matching windows are never reused. Multiple matching
  windows, an unexpected window in the target, or a matching window elsewhere
  (including the scratchpad) cause refusal.
- Reject floating/fullscreen matches, unexpected wrapper containers, hierarchy
  changes, lost target ownership, workspace focus changes (including away and back),
  and target migration to another output. The loader may restore **in-workspace** focus
  to an owned container a bounded number of times; it never restores focus across
  workspaces or outputs.
- Mutate only windows and containers proven to belong to this load. Never close,
  relocate, or resize preexisting windows; never manipulate the global scratchpad.
  Verify IPC replies and observed hierarchy after operations.
- Size split siblings to their requested relative shares, aiming within 1 percentage
  point with up to three passes per split. Residual differences up to 5 percentage
  points are tolerated; larger/unknown differences warn but do not invalidate an
  otherwise correct layout. App minimum sizes, borders, and gaps can affect results.
- On failure stop launching or mutating; **leave all windows in place**. No retries,
  destructive rollback, or automatic relaunching. The last log helps identify which
  windows may remain.

Window selectors identify types, not their process of origin. Sway assignment rules,
single-instance applications, and simultaneous launches can defeat reliable matching.
Sway IPC is not transactional: event and snapshot checks reduce hazards but cannot
promise atomic isolation from concurrent desktop activity. Sway's launch-context
inheritance is deliberately avoided; wrapping the executable in a subreaper or unusual
Sway `exec` chain can still cause a refusal. See [internals](INTERNALS.md).

## Capturing layouts and profiles

**Single workspace:**

```sh
sway-stage capture > draft.yaml
sway-stage capture --no-infer-commands > draft.yaml
# After editing, place the draft in a layout directory and validate it:
sway-stage --layouts-dir . check draft
```

Standalone capture reads one Sway tree; it never changes focus or layout. It
preserves groups, order, relative split sizes, active tab/stack selections, and final
focus. It generates window IDs (`window1`, `window2`, ...). Selectors use app ID or
class when available. `command: null` or `match: null` marks information that must
be filled before loading. A shell redirect may create/truncate its target even when
capture fails, so use a separate output file until reviewed.

Capture refuses empty workspaces, floating/fullscreen content, scratchpad content,
unsupported or malformed trees, and ambiguous/missing focus history; it does not
silently guess a layout. Captured layouts are **drafts**, not session snapshots:
application state, cwd, environment, titles, and output placements are not recovered.

**Multiple workspaces:**

```sh
sway-stage profile capture work 1 3 music
sway-stage profile capture work --all
sway-stage profile capture work --all --no-infer-commands
```

Capture uses one read-only Sway tree for all targets and never selects workspaces.
Arguments select existing numeric or exact named workspaces in the specified order.
`--all` selects populated ordinary workspaces, numeric ascending then named lexical;
unsupported populated workspaces make the entire capture fail. Inactive workspace
focus must be recoverable from its history; otherwise capture refuses to guess.

For the first example it creates `profiles/work.yaml` and
`layouts/work-001.yaml`, `work-002.yaml`, `work-003.yaml` under the respective
configured directories; the profile refers to those layouts in order. Generated
profiles omit `output` expectations. Captured drafts with null placeholders will fail
`profile check` until edited. Capture names must fit the regular name grammar;
menu layout saves allow at most 250 characters and profile capture names at most 246.

**Command inference** is best-effort Linux process inspection, using Sway's captured
PID and bounded `/proc` reads. It can recognize verified Flatpak, GLib desktop-file,
and AppImage launches, or recover an ordinary argv. Snap is not supported. Process
identity is checked both **before and after** inference, but cannot eliminate all
races or recover original launch intent. Inferred arguments may expose secrets and
may not replay correctly; **review commands and referenced launcher files before
loading or sharing**. `--no-infer-commands` leaves all commands null. For provenance
and fallback rules see [internals](INTERNALS.md#command-inference).

**Saving:** profile capture and menu saves do not overwrite existing `.yaml` or
`.yml` names, including symlinks. Draft files are owner-only when newly created;
new directories are private. Complete files are staged and published exclusively.
Profile capture publishes layouts first, the profile last. This is **not an atomic
multi-file transaction**: filesystem failure may leave complete layout drafts or
staging files without a profile. Inspect reported paths; no destructive rollback is
attempted. Capture/save errors do not replace the previous load log.

## Fuzzel menu

`menu` requires Fuzzel 1.11+ on `PATH`. A single selector (`> `) offers
`Layout: NAME`, `Profile: NAME`, `Capture workspace layout…`, and, when eligible,
`Capture session profile…`. Invalid configurations are labeled `[invalid]` and
cannot load. Only listed selections are accepted; Escape cancels without side effects.

- On an **empty** focused workspace: offer layouts, profiles, and eligible session
  capture. On a **populated capturable** workspace: offer workspace capture,
  profiles, and eligible session capture. Unsupported capture state still permits
  profiles when available.
- The menu snapshots workspace state **before** its first prompt. Capture uses that
  original snapshot, even if focus changes during prompts. Saved drafts ask for
  `Layout name: ` or `Profile name: `; invalid names/collisions may be retried.
- Layout selection revalidates the file and original workspace identity/emptiness.
  With `cwd_select: true`, a `Directory: ` prompt offers the configured root or
  immediate non-hidden child directories; a window-level cwd still takes precedence.
  Profile selection revalidates all referenced layouts and runs a new preflight.
- `menu --no-infer-commands` affects capture only. Successful loads are silent;
  successful saves attempt a confirmation notification. Prompt cancellation and
  invalid selections preserve the existing load log.

## Diagnostics and troubleshooting

Loads, including profile and menu loads, write one diagnostics session to
`${XDG_STATE_HOME:-$HOME/.local/state}/sway-stage/last-run.log` (the state base
must be absolute). Newly created logs are `0600` in `0700` directories; existing
log symlinks are not followed. If writing the log fails, diagnostics fall back to
stderr. Logs record stages, observed IDs, IPC results, and sizing without full
argv or environment values by default. No daemon or telemetry is involved.

Most errors also attempt a concise `notify-send` notification. Notification failure
never masks the command's result. Successful loads do not notify; warnings about
sizing may notify. Capture, menu cancellation, and menu errors before loading
leave the existing load log untouched.

| Symptom | Check |
| --- | --- |
| Occupied workspace | Load from a keybinding on an empty workspace; the invoking terminal counts. |
| Window timeout / wrong location | Executable, exact app ID/class, Sway assignment rules, single-instance behavior. |
| Focus/output interruption | Keep the target focused on its original output; inspect partial windows before retrying. |
| Rejected draft | Replace `null` commands/selectors, review inferred args, then `check`. |
| Menu error | Fuzzel 1.11+, its stderr, available choices, and configuration. |
| Incorrect sizing | Gaps, borders, minimum sizes, and warning threshold. |

## Platform compatibility

**Sway 1.10 or newer is required** for `load`, `profile load`, `capture`,
`profile capture`, and `menu`. These commands query the connected compositor's
version through read-only native IPC. They refuse older versions, malformed version
responses, or a failed version query before launching applications, selecting
workspaces, or mutating layouts. The menu also refuses before opening Fuzzel.
Offline list/check/show commands remain usable without Sway. Sway 1.9 and earlier
lack IPC state information used by validation and safety checks.

There is no maximum version gate. Newer versions are accepted by the minimum check,
but acceptance does not establish compatibility with untested releases.

This Linux/glibc build uses glibc `_Fork` and `close_range` (glibc 2.34+).
Complete process-exit reporting also uses Linux `pidfd_open` (5.3+) and
`close_range` (5.9+); if unavailable, launch can continue but early process-exit
information may be lost. Other libcs are untested; a distributed binary can
require newer glibc symbols than the source-level minimum.

The original documentation records local offline/headless verification on
x86_64 Debian 13 (glibc 2.41, Linux 6.12, Sway 1.10.1, Foot 1.21.0).
The full headless CI suite has also passed against upstream Sway releases 1.10.0,
1.11.0, and 1.12.0 on Ubuntu 26.04 amd64. This covers native Wayland applications
and virtual one-/two-output scenarios; it does not verify Xwayland, physical
outputs, or visual rendering. Other systems and future releases remain unverified.
See [CI verification](DEVELOPMENT.md#ci) and
[release readiness](DEVELOPMENT.md#release-readiness).
