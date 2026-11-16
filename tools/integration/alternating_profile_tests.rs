use super::*;
use crate::fixtures::{FOOT, RUN_ID, SLEEP, assert_contains, two_output_outputs};
use crate::headless::{FIRST_OUTPUT_INITIAL_WORKSPACE, TWO_OUTPUT_WORKSPACE_RULES};
use crate::ipc::WorkspaceEvent;
use crate::output_expectation::{self, SECOND_UNPLACED_WORKSPACE, UNPLACED_WORKSPACE};
use crate::output_migration;
use crate::profile::verify_workspace_events;
use crate::scenarios::ConfiguredNode;
use serde_json::json;
use std::collections::HashSet;

const LAUNCH_SCRIPT: &str = "/private path/record-launch.sh";
const LAUNCH_RECORD: &str = "/private path/launches";

const FIXTURE_ID: i64 = 5;
const SECOND_OUTPUT_ID: i64 = 11;

fn workspace(id: i64, name: &str) -> WorkspaceIdentity {
    WorkspaceIdentity {
        id,
        name: name.into(),
    }
}

/// The fixture's workspace, then the targets as a load would leave them: the original `2`,
/// then new workspaces `6`, `7`, and `8`.
fn chain() -> Vec<WorkspaceIdentity> {
    vec![
        workspace(FIXTURE_ID, "1"),
        workspace(SECOND_OUTPUT_ID, "2"),
        workspace(30, "6"),
        workspace(31, "7"),
        workspace(32, "8"),
    ]
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
fn entries_alternate_outputs_and_sway_rules_place_every_created_target() {
    // The load starts on the fixture's workspace on the first output, so every selection
    // changes outputs.
    let mut previous_output = OUTPUT_NAME;
    for entry in &ENTRIES {
        assert_ne!(
            entry.expected_output, previous_output,
            "{}",
            entry.workspace
        );
        previous_output = entry.expected_output;
    }
    assert_eq!(ENTRIES.len(), 4);

    // Only the first target exists beforehand; every other has a rule for its own output.
    assert_eq!(ENTRIES[0].workspace, SECOND_OUTPUT_INITIAL_WORKSPACE);
    for entry in &ENTRIES[1..] {
        assert!(
            TWO_OUTPUT_WORKSPACE_RULES.contains(&(entry.workspace, entry.expected_output)),
            "{}",
            entry.workspace
        );
    }

    // Targets are distinct and never another two-output scenario's workspace.
    let targets: HashSet<&str> = ENTRIES.iter().map(|entry| entry.workspace).collect();
    assert_eq!(targets.len(), ENTRIES.len());
    for other_scenario_workspace in [
        FIRST_OUTPUT_INITIAL_WORKSPACE,
        crate::headless::SECOND_OUTPUT_ASSIGNED_WORKSPACE,
        UNPLACED_WORKSPACE,
        SECOND_UNPLACED_WORKSPACE,
        output_migration::TARGET_WORKSPACE,
    ] {
        assert!(!targets.contains(other_scenario_workspace));
    }
}

#[test]
fn the_profile_names_each_target_layout_and_expected_output_in_entry_order() {
    assert_eq!(
        output_expectation::profile_configuration(&ENTRIES),
        "version: 1\nworkspaces:\n\
         \x20 - workspace: 2\n    layout: profile-first\n    output: HEADLESS-2\n\
         \x20 - workspace: 6\n    layout: profile-second\n    output: HEADLESS-1\n\
         \x20 - workspace: 7\n    layout: profile-third\n    output: HEADLESS-2\n\
         \x20 - workspace: 8\n    layout: profile-fourth\n    output: HEADLESS-1\n"
    );
}

#[test]
fn distinct_layouts_record_every_launch_and_share_one_app_id() {
    let layouts = layouts(RUN_ID, FOOT, SLEEP, LAUNCH_SCRIPT, LAUNCH_RECORD);
    let layout_names: Vec<&str> = layouts.iter().map(|layout| layout.name).collect();
    assert_eq!(
        layout_names,
        [
            "profile-first",
            "profile-second",
            "profile-third",
            "profile-fourth"
        ]
    );
    let window_counts: Vec<usize> = layouts
        .iter()
        .map(|layout| layout.layout.leaves().len())
        .collect();
    assert_eq!(window_counts, [2, 2, 3, 4]);

    // Every window of every layout has the one app ID, unique to the scenario and run, so
    // each selector matches every earlier entry's windows too.
    let shared_app_id = format!("sway-stage-it-{RUN_ID}-{SCENARIO}-shared");
    for layout in &layouts {
        for leaf in layout.layout.leaves() {
            assert_eq!(leaf.app_id, shared_app_id);
            assert!(!leaf.requires_process_evidence);
            assert_eq!(
                &leaf.command[..6],
                &[
                    "/bin/sh",
                    LAUNCH_SCRIPT,
                    &format!("{}-{}", layout.name, leaf.layout_id),
                    LAUNCH_RECORD,
                    FOOT,
                    &format!("--app-id={shared_app_id}"),
                ]
            );
        }
    }

    // The last layout nests a group inside its split root and selects a nested window.
    let ConfiguredNode::Group { children, .. } = &layouts[3].layout else {
        panic!("mixed-nesting has a split root");
    };
    assert!(matches!(children[1], ConfiguredNode::Group { .. }));
    assert_eq!(layouts[3].expected_focus, "window3");
}

#[test]
fn the_load_selects_each_target_once_and_removes_no_workspace() {
    let chain = chain();
    let expected = load_events(&chain[0], &chain[1..]);
    let observed = || {
        vec![
            workspace_event("focus", SECOND_OUTPUT_ID, "2", Some(FIXTURE_ID)),
            workspace_event("init", 30, "6", None),
            workspace_event("focus", 30, "6", Some(SECOND_OUTPUT_ID)),
            workspace_event("init", 31, "7", None),
            workspace_event("focus", 31, "7", Some(30)),
            workspace_event("init", 32, "8", None),
            workspace_event("focus", 32, "8", Some(31)),
        ]
    };
    verify_workspace_events(&observed(), &expected).unwrap();

    // Any removal fails, even of the fixture's or an earlier target's hidden workspace.
    for (removed_id, removed_name) in [(FIXTURE_ID, "1"), (SECOND_OUTPUT_ID, "2"), (30, "6")] {
        let mut with_removal = observed();
        with_removal.push(workspace_event("empty", removed_id, removed_name, None));
        assert!(verify_workspace_events(&with_removal, &expected).is_err());
    }

    // The existing workspace is never recreated, and every created target is announced.
    let mut recreated = observed();
    recreated.insert(0, workspace_event("init", SECOND_OUTPUT_ID, "2", None));
    assert!(verify_workspace_events(&recreated, &expected).is_err());
    let mut unannounced = observed();
    unannounced.remove(3);
    assert!(verify_workspace_events(&unannounced, &expected).is_err());

    // A selection from anywhere but the previous target, or out of order, fails.
    let mut from_fixture = observed();
    from_fixture[4] = workspace_event("focus", 31, "7", Some(FIXTURE_ID));
    assert!(verify_workspace_events(&from_fixture, &expected).is_err());
    let mut reordered = observed();
    reordered.swap(4, 6);
    assert!(verify_workspace_events(&reordered, &expected).is_err());
}

#[test]
fn each_cleanup_return_removes_only_the_emptied_workspace_its_output_showed() {
    let chain = chain();
    let focus = |from: usize, to: usize| {
        workspace_event("focus", chain[to].id, &chain[to].name, Some(chain[from].id))
    };
    let empty = |index: usize| workspace_event("empty", chain[index].id, &chain[index].name, None);

    // From 8 to 7: 8 stays shown on the first output, and the second output showed 7.
    verify_workspace_events(&[focus(4, 3)], &cleanup_return_events(&chain, 3)).unwrap();
    // From 7 to 6: the first output showed the emptied 8.
    verify_workspace_events(&[focus(3, 2), empty(4)], &cleanup_return_events(&chain, 2)).unwrap();
    // From 6 to 2: the second output showed the emptied 7.
    verify_workspace_events(&[focus(2, 1), empty(3)], &cleanup_return_events(&chain, 1)).unwrap();
    // From 2 to the fixture's: the first output showed the emptied 6; 2 stays shown.
    verify_workspace_events(&[focus(1, 0), empty(2)], &cleanup_return_events(&chain, 0)).unwrap();

    // The one-output expectation, removing the workspace focus leaves, is wrong here.
    assert!(
        verify_workspace_events(&[focus(1, 0), empty(1)], &cleanup_return_events(&chain, 0))
            .is_err()
    );
    // So are a missing removal and an extra one.
    assert!(verify_workspace_events(&[focus(3, 2)], &cleanup_return_events(&chain, 2)).is_err());
    assert!(
        verify_workspace_events(&[focus(4, 3), empty(4)], &cleanup_return_events(&chain, 3))
            .is_err()
    );
}

fn outputs_showing(first_output_workspace: &str, second_output_workspace: &str) -> Vec<Output> {
    let mut outputs = two_output_outputs();
    outputs[0].current_workspace = Some(first_output_workspace.into());
    outputs[1].current_workspace = Some(second_output_workspace.into());
    outputs
}

#[test]
fn each_output_shows_the_last_target_the_profile_placed_on_it() {
    verify_shown_workspaces(&outputs_showing("8", "7")).unwrap();

    for (first_output_workspace, second_output_workspace) in [("6", "7"), ("8", "2"), ("1", "7")] {
        let wrong = outputs_showing(first_output_workspace, second_output_workspace);
        assert_contains(
            &verify_shown_workspaces(&wrong).unwrap_err(),
            "does not show workspace",
        );
    }
}
