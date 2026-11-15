use super::*;
use crate::fixtures::{FOOT, RUN_ID, SLEEP};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

const LAUNCH_SCRIPT: &str = "/private path/record-launch.sh";
const LAUNCH_RECORD: &str = "/private path/launches";
const MISSING_EXECUTABLE: &str = "/private path/missing-application";

fn fixture(case: Case) -> FailingLoad {
    failing_load(
        case,
        RUN_ID,
        FOOT,
        SLEEP,
        LAUNCH_SCRIPT,
        LAUNCH_RECORD,
        MISSING_EXECUTABLE,
    )
}

fn recorded(layout_name: &str, leaf_id: &str) -> Vec<String> {
    vec![
        "/bin/sh".into(),
        LAUNCH_SCRIPT.into(),
        format!("{layout_name}-{leaf_id}"),
        LAUNCH_RECORD.into(),
    ]
}

fn completed(code: i32, stderr: &str) -> CompletedProcess {
    CompletedProcess {
        status: ExitStatus::from_raw(code << 8),
        stdout: String::new(),
        stderr: stderr.into(),
    }
}

const PARTIAL_REPORT: &str = "sway-stage: wait leaf=\"window2\": \
     layout \"layout-partial-failure\": window timeout: after 5000ms waiting for \
     AppId(\"window2-id\"): no new windows observed. Launch process exited successfully \
     (delegation is allowed).; stopped without cleanup or relaunch; inspect windows before \
     retrying; log: /private/last-run.log\n";
const SPAWN_REPORT: &str = "sway-stage: launch leaf=\"window1\" workspace=4: \
     layout \"layout-missing-executable\": spawn application: No such file or directory \
     (os error 2); check executable, child PATH and cwd; log: /private/last-run.log\n";

#[test]
fn the_partial_layout_records_every_launch_and_starts_no_window_for_its_second_leaf() {
    let load = fixture(Case::PartialLaunch);
    let name = "layout-partial-failure";
    assert_eq!(load.layout.name, name);
    assert_eq!(load.layout.window_timeout_ms, PARTIAL_WINDOW_TIMEOUT_MS);

    let leaves = load.layout.layout.leaves();
    assert_eq!(leaves.len(), 3);
    for leaf in &leaves {
        assert_eq!(leaf.command[..4], recorded(name, &leaf.layout_id));
        assert!(leaf.app_id.contains(RUN_ID) && leaf.app_id.contains(name));
    }
    assert_eq!(leaves[0].command[4], FOOT);
    assert_eq!(leaves[1].command[4..], [SLEEP, "0"]);
    assert_eq!(leaves[2].command[4], FOOT);

    // Only the first window is accepted, focused, and left in place.
    let accepted_part = load.accepted_part.expect("the first window is accepted");
    let ConfiguredNode::Window(accepted_window) = &accepted_part.layout else {
        panic!("the accepted part is the first window alone");
    };
    assert_eq!(accepted_window.layout_id, "window1");
    assert_eq!(accepted_window.command, leaves[0].command);
    assert_eq!(accepted_part.expected_focus, "window1");
    assert_eq!(
        load.expected_launch_record,
        format!("{name}-window1\n{name}-window2\n")
    );
}

#[test]
fn the_missing_executable_is_spawned_directly_and_nothing_is_recorded() {
    let load = fixture(Case::MissingExecutable);
    let name = "layout-missing-executable";
    assert_eq!(load.layout.name, name);

    let leaves = load.layout.layout.leaves();
    assert_eq!(leaves.len(), 2);
    // No recorder or shell could start in its place and then fail.
    assert_eq!(leaves[0].command, [MISSING_EXECUTABLE]);
    assert_eq!(leaves[1].command[..4], recorded(name, "window2"));
    assert_eq!(leaves[1].command[4], FOOT);

    assert!(load.accepted_part.is_none());
    assert_eq!(load.expected_launch_record, "");
}

#[test]
fn a_partial_failure_requires_the_second_window_timeout_and_stop_advice() {
    assert_eq!(
        verify_failure_report(Case::PartialLaunch, &completed(1, PARTIAL_REPORT)),
        Ok(())
    );

    let without_stop_advice =
        PARTIAL_REPORT.replace("stopped without cleanup or relaunch", "stopped");
    let wrong_window = PARTIAL_REPORT.replace("window2", "window3");
    for (code, stderr) in [
        (2, PARTIAL_REPORT),
        (1, without_stop_advice.as_str()),
        (1, wrong_window.as_str()),
        (1, SPAWN_REPORT),
    ] {
        assert!(
            verify_failure_report(Case::PartialLaunch, &completed(code, stderr)).is_err(),
            "{code}: {stderr}"
        );
    }

    let mut with_stdout = completed(1, PARTIAL_REPORT);
    with_stdout.stdout = "unexpected\n".into();
    assert!(verify_failure_report(Case::PartialLaunch, &with_stdout).is_err());
}

#[test]
fn a_missing_executable_requires_the_spawn_failure_before_any_launch() {
    assert_eq!(
        verify_failure_report(Case::MissingExecutable, &completed(1, SPAWN_REPORT)),
        Ok(())
    );

    // Stop advice means an application started; a timeout means the spawn succeeded.
    let after_a_launch = SPAWN_REPORT.replace(
        "; log:",
        "; stopped without cleanup or relaunch; inspect windows before retrying; log:",
    );
    let wrong_window = SPAWN_REPORT.replace("window1", "window2");
    let other_layout = SPAWN_REPORT.replace("layout-missing-executable", "other");
    for (code, stderr) in [
        (2, SPAWN_REPORT),
        (1, after_a_launch.as_str()),
        (1, wrong_window.as_str()),
        (1, other_layout.as_str()),
        (1, PARTIAL_REPORT),
    ] {
        assert!(
            verify_failure_report(Case::MissingExecutable, &completed(code, stderr)).is_err(),
            "{code}: {stderr}"
        );
    }
}
