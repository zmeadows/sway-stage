//! Development-only integration harness, started only by `tools/integration/run.sh`.
//! Agents must NEVER execute this example or that script, including help, filtered, or
//! partial runs. Only the user may run them. `offline_tests.rs` tests the pure decisions
//! without this entry point.
//!
//! One executable with three roles, chosen by the first argument:
//! - none, or `--filter TEXT`: the launcher, which starts a private headless Sway and runs
//!   the scenario runner inside it;
//! - `launcher::RUNNER_FLAG`: the scenario runner, which the launcher starts with the headless
//!   session's environment;
//! - `process::PROBE_FLAG`: a terminal child used by process-execution. It never connects
//!   to Sway.

mod capture;
mod headless;
mod ipc;
mod launcher;
mod layout;
mod lifecycle;
mod observation;
mod ownership;
mod preservation;
mod process;
mod profile;
mod scenarios;

use std::env;
use std::ffi::OsString;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

/// Long enough for process-execution to inspect the probe; window cleanup ends it sooner.
const PROBE_LIFETIME: Duration = Duration::from_secs(300);

fn main() -> ExitCode {
    let arguments: Vec<OsString> = env::args_os().skip(1).collect();
    let role = arguments.first().and_then(|first| first.to_str());

    match role {
        Some(process::PROBE_FLAG) => {
            thread::sleep(PROBE_LIFETIME);
            ExitCode::SUCCESS
        }
        Some(launcher::RUNNER_FLAG) => {
            let filter = match launcher::parse_filter(&arguments[1..]) {
                Ok(filter) => filter,
                Err(error) => {
                    eprintln!("FAIL  runner arguments: {error}");
                    return ExitCode::FAILURE;
                }
            };
            if lifecycle::run(filter.as_deref()) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        _ => launcher::run(&arguments),
    }
}
