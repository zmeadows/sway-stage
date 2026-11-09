# sway-stage

A lightweight declarative workspace launcher for the Sway window manager on Linux.

The planned executable will read YAML layouts and launch applications into nested,
proportionally sized tiles on the currently focused, empty workspace. It will leave
existing windows alone and stop if workspace focus changes during construction.

**Status:** specification and engineering baseline only. No CLI commands, YAML parsing,
Sway integration, or precompiled releases are implemented. The binary is a placeholder
that reports this status and exits unsuccessfully.

- [Specification](docs/SPECIFICATION.md): planned behavior and YAML format.
- [Roadmap](docs/PLAN.md): milestones and dependency review.
- [Four-window example](examples/four-windows.yaml): illustration of the planned format.
- [Engineering conventions](AGENTS.md): contributor guidance and safety invariants.

Building the scaffold requires Rust 1.89 or newer (edition 2024):

```sh
cargo build --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```
