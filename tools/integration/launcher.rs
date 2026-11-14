//! The integration suite's only entry point, run by `tools/integration/run.sh`. It parses
//! the interface, starts a private headless compositor, runs the scenario runner as a child
//! inside that compositor's environment, and stops the compositor on every outcome.
//!
//! The launcher itself never talks to the scenarios' compositor except through
//! `HeadlessSession`, and never starts anything with its own, inherited environment.

use std::env;
use std::ffi::OsString;
use std::path::Path;
use std::process::{Child, ExitCode, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::headless::{HeadlessSession, OUTPUT_NAME, SessionDirectory, VIRTUAL_OUTPUT_RECT};
use crate::{lifecycle, process};

/// The private first argument that selects the scenario runner. Only `run` passes it, through
/// the session's command environment; the runner still verifies the compositor itself.
pub const RUNNER_FLAG: &str = "--integration-headless-runner";
const USAGE: &str = "usage: tools/integration/run.sh [--filter TEXT]";
/// Distinguishes a usage error from a failed run.
const USAGE_EXIT_CODE: u8 = 2;

/// Backstops only: every runner operation has its own deadline, so a passing or failing
/// runner exits long before these. The setup allowance covers checks and the fixture.
const RUNNER_SETUP_ALLOWANCE: Duration = Duration::from_secs(5 * 60);
/// Per selected scenario, generous enough for replays and slow window starts.
const RUNNER_ALLOWANCE_PER_SCENARIO: Duration = Duration::from_secs(5 * 60);
/// After Ctrl-C the runner stops at its next interruption check, normally within a second.
const RUNNER_INTERRUPT_GRACE: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The entire interface: no arguments, or `--filter TEXT`.
pub fn parse_filter(arguments: &[OsString]) -> Result<Option<String>, String> {
    match arguments {
        [] => Ok(None),
        [option, text] if option == "--filter" => {
            let text = text.to_str().ok_or("--filter TEXT must be UTF-8")?;
            if text.is_empty() {
                return Err("--filter TEXT must not be empty".into());
            }
            Ok(Some(text.to_owned()))
        }
        [option] if option == "--filter" => Err("--filter requires TEXT".into()),
        _ => Err(format!("unsupported arguments {arguments:?}")),
    }
}

/// Succeeds only when every selected scenario passed and the compositor stopped cleanly.
pub fn run(arguments: &[OsString]) -> ExitCode {
    let filter = match parse_filter(arguments) {
        Ok(filter) => filter,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            return ExitCode::from(USAGE_EXIT_CODE);
        }
    };
    // A filter that matches nothing is refused before any compositor starts.
    let selected = match lifecycle::select_scenarios(filter.as_deref()) {
        Ok(selected) => selected,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            return ExitCode::from(USAGE_EXIT_CODE);
        }
    };
    println!(
        "Selected {} of {} scenarios.",
        selected.len(),
        lifecycle::suite_scenario_names().len()
    );

    // As in the runner, Ctrl-C only records a request; ordinary control flow stops.
    if let Err(error) = process::install_sigint_handler() {
        eprintln!("FAIL  setup: {error}");
        return ExitCode::FAILURE;
    }
    let harness_executable = match env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("FAIL  setup: locate the harness executable: {error}");
            return ExitCode::FAILURE;
        }
    };
    let directory = match SessionDirectory::create() {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("FAIL  session directory: {error}");
            return ExitCode::FAILURE;
        }
    };

    let selected_count = u32::try_from(selected.len()).expect("the suite has few scenarios");
    let runner_timeout = RUNNER_SETUP_ALLOWANCE + RUNNER_ALLOWANCE_PER_SCENARIO * selected_count;
    let session_result = run_in_headless_session(
        &directory,
        &harness_executable,
        filter.as_deref(),
        runner_timeout,
    );
    if let Err(error) = session_result {
        eprintln!("FAIL  {error}");
        eprintln!(
            "Diagnostics preserved in {}: sway.log, final-tree.json, \
             tmp/sway-stage-integration-*/ (scenario YAML), and \
             home/.local/state/sway-stage/last-run.log",
            directory.path().display()
        );
        return ExitCode::FAILURE;
    }

    if let Err(error) = directory.remove() {
        eprintln!("FAIL  cleanup: {error}");
        return ExitCode::FAILURE;
    }
    println!("Headless Sway stopped; session files removed.");
    ExitCode::SUCCESS
}

