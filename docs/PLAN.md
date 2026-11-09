# Implementation plan

Keep one binary crate, synchronous control flow, and a small set of concrete types. Add
modules only as implemented responsibilities justify them. The
[specification](SPECIFICATION.md) owns behavior; this document owns sequencing and open
engineering decisions. Milestones 1 and 2A are implemented; 2B–4 remain pending.

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

## 2A. Reliable event observation

Implemented as the development-only `cargo run --example observe_sway`, with its concrete
observation component in `examples/support/observation.rs`. The existing `inspect_sway`
example and Milestone 1 CLI behavior are unchanged. The new example observes for ten
seconds, periodically querying fresh state, and exits on lost observation or focus. It
does not require an empty workspace or accept window ownership; those are loader guards
for 2B. It sends only subscriptions, workspace/tree queries, and tick fences; no Sway
commands, application launches, focus changes, or desktop mutations. Tick fences are
visible to other tick subscribers. No code was run against a live compositor.

The event reader acknowledges successful workspace/window/tick subscriptions before any
snapshot query. One dedicated thread owns the blocking `swayipc` event stream. Its FIFO
channel retains at most 256 messages; overflow invalidates all history and is fatal.
The reader never blocks on channel sends. `Instant` deadlines bound consumer waits,
subscription acknowledgement, snapshot replies, and fence receipt. Idle expiry leaves
the reader intact, including partially received headers and payloads. A cloned socket
provides `shutdown(Both)` cancellation; dropping the observer shuts down and joins the
reader even during idle or incomplete reads. The first failure remains terminal.

Each snapshot uses a separate, disposable query connection and a temporary query thread.
At the shared deadline, socket shutdown interrupts blocked reads/writes; failed query
connections are never reused. Workspace identity is frozen from the initial focused
workspace; snapshots reject missing targets, duplicate identities, and inconsistent focus.
Focus-away events abort even if a subsequent event and snapshot show the target again.
History since subscription is checked against the initial target too: focus activity
during initialization may conservatively abort. Reloads and target destruction also fail.
Window events retain their decoded change/container data in wire order for future guards.

