# sway-stage

Turn an empty Sway workspace into a repeatable working layout.

`sway-stage` reads a YAML file, launches applications sequentially into nested tiles,
applies relative sizes, selects your final focus, and exits. Use it for arrangements
such as a browser beside a column of terminals, without scripting window placement.

Ordinary `load` constructs one layout on the **currently focused, empty workspace**.
Named profiles sequentially construct layouts across multiple explicitly targeted, empty
Sway workspaces; see the [profile reference and example](docs/SPECIFICATION.md#named-profiles-version-1).
Applications are launched fresh; existing windows are never rearranged. Unexpected conditions
cause a conservative stop, leaving any applications already started in place.

## Get started

Requires Linux, a running Sway session with `SWAYSOCK` set, and Rust 1.89 or newer to
build. Install the applications your layouts use. Fuzzel 1.11+ is optional for the menu;
`notify-send` enables desktop notifications. The first public release is in preparation;
build from source:

```sh
git clone https://github.com/zmeadows/sway-stage.git
cd sway-stage
cargo build --release --locked
install -Dm755 target/release/sway-stage ~/.local/bin/sway-stage
```

Ensure `~/.local/bin` is on your Sway session's `PATH`.

Save this as `~/.config/sway-stage/layouts/dev.yaml` (create the directory first), or
under `$XDG_CONFIG_HOME/sway-stage/layouts` if you set `XDG_CONFIG_HOME`:

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

With [Foot](https://codeberg.org/dnkl/foot) installed, this opens two terminals side by
side at a 2:1 width ratio, then focuses the right one. Each `id` is an arbitrary layout-local name
used by `focus`; `match` identifies the actual Sway window by Wayland app ID or Xwayland class.
Commands are literal argument lists, without shell expansion. Only load layouts you trust.

Split children can contain further horizontal or vertical splits; see the
[nested example](examples/four-windows.yaml). Tabbed and stacked groups use `active` to
select a direct child (one-based, first by default). Default focus follows these selections;
explicit `focus` takes precedence. See the [mixed-groups example](examples/mixed-groups.yaml)
and [selection reference](docs/SPECIFICATION.md#yaml-format-version-1).

Validate it, then add a keybinding to `~/.config/sway/config` and reload your Sway config:

```sh
sway-stage check dev
```

```text
bindsym $mod+Shift+d exec sway-stage load dev
bindsym $mod+Shift+m exec sway-stage menu
```

Invoke Load on a completely empty workspace and keep it focused until construction ends.

## Commands

| Command | Purpose |
| --- | --- |
| `sway-stage list` | List layout names. |
| `sway-stage check NAME` | Validate without contacting Sway or launching anything. |
| `sway-stage load NAME` | Launch a layout on the focused, empty workspace. |
| `sway-stage profile check NAME` | Validate a profile and all referenced layouts without contacting Sway. |
| `sway-stage profile load NAME` | Launch layouts in order on the profile's explicitly targeted, empty workspaces. |
| `sway-stage capture > draft.yaml` | Capture the current arrangement as an editable draft. |
| `sway-stage menu` | Load a layout or save a named capture through Fuzzel. |

On an empty workspace, the menu shows valid layouts normally and marks invalid or
incomplete layouts `[invalid]`. Invalid layouts cannot launch; run `sway-stage check NAME`
for detailed validation errors. Validation does not guarantee applications will launch.

Use `--layouts-dir PATH` to read layouts and save menu captures elsewhere.
Captures preserve arrangement, sizes, selectors, tab/stack selections, and focus, and attempt to
reconstruct launch commands. Inference is best-effort; unresolved commands remain `null`.
Inspect and edit these drafts before loading. Review inferred commands before loading or
sharing: arguments may contain secrets or private paths and may not reproduce the application.
Use `--no-infer-commands` to leave all commands as `null`:

```sh
sway-stage capture
sway-stage capture --no-infer-commands

sway-stage menu
sway-stage menu --no-infer-commands
```

## Troubleshooting

Desktop error notifications give a short summary. Each load attempt (including a menu Load
selection or a whole profile load) records detailed diagnostics in
`$XDG_STATE_HOME/sway-stage/last-run.log`, or
`$HOME/.local/state/sway-stage/last-run.log` when `XDG_STATE_HOME` is unset or empty.
The state directory must be absolute. Inspect the latest load with:

```sh
cat "${XDG_STATE_HOME:-$HOME/.local/state}/sway-stage/last-run.log"
```

Capture, menu cancellation, and menu errors before loading preserve the previous log;
use terminal stderr for their details. If logging is unavailable, diagnostics go to stderr.
See the [troubleshooting reference](docs/SPECIFICATION.md#diagnostics-and-troubleshooting)
for common causes.

## Documentation

- [User reference](docs/SPECIFICATION.md): YAML, capture workflow, menu, safety, troubleshooting.
- Reference configurations to adapt to your applications and directories:
  [two terminals](examples/two-foot.yaml), [nested splits](examples/four-windows.yaml),
  [mixed native groups](examples/mixed-groups.yaml), and
  [working-directory selection](examples/working-directory.yaml).
- [Development](docs/DEVELOPMENT.md): offline checks and user-only manual testing.
- [Roadmap](docs/PLAN.md): remaining release work.
- [Agent instructions](AGENTS.md): engineering and safety requirements.

Licensed under the [MIT License](LICENSE).
