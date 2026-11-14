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
external tools. Profile CLI transcripts additionally cover whole-profile preflight,
retained configurations after file removal, sequential selection/creation and construction,
command failures/timeouts, conflicting identities/focus/history/occupancy, partial failure,
completed-workspace diagnostics, and single fatal notifications. Exact command transcripts
refuse retries, later selections, and rollback commands. They do not use the live compositor.
Profile-capture fixtures cover one read-only GET_TREE, numeric and exact named selection,
deterministic `--all` ordering, independent inactive focus, unresolved drafts, directory
permissions, collisions, exclusive publication, residual-file diagnostics, and launch-log
preservation. Offline publication tests create concurrent destination conflicts after staging
and require complete layouts with no newly published profile after failure.
Some CLI fixtures bind temporary Unix sockets, which restrictive sandboxes may forbid. The separate offline harness target is
`cargo test --locked --test integration_harness`; it has no harness entry point and must
never read `SWAYSOCK`, connect to Sway, or start processes.

## Headless integration harness

`tools/integration/run.sh` runs the integration suite in a private headless Sway that exists
only for that run. Contributors and AI agents may run it; agents follow the restrictions in
[AGENTS.md](../AGENTS.md). It never touches your own desktop. The user-only checks that do
are under [manual application acceptance](#manual-application-acceptance) and
[manual profile acceptance](#manual-profile-acceptance).

Prerequisites: `sway`, `swaymsg`, Foot, `sleep`, and `setpriv` (util-linux) on `PATH`, plus
`/bin/sh` and a Linux `/proc` that exposes process arguments. Foot is mandatory: it is the
only application the harness launches, and it cannot be replaced or configured. From the
repository root:

```sh
tools/integration/run.sh                  # all 25 scenarios
tools/integration/run.sh --filter TEXT    # only scenarios whose names contain TEXT
```

`run.sh` builds `sway-stage` and the harness in release mode, then runs the selected
scenarios. `--filter` is a case-sensitive substring match over every scenario name and keeps
suite order: `--filter single-window` runs one scenario and `--filter profile` the four
profile scenarios. Any other argument, a missing or empty TEXT, or a filter that matches no
scenario exits with status 2 before anything starts.

Each run starts and stops its own Sway on the wlroots headless backend, with one 2560×1440
`HEADLESS-1` output and no Xwayland, bar, or input devices. Its sockets and private home,
XDG, and temporary directories live in a new owner-only session directory under `$TMPDIR`
(default `/tmp`), and everything it runs receives an environment built from nothing but
`PATH`. It never connects to your session's compositor, notification daemon, or launch log,
so it is safe to start from a terminal inside your Sway session.

A passing run prints a ready line, one `PASS` line per selected scenario, `N of N selected
scenarios passed.`, and `Headless Sway stopped; session files removed.` It exits 0 only
when every selected scenario passed and the compositor stopped cleanly. Foot windows end
with the compositor; they are never signalled.

The first failure prints `FAIL  STAGE: reason`, runs no further scenarios, closes no
windows, stops the compositor, and exits 1. The session directory is kept and its path
printed. Inspect `sway.log`; `final-tree.json`, the raw tree recorded just before shutdown
(absent after Ctrl-C); `tmp/sway-stage-integration-*/` for the generated layouts, profiles,
and captures; and `home/.local/state/sway-stage/last-run.log` for the last sway-stage load.

Ctrl-C fails the run at the runner's next check in the same way. The harness signals only
its own direct children by PID, never process groups or applications, and if the launcher
itself is killed, the kernel kills the compositor (`setpriv --pdeathsig`). On 2026-11-14 the
user ran the complete suite with all 25 scenarios passing, and verified that Ctrl-C
interruption and forced launcher termination leave no compositor, Foot, or test process
running. After changing the launcher or the harness's interruption, shutdown, or process
handling, repeat those user-run checks: interrupt separate runs during a load, a capture,
and a cleanup, and kill one launcher from another terminal with
`kill -KILL "$(pgrep -o -x integration)"` (the oldest `integration` process). Each must fail
naming the interrupted stage, with no subsequent scenarios or closes; an already dispatched
close may complete. A few seconds after each run, this must print nothing:

```sh
grep -l sway-stage-headless /proc/[0-9]*/environ 2>/dev/null
```

The suite exercises sway-stage against a real Sway, but only with Foot, one virtual output,
and no input devices or desktop services. Other applications, physical outputs, keybinding
launch context, and visual rendering need the occasional user-run checks under
[manual application acceptance](#manual-application-acceptance).

### Scenarios

The runner requires a fresh compositor (only workspace `1`, focused and empty) before its
first command, then establishes its workspaces explicitly:

| Workspace | Role |
| --- | --- |
| `1` | The compositor's initial workspace. The foreign fixture loads here and stays for the whole run. |
| `2` | Created by one fenced switch from `1`. Every standard scenario, and the first target of every profile scenario, runs here; `profile-named-success` temporarily renames it. |
| `3` | The second target of the successful and partial profile scenarios. Each profile load creates it, and Sway removes it once the run returns to `2`. `profile-occupied-refusal` targets the occupied workspace `1` instead. |

The suite runs twenty-one standard Foot scenarios, then four profile scenarios. The
original eleven standard scenarios cover single window, horizontal/vertical pairs, uneven
siblings, mixed nesting, deep nesting (including same-axis groups), sibling nested groups,
process execution, default final focus, occupied-workspace refusal, and capture/reload
round-tripping. Each verifies raw-tree identity, order, hierarchy, sizing, final focus,
capture, and guarded cleanup. The nine native-group additions are listed below; the
twenty-first, `duplicate-foot-ids`, gives two windows one shared app ID (described below).

| Scenario | Independent expectations |
| --- | --- |
| `root-tabbed` | Three tabs, omitted global focus, first child selected. |
| `root-stacked` | Three stacked windows, explicit focus on the second. |
| `singleton-groups` | Singleton tabbed group inside a stacked root; both select their first child. |
| `mixed-native-nesting` | Split containing tabs, nested vertical split, and a later sibling outside the tabs. |
| `independent-native-groups` | Capture/reload with non-first selections in independent and nested tab/stack groups, including inactive groups. |
| `mixed-capture-reload` | Vertical split containing tabs, a singleton stack, and a weighted horizontal split; capture and replay with interior focus. |
| `initially-tabbed` | Load `mixed-native-nesting` after establishing an empty tabbed workspace. |
| `initially-stacked` | The same mixed hierarchy after establishing an empty stacked workspace. |
| `outside-focus-capture-reload` | Capture/reload with a non-first tab selected while global focus stays in an unrelated split branch. |

The `initially-tabbed` and `initially-stacked` cases recheck the selected target's identity,
focus, output, and emptiness, apply `layout tabbed` or `layout stacking`, check the reply,
then require a fresh fenced tree to report `tabbed` or `stacked` while still empty. Unexpected
workspace/window activity fails preparation without a load; observation history is retained.
Both reuse ordinary hierarchy, sizing, selection, capture, preservation, and success-only
cleanup checks. Their requested split root leaves an ordinary split workspace after success;
no layout reset or placeholder windows are used. Sway's
[`layout` implementation](https://github.com/swaywm/sway/blob/master/sway/commands/layout.c)
updates the empty workspace directly.

Single-window, horizontal-pair, and duplicate-foot-ids cases also check inferred commands.

Expected profile failures count as passes only after independent verification:

| Scenario | Independent expectations |
| --- | --- |
| `profile-success` | Validate and load two distinct Foot layouts in profile order; check fresh IDs, workspace/output ancestry, hierarchy, window order, sizing, each target's selected focus, and final focus on the second target. The first layout includes nested tabs, a singleton stack, and weighted splits. Capture both targets and verify inactive focus, hierarchy, sizing, selections, ordinal references, and unchanged desktop observations/history. |
| `profile-occupied-refusal` | Use the still-live foreign fixture as the second entry; require exit 1 and its occupied-target diagnostic, no recorded launches, no workspace activity, and an unchanged complete observed desktop. |
| `profile-partial-failure` | Complete the first Foot layout, then run `sleep 0` with a 500 ms matching-window timeout on the second target. Require exit 1, completed/failed workspace diagnostics, an intact first layout, an empty focused second target, and no observed rollback. |
| `profile-named-success` | Temporarily rename the exact empty focused primary slot to a unique `stage-… music` name with `num = -1`; load a mixed named/numeric profile and independently verify names, numbers, IDs, outputs, hierarchy, and final focus. Capture both the inactive named target and focused numeric target with the same independent draft/reference and preservation checks. After verified owned-window cleanup, restore the original numeric name with fresh identity, output, collision, emptiness, and event checks. |

The named scenario reuses workspace `2`; no fourth workspace is required. It accepts exactly
one expected rename event per rename and retains the existing event subscription. Any
unexpected result or interruption stops the run without renaming the workspace back or
closing windows. It never renames an occupied workspace.

Both success cases capture with inference disabled into additional private directories,
using only the two explicit targets; the harness never runs `--all`. All four use private
layout/profile directories, explicit directory overrides, and the pinned Sway socket.
Private launch records cover commands that produce no windows; ordered window events on
either side of the single expected transition independently establish launch bindings. The
inactive first workspace is checked through its saved child-focus path. The foreign fixture
and unrelated windows retain their usual preservation checks. Intentional profile errors
still attempt sway-stage's error notification, which cannot reach any notification daemon
through the session's nonexistent D-Bus bus; stderr and exit status remain checked.

If the second target is absent at scenario start, the profile must create it. If it already
exists empty, its identity/output must survive. The harness never destroys a workspace to
force creation. Natural removal of an empty unfocused workspace is allowed after cleanup.
On independently verified success or expected failure, cleanup closes the second target's
owned windows first, returns to the primary, then establishes fresh evidence before closing
its windows. Unexpected or ambiguous outcomes leave remaining windows untouched. The
short-lived timeout command has no window to close.

With one virtual output, every target shares `HEADLESS-1`. The harness still checks actual
output placement and rejects migration, without assigning or rearranging outputs, but it
does not cover multiple outputs. Record `sway --version` with results. The event/snapshot
limits described below still apply; an unchanged tree cannot prove that an unobservable
no-op IPC command was never attempted.

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
omit them. The replay file retains captured weights, active selections, selectors, IDs,
hierarchy, and focus.
Only successful exact-ID cleanup and an empty, preserved desktop permit the second load. A
fresh synchronized baseline and new launch bindings replace the first cycle's evidence;
retired container IDs are refused. The reconstructed layout is independently checked
against the captured weights with the ordinary sizing tolerance, then captured and safely
cleaned again. The `independent-native-groups` and `outside-focus-capture-reload` cases use
the same round-trip machinery to verify non-first selections in inactive and nested groups,
including global focus in an unrelated branch. Expected `active` fields are checked in
the captured YAML and each selected direct child is checked in both observed trees.
The suite has four replay scenarios. Replays are additional construction cycles within
those passes, not additional scenario counts. Failure reports the
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
that history and checks every header group, including singletons. Capture records selected
children as `active` when they are not first, including in nested and inactive groups,
alongside global `focus`. Replay verifies these selections with explicit global focus taking
precedence along its ancestry. Missing or contrary history in multi-child groups fails
capture; no rectangle/visibility approximation is used.
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
Sway transitions.

In `duplicate-foot-ids`, both Foot windows share one run-specific app ID and differ only in
a literal `--title` launch argument. Ownership therefore also requires each window's
`/proc/PID/cmdline` to match its own leaf's launch arguments. Inferred commands must contain
both the shared app-ID argument and the correct distinct run-specific title argument from
each leaf's original launch array.
Swapped, duplicate, or missing titles fail, even though both selectors agree. Titles reported
by Sway are never ownership evidence. Normalizing only the inferred command arrays must
reproduce the verified inference-disabled capture exactly; tree, event-history, and foreign-window
checks also establish that inference left the observed desktop unchanged. Recovered commands
are never printed in diagnostics.

The occupied-workspace case first loads and independently verifies a two-window layout,
then attempts a different valid layout on that occupied target. It requires exit 1 and the
specific occupied-workspace prelaunch diagnostic. Synchronized trees must preserve every
modeled container ID, identity, wrapper, ordering, geometry, and focus across the refusal;
unexpected window or non-urgent workspace events fail it. A timeout, interruption, or
unrelated error never counts as an expected refusal.

The foreign fixture is loaded before any scenario: a single Foot window with a run-unique
app ID on workspace `1`. Ordered new-window events and independent tree/layout checks
establish ownership; then the harness makes an expected, fenced switch to workspace `2`. The
fixture must stay present and unchanged through every scenario and is verified once more
after the last one. It is never closed; compositor shutdown ends it. It is a setup load, not
a counted scenario.

Immediately before each load, the harness records every foreign window, including scratchpad
windows. After load and capture, before each close, during close observation, and after cleanup,
it compares window identities, workspace/output ancestry, parent containers, sibling ordering
inside workspaces, geometry, and floating/fullscreen/scratchpad state. The fixture also retains
its original creation evidence across all scenarios. Titles, urgency, marks, and incidental
focus metadata are excluded from preservation comparisons; focus and global structural
activity retain their separate strict event checks. Any preservation uncertainty fails the
run and stops further closes and launches. A preservation baseline alone never
authorizes closing a preexisting window.

Snapshots and tick fences do not provide transactional isolation. Relevant unexpected events
fail even if a later tree returns to baseline. Harmless title/urgency/mark events are allowed.
Changes without corresponding observable events, briefly made and restored between snapshots,
cannot be ruled out; a pass establishes unchanged observed state plus accepted event history.

Sizing above 1 percentage point is diagnostic; above 5 fails a harness scenario.

Cleanup is an explicit success phase, never a failure handler. Each exact-ID close requires
fresh independent ownership, event-history, state, and hierarchy evidence. Selectors alone
are insufficient; the process-execution and repeated-app-ID Foot windows additionally require
literal process-argument evidence. Failure stops subsequent scenarios and closes, leaving
remaining windows to the compositor's shutdown. Gaps, minimum sizes, or version differences
can explain conservative failures.

The harness's guards against unexpected focus changes, foreign-window changes, and
owned-window state changes are covered by its offline tests; nothing else connects to the
private compositor during a run.

## Manual application acceptance

These user-only checks complement the headless suite on disposable workspaces in your own
session. They cover applications other than Foot (including Xwayland), physical outputs,
keybinding launch context, and visual rendering. Run them occasionally, such as before a
release or after changing launching, window matching, or focus handling.

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

For capture, compare inference-disabled and default inferred drafts with the original hierarchy,
selectors, proportions, and focus. Commands captured with `--no-infer-commands` must be null.
`check` must reject placeholders;
fill them, check, load on another empty target, and recapture. Expect at most small weight
rounding/sizing differences (aim within 1 percentage point). Confirm floating, fullscreen,
and structural-container focus refuse capture without YAML. Native tabbed/stacked groups,
including singletons, must capture successfully with weights only beneath splits. Capture must
leave desktop state and the launch log unchanged.

For the native-group visual check, record `sway --version`, then load a trusted local
native-group layout on a disposable workspace. Check tab/stack header order and singleton
headers visually. Confirm default first-child selection, configured non-first selections in
nested and inactive groups, and explicit global focus taking precedence along its ancestry.
Capture from a delayed command on another workspace and compare the draft; fill only its
null commands, check it, then load on another empty target.

Real AppImage, Flatpak, and desktop-launcher command inference has offline coverage only;
to check it live, capture a disposable workspace manually and review the recovered command.
General edited-draft replay remains covered by the existing round-trip scenarios.

For optional unified-menu acceptance, use one `sway-stage menu` keybinding with temporary
layout and profile directories and trusted applications on disposable workspaces:

1. On an empty workspace, confirm `> ` lists `Layout: NAME` then `Profile: NAME`,
   each in lexical order. Include equal names and an invalid configuration; verify labels
   stay distinct and invalid selections give the appropriate check command.
2. On a tiled capturable workspace, confirm capture appears first, followed by profiles.
   Select capture, then check name retries, collisions, Escape, private draft publication,
   and the save notification. Repeat with `--no-infer-commands` and review recovered
   arguments. The draft must retain the original hierarchy, weights, selectors, and focus
   even when workspace or window focus changes during either prompt.
3. With floating/fullscreen windows or unsupported containers/focus, confirm only profiles
   appear. Load a profile targeting separate empty disposable workspaces from a populated
   unrelated workspace. Check YAML order, numeric and named targets, silent success, and
   whole-profile occupied-target refusal before any launch or selection.
4. Check Escape at the primary, directory, and capture-name prompts preserves the launch log
   and sends no failure notification. With no usable actions, expect an error and no selector.
   Initial snapshot failure must still allow profiles when available.
5. Select an individual layout after switching workspaces or recreating its original name;
   it must refuse without launching or restoring focus. Check normal sizing/focus and
   occupied-target refusal. Change a layout, profile, or referenced layout to invalid YAML
   while the selector is open: selection must refuse and preserve the previous launch log.

Capture and cancellation preserve the launch log; actual loading attempts replace it.
All prompts should use the original output when known. These are user-only checks;
offline fake-Fuzzel and IPC transcripts provide the primary automated menu coverage.

For project-directory selection, use a trusted test layout with `cwd: ~/Projects` and
`cwd_select: true`, with at least two non-hidden child directories. On an empty disposable
workspace, run `menu`, select the layout, and confirm the sorted `Directory: ` basenames.
Choose a project and inspect the terminal's `pwd`; repeat with a window-level `cwd` override.
Check Escape at each prompt preserves the previous launch log. In separate runs, remove a
listed directory while its prompt is open, type an unoffered name, and switch workspaces
before selecting: each must refuse without launches or focus restoration. A root with no eligible children
must still offer its base directory. Direct `load NAME` must use the root without prompting.

For loader interference, separately switch away and back during a load, introduce an
unexpected window, and change an accepted window's state. Expect no further launches or
layout mutations, with partial results left in place. Verify real keybinding launch context
as well as delayed-terminal invocation; harness interference checks do not substitute.

For in-workspace focus restoration, enable `focus_follows_mouse` and load a layout with a
slow-starting application. In separate runs, briefly move the pointer across the target's
windows while that application starts, just before a later launch, and during final focus.
Expect completion with restorations recorded in `last-run.log`. Continued movement must
stop the run after three restorations of one launch or focus request, and moving the pointer
onto another output must abort without returning focus. This live behavior remains unverified.

When two outputs are available, check menu prompts stay on original output A while pointer
and workspace focus move to B; saved captures must still describe A's pre-menu state.
During separate loads, move the focused target workspace to another output, then test an
away-and-back move: both must abort without recovery. Moving only an unrelated workspace
or changing output geometry while the target retains its original output should not alone
abort. Output removal should report resulting Fuzzel errors without focus repair. These
hardware checks remain incomplete; their safety logic has offline coverage and does not
block continued development. Multi-seat behavior and a supported Sway version remain
unverified; do not infer compatibility from one run.

## Manual profile acceptance

User only: use a private copy of the single profile example in the
[specification](SPECIFICATION.md#named-profiles-version-1), pointing to trusted test layouts.
Replace the example targets with unused disposable numbers and names in your existing session.
Use harmless applications with distinct, verified selectors; keep all targets empty and input
idle. Build first, run `sway-stage profile check work` with your directory overrides, and use
those same overrides for `sway-stage profile load work` in each independent run below.
The profile coordinator selects the targets itself. The harness's profile scenarios above
automate the basic two-target cases in a private compositor; these separate manual checks
cover additional conditions and interference in your own session.

1. **Normal loading:** include an existing empty numbered workspace with a descriptive name,
   an existing true named workspace (`num = -1`), and a missing named target containing
   spaces. Load from another workspace. Verify YAML order, each hierarchy,
   sizing/final focus, and final target focus. Expect silent success and one launch log
   showing all three completed targets/container IDs.
2. **Occupied refusal:** put a window on the last target, keeping earlier targets empty.
   Record original focus and existing windows, then load. Expect exit 1 before any switch
   or application launch, unchanged windows/focus, zero completed workspaces, and one fatal
   notification. Repeat with a floating window and an empty structural container on a named
   target. Remove test windows manually between runs.
3. **Partial failure:** on fresh empty targets, make a private copy of the second layout
   launch one valid window followed by an executable that does not exist. Offline check
   should pass. Load and verify the first layout remains complete, the second retains its
   partial result, and the third is never selected or launched. Expect exit 1, one fatal
   notification, current workspace/layout plus the first completed workspace ID, and partial
   layout advice. There must be no cleanup or focus restoration. Separately test away-and-back
   focus during construction; it must stop further work. Inspect and close test windows
   yourself before another attempt.
4. **Multi-monitor placement:** with two outputs, use your existing Sway workspace rules to
   place empty targets on different outputs. Verify selection reaches each numbered or named
   workspace on its existing output, and missing targets follow Sway's normal placement rules. In separate
   runs move a target to another output during construction, including away and back; expect
   an abort with all windows left in place and no focus restoration. Profiles must never move
   a workspace or assign an output. Record the Sway version and outputs tested.

5. **Profile capture:** with existing disposable layouts on two reserved targets, leave
   the second focused and run `sway-stage --layouts-dir ./capture-layouts --profiles-dir
   ./capture-profiles profile capture work 8 10 --no-infer-commands` using unused private
   directories and your actual target numbers. Compare both drafts' focus, nested tab/stack
   selections, hierarchy, and sizing to the desktop without switching during capture. Check
   `work.yaml` references `work-001` and `work-002` in that order. Repeat separately with
   command inference enabled and a different profile name; review recovered commands.
   Missing metadata must retain placeholders, and `profile check` must refuse them until
   edited. Repeat an existing name: it must fail without replacing files or the launch log.
   Use offline fixtures for `--all`; do not include unrelated real workspaces in live tests.

The additional manual profile variants above remain unverified; cross-monitor and
interference runs provide optional further coverage. A passing headless suite does not
establish broader compositor compatibility or transactional isolation.

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
a user-facing layout command. Both connect to the compositor named by `SWAYSOCK`, normally
your own session, so they remain user-run; the headless harness permission does not extend
to them.
