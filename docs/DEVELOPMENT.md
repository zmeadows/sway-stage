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

Before running, start on an existing workspace other than the target. Choose an explicit
positive decimal workspace number (at most nine digits) that is absent or completely empty,
including floating windows and empty containers. A number belonging to a differently named
workspace is refused. The starting workspace name must begin with an ASCII letter/digit
and contain only ASCII letters/digits, spaces, or `_ . : + -`, allowing safe restoration.
Preflight is repeated immediately before switching; the harness never selects an alternate
target.

Run manually, one invocation at a time:

```sh
./target/release/examples/integration --workspace 9
./target/release/examples/integration --workspace 9 --extended
```

| Option | Meaning |
| --- | --- |
| `--workspace NUMBER` | Required disposable target; the harness explicitly switches to it. |
| `--extended` | Append Firefox, Xterm/Xwayland, nested browser, and repeated-app-ID cases. |
| `--binary PATH` | Select another sway-stage build; default is this checkout's `target/release/sway-stage`. |
| `--firefox-app-id EXACT_ID` | Override the native ID inferred from adjacent `application.ini` metadata. |

The standard run covers nine Foot scenarios: single window, horizontal/vertical pairs,
uneven siblings, mixed nesting, deep nesting (including same-axis groups), sibling nested
groups, process execution, and default final focus. Each verifies raw-tree identity, order,
hierarchy, sizing, final focus, capture, and guarded cleanup.
Single-window and horizontal-pair cases also check inferred commands. Expect nine passes;
extended runs expect thirteen when all optional prerequisites exist, otherwise explicit SKIPs.
The process-execution case uses a private relative cwd and a bounded harness helper as
Foot's direct child. Linux `/proc` checks the child's exact literal arguments, cwd, and
run-specific environment override; verified window cleanup must also observe its exit.
No shell evaluates the test arguments.

Sizing above 1 percentage point is diagnostic; above 5 fails a harness scenario.

Extended prerequisites:

- Native `firefox-esr` (preferred) or `firefox` on `PATH`, a valid `WAYLAND_DISPLAY`, and
  readable `/proc/PID/cmdline` for Firefox and repeated-ID Foot ownership evidence.
  Firefox must preserve literal launch arguments; wrappers/sandboxed builds obscuring them
  fail conservatively. Use `--firefox-app-id` only after verifying your installation's ID.
- Xterm coverage needs `xterm`, `Xwayland`, and `xdpyinfo` on `PATH`, and `DISPLAY` reaching
  this session's Xwayland server with the `XWAYLAND` extension. An unavailable server/probe
  skips the case; a subsequent launch or class mismatch fails it.

Firefox uses private profiles/HOME/XDG directories and no normal profile. Browser directories
are retained and reported on success and failure: remove them manually only after all their
processes exit. Firefox has no automatic lifetime limit. Foot and Xterm commands normally
expire within five minutes of launch. Missing optional subsystems skip; failures with
available applications stop the suite.

Cleanup is an explicit success phase, never a failure handler. Each exact-ID close requires
fresh independent ownership, event-history, state, and hierarchy evidence. Selectors alone
are insufficient; Firefox and repeated-ID Foot additionally require literal process-argument
evidence. Failure stops subsequent scenarios and closes, leaving remaining windows alone.
Read reported container IDs, selectors, PIDs, and retained paths before manual intervention;
diagnostic candidates do not imply ownership. Rules, gaps, minimum sizes, desktop activity,
or version differences can explain conservative failures.

Restoration is separate from cleanup. It requires trusted focus history, the target still
focused, and the original workspace's exact identity/name still present. Otherwise it is
skipped; a destroyed original workspace is not recreated. An ordinary check failure may
permit guarded restoration; uncertain ownership/control or interruption does not.

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
