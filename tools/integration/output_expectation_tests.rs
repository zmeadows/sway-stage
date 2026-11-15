use super::*;
use crate::fixtures::{
    FOOT, OTHER_WORKSPACE_ID, RUN_ID, SECOND_OUTPUT_WORKSPACE_ID, SLEEP, assert_contains,
    parse_tree, two_output_desktop, two_output_outputs, two_output_workspaces_reply,
};
use crate::headless::{OutputSetup, SECOND_OUTPUT_ASSIGNED_WORKSPACE, TWO_OUTPUT_WORKSPACE_RULES};
use crate::ipc::WorkspaceEvent;
use crate::profile::verify_workspace_events;
use serde_json::json;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

const LAUNCH_SCRIPT: &str = "/private path/record-launch.sh";
const LAUNCH_RECORD: &str = "/private path/launches";

#[test]
fn every_case_has_a_distinct_name_its_outcome_and_a_last_refused_entry() {
    let names: Vec<&str> = CASES.iter().map(|case| case.name()).collect();
    assert_eq!(
        names,
        [
            "profile-output-mismatch-refusal",
            "profile-output-unavailable-refusal",
            "profile-created-output-mismatch",
            "profile-later-created-output-mismatch",
            "profile-existing-other-output",
        ]
    );

    for case in CASES {
        let entries = case.entries();
        match case.outcome() {
            Outcome::Success => assert_eq!(case.refused_entry(), None),
            Outcome::PreflightRefusal | Outcome::SelectionRefusal => {
                assert_eq!(case.refused_entry(), entries.last());
            }
        }
        // Every two-entry case starts with the same valid first entry.
        if entries.len() == 2 {
            assert_eq!(entries[0], FIRST_OUTPUT_ENTRY);
        }
    }
    // Both preflight refusals stop at a later entry, after a valid first one.
    assert_eq!(Case::PreflightMismatch.entries().len(), 2);
    assert_eq!(Case::PreflightUnavailable.entries().len(), 2);
}

#[test]
fn created_targets_have_no_placement_rule_and_the_unavailable_output_never_exists() {
    for workspace in [UNPLACED_WORKSPACE, SECOND_UNPLACED_WORKSPACE] {
        assert!(
            TWO_OUTPUT_WORKSPACE_RULES
                .iter()
                .all(|(ruled_workspace, _)| *ruled_workspace != workspace),
            "{workspace}"
        );
    }
    assert_ne!(UNPLACED_WORKSPACE, SECOND_UNPLACED_WORKSPACE);
    assert_ne!(UNPLACED_WORKSPACE, SECOND_OUTPUT_ASSIGNED_WORKSPACE);

    for setup in [OutputSetup::OneOutput, OutputSetup::TwoOutputs] {
        assert!(
            setup
                .outputs()
                .iter()
                .all(|(name, _)| *name != UNAVAILABLE_OUTPUT)
        );
    }

    // The refused targets: the existing initial workspace of the second output, an absent
    // workspace expecting an absent output, and absent workspaces expecting the second.
    let refused = |case: Case| case.refused_entry().unwrap();
    assert_eq!(
        (
            refused(Case::PreflightMismatch).workspace,
            refused(Case::PreflightMismatch).expected_output
        ),
        (SECOND_OUTPUT_INITIAL_WORKSPACE, OUTPUT_NAME)
    );
    assert_eq!(
        refused(Case::PreflightUnavailable).expected_output,
        UNAVAILABLE_OUTPUT
    );
    for case in [Case::CreatedMismatch, Case::LaterCreatedMismatch] {
        assert_eq!(refused(case).expected_output, SECOND_OUTPUT_NAME);
    }
    assert_eq!(
        Case::ExistingOtherOutput.entries(),
        [OutputEntry {
            workspace: SECOND_OUTPUT_INITIAL_WORKSPACE,
            expected_output: SECOND_OUTPUT_NAME,
            layout_name: "profile-first",
            standard_layout: "uneven-siblings",
        }]
    );
}

