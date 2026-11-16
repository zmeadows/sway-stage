//! Development-only integration harness, started only by `tools/integration/run.sh`, which
//! contributors and agents may run (agents under AGENTS.md). Never invoke this executable
//! or its runner and probe roles directly. `offline_tests.rs` tests the pure decisions
//! without this entry point.
//!
//! One executable with three roles, chosen by the first argument:
//! - none, or `--filter TEXT`: the launcher, which starts a private headless Sway for each
//!   output setup with selected scenarios and runs the scenario runner inside it;
//! - `launcher::RUNNER_FLAG` or `launcher::TWO_OUTPUT_RUNNER_FLAG`: the scenario runner for
//!   that setup, which the launcher starts with the headless session's environment;
//! - `process::PROBE_FLAG`: a terminal child used by process-execution. It never connects
//!   to Sway.

mod alternating_profile;
mod capture;
mod floating_refusal;
mod headless;
mod held_launch;
mod interference;
mod ipc;
mod launch_context;
mod launcher;
mod layout;
mod lifecycle;
mod load_failure;
mod observation;
mod output_expectation;
mod output_migration;
mod ownership;
mod preservation;
mod process;
mod profile;
mod scenarios;
mod secondary_output;
mod session_roundtrip;

use std::env;
use std::ffi::OsString;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use headless::OutputSetup;

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
        Some(launcher::RUNNER_FLAG) => run_scenarios(OutputSetup::OneOutput, &arguments[1..]),
        Some(launcher::TWO_OUTPUT_RUNNER_FLAG) => {
            run_scenarios(OutputSetup::TwoOutputs, &arguments[1..])
        }
        _ => launcher::run(&arguments),
    }
}

fn run_scenarios(setup: OutputSetup, arguments: &[OsString]) -> ExitCode {
    let filter = match launcher::parse_filter(arguments) {
        Ok(filter) => filter,
        Err(error) => {
            eprintln!("FAIL  runner arguments: {error}");
            return ExitCode::FAILURE;
        }
    };
    if lifecycle::run(setup, filter.as_deref()) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
