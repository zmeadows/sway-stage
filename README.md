# sway-stage

A lightweight declarative workspace launcher for the Sway window manager on Linux.

`sway-stage` reads a named YAML layout, launches its applications one at a time into
ordered, nested horizontal and vertical tiles on the currently focused, empty workspace,
applies relative sizes, focuses the window you choose, and exits. Use it to turn an empty
workspace into a repeatable arrangement, such as a browser beside a column of terminals,
without hand-written `swaymsg` sequences.

It is deliberately cautious. It never touches windows it did not launch, and it stops
instead of guessing when something unexpected happens: the workspace is occupied, focus
moves away, or a window appears in the wrong place.

**Status:** early development; there is no release yet. `load`, `list`, `check`,
`capture`, and `menu` are implemented. Automated tests run offline against recorded Sway IPC
traffic, and the included examples have been loaded manually on Sway; no minimum Sway
version is established yet.

## Requirements

- Linux and a running Sway session, with `SWAYSOCK` set (Sway sets it).
- Rust 1.89 or newer to build.
- The applications your layouts launch.
- Optional: Fuzzel 1.11 or newer for `menu`; `notify-send` for desktop notifications.

## Build

```sh
git clone https://github.com/zmeadows/sway-stage.git
cd sway-stage
cargo build --release --locked
```

The executable is `target/release/sway-stage`. Copy it to a directory on your `PATH` to
use it from Sway keybindings.

## Layouts

Each layout is one `NAME.yaml` or `NAME.yml` file in `$XDG_CONFIG_HOME/sway-stage/layouts`
(by default `~/.config/sway-stage/layouts`). `--layouts-dir PATH` reads another directory.
Names use `[A-Za-z0-9][A-Za-z0-9_-]*`. For example, `dev.yaml`:

```yaml
version: 1
focus: editor # optional; defaults to the first window
layout:
  split: horizontal # left to right; `vertical` is top to bottom
  children:
    - id: browser
      weight: 3 # 3/5 of the width; weights are relative to siblings
      command: [firefox, --new-window]
      match:
        app_id: firefox
    - split: vertical
      weight: 2
      children:
        - id: editor
          command: [foot, --app-id=dev-editor]
          match:
            app_id: dev-editor
        - id: shell
          command: [foot, --app-id=dev-shell]
          match:
            app_id: dev-shell
```

- A split has `split: horizontal` or `split: vertical` and at least two ordered
  `children`, which may themselves be splits.
- A window has a unique `id`, a `command` given as an argument list, and a `match` with
  exactly one exact Wayland `app_id` or Xwayland `class`. Optional `cwd` (relative to the
  layout file) and `env` apply only to that application.
- Any node may have a positive `weight` (default `1`).
- `window_timeout_ms` sets how long to wait for each window (default `20000`, at most
  `300000`).

Commands run directly, without a shell: there is no variable expansion, `~` expansion, or
globbing. Layout files are executable instructions, so only load layouts you trust. The
[`examples`](examples) directory has single-window, two-window, and four-window layouts,
and the [specification](docs/SPECIFICATION.md) defines the complete format.

## Usage

```sh
sway-stage list          # print layout names; does not read their contents
sway-stage check dev     # validate without contacting Sway or launching anything
sway-stage load dev      # build the layout on the focused, empty workspace
sway-stage capture > draft.yaml  # draft a layout from the focused workspace
sway-stage menu                 # load a layout or save a named capture through Fuzzel
sway-stage menu --infer-commands # opt into command inference for menu captures
```

`load` needs the focused workspace to be completely empty, so run it from a Sway
keybinding, or from a terminal on another workspace after focusing an empty one:

```text
# ~/.config/sway/config
bindsym $mod+Shift+d exec sway-stage load dev
```

```sh
swaymsg workspace 9 && sway-stage load dev
```

Keep the target workspace focused until `sway-stage` exits. A successful `load` prints
nothing and exits 0; failures exit 1 and invalid usage exits 2.

`menu` lists `Load: NAME` entries and a `Capture current layout…` action, even if no
layouts exist. Loading still requires an empty workspace at load time. Capture saves the
arrangement from before the menu opened; later focus changes do not alter it. Both prompts
use the original monitor when known. After asking for a name, it saves `NAME.yaml` in the
layouts directory: `--layouts-dir PATH` if given, otherwise
`$XDG_CONFIG_HOME/sway-stage/layouts` (by default `~/.config/sway-stage/layouts`). Names
use the layout-name grammar, at most 250 characters; existing `.yaml` or `.yml` layouts are
never overwritten. Escape at either prompt cancels without saving or loading. For example:

```text
bindsym $mod+Shift+m exec sway-stage menu
```

A saved capture is a draft: it appears in `list` and the menu immediately, but `check` and
`load` reject it until you edit it in place, review any inferred commands, and fill
remaining placeholders. Run `sway-stage check NAME` before loading it. Command inference
stays opt-in.

### Capturing an arrangement

`capture` takes one snapshot of the focused workspace and prints a layout draft to stdout.
Later focus or desktop changes do not invalidate it. It only reads
Sway: nothing is launched, moved, resized, or focused, and no file is written except
where you redirect stdout. The draft keeps the split hierarchy, window order, sizes as
percentage weights, each window's exact app ID (or Xwayland class), and the focused
window. Windows are named `window1`, `window2`, ... in layout order.

Default capture gives every window `command: null`; a window without an identifier
has `match: null`. Optional `capture --infer-commands` recovers current Linux process
arguments where possible, keeping nulls on failure. Review recovered commands before
executing or sharing: they may contain secrets and may not reproduce the application.
`check` and `load` reject remaining placeholders until you replace them, for example with `command: [foot, --app-id=notes]`
and `match: {app_id: notes}`. Run `capture` from a keybinding, from a terminal that is
part of the arrangement, or from elsewhere with a delay:

```sh
sleep 5; sway-stage capture > notes.yaml  # meanwhile, switch to the arranged workspace
sway-stage --layouts-dir . check notes    # after replacing the placeholders
```

Then move the file into your layouts directory.

Capture refuses an empty workspace and anything the format cannot express: floating,
fullscreen, tabbed, or stacked windows, or focus on a container rather than a window. It
also stops if windows open, close, move, or change focus while it reads. On failure it
prints nothing to stdout.

## Restrictions and limitations

- `sway-stage` never chooses, creates, or switches workspaces or outputs. Any tiled or
  floating occupant of the target, even an empty container, makes `load` refuse to start.
- Switching away from the target, even briefly, stops the run.
- Only windows that appear after each launch can match. A matching window on another
  workspace, two new matching windows, a matching window that is floating or fullscreen,
  or an unexpected window entering the target stops the run. Give each window a
  distinctive app ID where the application supports it (for example `foot --app-id=...`),
  and avoid launching other windows with the same ID at the same time: an ID cannot prove
  which process made a window.
- Any window move event, even on another workspace, stops the run, because Sway's move
  events do not say which workspaces were involved.
- On any failure, `sway-stage` stops launching and rearranging. Applications it already
  started and any partial layout stay in place; nothing is closed, moved, or retried.
  Inspect the workspace before trying again.
- Applications are started directly rather than through Sway's `exec`. Sway places each
  window by its normal rules, so assignment rules, inherited activation or launch context,
  and single-instance applications can put a window elsewhere; `sway-stage` then stops.
- Sizes are approximate. Each child is adjusted to within about 1 percentage point of its
  share, in at most three passes per split. Gaps, borders, and minimum sizes can prevent
  that; deviations beyond 5 points produce a warning, not a failure.
- Only horizontal and vertical splits are supported; there are no tabbed, stacked, or
  floating layouts, no session restoration, and no reuse of existing windows.

## Diagnostics

Errors from `load`, `menu`, and `capture` go to stderr and, when `notify-send` is available, to a
desktop notification. Each loading attempt (`load` or a menu Load selection) replaces a
private log at `$XDG_STATE_HOME/sway-stage/last-run.log` (by default
`~/.local/state/sway-stage/last-run.log`) recording stages, container IDs, Sway commands
and replies, and sizing results. It omits window titles, full command arguments, and
environment values. Menu cancellation, selection errors, and captures leave the launch
log alone; standalone `capture` never writes or replaces it. `list`, `check`, `--help`,
and `--version` never contact Sway, write logs, or send notifications.

## Documentation

- [Specification](docs/SPECIFICATION.md): commands, layout format, and safety behavior.
- [Roadmap](docs/PLAN.md): implementation status, design decisions, Sway findings, and
  the manual verification procedure.
- [Examples](examples): layouts to check and adapt.
- [Contributor guidance](AGENTS.md): engineering standards, safety invariants, and the
  required checks. Automated tests never contact a running Sway session.

Licensed under the [MIT License](LICENSE).