#[test]
fn profiles_name_each_target_layout_and_expected_output_in_entry_order() {
    assert_eq!(
        configuration(Case::LaterCreatedMismatch),
        "version: 1\nworkspaces:\n\
         \x20 - workspace: 4\n    layout: profile-first\n    output: HEADLESS-1\n\
         \x20 - workspace: 5\n    layout: profile-second\n    output: HEADLESS-2\n"
    );
    assert_eq!(
        configuration(Case::PreflightMismatch),
        "version: 1\nworkspaces:\n\
         \x20 - workspace: 4\n    layout: profile-first\n    output: HEADLESS-1\n\
         \x20 - workspace: 2\n    layout: profile-second\n    output: HEADLESS-1\n"
    );
    assert_eq!(
        configuration(Case::ExistingOtherOutput),
        "version: 1\nworkspaces:\n\
         \x20 - workspace: 2\n    layout: profile-first\n    output: HEADLESS-2\n"
    );
}

#[test]
fn layouts_record_every_launch_with_app_ids_unique_to_the_case() {
    for case in CASES {
        let layouts = layouts(case, RUN_ID, FOOT, SLEEP, LAUNCH_SCRIPT, LAUNCH_RECORD);
        let layout_names: Vec<&str> = layouts.iter().map(|layout| layout.name).collect();
        let entry_layout_names: Vec<&str> = case
            .entries()
            .iter()
            .map(|entry| entry.layout_name)
            .collect();
        assert_eq!(layout_names, entry_layout_names, "{}", case.name());

        for layout in &layouts {
            for leaf in layout.layout.leaves() {
                assert!(leaf.app_id.contains(RUN_ID));
                assert!(leaf.app_id.contains(case.name()));
                assert_eq!(
                    &leaf.command[..4],
                    &[
                        "/bin/sh",
                        LAUNCH_SCRIPT,
                        &format!("{}-{}", layout.name, leaf.layout_id),
                        LAUNCH_RECORD
                    ]
                );
                assert_eq!(leaf.command[4], FOOT);
            }
        }
    }

    let existing = layouts(
        Case::ExistingOtherOutput,
        RUN_ID,
        FOOT,
        SLEEP,
        LAUNCH_SCRIPT,
        LAUNCH_RECORD,
    );
    assert_eq!(existing[0].layout.leaves().len(), 3);
}

fn workspace(id: i64, name: &str) -> WorkspaceIdentity {
    WorkspaceIdentity {
        id,
        name: name.into(),
    }
}

fn workspace_event(change: &str, id: i64, name: &str, old_id: Option<i64>) -> WorkspaceEvent {
    let old = old_id.map(|id| json!({"type": "workspace", "id": id, "name": "old", "num": -1}));
    serde_json::from_value(json!({
        "change": change,
        "current": {
            "type": "workspace",
            "id": id,
            "name": name,
            "num": name.parse::<i32>().unwrap(),
        },
        "old": old,
    }))
    .expect("a workspace event in Sway's format")
}

