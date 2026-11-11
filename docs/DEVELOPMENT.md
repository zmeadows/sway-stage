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
| `--extended` | Append Firefox, Xterm/Xwayland, nested browser, repeated-app-ID, and desktop-launcher replay cases. |
| `--binary PATH` | Select another sway-stage build; default is this checkout's `target/release/sway-stage`. |
| `--firefox-app-id EXACT_ID` | Override the native ID inferred from adjacent `application.ini` metadata. |

The standard run covers eleven Foot scenarios: single window, horizontal/vertical pairs,
uneven siblings, mixed nesting, deep nesting (including same-axis groups), sibling nested
groups, process execution, default final focus, occupied-workspace refusal, and capture/reload
round-tripping. Each verifies raw-tree identity, order, hierarchy, sizing, final focus,
capture, and guarded cleanup.
Single-window, horizontal-pair, and extended duplicate-foot-ids cases also check inferred
commands. Expect eleven passes; extended runs expect sixteen when all optional prerequisites
exist, otherwise explicit SKIPs.
The process-execution case uses a private relative cwd and a bounded harness helper as
Foot's direct child. Linux `/proc` checks the child's exact literal arguments, cwd, and
run-specific environment override; verified window cleanup must also observe its exit.
No shell evaluates the test arguments.

The `capture-reload-roundtrip` case builds four uniquely identified Foot windows with nested
horizontal/vertical splits, unequal weights, and interior-leaf focus. After full capture
comparison, it fills only the actual captured YAML's null commands with the corresponding
literal launch arrays and runs `check`. The replay file retains captured weights, selectors,
IDs, hierarchy, and focus. Only successful exact-ID cleanup and an empty, preserved desktop
permit the second load. A fresh synchronized baseline and new launch bindings replace the
first cycle's evidence; retired container IDs are refused. The reconstructed layout is
independently checked against the captured weights with the ordinary sizing tolerance,
then captured and safely cleaned again. Failure reports the initial/replay phase and leaves
remaining windows alone; no speculative cleanup or retry follows.

The fifth extended case, `desktop-launcher-roundtrip`, creates one private `.desktop` file
inside the harness's temporary directory, with a path containing spaces. Its ordinary
`Type=Application` entry directly starts a Foot window with a run-unique app ID and a
300-second `sleep` child, using desktop-entry argument quoting without shell wrappers.
The initial layout launches it through `gio launch`. Ordered window events, independent
Sway tree/layout/focus checks, exact Foot process arguments, and independently read GLib
metadata must establish ownership: the recorded launcher PID must match Sway's window PID,
and the path must exactly identify the unchanged private fixture. Every ownership check,
including the checks immediately before cleanup, requires this evidence.

Ordinary capture must match the structural draft; inferred capture must differ only in
its command, exactly `[gio, launch, PRIVATE_DESKTOP_FILE_PATH]`. State, event-history, and
foreign-window checks must remain unchanged. After verified initial cleanup, an empty
workspace, and a fresh baseline, the harness checks and reloads the actual inferred YAML,
without replacing its command. It independently verifies the new window's Foot argv,
matching launcher PID/path, layout, and focus, captures again, and performs final guarded
cleanup. This intentionally uses a single window to keep launcher provenance focused;
the standard round-trip scenario covers nested weights. Any uncertainty stops the case
without speculative cleanup. The private launcher and replay YAML are removed with the
harness's temporary files; nothing is installed in XDG application directories.

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
- Desktop-launcher coverage needs `gio` on `PATH` and readable `/proc/PID/cmdline` and
  `/proc/PID/environ` for Foot. Missing `gio` explicitly skips this scenario. The GLib
  launcher must directly launch the observed Foot process with matching PID metadata;
  wrappers, delegated server processes, or missing/contradictory metadata fail conservatively.
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
evidence, and the desktop-launcher case requires both Foot argv and launcher metadata.
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

Keep that target focused until exit. `single-foot` opens one terminal; `two-foot` opens equal
columns with left focus; `four-windows` launches browser, middle terminal, upper-right,
then lower-right, with width ratios 44:32:24, right heights 77:23, and browser focus.
Expect exit 0 without stdout, proportions near 1 percentage point of target, and warnings
only beyond 5 points. Inspect the launch log on failure.

For capture, compare default and inferred drafts with the original hierarchy, selectors,
proportions, and focus. Default commands must be null. `check` must reject placeholders;
fill them, check, load on another empty target, and recapture. Expect at most small weight
rounding/sizing differences (aim within 1 percentage point). Confirm floating, fullscreen,
tabbed/stacked, and structural-container focus refuse capture without YAML. Capture must
leave desktop state and the launch log unchanged.

For Flatpak acceptance, manually open TIDAL Hi-Fi (or another installed Flatpak app) tiled
on a disposable workspace and keep it focused while running
`./target/release/sway-stage capture --infer-commands > /tmp/flatpak-draft.yaml`.
Expect separate command elements `flatpak`, `run`, optional `--branch=...`/`--arch=...`,
and `com.mastermindzh.tidal-hifi`; no internal `/app/...` executable. The match must remain
`app_id: com.mastermindzh.tidal-hifi`. Compare a capture without inference: commands must
remain null and arrangement/selectors must agree. Review the draft, fill any other nulls,
and run `check` using a temporary layouts directory. To test relaunch, manually load the
reviewed draft on another empty disposable workspace and verify the matching application
opens. This optional procedure requires Flatpak; standard offline checks and the independent
Foot harness require neither Flatpak nor TIDAL. The harness's Foot argv/ownership checks
remain specific to its test-owned launches and should not be generalized for this scenario.

For menu acceptance, use a keybinding or delayed invocation with a temporary layouts
directory. Check normal Load sizing/focus, occupied-target refusal, both Escape cancellations,
invalid/colliding names, saved draft discovery, placeholder rejection, and edited-draft
validation. Repeat captures with `--infer-commands`, reviewing recovered arguments. Captures
must preserve pre-menu hierarchy, weights, selectors, and focus even when pointer/window
focus or workspace changes during either prompt. Both prompts should use the original
output. Capture/cancellation must leave the previous launch log untouched; Load replaces it.
A valid Load selection after switching to another workspace (or recreating the same name)
must refuse without launching, mutating, or restoring focus. Cancellation must work even
when the initial snapshot fails; invalid files still receive ordinary validation errors.

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
