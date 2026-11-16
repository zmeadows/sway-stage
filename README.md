# sway-stage

[![CI](https://github.com/zmeadows/sway-stage/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/zmeadows/sway-stage/actions/workflows/ci.yml?query=branch%3Amain) [![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Launch applications into a repeatable tiled layout on an **empty Sway workspace**.
`sway-stage` reads YAML, opens each application in order, constructs nested horizontal,
vertical, tabbed, or stacked groups, sizes split children, selects final focus, and exits.
It can also capture layouts as editable drafts and load named multi-workspace profiles.

It does **not** rearrange existing windows or restore an application's session. On an
unexpected condition it stops without closing any launched windows. See the
[behavior and safety reference](docs/REFERENCE.md#loading-and-safety).

## Install

Requires Linux with glibc 2.34+, a running Sway session (`SWAYSOCK`), and Rust 1.89+
to build. Install the applications your layouts will launch. [Fuzzel](https://codeberg.org/dnkl/fuzzel)
1.11+ is optional for the interactive menu; `notify-send` enables desktop notifications.
The first public release is in preparation; for now build from source:

```sh
git clone https://github.com/zmeadows/sway-stage.git
cd sway-stage
cargo build --release --locked
install -Dm755 target/release/sway-stage ~/.local/bin/sway-stage
```

Ensure `~/.local/bin` is on your Sway session's `PATH`. See
[platform limitations](docs/REFERENCE.md#platform-compatibility) before distributing binaries.

## First layout

Save as `~/.config/sway-stage/layouts/dev.yaml` (create the directory first):

```yaml
version: 1
focus: shell
layout:
  group: splith
  children:
    - id: notes
      weight: 2
      command: [foot, --app-id=stage-notes]
      match: {app_id: stage-notes}
    - id: shell
      command: [foot, --app-id=stage-shell]
      match: {app_id: stage-shell}
```

With Foot installed, this starts two terminals side by side, with a 2:1 width ratio,
then focuses `shell`. The `id` is local to the layout; `match` is the window's exact
Wayland app ID (or Xwayland class). Commands are argument arrays, **not shell strings**.
Only run layout files you trust.

```sh
sway-stage check dev
```

Add a Sway binding and reload your Sway configuration:

```text
bindsym $mod+Shift+m exec sway-stage menu
```

Load on a **completely empty workspace**, and keep that workspace focused until
construction finishes. A terminal in the target workspace counts as occupancy.
For nested groups, selection, and working directories see the
[layout format](docs/REFERENCE.md#layout-yaml).

## Commands

| Command | Purpose |
| --- | --- |
| `sway-stage list` | List discovered layouts. |
| `sway-stage check NAME` | Validate a layout offline. |
| `sway-stage load NAME [--cwd PATH]` | Build a layout on the focused empty workspace. |
| `sway-stage capture [--no-infer-commands]` | Print an editable draft of the focused workspace. |
| `sway-stage profile list` | List named profiles. |
| `sway-stage profile show NAME` | Display workspace mappings without loading layouts. |
| `sway-stage profile check NAME` | Validate a profile and its layouts offline. |
| `sway-stage profile load NAME` | Build layouts on the profile's target workspaces in order. |
| `sway-stage profile capture NAME 1 3 music` | Save multiple existing workspaces as drafts and a profile. |
| `sway-stage profile capture NAME --all` | Capture all populated ordinary workspaces. |
| `sway-stage menu [--no-infer-commands]` | Choose layout/profile loading or capture via Fuzzel. |

Profiles live in `~/.config/sway-stage/profiles/` by default; layouts in
`~/.config/sway-stage/layouts/`. Global `--layouts-dir` and `--profiles-dir` can override
their respective directories. The [profile reference](docs/REFERENCE.md#profiles)
explains numeric versus named workspaces, output checks, and preflight behavior.

Capture is read-only with respect to Sway. A draft preserves the layout, sizes,
tab/stack selections, and focus, but commands are inferred only **best-effort**.
Unresolved fields remain `null`. Inspect inferred arguments for secrets, complete
missing fields, then run `check` before loading. Use `--no-infer-commands` to suppress
command inference. [Capture details](docs/REFERENCE.md#capturing-layouts-and-profiles).

## Troubleshooting

Errors go to stderr, and loads write a detailed log at
`${XDG_STATE_HOME:-$HOME/.local/state}/sway-stage/last-run.log`:

```sh
cat "${XDG_STATE_HOME:-$HOME/.local/state}/sway-stage/last-run.log"
```

Loading never rolls back. After failure, inspect any remaining application windows
before retrying. Common causes include a nonempty target, an inaccurate `app_id`/`class`,
Sway window-assignment rules, and workspace or output changes during loading. See
[diagnostics](docs/REFERENCE.md#diagnostics-and-troubleshooting).

## Documentation

- [User reference](docs/REFERENCE.md): full YAML, CLI, profiles, capture, menu, and safety behavior.
- [Development](docs/DEVELOPMENT.md): offline checks, isolated headless suite, manual acceptance, release checklist.
- [Internals](docs/INTERNALS.md): architectural rationale and non-obvious constraints for maintainers and agents.
- [Examples](examples/): [two terminals](examples/two-foot.yaml), [nested splits](examples/four-windows.yaml), [mixed groups](examples/mixed-groups.yaml), [working directories](examples/working-directory.yaml).
- [Agent instructions](AGENTS.md): engineering and live-desktop safety requirements.

Licensed under the [MIT License](LICENSE).
