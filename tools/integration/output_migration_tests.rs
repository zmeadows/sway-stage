use super::*;
use crate::fixtures::{
    FOOT, OTHER_WORKSPACE_ID, RUN_ID, SECOND_OUTPUT_WORKSPACE_ID, SLEEP, assert_contains, node_mut,
    parse_tree, rect, two_output_desktop, two_output_outputs, two_output_workspaces_reply,
    wayland_window,
};
use crate::headless::{
    SECOND_OUTPUT_ASSIGNED_WORKSPACE, SECOND_OUTPUT_INITIAL_WORKSPACE, TWO_OUTPUT_WORKSPACE_RULES,
};
use crate::ipc::WorkspaceEvent;
use crate::output_expectation::{SECOND_UNPLACED_WORKSPACE, UNPLACED_WORKSPACE};
use crate::profile::verify_workspace_events;
use serde_json::{Value, json};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

const LAUNCH_SCRIPT: &str = "/private path/record-launch.sh";
const LAUNCH_RECORD: &str = "/private path/launches";
/// The output container IDs in `two_output_desktop`.
const FIRST_OUTPUT_ID: i64 = 4;
const SECOND_OUTPUT_ID: i64 = 12;
const TARGET_ID: i64 = 30;

fn hold_prefix() -> Vec<String> {
    vec!["/bin/sh".into(), "/private path/hold.sh".into()]
}

fn fixture_workspace() -> WorkspaceIdentity {
    WorkspaceIdentity {
        id: OTHER_WORKSPACE_ID,
        name: "1".into(),
    }
}

#[test]
fn the_target_is_created_on_the_focused_first_output_by_no_rule() {
    assert!(
        TWO_OUTPUT_WORKSPACE_RULES
            .iter()
            .all(|(ruled_workspace, _)| *ruled_workspace != TARGET_WORKSPACE)
    );
    assert_ne!(ORIGINAL_OUTPUT, DESTINATION_OUTPUT);
    // No other two-output scenario uses the target.
    for other_scenario_workspace in [
        SECOND_OUTPUT_INITIAL_WORKSPACE,
        SECOND_OUTPUT_ASSIGNED_WORKSPACE,
        UNPLACED_WORKSPACE,
        SECOND_UNPLACED_WORKSPACE,
    ] {
        assert_ne!(TARGET_WORKSPACE, other_scenario_workspace);
    }
    for entry in &crate::alternating_profile::ENTRIES {
        assert_ne!(TARGET_WORKSPACE, entry.workspace);
    }
}

#[test]
fn only_the_second_launch_is_held_and_the_first_window_is_accepted_alone() {
    let migrating = migrating_load(
        RUN_ID,
        FOOT,
        SLEEP,
        LAUNCH_SCRIPT,
        LAUNCH_RECORD,
        &hold_prefix(),
    );
    let leaves = migrating.layout.layout.leaves();
    assert_eq!(leaves.len(), 3);
    assert_eq!(
        migrating.launch_record_through_hold,
        format!("{SCENARIO}-window1\n{SCENARIO}-window2\n")
    );

    // The launch recorder runs first, then only the held window waits for the harness.
    for leaf in &leaves {
        assert_eq!(
            &leaf.command[..4],
            &[
                "/bin/sh",
                LAUNCH_SCRIPT,
                &format!("{SCENARIO}-{}", leaf.layout_id),
                LAUNCH_RECORD
            ]
        );
        let held = leaf.command[4..].starts_with(&hold_prefix());
        assert_eq!(held, leaf.layout_id == HELD_LAYOUT_ID, "{}", leaf.layout_id);
        assert!(leaf.app_id.contains(RUN_ID) && leaf.app_id.contains(SCENARIO));
    }

    let ConfiguredNode::Window(accepted) = &migrating.accepted_before_move.layout else {
        panic!("the first window is accepted alone");
    };
    assert_eq!(accepted.layout_id, "window1");
    assert_eq!(accepted.command, leaves[0].command);
    assert_eq!(migrating.accepted_before_move.expected_focus, "window1");
}

fn snapshot(tree: Value, focused_name: &str, focused_workspace_id: i64) -> Snapshot {
    Snapshot {
        tree: parse_tree(tree),
        workspaces: two_output_workspaces_reply(focused_name),
        focused_workspace_id,
        window_events: Vec::new(),
        workspace_events: Vec::new(),
    }
}

#[test]
fn the_baseline_is_the_fixture_on_the_first_output_and_one_empty_shown_workspace() {
    // Focus may be on either workspace: an earlier cross-output profile leaves it on the
    // second output's.
    for (focused_name, focused_id) in [("1", OTHER_WORKSPACE_ID), ("2", SECOND_OUTPUT_WORKSPACE_ID)]
    {
        let observed = snapshot(two_output_desktop(focused_id), focused_name, focused_id);
        let baseline =
            MigrationBaseline::record(&observed, &two_output_outputs(), &fixture_workspace())
                .unwrap();
        assert_eq!(baseline.original_output_id, FIRST_OUTPUT_ID);
        assert_eq!(baseline.destination_output_id, SECOND_OUTPUT_ID);
        assert_eq!(
            baseline.destination_shown_workspace,
            WorkspaceIdentity {
                id: SECOND_OUTPUT_WORKSPACE_ID,
                name: "2".into()
            }
        );
    }

    let fresh = || {
        snapshot(
            two_output_desktop(OTHER_WORKSPACE_ID),
            "1",
            OTHER_WORKSPACE_ID,
        )
    };
    let record = |observed: &Snapshot, outputs: &[Output], fixture: &WorkspaceIdentity| {
        MigrationBaseline::record(observed, outputs, fixture).map(|_| ())
    };

    // The destination's workspace must be empty and shown there.
    let mut occupied_tree = two_output_desktop(OTHER_WORKSPACE_ID);
    node_mut(&mut occupied_tree, SECOND_OUTPUT_WORKSPACE_ID)["nodes"] =
        json!([wayland_window(40, "app", rect(2560, 0, 1920, 1080))]);
    let occupied = snapshot(occupied_tree, "1", OTHER_WORKSPACE_ID);
    assert_contains(
        &record(&occupied, &two_output_outputs(), &fixture_workspace()).unwrap_err(),
        "must be empty",
    );
    let mut not_shown = two_output_outputs();
    not_shown[1].current_workspace = Some("3".into());
    assert_contains(
        &record(&fresh(), &not_shown, &fixture_workspace()).unwrap_err(),
        "must show workspace",
    );

    // Another fixture identity, or the fixture's workspace on the second output, fails.
    let other_fixture = WorkspaceIdentity {
        id: SECOND_OUTPUT_WORKSPACE_ID,
        name: "2".into(),
    };
    assert!(record(&fresh(), &two_output_outputs(), &other_fixture).is_err());
    let missing_fixture = WorkspaceIdentity {
        id: 99,
        name: "1".into(),
    };
    assert_contains(
        &record(&fresh(), &two_output_outputs(), &missing_fixture).unwrap_err(),
        "and one other",
    );
}

