# Development and manual verification

Read [AGENTS.md](../AGENTS.md) for engineering, dependency/MSRV, and ownership requirements,
and the [specification](SPECIFICATION.md) for behavior. The [roadmap](PLAN.md) tracks only
outstanding release work.

## Offline verification

Use Rust 1.89 or newer; retain the locked dependency graph and verify the MSRV:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo check --locked --all-targets
cargo test --locked
cargo build --release --locked
cargo +1.89.0 check --locked --all-targets
```

Automated tests use recorded trees/events, socket fixtures, temporary files, and fake
external tools. They do not use the live compositor. Some CLI fixtures bind temporary Unix
sockets, which restrictive sandboxes may forbid. The separate offline harness target is
`cargo test --locked --test integration_harness`; it has no harness entry point and must
never read `SWAYSOCK`, connect to Sway, or start processes.

## Live testing: user only

**AI agents must never execute the live integration harness, even with `--help`, partial
runs, or trial runs.** Agents may compile it, inspect it, and run its offline tests only.
Agents must never issue commands that mutate the user's live Sway desktop, including
loads, application launches/termination, focus changes, or window moves/resizes.
All live commands below are exclusively for the user's manual execution.

Use disposable workspaces in the existing Sway session, with `SWAYSOCK` pointing to it.
A disposable workspace is **not compositor isolation**: rules, outputs, focus, and events
are shared with the rest of the session. Leave input idle during harness runs; do not rely
on guards for transactional protection. No nested compositor, VNC, or virtual monitor is
required. Only one Sway seat is supported by the harness.

## Integration harness

Prerequisites: `swaymsg`, Foot, and `sleep` on `PATH`, plus the two release executables.
From the repository root, build (agents may perform these build steps):

```sh
cargo build --release --locked
cargo build --release --locked --example integration
```

Before running, start on an existing workspace other than either disposable workspace.
Choose two distinct explicit positive decimal workspace numbers (at most nine digits each),
using `--workspace` for the layout target and `--foreign-workspace` for the foreign fixture.
Both must be absent or completely empty, including floating windows and empty containers.
A number belonging to a differently named workspace is refused. The starting workspace name must begin with an ASCII letter/digit
and contain only ASCII letters/digits, spaces, or `_ . : + -`, allowing safe restoration.
Preflight is repeated immediately before switching; the harness never selects an alternate
target or fixture. Never designate a workspace containing user windows as the fixture.

Run manually, one invocation at a time:

```sh
./target/release/examples/integration --workspace 9 --foreign-workspace 8
./target/release/examples/integration --workspace 9 --foreign-workspace 8 --extended
```

| Option | Meaning |
| --- | --- |
| `--workspace NUMBER` | Required disposable target; the harness explicitly switches to it. |
| `--foreign-workspace NUMBER` | Required second disposable workspace, distinct from the target and starting workspace; holds one verified Foot fixture. |
| `--extended` | Append Firefox, Xterm/Xwayland, nested browser, repeated-app-ID, desktop-launcher replay, unresolved-command, and two optional package-inference cases. |
| `--appimage PATH` | Enable real AppImage inference with an existing executable; requires `--extended`. |
| `--flatpak APP_ID` | Enable real Flatpak inference with an installed application ID; requires `--extended`. |
| `--binary PATH` | Select another sway-stage build; default is this checkout's `target/release/sway-stage`. |
| `--firefox-app-id EXACT_ID` | Override the native ID inferred from adjacent `application.ini` metadata. |

The standard run covers nineteen Foot scenarios. The original eleven cover: single window, horizontal/vertical pairs,
uneven siblings, mixed nesting, deep nesting (including same-axis groups), sibling nested
groups, process execution, default final focus, occupied-workspace refusal, and capture/reload
round-tripping. Each verifies raw-tree identity, order, hierarchy, sizing, final focus,
capture, and guarded cleanup. The eight native-group additions are:

| Scenario | Independent expectations |
| --- | --- |
| `root-tabbed` | Three tabs, omitted global focus, first child selected. |
| `root-stacked` | Three stacked windows, explicit focus on the second. |
| `singleton-groups` | Singleton tabbed group inside a stacked root; both select their first child. |
| `mixed-native-nesting` | Split containing tabs, nested vertical split, and a later sibling outside the tabs. |
| `independent-native-groups` | Differently nested tab/stack groups, explicit focus overriding its ancestors while unrelated inactive groups retain first-child selection. |
| `mixed-capture-reload` | Vertical split containing tabs, a singleton stack, and a weighted horizontal split; capture and replay with interior focus. |
| `initially-tabbed` | Load `mixed-native-nesting` after establishing an empty tabbed workspace. |
| `initially-stacked` | The same mixed hierarchy after establishing an empty stacked workspace. |

The final two standard cases recheck the selected target's identity, focus, output, and
emptiness, apply `layout tabbed` or `layout stacking`, check the reply, then require a fresh
fenced tree to report `tabbed` or `stacked` while still empty. Unexpected workspace/window
activity fails preparation without a load or restoration; observation history is retained.
Both reuse ordinary hierarchy, sizing, selection, capture, preservation, and success-only
cleanup checks. Their requested split root leaves an ordinary split workspace after success;
no layout reset or placeholder windows are used. Sway's
[`layout` implementation](https://github.com/swaywm/sway/blob/master/sway/commands/layout.c)
updates the empty workspace directly; installed-version behavior still needs live acceptance.

Single-window, horizontal-pair, and extended duplicate-foot-ids cases also check inferred
commands. Expect nineteen passes for the standard suite. Extended runs add eight cases,
for twenty-seven total, plus the separate foreign fixture. With the original optional prerequisites
available, an extended run without package flags reports twenty-five passes and two package
SKIPs; supplying one package enables twenty-six possible passes, and both enable twenty-seven.
Other missing optional prerequisites still produce explicit SKIPs.
The process-execution case uses a private absolute cwd and a bounded harness helper as
Foot's direct child. Linux `/proc` checks the child's exact literal arguments, cwd, and
run-specific environment override; verified window cleanup must also observe its exit.
No shell evaluates the test arguments.

The `capture-reload-roundtrip` and `mixed-capture-reload` cases each build four uniquely
identified Foot windows with unequal split weights and interior-leaf focus. The former
uses nested horizontal/vertical splits; the latter includes tabs and a singleton stacked
group. After full capture comparison, each fills only the actual captured YAML's null
commands with the corresponding literal launch arrays and runs `check`. Only children of
splits have captured weights, including tab/stack groups beneath splits; header children
omit them. The replay file retains captured weights, selectors, IDs, hierarchy, and focus.
Only successful exact-ID cleanup and an empty, preserved desktop permit the second load. A
fresh synchronized baseline and new launch bindings replace the first cycle's evidence;
retired container IDs are refused. The reconstructed layout is independently checked
against the captured weights with the ordinary sizing tolerance, then captured and safely
cleaned again. The standard suite has two replay scenarios; the extended suite adds the
existing desktop-launcher replay for three total. Replays are additional construction
cycles within those passes, not additional scenario counts. Failure reports the
initial/replay phase and leaves remaining windows alone; no speculative cleanup or retry
follows.

Native selection checks use the first entry of each tabbed/stacked container's `focus`
array, separately from the window's global `focused` flag. The
[Sway IPC contract](https://github.com/swaywm/sway/blob/master/sway/sway-ipc.7.scd)
defines child focus order; [IPC serialization](https://github.com/swaywm/sway/blob/master/sway/ipc-json.c)
filters the default seat's focus stack to direct children. The same ordering drives
[`seat_get_active_tiling_child`](https://github.com/swaywm/sway/blob/master/sway/input/seat.c)
and the [rendered inactive selection](https://github.com/swaywm/sway/blob/master/sway/desktop/transaction.c).
This supports checking inactive and nested groups without focusing them. The harness requires
that history and checks every header group, including singletons. Capture preserves the
history in the desktop, but its YAML records only global focus; replay checks first-child
defaults again. Missing or contrary history fails; no rectangle/visibility approximation is used.
Only one seat is supported. These upstream source semantics do not establish compatibility
with an installed release; record `sway --version` with manual results.

Cleanup predicts the [view-unmap path](https://github.com/swaywm/sway/blob/master/sway/tree/view.c)
and [`container_reap_empty`](https://github.com/swaywm/sway/blob/master/sway/tree/container.c):
empty ancestors disappear, surviving tabbed/stacked groups retain their IDs/layouts and may
have one child. Only redundant single-child split wrappers normalize away. This is distinct
from Sway's explicit container-flattening operation. After each individually verified window
close, exact surviving header nesting/identity must agree before any next close. Unexpected
flattening, retained empty groups, reparenting, or foreign-window changes stop cleanup and
leave the remaining windows. Offline fixtures establish the verifier's reaction, not actual
Sway transitions. Singleton construction and cleanup still need live acceptance.

For native-group manual acceptance:

1. Build both release executables with the commands above and record `sway --version`.
2. From a third workspace, run the standard command above with two absent/empty disposable
   workspace numbers. Leave input idle until it finishes; expect nineteen passes, including
   both capture/reload cycles. Preserve output on failure and inspect reported remaining
   windows manually before another run. Do not rerun into a partially populated target.
3. On a separate disposable workspace, visually check tab/stack header order and singleton
   headers with a trusted local native-group layout. Confirm default first-child selection,
   explicit global focus along nested ancestors, and first selections in unrelated inactive
   groups. Capture from a delayed command on another workspace and compare the draft; fill
   only its null commands, check it, then load on another empty target. Do not interact with
   windows during the harness itself: it deliberately treats interference as failure.
4. Run `--extended` separately for existing optional coverage. Repeat the interference/interrupt
   procedures below as separate runs; a standard pass alone does not establish those guards live.

No native-group live run has been performed as part of Stage 5 implementation.

The fifth extended case, `desktop-launcher-roundtrip`, creates a uniquely named `.desktop`
file under a private `XDG_DATA_HOME/applications` inside the harness's temporary directory,
with a path containing spaces. Its ordinary `Type=Application` entry directly starts a
Foot window with a run-unique app ID and a 300-second `sleep` child, using desktop-entry
argument quoting without shell wrappers.
The initial layout launches it through `gtk-launch`, with the private `XDG_DATA_HOME`
supplied only to that fixture launcher. Ordered window events, independent Sway
tree/layout/focus checks, exact Foot process arguments, and independently read GLib
metadata must establish ownership: the recorded launcher PID must match Sway's window PID,
and the path must exactly identify the unchanged private fixture. Every initial-cycle
ownership check, including the checks immediately before cleanup, requires this evidence.

Ordinary capture must match the structural draft; inferred capture must differ only in
its command, exactly `[gio, launch, PRIVATE_DESKTOP_FILE_PATH]`. State, event-history, and
foreign-window checks must remain unchanged. After verified initial cleanup, an empty
workspace, and a fresh baseline, the harness checks and reloads the actual inferred YAML,
without replacing its command or supplying private XDG overrides. Replay ownership requires
fresh ordered window events, exact Foot argv, the unchanged fixture, and the same Sway
identity, placement, layout, focus, and preservation checks. It does not claim launcher
provenance: GLib 2.84.4's `gio launch` supplies the PID but loses the filename metadata.
The second inferred capture must therefore either leave `command: null` (the conservative
production behavior for PID-only metadata) or reconstruct the exact same `gio launch`
array on newer GLib. Ordinary capture and final guarded cleanup still run.
This intentionally uses a single window to keep launcher provenance focused;
the standard round-trip scenario covers nested weights. Any uncertainty stops the case
without speculative cleanup. The private launcher and replay YAML are removed with the
harness's temporary files; global desktop configuration is untouched.

The sixth extended case, `unresolved-launcher`, uses the same private GTK/Foot launcher
infrastructure and prerequisites, with its own unique fixture. Exact Foot argv, matching
GLib path/PID provenance, ordered window events, layout, focus, and foreign-window checks
first establish ownership. The harness then renames only its private `.desktop` file within
its private directory for one bounded `capture --infer-commands` invocation. It restores the
same file and independently checks its inode and contents before any subsequent ownership
check or cleanup, including when capture fails or interruption is requested. A retained
`/proc` directory descriptor and exact process evidence before and after establish that the
original Foot process remained present across capture without following a reused PID.

The capture must exit zero, retain the default draft's selectors, IDs, hierarchy, sizing,
and focus, and leave its single `command: null`. Stdout is checked independently of stderr,
which must report `Commands inferred: 0; unresolved: 1. Manual editing required.` No
speculative launcher or executable may substitute for the null. Fresh snapshots and event
history must show unchanged desktop state and foreign windows, with no additional window
launch. Only complete success permits normal guarded cleanup. Any scenario failure stops
the suite without retries, window cleanup, or workspace restoration. If the private file
cannot be restored and verified, its directory is retained and reported for manual inspection;
all windows remain untouched. This case adds no standard-suite dependencies and requires
no application packages.

The seventh and eighth extended cases, `appimage-inference` and `flatpak-inference`, are
optional real-package capture checks. Supply either flag independently, or both; the other
package case prints `SKIP ... --appimage not supplied` or `SKIP ... --flatpak not supplied`.
Both flags require `--extended`; Clap rejects either without it before contacting Sway.
No package downloads, installation, desktop entries, or app-ID/class overrides are involved.

After building, run these manually from a third workspace, one invocation at a time. Replace
the image path and Flatpak ID with existing local applications:

```sh
# AppImage only
./target/release/examples/integration --workspace 9 --foreign-workspace 8 --extended \
    --appimage "$HOME/Applications/LiteXL.AppImage"

