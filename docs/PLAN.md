# Implementation plan

Keep one binary crate, synchronous control flow, and a small set of concrete types. Add
modules only as implemented responsibilities justify them. The
[specification](SPECIFICATION.md) owns behavior; this document owns sequencing and open
engineering decisions. Milestone 1 is complete; milestones 2–4 remain pending.

## Preliminary step: read-only IPC inspection

Implemented before milestone 1: `cargo run --example inspect_sway` uses synchronous
`swayipc` 4.0.0 to query workspaces, identify the focused workspace by container ID in the
full tree, and display its layout, occupancy, and nested tiled/floating hierarchy without
window titles. Both child lists must be empty for the workspace to count as empty.
`SWAYSOCK` is required: construct a `Connection` from a `UnixStream` to avoid
`Connection::new()` preferring `I3SOCK` or spawning socket-discovery processes.

This development example issues only `GET_WORKSPACES` and `GET_TREE`. It does not
implement a public CLI command or any layout-engine behavior. The two queries are not
atomic; a missing target or focus observed outside it fails, but switches away and back
are not detected. Event history and bounded observation remain milestone 2 work.
Build/lint checks and temporary fake-socket checks passed, including nested/floating
trees, optional metadata, occupancy, and failure paths. Live Sway behavior is unverified;
manual inspection is still required.

Tree/API findings: `Node::id` identifies every Sway container, while `Node::window` is an
optional X11 window ID. App IDs and Xwayland classes are optional, and a workspace's
`focused` flag need not be true when a descendant has focus. Traverse `nodes` and
`floating_nodes` explicitly, preserving order and split wrappers. Sway's `percent` is
parent-area share, not the normalized sibling dimension required for future layout
weights; it may be absent. The example labels this distinction as `parent_area`.

## 1. Layout validation and discovery

Implemented: a typed split/window model, strict YAML validation, immediate-file discovery,
and Clap `check`/`list` commands. Help identifies the remaining commands as unimplemented.
Neither command contacts Sway, launches processes, writes logs, or sends notifications.
Discovery uses XDG/HOME or an explicit directory, rejects invalid/duplicate names, and
returns lexical order without parsing layouts for `list`.

The four-window example validates. Twelve configuration test groups and six CLI/filesystem
test groups cover strict mapping/scalar boundaries, malformed and unsupported YAML,
weights, nested order, IDs/selectors, cwd/environment semantics, timeouts, null capture
placeholders, exact resource-limit boundaries, XDG resolution, and exit codes. Filesystem
tests use temporary directories and child-specific environment settings without compositor
sockets or external tools. Local formatting, Clippy, all-target compilation, tests, release
build, and locked Rust 1.89 compilation pass. Hosted CI has not been run in this change.

## 2. Safe sequential construction

First prove the synchronous IPC client's bounded event observation using a disposable
test socket: workspace switches away and back must be detected, and idle waits must
expire. Then implement guarded `load`, beginning with one window and extending to nested
splits and relative sizing. Add stage logging and bounded best-effort notifications with
the first working load path. Keep process launching separate from Sway command strings.

Complete when recorded trees/events and simulated IPC replies verify occupied-workspace
refusal, global new-window matching, delayed app IDs, repeated selectors, ambiguous and
wrong-workspace windows, floating/unexpected windows, workspace switches, application
timeouts, IPC failures, accepted-window loss, and partial failure without cleanup or
relaunch. Verify nested ownership, command failure handling, weight tolerance, bounded
resizing, and default/overridden final focus. No mutation may target preexisting windows.
No automated test connects to the user's Sway socket.

An explicitly authorized, isolated Sway smoke test must reproduce the reference's four
windows and test abort behavior before declaring this milestone complete. Until then,
report compositor behavior as unverified. Do not build a generic backend/plugin framework
to accommodate tests; pure tree calculations and a small IPC boundary are sufficient.

## 3. Capture and optional picker

Implement read-only `capture`, preserving nested hierarchy and proportions while emitting
missing-command placeholders. Add `pick` through an optional `fuzzel --dmenu` subprocess
that delegates to the same load path.

Complete when fixture-based captures preserve order, focus, and proportions; unsupported
floating/tabbed/stacked/fullscreen layouts fail before output; missing match metadata
remains an explicit draft; and filling placeholders yields a checkable layout. Fake picker
tests cover exact selection, cancellation, missing executable, and invalid output. Confirm
neither tool is required by unrelated commands and capture never issues mutations.

## 4. First Linux release

Review actual behavior against the specification, update README with working usage, and
document demonstrated limitations. The project uses the MIT license; choose a repository
URL before publishing.
Add a small tagged-release workflow after the behavior is validated; reuse CI checks.

Complete when precompiled Linux archives and checksums can be reproduced from a tag,
installation is documented and smoke-tested, the supported Sway version and Linux
architecture/libc baseline are recorded, and optional-tool failures are understandable.
Start with x86_64 Linux; add ARM64 only with a validated build/run path. Do not claim
static linking, other platforms, or untested compositor versions.

## Dependency review

