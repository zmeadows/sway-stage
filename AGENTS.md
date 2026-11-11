# Agent instructions for sway-stage

## Purpose and priorities

`sway-stage` is a small Linux CLI that constructs declarative tiled layouts on the
currently focused, empty Sway workspace. Prioritize, in order:

1. Preserving the user's windows and workspace state.
2. Correct behavior with understandable failures.
3. Readable, straightforward Rust that is easy to debug and change.
4. A small executable and little ongoing maintenance cost.

Read the affected code and [specification](docs/SPECIFICATION.md) before editing.
Use [the roadmap](docs/PLAN.md) for outstanding release work.
Do not add session restoration, existing-window reuse, a daemon, automatic enforcement,
other compositors, or a graphical editor unless explicitly requested.

## Engineering principles

Readability is an engineering requirement here, not a cosmetic preference: safety checks
that a maintainer cannot follow will eventually be broken. Naming, visual structure, and
the readability review below apply to every source change.

- Write code a reader can follow in execution order. Prefer concrete types, ordinary
  functions, `if`, `match`, loops, early returns, and named intermediate values when
  they express the operation clearly. Concise idiomatic expressions are welcome when
  they are easier to read.
- **Descriptive naming.** Choose identifiers that state a value's role, meaning, or units
  and an operation's effect: `preexisting_ids`, `expected_hierarchy`, `child_index`,
  `window_timeout_ms`, `apply_sizing`. Use a specific name wherever a generic one (`node`,
  `expected`, `existing`, `index`, `value`) could be confused with a nearby concept,
  especially configured layout versus observed Sway state (`LayoutNode` versus
  `swayipc::Node`). Short names are fine when the scope makes them obvious; do not
  sacrifice clarity for brevity or expand names that merely restate their type.
- **Visual structure.** Separate meaningful phases (validation, preparation, computation,
  side effects, verification, reporting) with intentional blank lines, and keep tightly
  related statements together. Never compact a function into a wall of statements because
  the formatter permits it, and never insert blank lines mechanically. Prefer a named
  intermediate value to a dense expression or awkward formatter wrapping; do not add
  helpers only to create visual separation.
- Keep related operations together and side effects apparent. A coherent longer
  function is preferable to layers of trivial delegation. Extract helpers to clarify
  responsibilities, enforce invariants, or remove meaningful duplication.
- Start with a direct implementation. Traits, generics, builders, macros, callback
  systems, and frameworks must solve a demonstrated problem. Do not build an IPC
  abstraction framework solely to mock tests.
- Make authoritative state obvious. Use ownership, enums, and `Result` to express real
  constraints; avoid independently mutable copies of the same facts. Keep launch,
  observation, validation, and mutation ordering explicit.
- Validate at boundaries: CLI, YAML, filesystem, processes, and IPC. Unknown keys and
  ambiguous window matches are errors. Include actionable context in errors; reserve
  panics and `unwrap`/`expect` for established internal invariants, never ordinary input
  or external failures.
- Comments explain constraints, surprising choices, and ordering requirements. Avoid
  narrating obvious statements, unexplained magic values, or commented-out alternatives.
- Follow `.editorconfig` and `rustfmt.toml`: UTF-8, LF, four-space Rust indentation,
  two-space YAML indentation, and 100-column Rust formatting. `cargo fmt` is necessary
  but not sufficient; it makes no naming or phase-separation decisions. For genuinely
  table-like code only, a narrowly scoped `#[rustfmt::skip]` may preserve one entry per
  line; rustfmt governs everything else.
- Make the smallest coherent change. Avoid speculative generalization, unrelated
  cleanup, module scaffolding, and churn. Add a module only for a substantial, coherent
  responsibility; keep one binary crate until there is a concrete reason to change.

Use Rust 2024 with MSRV **1.89**. Use synchronous native Sway IPC; do not add an async
runtime or invoke `swaymsg` for core behavior. Prefer the standard library for process
launches and simple filesystem work. Keep `Cargo.lock` checked in. Add dependencies only
when used, review maintenance, transitive dependencies, features, licensing, and compiler
compatibility, and verify the locked graph on the MSRV. Do not silently raise it.

## Safety invariants

- Freeze the focused workspace as the target; never choose a workspace number or output
  for the user. Refuse any tiled or floating occupancy before the first mutation.
- Observe workspace focus changes throughout construction, including a switch away and
  back between snapshots. Stop on a switch, missing target, IPC failure, or lost safety
  precondition. Freeze the containing output identity too; stop on target output migration,
  including away and back. Never switch back automatically.
- Snapshot existing window identities across the tree before launching, and before each
  subsequent launch. Only newly observed, unclaimed windows with the explicit expected
  app ID or Xwayland class can be accepted. Launch one application at a time and bound
  every wait with a monotonic deadline.
