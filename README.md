# sway-stage

Turn an empty Sway workspace into a repeatable working layout.

`sway-stage` reads a YAML file, launches applications sequentially into nested tiles,
applies relative sizes, selects your final focus, and exits. Use it for arrangements
such as a browser beside a column of terminals, without scripting window placement.

Layouts load into the **currently focused, empty workspace**. Applications are launched
fresh; existing windows are never rearranged. Unexpected conditions cause a conservative
stop, leaving any applications already started in place.

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
side at a 2:1 width ratio, then focuses the right one. Split children can contain further
horizontal or vertical splits; see the [nested example](examples/four-windows.yaml).
Commands are literal argument lists, without shell expansion. Only load layouts you trust.

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
| `sway-stage capture > draft.yaml` | Capture the current arrangement as an editable draft. |
| `sway-stage menu` | Load a layout or save a named capture through Fuzzel. |

Use `--layouts-dir PATH` to read layouts and save menu captures elsewhere.
Captures preserve arrangement, sizes, selectors, and focus; they need commands filled in
before loading. `capture --infer-commands` and `menu --infer-commands` can recover process
arguments as a starting point. Review them for secrets and reproducibility: captures are
drafts, not executable session restorations.

## Documentation

- [User reference](docs/SPECIFICATION.md): YAML, capture workflow, menu, safety, troubleshooting.
- [Examples](examples): single-window, two-window, and nested layouts to adapt.
- [Development](docs/DEVELOPMENT.md): offline checks and user-only manual testing.
- [Roadmap](docs/PLAN.md): remaining release work.
- [Agent instructions](AGENTS.md): engineering and safety requirements.

Licensed under the [MIT License](LICENSE).