fn workspace_event(change: &str, id: i64, name: &str) -> WorkspaceEvent {
    serde_json::from_value(json!({
        "change": change,
        "current": {"type": "workspace", "id": id, "name": name, "num": name.parse::<i32>().unwrap()},
        "old": null,
    }))
    .expect("a workspace event in Sway's format")
}

#[test]
fn the_move_removes_the_destinations_shown_workspace_then_reports_the_target() {
    let shown = WorkspaceIdentity {
        id: SECOND_OUTPUT_WORKSPACE_ID,
        name: "2".into(),
    };
    let target = WorkspaceIdentity {
        id: TARGET_ID,
        name: TARGET_WORKSPACE.into(),
    };
    let expected = move_events(&shown, &target);
    let observed = || {
        vec![
            workspace_event("empty", SECOND_OUTPUT_WORKSPACE_ID, "2"),
            workspace_event("move", TARGET_ID, TARGET_WORKSPACE),
        ]
    };
    verify_workspace_events(&observed(), &expected).unwrap();

    // Without the removal, in the other order, for another workspace, or with a created
    // replacement on the original output, the move is not the expected one.
    assert!(verify_workspace_events(&observed()[1..], &expected).is_err());
    let mut reordered = observed();
    reordered.reverse();
    assert!(verify_workspace_events(&reordered, &expected).is_err());
    let mut other_moved = observed();
    other_moved[1] = workspace_event("move", OTHER_WORKSPACE_ID, "1");
    assert!(verify_workspace_events(&other_moved, &expected).is_err());
    let mut replacement = observed();
    replacement.push(workspace_event("init", 31, "3"));
    assert!(verify_workspace_events(&replacement, &expected).is_err());
}

fn completed_process(code: i32, stderr: &str) -> CompletedProcess {
    CompletedProcess {
        status: ExitStatus::from_raw(code << 8),
        stdout: String::new(),
        stderr: stderr.into(),
    }
}

fn failure_stderr(detail: &str) -> String {
    format!(
        "sway-stage: wait leaf=\"window2\": layout \"{SCENARIO}\": {detail}; stopped without \
         cleanup or relaunch; inspect windows before retrying; log: /private/last-run.log\n"
    )
}

#[test]
fn only_the_targets_output_change_during_the_held_wait_is_the_expected_failure() {
    let move_event = failure_stderr(&format!(
        "target workspace id={TARGET_ID} moved to another output"
    ));
    let output_snapshot = failure_stderr(&format!(
        "snapshot: target workspace id={TARGET_ID} changed output from id={FIRST_OUTPUT_ID} \
         to id={SECOND_OUTPUT_ID}"
    ));
    let verify = |process: &CompletedProcess| {
        verify_failure_report(process, TARGET_ID, FIRST_OUTPUT_ID, SECOND_OUTPUT_ID)
    };
    assert_eq!(
        verify(&completed_process(1, &move_event)).unwrap(),
        OutputChangeReport::MoveEvent
    );
    assert_eq!(
        verify(&completed_process(1, &output_snapshot)).unwrap(),
        OutputChangeReport::Snapshot
    );

    let wrong_reports = [
        // A focus loss of the same target, either way sway-stage notices it.
        failure_stderr(&format!(
            "workspace focus changed from target id={TARGET_ID}"
        )),
        failure_stderr(&format!("snapshot: focus left target id={TARGET_ID}")),
        // The right cause in another wait, layout, or for another workspace or output.
        move_event.replace("window2", "window3"),
        move_event.replace(SCENARIO, "interference-switch-away"),
        move_event.replace(&format!("id={TARGET_ID}"), "id=31"),
        output_snapshot.replace(&format!("to id={SECOND_OUTPUT_ID}"), "to id=99"),
        output_snapshot.replace(&format!("from id={FIRST_OUTPUT_ID}"), "from id=99"),
        // Any other failure beside the right one.
        format!("{move_event}sway-stage: window timeout: wait leaf=\"window2\"\n"),
        // No stop advice after the started application.
        move_event.replace("; stopped without cleanup or relaunch", ""),
    ];
    for wrong in &wrong_reports {
        assert!(verify(&completed_process(1, wrong)).is_err(), "{wrong}");
    }

    for code in [0, 2] {
        assert!(verify(&completed_process(code, &move_event)).is_err());
    }
    let mut with_stdout = completed_process(1, &move_event);
    with_stdout.stdout = "loaded\n".into();
    assert!(verify(&with_stdout).is_err());
}
