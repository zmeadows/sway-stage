# Architecture and invariants

This is a compact maintainer/agent reference for decisions not obvious from CLI help
or YAML examples. It is **not** an alternate user specification or an exhaustive
scenario catalogue. The observable contract belongs in [REFERENCE.md](REFERENCE.md),
mandatory engineering and live-desktop safety rules in [AGENTS.md](../AGENTS.md),
and verification commands in [DEVELOPMENT.md](DEVELOPMENT.md). When implementation
and documentation disagree, investigate; do not quietly change the contract.

## Execution boundaries

`sway-stage` is a short-lived synchronous CLI, not a daemon or session-restoration
service. The main paths are:

| Path | Important ownership |
| --- | --- |
| `src/main.rs`, `src/discovery.rs`, `src/config.rs` | Parse CLI and validate layouts without compositor access. |
| `src/profile.rs` | Validate/preflight all profile targets, select them sequentially, delegate each layout to the ordinary loader. |
| `src/launch.rs`, `src/observation.rs` | Snapshot and track target identity, launch/accept windows, build and verify the hierarchy, size/select focus. |
| `src/application_process.rs` | Launch outside inherited Sway `exec` activation ancestry. |
| `src/capture.rs`, `src/command_inference.rs` | Convert a single Sway tree into an editable draft, optionally inspect bounded process metadata. |
| `src/profile_capture.rs`, `src/draft_file.rs` | Render and publish multiple complete drafts without overwriting targets. |
| `src/menu.rs`, `src/diagnostics.rs`, `src/failure.rs` | Isolated prompts, pre-load validation, one load log, user-facing failures. |

The loader is intentionally conservative: the ability to *detect and refuse* an
unsafe state matters more than opportunistically repairing it.

## Observation, identity, and ownership

**Frozen target:** standalone loading identifies the focused workspace by observed
container identity (not just its textual name) and freezes its containing output.
Profiles own workspace selection but never relax the ordinary loader's independent
post-selection observation. An away-and-back focus change or target-output migration
can be detected from event history even when the latest tree again looks correct.
A missing history segment or contradictory observations is a refusal, not evidence
that nothing happened.

**Not all focus changes are equal:** workspace/output changes abort; focus changes
among the run's own windows inside the target may be restored by an explicit
container-ID focus after revalidating ownership and structure. Each request has a
bounded restoration count and cannot extend the launch deadline. A restoration
never repairs a moved window, changed tab/stack selection, or foreign activity.

**Window acceptance:** the observer takes a global baseline before a launch and
rechecks all window IDs before each subsequent launch. Only a new, unclaimed,
exactly matching `app_id`/Xwayland class can be accepted, and only if its observed
workspace, tiled state, identity, and hierarchy are consistent. A matching window
on another workspace, a second candidate, or foreign activity inside the target
is unsafe. Matching selectors identify *types*, not process ancestry; neither PID
nor a matching existing window proves creation by this run.

**Mutation scope:** every Sway command is checked for reported success and its
actual effect in a subsequent tree. Mutation criteria refer to specific accepted
container IDs; containers with foreign descendants are off limits. This applies
to sizing, structural wrapping, tab/stack selection, and final focus. Sway IPC
operations are not atomic, so even these checks do not prove complete isolation
from concurrent user actions. Failures leave partial results intact.

**IPC/event mechanics:** `src/observation.rs` combines events with fresh GET_TREE
snapshots and tick fences because a snapshot alone misses transient changes.
Its event reader blocks on complete IPC frames: resuming after a timeout in the
middle of a frame can corrupt framing. Some property changes require polling a
fresh tree rather than waiting for a specific event. Raw IPC command replies must
be checked for success; a high-level parsed result may not expose all failure
flags. These details belong near the corresponding code, not in the user guide.

## Launch context and process lifetime

When Sway starts `sway-stage` from `exec` or a keybinding, its activation token or
recorded process ancestry may place newly launched windows on the **invoking**
workspace rather than the current profile target. The application launcher therefore
removes `XDG_ACTIVATION_TOKEN` and `DESKTOP_STARTUP_ID`, and spawns through a short-lived
supervisor outside the original ancestry. User configuration cannot reintroduce
those reserved environment variables.

The supervisor reports the direct application's PID and, when available, its exit
status during the bounded window wait. It does not own application lifetime after
acceptance or timeout. Linux `pidfd_open` and `close_range` improve exit reporting;
missing kernel support must not silently change the safety policy. The post-fork
code has restricted async-signal-safe operations; see `src/application_process.rs`
for the exact boundary. Do not replace the supervisor with a generic shell wrapper
or a subreaper: doing so can restore the Sway ancestry placement problem.

A process exiting does not prove no window will appear (single-instance delegation
is common). Accordingly, an observed exit supplements diagnostics but does not
shorten the configured window deadline.

## Hierarchy, sizes, and selections

Group order is semantic. Depth-first creation preserves split nesting, tab/stack
header order, and intermediate containers, including a split nested inside another
split of the same axis and singleton native tab/stack groups. Flattening, inserting
unanticipated wrappers, or changing child ancestry is not an acceptable visual
approximation. Sway `workspace_layout`, assignment rules, and compositor versions
may produce structures that must be refused rather than repaired.