# Flatpak only
./target/release/examples/integration --workspace 9 --foreign-workspace 8 --extended \
    --flatpak org.gnome.Calculator

# Both
./target/release/examples/integration --workspace 9 --foreign-workspace 8 --extended \
    --appimage "$HOME/Applications/LiteXL.AppImage" --flatpak org.gnome.Calculator
```

Prefer simple GUI applications that independently create exactly one tiled, focused window.
Close existing instances manually before the run. These cases use the application's ordinary
settings and may change its own saved state; they do not create private application profiles.
The package must start without prompts, splash windows, floating/fullscreen rules, or another
window. Discovery waits at most sixty seconds; capture and IPC operations have their own
bounded deadlines. Linux `/proc` must expose the process, executable, parent chain, and the
package-specific evidence below. Wrappers, singleton/delegated launches, reparenting outside
the direct launch ancestry, inaccessible metadata, or ambiguous identities fail conservatively.
The window PID need not equal the launcher PID, but a live, stable ancestry link is required.
Do not weaken these requirements to accommodate an arbitrary application.

Each case records every preexisting window/container ID on a freshly verified empty target,
then directly launches the package without a declarative loader or guessed selector. Global
creation events and the tree must agree on exactly one new safe window. Independent process
and package provenance must then establish ownership; only afterward does the harness obtain
its actual native Sway app ID or Xwayland class. A title, package name, selector, or creation
event alone never authorizes cleanup. Retained `/proc` descriptors, start times, executable
paths/inodes, fresh ancestry, and package evidence are rechecked across capture and immediately
before the exact-container close. Any additional window or unexpected workspace activity fails.

AppImage prerequisites are an existing executable type 1 or type 2 image with its runtime
header and a readable mounted image filesystem (FUSE, SquashFS, or ISO9660). Relative paths
and symlinks resolve to an absolute canonical path, and that resolved path is invoked directly.
This deliberately avoids selecting a different bundled program through an invocation alias:
[AppImage documents](https://docs.appimage.org/packaging-guide/environment-variables.html)
`APPIMAGE` as the resolved image path and `ARGV0` as the original invocation name. The observed
`APPIMAGE` must identify the unchanged configured host file; `APPDIR` must identify an actual
image mount containing the window process's executable on the same filesystem. `ARGV0`, when
present, must agree with the direct invocation. Environment fields alone cannot prove ownership.
Extract-and-run images, unusual wrappers, and executables outside the mounted image are unsupported.
The expected inferred command is exactly one element: the resolved host AppImage path.
No environment metadata is fabricated to obtain this result.

Flatpak prerequisites are `flatpak` on `PATH`, an installed application, and readable sandbox
metadata and mount namespaces. The harness checks `flatpak info --show-ref APP_ID`, then launches
exactly `flatpak run APP_ID`, without a desktop file. It independently queries
[`flatpak ps`](https://docs.flatpak.org/en/latest/flatpak-command-reference.html#flatpak-ps)
for instance ID, application, architecture, branch, wrapper PID, and sandbox PID. Those values
must agree with `/proc/WINDOW_PID/root/.flatpak-info` and the configured application. The
sandbox PID must descend from the launch and share the window process's separate mount
namespace and metadata; the executable must resolve inside that sandbox. Expected argv comes
from the running instance table: `flatpak`, `run`, `--branch=ACTUAL_BRANCH`,
`--arch=ACTUAL_ARCH`, `APP_ID`. No branch or architecture is hard-coded. Flatpak versions or
launch mechanisms that obscure this evidence are unsupported.

Both cases verify ordinary capture against the observed single-window model, then require
inferred capture to differ only in the exact recovered command. The inferred YAML must pass
`sway-stage check`. Fresh trees and continuous event history verify hierarchy, geometry,
focus, output identity, and foreign-window preservation before/after capture, with no additional
window launch. These cases do not reload the application; existing round-trip cases cover replay.
A pass prints `PASS appimage-inference` or `PASS flatpak-inference` with provenance/capture
coverage. A configured missing/unusable application, unresolved or incorrect inference, or
uncertain provenance prints `FAIL` with the stage and cause and stops the suite; it never
becomes a SKIP. Failure retains windows and forbids further launches, closes, and restoration.
Diagnostics report launch PID and candidate container IDs without claiming ownership.

Only complete success enters the existing guarded cleanup, augmented with fresh package
provenance. It requests a close of the exact verified window and observes disappearance;
a close prompt or late extra window fails the run. The harness never kills the package,
its descendants, or a package name. A package may stay running after its window closes;
there is no automatic application lifetime limit. Inspect retained applications/windows
manually before another run. As with all harness cases, snapshots and fences are not
transactional isolation against concurrent desktop activity.

For `duplicate-foot-ids`, inferred commands must contain both the shared app-ID argument and
the correct distinct run-specific title argument from each leaf's original launch array.
Swapped, duplicate, or missing titles fail, even though both selectors agree. Titles reported
by Sway are never ownership evidence. Normalizing only the inferred command arrays must
reproduce the verified default capture exactly; tree, event-history, and foreign-window
checks also establish that inference left the observed desktop unchanged. Recovered commands
are never printed in diagnostics.

The occupied-workspace case first loads and independently verifies a two-window layout,
then attempts a different valid layout on that occupied target. It requires exit 1 and the
specific occupied-workspace prelaunch diagnostic. Synchronized trees must preserve every
modeled container ID, identity, wrapper, ordering, geometry, and focus across the refusal;
unexpected window or non-urgent workspace events fail it. A timeout, interruption, or
unrelated error never counts as an expected refusal. The suite reports this verification
separately from successful layouts and optional SKIPs. Only this expected-refusal invocation
uses a private no-op `notify-send` stub on its child `PATH`, suppressing the intentional
error notification while preserving stderr and exit-status checks. Other invocations retain
normal notifications and the harness's environment is unchanged.

Before the primary target is selected, the harness switches to the separately preflighted
fixture workspace and loads a single Foot window with a run-unique app ID. Ordered new-window
events and independent tree/layout checks establish ownership. It then makes an expected,
fenced switch to the primary target. The fixture stays present throughout the suite; its
terminal expires after 3600 seconds if left behind. It is an additional setup load, not an
extra counted layout scenario. Its identity and location are printed on creation and failure.
On complete scenario success, the harness verifies the fixture again, performs an expected
switch back, and applies the ordinary exact-ID, freshly verified cleanup procedure before
attempting original-workspace restoration. Every switch retains continuous focus history;
there is no switch back or fixture cleanup after uncertain failures.

Immediately before each load, the harness records every foreign window, including scratchpad
windows. After load and capture, before each close, during close observation, and after cleanup,
it compares window identities, workspace/output ancestry, parent containers, sibling ordering
inside workspaces, geometry, and floating/fullscreen/scratchpad state. The fixture also retains
its original creation evidence across all scenarios. Titles, urgency, marks, and incidental
focus metadata are excluded from preservation comparisons; focus and global structural
activity retain their separate strict event checks. Any preservation uncertainty fails the
run and stops further closes, launches, and restoration. A preservation baseline alone never
authorizes closing a preexisting window.

Snapshots and tick fences do not provide transactional isolation. Relevant unexpected events
fail even if a later tree returns to baseline. Harmless title/urgency/mark events are allowed.
Changes without corresponding observable events, briefly made and restored between snapshots,
cannot be ruled out; a pass establishes unchanged observed state plus accepted event history.

Sizing above 1 percentage point is diagnostic; above 5 fails a harness scenario.

Extended prerequisites:

- Native `firefox-esr` (preferred) or `firefox` on `PATH`, a valid `WAYLAND_DISPLAY`, and
  readable `/proc/PID/cmdline` for Firefox and repeated-ID Foot ownership evidence.
  Firefox must preserve literal launch arguments; wrappers/sandboxed builds obscuring them
  fail conservatively. Use `--firefox-app-id` only after verifying your installation's ID.
- Xterm coverage needs `xterm`, `Xwayland`, and `xdpyinfo` on `PATH`, and `DISPLAY` reaching
  this session's Xwayland server with the `XWAYLAND` extension. An unavailable server/probe
  skips the case; a subsequent launch or class mismatch fails it.
- Desktop-launcher coverage needs `gtk-launch` and `gio` on `PATH` and readable
  `/proc/PID/cmdline` and `/proc/PID/environ` for Foot. Missing either launcher explicitly
  skips both optional launcher scenarios. The initial GTK launch must directly start the
  observed Foot process with matching PID and filename metadata; wrappers, delegated server
  processes, or missing/contradictory initial metadata fail conservatively. Replay uses
  independent Foot argv and window-event evidence because older gio loses the filename.
  No full desktop environment is required. Fixture paths containing `%` or control
  characters are refused because the fixture deliberately avoids desktop field codes.

Firefox uses private profiles/HOME/XDG directories and no normal profile. Browser directories
are retained and reported on success and failure: remove them manually only after all their
processes exit. Firefox has no automatic lifetime limit. Foot and Xterm commands normally
expire within five minutes of launch. Missing optional subsystems skip; failures with
available applications stop the suite.

Cleanup is an explicit success phase, never a failure handler. Each exact-ID close requires
fresh independent ownership, event-history, state, and hierarchy evidence. Selectors alone
are insufficient; Firefox and repeated-ID Foot additionally require literal process-argument
evidence. The desktop-launcher case always requires Foot argv and an unchanged fixture;
the initial GTK launch additionally requires exact launcher PID/path metadata.
Failure stops subsequent scenarios and closes, leaving remaining windows alone.
Read reported container IDs, selectors, PIDs, and retained paths before manual intervention;
diagnostic candidates do not imply ownership. Rules, gaps, minimum sizes, desktop activity,
or version differences can explain conservative failures.

Restoration is separate from cleanup. It requires trusted focus history, a recorded disposable
workspace still focused (the primary target or fixture workspace), and the original workspace's
exact identity/name still present. Otherwise it is skipped; a destroyed original workspace is
not recreated. An ordinary check failure may permit guarded restoration; uncertain ownership/control or interruption does not.

After harness safety changes, manually test separate runs interrupted during load, capture,
and cleanup with Ctrl-C or SIGINT to the harness PID only. Expect failure, remaining-window
and retained-profile diagnostics, no subsequent closes/scenarios, and no restoration once
interruption is observed. An already dispatched close may complete. The harness reaps only
its direct children by PID; it must not signal application descendants or process groups.
Also test away-and-back workspace focus, a foreign window, and an owned-window state change:
each must stop conservatively. These exercise harness guards, not the loader's abort behavior.

## Manual application acceptance

Use fresh disposable targets and run checks separately. Build first and validate the examples
with `sway-stage --layouts-dir examples check NAME`. Foot is required; adapt Firefox's
executable/ID locally to your installation for `four-windows`. From a terminal on another
workspace, allow time to focus an empty target:

```sh
sleep 5; ./target/release/sway-stage --layouts-dir examples load four-windows
```

Keep that target focused until exit. `two-foot` opens equal
columns with left focus; `four-windows` launches browser, middle terminal, upper-right,
then lower-right, with width ratios 44:32:24, right heights 77:23, and browser focus.
Expect exit 0 without stdout, proportions near 1 percentage point of target, and warnings
only beyond 5 points. Inspect the launch log on failure.

For capture, compare default and inferred drafts with the original hierarchy, selectors,
proportions, and focus. Default commands must be null. `check` must reject placeholders;
fill them, check, load on another empty target, and recapture. Expect at most small weight
rounding/sizing differences (aim within 1 percentage point). Confirm floating, fullscreen,
and structural-container focus refuse capture without YAML. Native tabbed/stacked groups,
including singletons, must capture successfully with weights only beneath splits. Capture must
leave desktop state and the launch log unchanged.

For optional real AppImage and Flatpak command inference, use the package harness cases
above instead of repeating manual capture acceptance. General edited-draft replay remains
covered by the existing round-trip scenarios and can also be checked manually if desired.

For menu acceptance, use a keybinding or delayed invocation with a temporary layouts
directory. On an empty disposable workspace, confirm sorted layout names without prefixes
appear under `Load layout: `; with no layouts, expect an error and no prompt. On a tiled,
capturable workspace, confirm `Capture layout as: ` opens directly as the only prompt,
including with no layouts or duplicate discovery names. With floating or fullscreen windows,
empty structural containers, or parent focus, expect capture guidance and no prompt. Check normal Load sizing/focus, occupied-target refusal, Escape at each prompt,
invalid/colliding names, saved draft discovery, placeholder rejection, and edited-draft
validation. Repeat captures with `--infer-commands`, reviewing recovered arguments. Captures
must preserve pre-menu hierarchy, weights, selectors, and focus even when pointer/window
focus or workspace changes during the name prompt. The prompt should use the original
output. Capture/cancellation must leave the previous launch log untouched; Load replaces it.
A valid Load selection after switching to another workspace (or recreating the same name)
must refuse without launching, mutating, or restoring focus. An initial snapshot failure
must report an error without opening Fuzzel or replacing the launch log. On an eligible empty
workspace, selecting an invalid file must still receive ordinary validation errors.

For project-directory selection, use a trusted test layout with `cwd: ~/Projects` and
`cwd_select: true`, with at least two non-hidden child directories. On an empty disposable
workspace, run `menu`, select the layout, and confirm the sorted `Launch in: ` basenames.
Choose a project and inspect the terminal's `pwd`; repeat with a window-level `cwd` override.
Check Escape at each prompt preserves the previous launch log. In separate runs, remove a
listed directory while its prompt is open, type an unoffered name, and switch workspaces
before selecting: each must refuse without launches or focus restoration. An empty root
must report an error. Direct `load NAME` must use the root without prompting.

For loader interference, separately switch away and back during a load, introduce an
unexpected window, and change an accepted window's state. Expect no further launches or
layout mutations, with partial results left in place. Verify real keybinding launch context
as well as delayed-terminal invocation; harness interference checks do not substitute.

When two outputs are available, check menu prompts stay on original output A while pointer
and workspace focus move to B; saved captures must still describe A's pre-menu state.
During separate loads, move the focused target workspace to another output, then test an
away-and-back move: both must abort without recovery. Moving only an unrelated workspace
or changing output geometry while the target retains its original output should not alone
abort. Output removal should report resulting Fuzzel errors without focus repair. These
hardware checks remain incomplete; their safety logic has offline coverage and does not
block continued development. Multi-seat behavior and a supported Sway version remain
unverified; do not infer compatibility from one run.

## Maintainer notes

The harness intentionally verifies through its own `swaymsg` JSON types and scenario model,
independent of the production parser/IPC/observer. Capture comparison is deliberately narrow;
update its expectations explicitly if serialization changes. Do not weaken ownership checks
to make a scenario pass.

The production observer combines event history with fresh trees because IPC is not atomic.
Keep its blocking event reader: retrying a timed-out partial swayipc frame can corrupt
framing. Property changes may require tree polling without events. The guarded command
reader checks raw reply success because swayipc-types can discard that flag. Sizes use
sibling axis extents, not Sway's parent-area `percent`; successful resize replies do not
prove geometry changed. Detailed mechanics belong in source and comments.

Development-only diagnostics are Cargo examples: `inspect_sway` prints a focused-workspace
snapshot; `observe_sway` observes for ten seconds and sends visible tick fences. Neither is
a user-facing layout command. All desktop verification remains user-run.
