//! Development-only Sway integration harness, the Rust replacement for
//! `scripts/integration.py`.
//!
//! Only the user may run it, in an isolated, disposable Sway session; agents must never
//! execute it. The port is incomplete: bounded processes, `swaymsg` queries, event fences,
//! and snapshots exist, but scenarios, verification, cleanup, and restoration do not.

// Temporary: remove each allowance once the entry point uses its module. `expect` is not
// used because Rust 1.89 counts code reached from an `expect`ed module as used.
#[allow(dead_code, reason = "harness foundation is not connected to main yet")]
mod ipc;
#[allow(dead_code, reason = "harness foundation is not connected to main yet")]
mod observation;
#[allow(dead_code, reason = "harness foundation is not connected to main yet")]
mod process;

use std::process::ExitCode;

fn main() -> ExitCode {
    // Refuse before reading arguments, connecting to Sway, or starting any process.
    eprintln!(
        "integration: the Rust harness is incomplete and cannot run scenarios; \
         use scripts/integration.py"
    );
    ExitCode::FAILURE
}