#[test]
fn selections_announce_created_targets_and_remove_no_workspace() {
    let fixture = workspace(OTHER_WORKSPACE_ID, "1");
    let created = [workspace(30, "4"), workspace(31, "5")];

    let later = selection_events(Case::LaterCreatedMismatch, &fixture, &created);
    let later_observed = || {
        vec![
            workspace_event("init", 30, "4", None),
            workspace_event("focus", 30, "4", Some(OTHER_WORKSPACE_ID)),
            workspace_event("init", 31, "5", None),
            workspace_event("focus", 31, "5", Some(30)),
        ]
    };
    verify_workspace_events(&later_observed(), &later).unwrap();

    // A removed workspace, a missing creation, or a selection from elsewhere fails.
    let mut removed = later_observed();
    removed.push(workspace_event("empty", 30, "4", None));
    assert!(verify_workspace_events(&removed, &later).is_err());
    assert!(verify_workspace_events(&later_observed()[1..], &later).is_err());
    let mut from_elsewhere = later_observed();
    from_elsewhere[3] = workspace_event("focus", 31, "5", Some(OTHER_WORKSPACE_ID));
    assert!(verify_workspace_events(&from_elsewhere, &later).is_err());

    let only_created = selection_events(Case::CreatedMismatch, &fixture, &created[..1]);
    verify_workspace_events(&later_observed()[..2], &only_created).unwrap();

    // Selecting the existing workspace only moves focus there.
    let existing = [workspace(SECOND_OUTPUT_WORKSPACE_ID, "2")];
    let selected = selection_events(Case::ExistingOtherOutput, &fixture, &existing);
    let focus = || {
        workspace_event(
            "focus",
            SECOND_OUTPUT_WORKSPACE_ID,
            "2",
            Some(OTHER_WORKSPACE_ID),
        )
    };
    verify_workspace_events(&[focus()], &selected).unwrap();
    let recreated = [
        workspace_event("init", SECOND_OUTPUT_WORKSPACE_ID, "2", None),
        focus(),
    ];
    assert!(verify_workspace_events(&recreated, &selected).is_err());
}

fn two_output_snapshot() -> Snapshot {
    Snapshot {
        tree: parse_tree(two_output_desktop(OTHER_WORKSPACE_ID)),
        workspaces: two_output_workspaces_reply("1"),
        focused_workspace_id: OTHER_WORKSPACE_ID,
        window_events: Vec::new(),
        workspace_events: Vec::new(),
    }
}

#[test]
fn the_unavailable_output_must_be_absent_from_the_outputs_and_the_tree() {
    require_unavailable_output(&two_output_snapshot(), &two_output_outputs()).unwrap();

    let mut listed = two_output_outputs();
    listed[1].name = UNAVAILABLE_OUTPUT.into();
    assert_contains(
        &require_unavailable_output(&two_output_snapshot(), &listed).unwrap_err(),
        "HEADLESS-9 must not exist",
    );

    let mut in_tree = two_output_snapshot();
    in_tree.tree.nodes[2].name = Some(UNAVAILABLE_OUTPUT.into());
    assert!(require_unavailable_output(&in_tree, &two_output_outputs()).is_err());
}

const PLACEMENT_HELP: &str = "Sway's workspace/output rules control placement; configure those \
     rules before loading (Sway Stage never assigns outputs or moves workspaces)";

fn completed_process(code: i32, stderr: &str) -> CompletedProcess {
    CompletedProcess {
        status: ExitStatus::from_raw(code << 8),
        stdout: String::new(),
        stderr: stderr.into(),
    }
}

/// The report sway-stage writes for each refused case, completed targets included.
fn refusal_stderr(case: Case) -> String {
    let report = match case {
        Case::PreflightMismatch => format!(
            "profile load: workspace 2, layout \"profile-second\"; completed 0 workspaces []; \
             workspace preflight: profile \"profile-output-mismatch-refusal\": profile output \
             mismatch: workspace 2: expected output \
             \"HEADLESS-1\", observed output \"HEADLESS-2\"; {PLACEMENT_HELP}; no applications \
             launched"
        ),
        Case::PreflightUnavailable => format!(
            "profile load: workspace 5, layout \"profile-second\"; completed 0 workspaces []; \
             workspace preflight: profile \"profile-output-unavailable-refusal\": profile \
             output unavailable: workspace 5: expected output \
             \"HEADLESS-9\" is not active; workspace does not yet exist; {PLACEMENT_HELP}; no \
             applications launched"
        ),
        Case::CreatedMismatch => format!(
            "profile load: workspace 4, layout \"profile-first\"; completed 0 workspaces []; \
             workspace selection: profile \"profile-created-output-mismatch\": workspace \
             selection: profile output mismatch: workspace 4: \
             expected output \"HEADLESS-2\", observed output \"HEADLESS-1\"; {PLACEMENT_HELP}; \
             no applications launched"
        ),
        Case::LaterCreatedMismatch => format!(
            "profile load: workspace 5, layout \"profile-second\"; completed 1 workspaces \
             [(4, 30)]; workspace selection: profile \"profile-later-created-output-mismatch\": \
             workspace selection: profile output mismatch: \
             workspace 5: expected output \"HEADLESS-2\", observed output \"HEADLESS-1\"; \
             {PLACEMENT_HELP}; application launches may have left partial layouts; stopped \
             without cleanup or relaunch; inspect windows before retrying"
        ),
        Case::ExistingOtherOutput => unreachable!("the existing-workspace case succeeds"),
    };
    format!("sway-stage: {report}; log: /private/last-run.log\n")
}