Split `weight` represents a **sibling-axis extent** share, not Sway's parent-area
`percent` field. Outer split sizing precedes inner splits; tabbed/stacked groups
have no sibling weights but may contain weighted splits. Bounded resize passes
converge where possible; a successful resize reply is not proof geometry changed.
Unverifiable geometry or >5 percentage-point residual error warns; structural,
ownership, and selection failures remain fatal.

Tab/stack `active` values select one-based *direct* children. Apply nested
selections, including inactive branches, then global focus; the final focused
window can override its ancestors' `active` choices. Verify all selections against
observed focus histories, not only the globally visible tab.

## Profiles and output expectations

A profile is a bounded ordered plan, not a transaction. Full profile/layout
validation and occupancy preflight happen before the first workspace selection.
The coordinator distinguishes Sway workspace **number** from an exact name with
`num = -1`, and never interpolates arbitrary user strings into commands. Exact
names use a narrow validated ASCII grammar, with reserved Sway command words
rejected. `output` is only an assertion: existing targets must be on it and a
missing target's output must exist; actual placement after creation is verified.

The coordinator intentionally selects workspaces; the individual loader never
does. Sway can destroy the previously visible empty workspace during a selection.
The coordinator tolerates only the specifically anticipated removal of the
previously visible empty workspace on the selected target's output. All other
unexpected transitions are errors. A later error stops subsequent entries, leaving
completed and partially launched windows untouched; one diagnostics session covers
the entire profile with progress information.

## Capture and publication

Capture reads one bounded Sway tree and never sends mutation IPC. It serializes
native groups, sibling proportions, active selections, and focus, but **not**
application state or all launch context. Nonfocused workspace capture uses recorded
focus histories; if it cannot identify the focused leaf reliably, it refuses rather
than choosing the first window. Single-child split wrappers are collapsed;
singleton tabbed/stacked groups are retained. Renderer and validator share the
same depth/node bounds so a draft can become a valid layout after placeholders
are filled.

A standalone capture writes a rendered draft to stdout. The *shell* controls
redirection and might truncate an existing destination before capture starts.
Menu saving and profile capture instead render before saving, stage private complete
files, and use exclusive publication. The profile is published **after** all layouts;
if any layout fails, no new profile is published. This is not cross-file atomicity:
other complete layouts or staging files may remain, and no destructive rollback
occurs. Source: `src/draft_file.rs`, `src/profile_capture.rs`.

The menu snapshots workspace state before opening Fuzzel. Capture deliberately
uses that frozen snapshot across name-prompt retries; a load rediscovering a file
must revalidate both the file and relevant workspace state after Fuzzel closes.
Profile loads use an entirely new preflight; the menu snapshot is not their baseline.

## Command inference

`src/command_inference.rs` reconstructs **host launch commands**, never a faithful
process tree or original CLI invocation. The captured Sway PID may already be stale.
Before **and after** inference it compares `/proc/PID/stat` start time and executable
path/device/inode. An observation change or missing metadata yields `command: null`.
This second check is implemented; an earlier specification paragraph incorrectly
claimed it was absent. The two checks are still not atomic and cannot detect all
same-executable execs or changes made and reversed between reads.

Inference is conservative and ordered:

1. Recognize a supported Flatpak from bounded `.flatpak-info` inside the process
   root; reconstruct `flatpak run` with valid app ID and reference branch/arch.
   Malformed/unusable Flatpak evidence does not license falling back to sandbox argv.
2. Otherwise, inspect bounded initial `/proc/PID/environ`. A verified exact-PID GLib
   desktop-file launcher yields `gio launch /absolute/path.desktop`; unresolved
   matching-PID GLib evidence blocks fallback. Inherited GLib metadata does not
   establish the current process's launcher.
3. Any Snap environment marker blocks ordinary argv/AppImage fallback: Snap is
   unsupported unless independently verified GLib launcher information applies.
4. Verified AppImage environment and executable membership can reconstruct the host
   image path. Inconsistent evidence (including unsupported `ARGV0` behavior) leaves
   a null command; it does not fall back to an internal executable's argv.
5. With no contrary launcher evidence, read bounded, well-formed UTF-8 cmdline argv.
   No app-title, PID-ancestor, or executable-name guessing is used.

Process environment and launch arguments may include private paths or secrets; do
not log full commands or environment values. Generated draft arguments remain
untrusted until inspected. `--no-infer-commands` avoids process inference.

**Version-sensitive GLib note:** the original documentation observed that GLib
2.84.4's `gio launch` loses desktop-file provenance on replay, while a later
implementation preserves it. This is a version-specific observation, not a generic
GLib guarantee. If repeated GLib launcher capture matters, check the installed
version and source behavior before revising expectations.

## Verification boundaries

The private harness intentionally has its own Sway tree parser/scenario model
independent of production's observer, reducing correlated defects. Scenario details
and precise assertions live in `tools/integration/`, not this document. The offline
harness test target does not start the compositor. Agents may use the isolated
launcher only as prescribed in `AGENTS.md`, never live-desktop diagnostic tools.

Keep safety invariants in agent instructions, concrete algorithms/comments near
source, user-facing semantics in the reference, and actual verification steps in
the development guide. This prevents four competing versions of the same rule.
