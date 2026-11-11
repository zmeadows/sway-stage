//! Manual, development-only load/capture checks. Agents must NEVER execute this example,
//! including help or partial runs. Only the user may run it in a disposable Sway session.
//! `offline_tests.rs` tests the pure decisions without this entry point.

mod capture;
mod ipc;
mod layout;
mod lifecycle;
mod observation;
mod ownership;
mod preservation;
mod process;
mod scenarios;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    // A terminal child used only by process-execution. It never connects to Sway.
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new(process::PROBE_FLAG)) {
        std::thread::sleep(std::time::Duration::from_secs(300));
        return ExitCode::SUCCESS;
    }
    let arguments = lifecycle::Arguments::parse();
    if lifecycle::run(arguments) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
