//! Development-only Sway integration harness, the Rust replacement for
//! `scripts/integration.py`.
//!
//! Only the user may run it, in an isolated, disposable Sway session; agents must never
//! execute it. The port is incomplete: bounded processes, `swaymsg` queries, event fences,
//! snapshots, scenario definitions, and independent layout, ownership, and capture
//! verification exist, but scenario execution, cleanup, and restoration do not.
//! `offline_tests.rs` tests the pure logic without this entry point.

// Temporary: remove each allowance once the entry point uses its module. `expect` is not
// used because Rust 1.89 counts code reached from an `expect`ed module as used.
#[allow(dead_code, reason = "verification is not connected to main yet")]
mod capture;
#[allow(dead_code, reason = "harness foundation is not connected to main yet")]
mod ipc;
#[allow(dead_code, reason = "verification is not connected to main yet")]
mod layout;
#[allow(dead_code, reason = "harness foundation is not connected to main yet")]
mod observation;
#[allow(dead_code, reason = "verification is not connected to main yet")]
mod ownership;
#[allow(dead_code, reason = "harness foundation is not connected to main yet")]
mod process;
#[allow(dead_code, reason = "scenarios are not connected to main yet")]
mod scenarios;

use std::process::ExitCode;

fn main() -> ExitCode {
    // Refuse before reading arguments, connecting to Sway, or starting any process.
    eprintln!(
        "integration: the Rust harness is incomplete and cannot run scenarios; \
         use scripts/integration.py"
    );
    ExitCode::FAILURE
}