After workspace/tree queries, `SEND_TICK` on the query connection supplies a unique fence
on the subscribed event connection. Consuming through that tick establishes that earlier
events have been read, unlike draining a temporarily empty local channel. Initial and
other clients' ticks cannot satisfy the fence. Snapshot history is also capped at 256
events, failing rather than retaining unbounded history. This ordering follows Sway's
[IPC handler](https://github.com/swaywm/sway/blob/master/sway/ipc-server.c), reviewed on
2026-11-09: it queues tick events before replying to `SEND_TICK`, and appends each client's
outgoing messages in order. This source review does not establish a supported Sway version.

Technical conclusions from the actual locked `swayipc` 4.0.0 implementation:

- `common.rs::receive_from_stream` uses separate `read_exact` calls for the 14-byte header
  and declared payload, with local buffers only. A read timeout can consume part of either
  buffer; retrying `EventStream::next` would start a new header and corrupt framing.
  Polling with socket timeouts or nonblocking retries is therefore unsuitable. The
  blocking reader plus channel deadlines avoids that problem without another IPC client,
  async runtime, or production protocol implementation.
- `EventStream` owns a private socket and exposes no cancellation method. Retaining a
  `UnixStream::try_clone` before subscription permits shutdown without competing reads.
  Its iterator always returns `Some`, including on EOF/errors; stop at the first error.
- Invalid magic, wrong query reply types, JSON decoding failures, unknown events/changes,
  and truncated frames are fatal. A stalled incomplete frame cannot satisfy a snapshot
  fence. An idle consumer timeout alone cannot distinguish silence from a stalled frame;
  the next bounded snapshot/fence performs that liveness check.
- The crate is not a hardened protocol validator: it allocates the declared payload size,
  disables JSON recursion limits, accepts defaulted/optional fields (including absent
  `nodes`), does not check for trailing JSON, and masks rather than validates the event
  high bit. The observer checks decoded workspace/container/focus invariants but cannot
  recover discarded wire information. This foundation assumes a normal local Sway peer,
  not a malicious IPC server. If strict validation of every malformed payload or resource
  caps is required, the smallest next step is a targeted upstream/client patch exposing
  strict bounded decoding, rather than a second general IPC implementation.

Snapshots and events are not an atomic view. The fence covers emitted events through a
known point, not changes after it, and the tree may predate some returned window events.
2B must reconcile event history with fresh trees before launch/acceptance; never treat
snapshot success as ownership or mutation permission. Not all app-ID/class updates have
dedicated events: use periodic snapshots within the window deadline even while idle.
The reviewed [Wayland app-ID handler](https://github.com/swaywm/sway/blob/master/sway/desktop/xdg_shell.c)
and [Xwayland class handler](https://github.com/swaywm/sway/blob/master/sway/desktop/xwayland.c)
do not emit dedicated identity IPC events.
Deadlines cover established sockets and I/O waits, not `UnixStream::connect`, OS scheduling,
or arbitrary CPU/allocation time inside the upstream decoder. All threads are joined;
no background worker is deliberately detached.

Fourteen automated fixture tests use disposable `UnixStream::pair` sockets, never
`SWAYSOCK`. They cover subscriptions, away-and-back focus including initialization,
queued event order/fences, idle and expired deadlines, later events, fragmented headers
and payloads across idle deadlines, malformed/unknown/truncated frames, EOF, subscription
failure/stalls, queue overflow, query failure/stalls, missing/rejected fences, inconsistent
snapshots, delayed identity metadata without events, and idle/partial-read shutdown.
The example's test harness is enabled so ordinary `cargo test --locked` runs these tests.
`serde_json` is now an explicit development dependency for fixtures; it was already
locked through `swayipc`, with no new packages or production dependencies.
Local formatting, Clippy with warnings denied, all-target compilation, all 32 tests,
release build, and locked Rust 1.89 all-target compilation pass. Socket tests required
running outside the execution sandbox because it denied socket timeout configuration;
they still used only disposable socket pairs. Hosted CI and live Sway remain unverified.

## 2B. Safe single-window launching and diagnostics

Pending. Move the proven observation boundary into the binary when it has a production
caller. Implement guarded `load` for one window, keeping process launches separate from
Sway command strings. Add stage logging and bounded best-effort notifications with this
first load path. Establish tiled/floating emptiness and global preexisting identities,
then launch once and observe with periodic snapshots and event-history guards.

Complete when recorded trees/events and simulated IPC replies verify occupied-workspace
refusal, global new-window matching, delayed app IDs, repeated selectors, ambiguous and
wrong-workspace windows, floating/unexpected windows, workspace switches, application
timeouts, IPC failures, accepted-window loss, and partial failure without cleanup or
relaunch. No mutation may target preexisting windows. No automated test connects to the
user's Sway socket. Revisit the decoding limitations above if the required trust boundary
extends beyond a normal local compositor.

## 2C. Nested layout construction, sizing, and final focus

Pending. Extend the guarded single-window path to nested splits, relative sizing, and
default/overridden final focus. Verify nested ownership, command replies and observed
hierarchy, weight tolerance, bounded resizing, accepted-window loss, and interference
before every mutation. Keep all failures non-destructive.

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
document demonstrated limitations. The MIT license and repository URL are set.
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
| Native synchronous IPC | [swayipc 4.0.0](https://docs.rs/swayipc/4.0.0/swayipc/struct.Connection.html), from [swayipc-rs](https://github.com/JayceFayne/swayipc-rs), remains a development dependency. Its direct dependencies are Serde, serde_json, and swayipc-types. The locked graph compiles on Rust 1.89. Milestone 2A proves channel deadlines, cancellable blocking reads, and ordered event fences with fixtures; decoding limitations are recorded above. |
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

- Milestone 2A proves bounded event observation with fixtures; 2B/2C must still prove
  race-aware launch/mutation sequencing and reconcile event history with fresh snapshots.
  Strict wire-validation limits and unbounded socket establishment remain documented above.
- App IDs/classes cannot prove launch causality, and IPC cannot make observation and
  mutation atomic. Test conservative aborts; do not disguise these limits as recoverable
  placement errors. No live-session checks were performed during repository setup.
- Supported Sway version and Linux ABI baseline remain unset. Cargo publishing stays
  disabled until release behavior is ready.