/// Starts the compositor, runs the scenario runner in it, and stops the compositor on every
/// outcome.
fn run_in_headless_session(
    directory: &SessionDirectory,
    harness_executable: &Path,
    filter: Option<&str>,
    runner_timeout: Duration,
) -> Result<(), String> {
    let session = HeadlessSession::start(directory).map_err(|error| format!("startup: {error}"))?;
    let output = VIRTUAL_OUTPUT_RECT;
    println!(
        "Headless Sway ready: pid {}, output {OUTPUT_NAME} {}x{} at {},{}, WAYLAND_DISPLAY={}",
        session.compositor_pid(),
        output.width,
        output.height,
        output.x,
        output.y,
        session.wayland_display().display()
    );
    println!("      session directory {}", directory.path().display());

    let runner_result = run_scenario_runner(&session, harness_executable, filter, runner_timeout);
    let shutdown_result = session.stop();

    match (runner_result, shutdown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(runner_error), Ok(())) => Err(runner_error),
        (Ok(()), Err(shutdown_error)) => Err(format!("shutdown: {shutdown_error}")),
        (Err(runner_error), Err(shutdown_error)) => {
            Err(format!("{runner_error}; shutdown: {shutdown_error}"))
        }
    }
}

/// The runner is this executable in runner mode, started through `session.command` so that
/// it, sway-stage, and every application see only the private compositor. It shares this
/// terminal for its PASS/FAIL lines.
fn run_scenario_runner(
    session: &HeadlessSession,
    harness_executable: &Path,
    filter: Option<&str>,
    timeout: Duration,
) -> Result<(), String> {
    let mut command = session.command(harness_executable);
    command.arg(RUNNER_FLAG);
    if let Some(text) = filter {
        command.args(["--filter", text]);
    }
    command.stdin(Stdio::null());

    // Deliberately not `process::spawn`, which isolates a child's process group: the runner
    // stays in this group, so Ctrl-C reaches it directly and it reports the interrupted
    // stage itself. sway-stage and its applications remain isolated in their own groups.
    // That also bypasses `spawn`'s interruption check, so repeat it: a runner started after
    // Ctrl-C never received that signal and would keep running scenarios.
    if process::interruption_requested() {
        return Err("interrupted before the scenario runner started".into());
    }
    let mut runner = command
        .spawn()
        .map_err(|error| format!("start scenario runner: {error}"))?;

    let status = wait_for_runner(&mut runner, timeout)?;
    if !status.success() {
        return Err(format!(
            "scenario runner failed ({status}); see its output above"
        ));
    }
    Ok(())
}

/// Waits for the runner to exit by itself. After the overall deadline, or a grace period
/// after Ctrl-C, it kills the runner, our direct child, and nothing else.
fn wait_for_runner(runner: &mut Child, timeout: Duration) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + timeout;
    let mut interrupt_deadline: Option<Instant> = None;
    loop {
        let exited = runner
            .try_wait()
            .map_err(|error| format!("wait for scenario runner: {error}"))?;
        if let Some(status) = exited {
            return Ok(status);
        }

        if interrupt_deadline.is_none() && process::interruption_requested() {
            interrupt_deadline = Some(Instant::now() + RUNNER_INTERRUPT_GRACE);
        }
        let now = Instant::now();
        let overdue_reason = if now >= deadline {
            Some(format!("did not finish within {timeout:?}"))
        } else if interrupt_deadline.is_some_and(|grace_end| now >= grace_end) {
            Some(format!(
                "did not stop within {RUNNER_INTERRUPT_GRACE:?} of the interrupt"
            ))
        } else {
            None
        };
        if let Some(reason) = overdue_reason {
            process::kill_and_reap(runner, "scenario runner")?;
            return Err(format!("scenario runner {reason} and was killed"));
        }

        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
#[path = "launcher_tests.rs"]
mod tests;