fn completed_targets(case: Case) -> Vec<WorkspaceIdentity> {
    if case == Case::LaterCreatedMismatch {
        vec![workspace(30, "4")]
    } else {
        Vec::new()
    }
}

#[test]
fn refusal_reports_require_the_entry_progress_phase_classification_and_launch_advice() {
    let refused_cases = [
        Case::PreflightMismatch,
        Case::PreflightUnavailable,
        Case::CreatedMismatch,
        Case::LaterCreatedMismatch,
    ];
    for case in refused_cases {
        let stderr = refusal_stderr(case);
        let completed = completed_targets(case);
        verify_refusal_report(case, &completed_process(1, &stderr), &completed).unwrap();

        // Every other case's report differs in its entry, phase, or classification.
        for other_case in refused_cases.into_iter().filter(|other| *other != case) {
            let other_stderr = refusal_stderr(other_case);
            assert!(
                verify_refusal_report(case, &completed_process(1, &other_stderr), &completed)
                    .is_err(),
                "{} accepted {}",
                case.name(),
                other_case.name()
            );
        }

        let other_completed_targets = if completed.is_empty() {
            stderr.replace(
                "completed 0 workspaces []",
                "completed 1 workspaces [(4, 30)]",
            )
        } else {
            stderr.replace(
                "completed 1 workspaces [(4, 30)]",
                "completed 0 workspaces []",
            )
        };
        for (change, wrong_stderr) in [
            (
                "another expected output",
                stderr.replace(
                    "expected output \"HEADLESS-",
                    "expected output \"HEADLESS-0",
                ),
            ),
            (
                "another observed output",
                stderr.replace(
                    "observed output \"HEADLESS-",
                    "observed output \"HEADLESS-0",
                ),
            ),
            ("other completed targets", other_completed_targets),
            (
                "also occupied",
                format!("{stderr}sway-stage: profile target occupied: workspace 4\n"),
            ),
            (
                "no launch advice",
                stderr
                    .replace("; no applications launched", "")
                    .replace("; application launches may have left partial layouts", ""),
            ),
        ] {
            // The unavailable-output report names no observed output.
            if wrong_stderr == stderr {
                continue;
            }
            assert!(
                verify_refusal_report(case, &completed_process(1, &wrong_stderr), &completed)
                    .is_err(),
                "{}: {change}",
                case.name()
            );
        }

        for code in [0, 2] {
            let wrong_status = completed_process(code, &stderr);
            assert!(verify_refusal_report(case, &wrong_status, &completed).is_err());
        }
        let mut with_stdout = completed_process(1, &stderr);
        with_stdout.stdout = "loaded\n".into();
        assert!(verify_refusal_report(case, &with_stdout, &completed).is_err());
    }

    // The two preflight classifications are never accepted for each other, even together.
    let both = format!(
        "{}{}",
        refusal_stderr(Case::PreflightMismatch),
        refusal_stderr(Case::PreflightUnavailable)
    );
    for case in [Case::PreflightMismatch, Case::PreflightUnavailable] {
        assert_contains(
            &verify_refusal_report(case, &completed_process(1, &both), &[]).unwrap_err(),
            "expected",
        );
    }
}