Dependencies and remaining candidates reviewed against their upstream documentation on
2026-11-09:

| Need | Candidate and decision |
| --- | --- |
| CLI | [Clap 4.6.7](https://docs.rs/clap/4.6.7/clap/) with derive, std, help, usage, and error-context; default features disabled. Declares Rust 1.85. |
| Typed YAML | [Serde](https://serde.rs/) with derive and [serde-saphyr 2.1.1](https://docs.rs/serde-saphyr/2.1.1/serde_saphyr/) with only deserialize enabled. The parser declares Rust 1.89. No serialization, comments, include, interpolation, robotics, or validation-framework features are enabled. |
| Field diagnostics | [serde_path_to_error 0.1.20](https://docs.rs/serde_path_to_error/0.1.20/serde_path_to_error/) supplies paths for nested deserialization errors; declares Rust 1.61 and adds only already-used Serde core/itoa dependencies. |
| Native synchronous IPC | [swayipc 4.0.0](https://docs.rs/swayipc/4.0.0/swayipc/struct.Connection.html), from [swayipc-rs](https://github.com/JayceFayne/swayipc-rs), added as a development dependency for inspection. Its direct dependencies are Serde, serde_json, and swayipc-types. The locked graph compiles on Rust 1.89; bounded waits and event history still need proof before construction. |
| Launching and optional tools | `std::process::Command` for applications, Fuzzel, and `notify-send`; no shell wrapper, async runtime, or Fuzzel crate. Bound helper-process waits. |
| Diagnostics and errors | Begin with standard-library I/O and contextual errors; add a logging/error crate only if concrete implementation needs justify it. |

The inspection example resolves 15 dependency packages including `swayipc` and
`swayipc-types` 2.0.3. The latter also pulls in `thiserror` 2 and Serde derive macros;
the graph includes their proc-macro dependencies and JSON formatting/parsing helpers.
`swayipc` has no selectable features; its types enable Serde derive, serde_json's
`unbounded_depth`, and default error support. No async runtime is included. Both IPC
crates are MIT licensed and declare no MSRV; the locked graph was checked with the
installed Rust 1.89.0 toolchain. Transitive licenses are permissive MIT/Apache-2.0 (with
Unlicense/MIT for memchr and an additional Unicode-3.0 license for unicode-ident).
Manifest/feature review and the [RustSec advisory index](https://rustsec.org/advisories/)
review found no applicable advisories; this is not a security guarantee.

Milestone 1 adds 23 locked dependency packages. The YAML deserializer uses granit-parser
2.1.0, numeric helpers, encoding_rs_io/encoding_rs, smallvec, and annotate-snippets;
Clap adds its builder/lexer/derive and their small helpers. Review of package manifests
found permissive licenses: MIT/Apache-2.0, with BSD-3-Clause additionally required by
encoding_rs. Existing license exceptions above still apply. Declared compiler baselines
do not exceed 1.89, and the complete locked graph compiles on installed Rust 1.89.0.
The current RustSec index review found no applicable advisory for the added versions;
this was a source review, not an automated vulnerability audit.

Strict YAML behavior is verified with fixtures: duplicate-key and merge-key policies
reject, aliases have a zero-event budget, unsupported tags are rejected on values and
field names, and the single-document API rejects trailing documents/content. String
fields require quotes around numeric/boolean scalars; small numeric deserializers also
reject quoted numbers rather than coercing them. Explicit nulls never trigger defaults.
Reads stop at 1 MiB plus one detection byte and require UTF-8. Parser collection depth
is capped at 68 (allowing children sequences and leaf field collections at layout depth
32), with bounded events, scalar bytes, nodes, anchor retention, and flow nesting before
semantic validation enforces 32 layout levels and 256 layout nodes. Parser errors carry
field paths and source locations; semantic tree errors identify the field and containing
node's location. Source snippets are disabled to avoid printing full commands or env maps.
No known format-contract guarantee is left unenforced. Serialization features can be
evaluated when capture is implemented.

Do not use archived [serde_yaml](https://github.com/dtolnay/serde-yaml) for a new project.
Recent releases and a compatible declared MSRV are useful evidence, not a maintenance or
security guarantee. Before adding each dependency, inspect current releases, advisories,
license, necessary features, transitive graph, and actual MSRV compilation. Keep the
lockfile committed; do not silently raise the compiler baseline to satisfy resolution.
There is no need for an async IPC crate, a custom IPC protocol implementation, a config
framework, or a process supervision system at this stage.

## Remaining decisions and proof points

- `swayipc` exposes subscriptions and UnixStream conversion, but bounded event reads,
  interrupt handling, and race-aware command sequencing need a concrete proof in milestone
  2. If the client cannot meet them, revisit that choice before implementing launch logic.
- App IDs/classes cannot prove launch causality, and IPC cannot make observation and
  mutation atomic. Test conservative aborts; do not disguise these limits as recoverable
  placement errors. No live-session checks were performed during repository setup.
- Hosting URL, supported Sway version, and Linux ABI baseline remain
  unset. Cargo publishing stays disabled until release metadata and behavior are ready.
