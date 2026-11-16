# Development and verification

Start with [AGENTS.md](../AGENTS.md) for mandatory engineering and desktop-safety
rules. The [user reference](REFERENCE.md) defines externally observable behavior;
[INTERNALS.md](INTERNALS.md) explains important implementation tradeoffs. This guide
covers how to verify changes and what remains before a release.

## Offline checks

The project uses Rust 2024, a locked dependency graph, and MSRV 1.89. Run from the
repository root:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo check --locked --all-targets
cargo test --locked
cargo build --release --locked
cargo +1.89.0 check --locked --all-targets
```

The last command needs the Rust 1.89 toolchain installed; do not silently upgrade
MSRV to make it pass. The `integration_harness` offline target can be run separately:

```sh
cargo test --locked --test integration_harness
```

Offline tests use configuration/CLI fixtures, recorded Sway trees and events, fake
tools, and temporary Unix sockets. They do not exercise the user's compositor.
Socket-restricted sandboxes may prevent some tests from running. Report skipped or
blocked verification accurately; do not substitute real-desktop execution.

## Isolated headless integration

**Only** use the supported launcher:

```sh
tools/integration/run.sh
tools/integration/run.sh --filter profile
```

`--filter TEXT` matches a case-sensitive substring of scenario names. Omitting it
runs the full suite. Unknown arguments, an empty/missing filter, or a filter with
no matches fail with status 2. Scenario names and coverage live in
`tools/integration/`; avoid maintaining a second full scenario catalogue here.

Prerequisites: `sway`, `swaymsg`, Foot, `sleep`, `setpriv` (util-linux), `/bin/sh`,
and a working Linux `/proc`; Fuzzel is **not** needed because menu scenarios use a
private substitute. Foot is the only real application launched by the suite.

`run.sh` builds the program and harness in release mode. Each run starts its own
private wlroots-headless Sway compositor, temporary home/XDG directories, and
isolated sockets. The harness normally uses a virtual `HEADLESS-1` output;
scenarios requiring cross-output behavior run in a separate two-output compositor.
It does not connect to the caller's compositor or session bus. Do **not** bypass
`run.sh`, invoke the compiled integration example directly, or pass through live
`SWAYSOCK`, `WAYLAND_DISPLAY`, or `DISPLAY` values. The enforceable rules are in
[AGENTS.md](../AGENTS.md).

A passing run prints a `PASS` line for each selected scenario and a final count,
then shuts down its compositor. The first failure prints `FAIL`, stops further
scenarios, and preserves its private session directory for diagnosis. Inspect the
reported directory:

| File | Purpose |
| --- | --- |
| `sway.log` | Compositor output and startup problems. |
| `final-tree.json` | Final observed tree when captured before shutdown. |
| `home/.local/state/sway-stage/last-run.log` | Last load attempt's diagnostics. |
| `tmp/sway-stage-integration-*/` | Generated layouts, profiles, and capture artifacts. |

Interruption and shutdown are safety-critical. If modifying harness launch, cleanup,
or signal paths, inspect them against the ownership constraints in `AGENTS.md`, then
repeat dedicated interruption checks in an authorized environment. Never treat a
failure to start headless Sway as permission to use the live desktop.

## CI

`.github/workflows/ci.yml` configures two independent jobs on pushes to `main` and
pull requests: offline formatting/Clippy/check/tests/release/MSRV checks, and the
full headless suite on an Ubuntu 26.04 runner with Sway and Foot. Ubuntu 24.04's
Sway 1.9 lacks tree fields the harness requires; the current integration job
therefore uses 26.04. The CI configuration is not itself evidence of a successful
run; inspect the actual run status before making release claims.

On integration failure or timeout, the CI workflow uploads an
`integration-diagnostics-attempt-N` artifact (14-day retention) containing the
preserved session's relevant diagnostics. For example:

```sh
gh run download RUN_ID --name integration-diagnostics-attempt-1
```

Read the run output first, then the preserved `sway.log`, tree, launch log, and
fixture files. A passing headless suite does **not** verify physical outputs,
real keybindings, unrelated applications, or visual rendering.

## Manual acceptance (user only)

These tests operate on a real desktop. **AI agents must not perform them.** The user
may run them manually on disposable workspaces with harmless, trusted applications.
A disposable workspace does not isolate Sway from the rest of the session. Run
independent cases on fresh empty targets, inspect failure logs, and clean up manually.

### Basic load and capture

- Validate the examples (`sway-stage --layouts-dir examples check NAME`). Load
  `two-foot`, `four-windows`, and `mixed-groups` on separate disposable workspaces;
  verify nested hierarchy, weights, tab/stack header order, singleton groups,
  `active` selection, and explicit versus default final focus. Record `sway --version`
  for visual checks. Include at least one application other than Foot and an
  Xwayland application if available.
- From a different workspace, initiate a delayed `capture` and move focus onto the
  target. Compare captured hierarchy, weights, selectors, selections, and focus.
  Repeat with `--no-infer-commands`; edit placeholders, `check` the draft, and
  reload on a new empty target. Capture must not mutate the original layout.
- Confirm refusal for floating/fullscreen content, unsupported structural state,
  ambiguous focus, and occupied loading targets. Check the launch log for the
  failure stage and verify all launched windows remain for manual inspection.

### Menu and working-directory selection

- Use one real keybinding to invoke `menu` with disposable layouts/profile directories.
  Verify empty versus populated options, distinct layout/profile names, `[invalid]`
  entries, capture name retries, collisions, confirmations, and Escape at each prompt.
- With `cwd_select: true`, verify the base directory and sorted immediate children,
  a window-level cwd override, direct `load` without prompting, and refusal if a
  directory disappears. Cancelling or choosing an invalid entry must preserve the
  prior load log.
- Capture several disposable workspaces through `Capture session profile…`, review
  drafts, then load their profile on emptied targets. Captures must reflect the
  original menu snapshot even if focus changes during a prompt.

### Interference and multi-output behavior

- Exercise a real Sway keybinding, not only a terminal invocation. During a separate
  slow load, switch away and back, insert an unexpected window, or alter an accepted
  window's state. Further launches and layout mutations must stop, without rollback.
- With `focus_follows_mouse`, check transient in-workspace focus movement during
  launch and final selection (bounded restoration), then sustained movement and
  another-output focus movement (refusal). Verify through `last-run.log`.
- If two physical outputs are available, confirm menu prompts stay on their original
  output, moving the *target* workspace to another output aborts (also away and
  back), while moving an unrelated workspace alone does not. Distinguish these
  hardware observations from headless virtual-output coverage.

### Profile behavior

- On fresh disposable targets, load a profile combining numbered, descriptively
  numbered, and exact named workspaces, including a missing target. Verify YAML
  order, resulting hierarchy/focus, and final selected workspace.
- Populate a **later** target (tiled, floating, then an empty structural container in
  separate runs). Whole-profile preflight must refuse before switching or launching.
- In a separate run, make a later layout fail after one window launches. Earlier
  layouts remain complete, the current one remains partial, and subsequent targets
  are not selected. Check diagnostic progress; manually clear windows afterward.
- Capture two known disposable workspaces with `profile capture` and independent
  destination directories. Confirm focus for the inactive workspace, draft names,
  collision refusals, and no change to the previous load log. Do **not** use `--all`
  against an ordinary session containing unrelated populated workspaces.
- With physical multi-monitor placement rules, verify optional `output` expectations
  are checked but never assigned or repaired. Record outputs and Sway version.

This checklist is selective: recorded-tree and isolated headless tests cover more
failure variations. The above tests are for behavior that private headless runs cannot
establish. Mark unperformed manual cases as **unverified**, rather than treating
coverage in another environment as equivalent.

## Release readiness

The following items were outstanding in the original `docs/PLAN.md`; they are
**not** claimed complete by this documentation rewrite:

- Visually verify native group headers (including singleton tab/stack), plus the
  manual application, keybinding, menu, profile, interference, and physical-output
  cases above where available. Record observed versions and remaining gaps.
- Confirm real CI status for locked offline/MSRV checks and the full headless suite;
  audit repository contents, dependencies, licensing, and release metadata.
- Package reproducible tagged Linux archives and checksums, initially for x86_64.
  Document demonstrated architecture and libc requirements; do not claim static
  linking or compatibility with untested systems.
- Smoke-test the packaged executable and installation steps, including sensible
  failure behavior when optional tools are absent.
- Once acceptance is satisfactory, publish v0.1.0 with archives, checksums,
  installation instructions, and release notes. No new architecture changes are
  committed as part of this checklist.

When these tasks move to repository issues or a milestone, replace this temporary
checklist with a link to that tracker. Do not maintain both independently.

## Maintainer map

- `src/config.rs`, `src/profile.rs`, `src/discovery.rs`: strict YAML and name rules.
- `src/launch.rs`, `src/observation.rs`, `src/application_process.rs`: guarded window
  creation, observation, ownership, sizing, and process launch context.
- `src/capture.rs`, `src/profile_capture.rs`, `src/command_inference.rs`,
  `src/draft_file.rs`: read-only snapshot conversion, inference, and publication.
- `src/menu.rs`, `src/diagnostics.rs`, `src/failure.rs`: UI and diagnostics.
- `tools/integration/`: authoritative scenarios, independent tree observation,
  process isolation, and cleanup. Do not weaken assertions to make tests pass.
- `tools/inspect_sway.rs` and `tools/observe_sway.rs` are **user-run** diagnostics;
  they connect to the `SWAYSOCK` in their environment, typically the live session.

For critical invariants and tradeoffs, consult [INTERNALS.md](INTERNALS.md) and
[AGENTS.md](../AGENTS.md), not a duplicated catalogue of test scenarios.
