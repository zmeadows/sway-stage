# Agent instructions for sway-stage

## Purpose and priorities

`sway-stage` is a small Linux CLI that constructs declarative tiled layouts on the
currently focused, empty Sway workspace, or sequentially on a named profile’s explicit
workspace numbers or exact non-numeric names. Prioritize, in order:

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
that a maintainer cannot follow will eventually be broken. These standards and the human
readability review below apply to all Rust code: production code, tests, Rust fixtures,
diagnostic tools, and the independent integration harness. Simplification must preserve
the safety invariants below.

- **Descriptive naming.** Name types, enum variants, struct fields, functions, arguments,
  and locals for their specific meaning and role. Prefer `target_workspace_id` over `id`
  when multiple identities are involved, and `child_window_index` over `index` when its
  role would otherwise be ambiguous. Include units where they matter: `window_timeout_ms`,
  `input_bytes`, `width_pixels`. Clearly distinguish configured layouts, expected
  hierarchies, observed Sway trees, accepted windows, and preexisting windows, including
  `LayoutNode` versus `swayipc::Node`. Function names should communicate the operation and
  whether it inspects, validates, constructs, mutates, or persists. Keep short names when
  their meaning is obvious within a small local scope; avoid redundant verbosity and
  mechanically repeating type information. Aim for maximum useful clarity, not maximum
  identifier length.
- **Visual structure.** Separate meaningful phases (validation, preparation, computation,
  IPC operations, state changes, verification, reporting) with intentional blank lines.
  Group tightly related statements and break up long sequences of unrelated work. Use
  blank lines around consequential guards or state transitions when they clarify the flow;
  never insert blank lines mechanically after every statement. Do not compact a function
  into a wall of statements because the formatter permits it, or extract helpers only to
  create visual separation.
- **Visible control flow.** Write straightforward procedural Rust that a reader can
  follow in execution order. Prefer ordinary functions, `if`, `match`, and loops when
  they make the operation clearer. Use early returns and `let ... else` for clear
  preconditions. Name intermediate values when deeply nested expressions, large
  expressions, or closure chains obscure meaning. Iterators, closures, and combinators
  are welcome when they are clearer. Make expensive operations, fallible calls, and
  side effects apparent at their call sites. Keep launch, observation, validation, and
  mutation ordering explicit.
- **Justified abstractions.** Start with a direct implementation; every abstraction must
  justify its cognitive cost. Traits, generics, builders, macros, callback systems,
  indirection, and frameworks must solve a demonstrated problem, not serve architectural
  neatness or hypothetical future needs. Do not build an IPC abstraction framework solely
  to mock tests. Keep a coherent operation together: a longer function can be clearer
  than layers of trivial delegation. Extract functions to isolate meaningful
  responsibilities or invariants, remove meaningful duplication, or clarify side effects.
  Judge maintainability by the total complexity a reader must understand, not function
  length or line count alone.
- **Authoritative state and ownership.** Use ordinary Rust ownership, enums, and `Result`
  to express real constraints. Prefer concrete types and clear lifetime relationships.
  Avoid unnecessary clones, allocations, conversions, and defensive state. Derive values
  when practical instead of synchronizing independently mutable copies of the same facts;
  do not duplicate enum state or other authoritative information with booleans. Any
  necessary duplicated state must have an explicit invariant and clear update points.
- **Boundary validation and errors.** Validate CLI, YAML, filesystem, process, and IPC
  boundaries. Unknown keys and ambiguous window matches are errors. Include actionable
  context in errors; reserve panics and `unwrap`/`expect` for established internal
  invariants, never ordinary input or external failures.
- **Comments and constants.** Explain why constraints, surprising choices, safety checks,
  and ordering requirements matter. Do not narrate obvious statements or leave
  commented-out alternatives. Make names and control flow explain ordinary operations
  without compensating comments. Use named constants for meaningful limits when they
  clarify intent; avoid unexplained magic values and constants for self-explanatory
  literals. Keep implementation details in source rather than expanding user-facing
  documentation to explain them.
- Follow `.editorconfig` and `rustfmt.toml`: UTF-8, LF, four-space Rust indentation,
  two-space YAML indentation, and 100-column Rust formatting. For genuinely table-like
  code only, a narrowly scoped `#[rustfmt::skip]` may preserve one entry per line;
  rustfmt governs everything else.