- A matching new window on another workspace is a fatal error. Do not move it. Reject
  floating, ambiguous, and unexpected target-workspace windows rather than repairing them.
- Never close, move, resize, or reuse a preexisting window. Never issue global scratchpad
  operations. Direct mutations only at this run's accepted windows and containers whose
  descendants are all owned by the run; revalidate before each mutation. Never use broad
  app-ID/class criteria for mutations or interpolate user strings into Sway commands.
- Check every IPC command reply, then verify the observed hierarchy. Do not equate an
  accepted command with the intended result. Size adjustment is bounded and tolerates
  small deviations; significant deviations produce a warning, not an endless resize loop.
- On failure, stop issuing layout mutations and launches. Leave windows and applications
  in place. No closing, killing, moving, destructive rollback, or automatic relaunching.
- Preserve argument boundaries and apply cwd/environment to the child process only.
  Do not implicitly evaluate a shell or rewrite application commands to inject IDs.
- Report the failing stage and cause to stderr and attempt a desktop error notification.
  Notification or log failures must not mask the original error. Do not log environment
  values or complete command arguments by default.

Sway IPC is not transactional. Treat event history, fresh snapshots, and ownership checks
as necessary guards; do not promise atomic isolation against concurrent user activity.

## Documentation and working process

Each kind of information has one home:

- `README.md`: concise introduction, installation, quick start, and links.
- `docs/SPECIFICATION.md`: authoritative implemented behavior, configuration contract,
  capture workflow, and troubleshooting.
- `docs/DEVELOPMENT.md`: contributor verification and user-only manual integration procedure.
- `docs/PLAN.md`: outstanding release work and explicitly undecided ideas only.
- `examples/`: user-facing illustrative YAML configurations, linked to the specification.
- `tools/`: development-only Rust diagnostics and the integration harness, registered in
  `Cargo.toml` as Cargo examples. The harness's offline tests are a separate
  `integration_harness` test target.
- `AGENTS.md`: durable engineering standards and safety constraints.
- Source and comments: implementation details; CLI help describes current commands.
  Keep it consistent with the specification.

Inspect callers and failure paths before editing. Test behavior at meaningful boundaries:
configuration fixtures, pure tree/weight calculations, and recorded IPC trees/events.
Introduce only the smallest test seam needed. Keep automated checks non-interactive and
independent of a live compositor. Never execute commands that modify the user's live
Sway session, including launching or terminating applications, changing focus, or moving
or resizing windows. Never run `sway-stage load` against the real compositor. All
live-desktop behavioral verification must be performed manually by the user on disposable
workspaces in their existing Sway session, following [development](docs/DEVELOPMENT.md).
A disposable workspace does not isolate the compositor or other workspaces.

The integration harness, the `integration` Cargo example in `tools/integration/`, is
exclusively for the user's manual execution on a disposable workspace. Agents must never
execute it, including `--help`, partial runs, or trial runs: never run
`cargo run --example integration` or the built example binary, and never connect it to a
live compositor. Validate it only by compilation, static inspection, and the offline
`integration_harness` tests. That test target is a separate crate root without the harness
entry point; its tests must never read `SWAYSOCK`, connect to a compositor, or start
processes. The harness's explicit switch to a disposable workspace and its owned-window
cleanup are development-harness exceptions to the CLI's no-switch/no-close policy, not
permission for agents to perform desktop operations.

Window cleanup must remain an explicit verified success phase that closes only windows
independently proven to belong to the run, never a destructor or failure handler. SIGINT
only records a request; ordinary control flow invalidates focus history, stops further
mutations, and skips restoration. Child process groups isolate terminal interrupts, but
termination must target only a harness-owned direct child by PID, never a process group,
descendant application, or process discovered through `/proc`.

**Human readability review.** After every source change, reread the modified code in full
as a maintainer meeting it for the first time, not only as a diff. Check that names are
informative enough to follow without reconstructing context; that control flow and phases
are visible at a glance; and that a reader can identify authoritative data, side effects,
failure paths, and the reason for each helper. Fix what fails this review before reporting
the work as done. Update only documentation affected by the change. Do not leave `dbg!`,
`todo!`, or `unimplemented!` in checked-in code.

Run applicable checks (use `cargo fmt --all` to format):

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo check --locked --all-targets
cargo test --locked
cargo build --release --locked
cargo +1.89.0 check --locked --all-targets
```

CI is Linux-only. Report checks that could not run and meaningful limitations; do not
install packages or toolchains without authorization. Do not commit or push unless asked.
If asked to commit, use a short Conventional Commit subject without generated-by
attribution or co-author trailers. External reference repositories are read-only.