- Make the smallest coherent change. Avoid speculative generalization, unrelated
  cleanup, module scaffolding, and churn. Add a module only for a substantial, coherent
  responsibility; keep one binary crate until there is a concrete reason to change.

Use Rust 2024 with MSRV **1.89**. Use synchronous native Sway IPC; do not add an async
runtime or invoke `swaymsg` for core behavior. Prefer the standard library for process
launches and simple filesystem work. Keep `Cargo.lock` checked in. Add dependencies only
when used, review maintenance, transitive dependencies, features, licensing, and compiler
compatibility, and verify the locked graph on the MSRV. Do not silently raise it.
Do not add dependencies for stylistic reasons.

## Safety invariants

- The ordinary loader freezes the focused workspace as its target and never selects or
  creates a workspace. Only the profile coordinator may select/create the workspaces
  explicitly configured by validated numeric identity or exact non-numeric name in a fully
  validated profile, sequentially in YAML order. Validate and retain all referenced
  configurations before IPC; preflight every target read-only
  before switching. Refuse any tiled or floating occupancy, including empty containers.
  Recheck focus, target identity/output, and occupancy before each selection, verify the
  bounded command reply and resulting state/history, then invoke the unchanged layout engine
  with the selected workspace ID. Never assign outputs or restore focus automatically.
- Observe workspace focus changes throughout construction, including a switch away and
  back between snapshots. Stop on a switch, missing target, IPC failure, or lost safety
  precondition. Freeze the containing output identity too; stop on target output migration,
  including away and back. Never switch back automatically. Keep profile coordination's
  expected-transition history separate from each individual load's fresh frozen observer;
  intentional profile selections must not relax construction's focus/output invariants.
  Stop the whole profile on failure, leaving earlier and partial layouts untouched. Use one
  diagnostics session and at most one fatal notification, retaining workspace progress even
  when the individual loader changes diagnostic stages.
- Snapshot existing window identities across the tree before launching, and before each
  subsequent launch. Only newly observed, unclaimed windows with the explicit expected
  app ID or Xwayland class can be accepted. Launch one application at a time and bound
  every wait with a monotonic deadline.
- A matching new window on another workspace is a fatal error. Do not move it. Reject
  floating, ambiguous, and unexpected target-workspace windows rather than repairing them.
- Never close, move, resize, or reuse a preexisting window. Never issue global scratchpad
  operations. Direct mutations only at this run's accepted windows and containers whose
  descendants are all owned by the run; revalidate before each mutation. Never use broad
  app-ID/class criteria for mutations or interpolate arbitrary user strings into Sway commands.
  The profile coordinator's sole string exception is a validated literal workspace name,
  restricted to its bounded ASCII allowlist and checked against reserved command keywords;
  revalidate it when building the quoted selection command.
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
or resizing windows. Never run `sway-stage load` or `sway-stage profile load` against the real
compositor. All live-desktop behavioral verification must be performed manually by the user on disposable
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
cleanup are development-harness exceptions to the ordinary loader's no-switch policy and
the CLI's no-close policy, not permission for agents to perform desktop operations.

Window cleanup must remain an explicit verified success phase that closes only windows
independently proven to belong to the run, never a destructor or failure handler. SIGINT
only records a request; ordinary control flow invalidates focus history, stops further
mutations, and skips restoration. Child process groups isolate terminal interrupts, but
termination must target only a harness-owned direct child by PID, never a process group,
descendant application, or process discovered through `/proc`.

**Human readability review.** After every source change, reread the modified code in full
as a new maintainer, not merely as a patch. Inspect surrounding code and callers as needed
to follow the complete operation. Answer these questions:

- Can a new maintainer understand the control flow at a glance, with distinct execution
  phases visually separated?
- Are identifiers informative enough to follow without reconstructing context?
- Can the reader identify authoritative values, side effects, validation boundaries,
  and failure paths?
- Are any expressions unnecessarily compressed or abstractions making the operation
  harder to follow?
- Would a simpler, more direct representation be clearer while preserving correctness
  and safety?

Fix what fails this review before reporting the work as done. Passing `cargo fmt` or
Clippy does not establish readability; review naming and human-readable grouping
separately. Update only documentation affected by the change. Do not leave `dbg!`, `todo!`,
or `unimplemented!` in checked-in code.

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
