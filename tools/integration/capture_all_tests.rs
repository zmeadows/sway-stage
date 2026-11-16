use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

use serde_json::{Value, json};

use super::*;
use crate::fixtures::{
    FOOT, RUN_ID, SLEEP, assert_contains, container, parse_tree, rect, wayland_window,
};
use crate::ipc::{Workspace, WorkspaceEvent};
use crate::profile::verify_workspace_events;

const LAUNCH_SCRIPT: &str = "/private path/record-launch.sh";
const LAUNCH_RECORD: &str = "/private path/launches";

fn workspace_node(workspace_id: i64, name: &str, children: Vec<Value>) -> Value {
    let mut workspace = container(
        workspace_id,
        "workspace",
        "splith",
        rect(0, 0, 1920, 1080),
        children,
    );
    workspace["name"] = json!(name);
    workspace["num"] = json!(name.parse::<i32>().unwrap_or(-1));
    workspace
}

fn output_node(output_id: i64, name: &str, workspaces: Vec<Value>) -> Value {
    let mut output = container(
        output_id,
        "output",
        "output",
        rect(0, 0, 1920, 1080),
        workspaces,
    );
    output["name"] = json!(name);
    output
}

fn window(container_id: i64) -> Value {
    wayland_window(container_id, "foot", rect(0, 0, 1920, 1080))
}

/// A desktop of `outputs`, plus Sway's hidden scratchpad output, whose workspace holds a
/// window. `get_workspaces` reports `listed_workspaces`, in that order, never the
/// scratchpad workspace.
fn desktop_snapshot(outputs: Vec<Value>, listed_workspaces: &[(i64, &str)]) -> Snapshot {
    let scratchpad = workspace_node(3, "__i3_scratch", vec![window(90)]);
    let mut nodes = vec![output_node(2, "__i3", vec![scratchpad])];
    nodes.extend(outputs);
    let mut root = container(1, "root", "splith", rect(0, 0, 4480, 1440), nodes);
    root["name"] = json!("root");

    let workspaces: Vec<Workspace> = listed_workspaces
        .iter()
        .map(|&(workspace_id, name)| {
            serde_json::from_value(json!({
                "id": workspace_id,
                "num": name.parse::<i32>().unwrap_or(-1),
                "name": name,
                "focused": name == "1",
            }))
            .expect("a workspace in Sway's format")
        })
        .collect();
    Snapshot {
        tree: parse_tree(root),
        workspaces,
        focused_workspace_id: listed_workspaces[0].0,
        window_events: Vec::new(),
        workspace_events: Vec::new(),
    }
}

/// The scenario's desktop during the second capture: the fixture's `1` and `notes` on the
/// first output, the empty `2` and populated `3` on the second, listed in creation order.
fn scenario_desktop() -> Snapshot {
    let first_output = output_node(
        4,
        "HEADLESS-1",
        vec![
            workspace_node(5, "1", vec![window(6)]),
            workspace_node(20, "notes", vec![window(21)]),
        ],
    );
    let second_output = output_node(
        12,
        "HEADLESS-2",
        vec![
            workspace_node(40, "2", Vec::new()),
            workspace_node(30, "3", vec![window(31)]),
        ],
    );
    desktop_snapshot(
        vec![first_output, second_output],
        &[(5, "1"), (20, "notes"), (30, "3"), (40, "2")],
    )
}

#[test]
fn the_scenario_captures_1_then_3_then_notes_and_skips_the_empty_2() {
    assert_eq!(
        expected_capture_targets(&scenario_desktop()).unwrap(),
        EXPECTED_TARGETS
    );

    // The harness creates `notes` before `3`, and the tree lists the first output's
    // workspaces before the second's, so neither order is the profile's.
    let creation_order = ["1", NAMED_WORKSPACE, SECOND_OUTPUT_WORKSPACE];
    assert_ne!(creation_order, EXPECTED_TARGETS);
    assert_eq!(EXPECTED_TARGETS, ["1", "3", "notes"]);
    assert_eq!(EMPTY_WORKSPACE, "2");
}

#[test]
fn targets_are_populated_numbers_by_value_then_names_by_bytes() {
    let mut floating_only = workspace_node(50, "10", Vec::new());
    floating_only["floating_nodes"] = json!([window(51)]);
    let first_output = output_node(
        4,
        "HEADLESS-1",
        vec![
            workspace_node(20, "notes", vec![window(21)]),
            workspace_node(5, "1", vec![window(6)]),
            workspace_node(22, "Zeta", vec![window(23)]),
            floating_only,
        ],
    );
    let second_output = output_node(
        12,
        "HEADLESS-2",
        vec![
            workspace_node(40, "2", Vec::new()),
            workspace_node(30, "9", vec![window(31)]),
            workspace_node(32, "3", vec![window(33)]),
        ],
    );
    let observed = desktop_snapshot(
        vec![first_output, second_output],
        &[
            (5, "1"),
            (20, "notes"),
            (22, "Zeta"),
            (50, "10"),
            (40, "2"),
            (30, "9"),
            (32, "3"),
        ],
    );

    // A floating window populates `10`; uppercase sorts before lowercase; the empty `2`
    // and the scratchpad's window are not captured.
    assert_eq!(
        expected_capture_targets(&observed).unwrap(),
        ["1", "3", "9", "10", "Zeta", "notes"]
    );
}

#[test]
fn a_listed_workspace_missing_from_the_tree_fails() {
    let mut observed = scenario_desktop();
    observed.workspaces.push(
        serde_json::from_value(json!({"id": 77, "num": 7, "name": "7", "focused": false})).unwrap(),
    );
    assert_contains(
        &expected_capture_targets(&observed).unwrap_err(),
        "missing from the tree",
    );
}

fn identity(id: i64, name: &str) -> WorkspaceIdentity {
    WorkspaceIdentity {
        id,
        name: name.into(),
    }
}

fn workspace_event(
    change: &str,
    workspace: &WorkspaceIdentity,
    old_id: Option<i64>,
) -> WorkspaceEvent {
    let old = old_id.map(|id| json!({"type": "workspace", "id": id, "name": "old", "num": -1}));
    serde_json::from_value(json!({
        "change": change,
        "current": {
            "type": "workspace",
            "id": workspace.id,
            "name": workspace.name,
            "num": workspace.name.parse::<i32>().unwrap_or(-1),
        },
        "old": old,
    }))
    .expect("a workspace event in Sway's format")
}

#[test]
fn selecting_3_creates_it_and_removes_the_hidden_empty_workspace_unless_it_is_3() {
    let named = identity(20, NAMED_WORKSPACE);
    let created = identity(30, SECOND_OUTPUT_WORKSPACE);

    // From the original `2` or the output migration's `9`: Sway creates `3` there, and the
    // empty workspace it hides is removed.
    for shown_before in [identity(11, "2"), identity(15, "9")] {
        let expected = second_output_selection_events(&named, &created, &shown_before);
        let observed = [
            workspace_event("init", &created, None),
            workspace_event("focus", &created, Some(named.id)),
            workspace_event("empty", &shown_before, None),
        ];
        verify_workspace_events(&observed, &expected).unwrap();
        // Without the removal, the hidden workspace would still be there.
        assert!(verify_workspace_events(&observed[..2], &expected).is_err());
    }

    // After the cross-output profile, `3` is already the shown empty workspace: only focus
    // moves, and nothing is created or removed.
    let existing = identity(13, SECOND_OUTPUT_WORKSPACE);
    let expected = second_output_selection_events(&named, &existing, &existing);
    let focus_only = [workspace_event("focus", &existing, Some(named.id))];
    verify_workspace_events(&focus_only, &expected).unwrap();
    let recreated = [
        workspace_event("init", &existing, None),
        workspace_event("focus", &existing, Some(named.id)),
    ];
    assert!(verify_workspace_events(&recreated, &expected).is_err());
}

#[test]
fn cleanup_selects_3_from_the_fixture_and_sway_removes_the_hidden_empty_2() {
    let fixture_workspace = identity(5, "1");
    let second_output = identity(30, SECOND_OUTPUT_WORKSPACE);
    let empty = identity(40, EMPTY_WORKSPACE);
    let expected = cleanup_selection_events(&fixture_workspace, &second_output, &empty);

    let observed = [
        workspace_event("focus", &second_output, Some(fixture_workspace.id)),
        workspace_event("empty", &empty, None),
    ];
    verify_workspace_events(&observed, &expected).unwrap();
    // The fixture's workspace stays: it holds the fixture and stays shown on its output.
    let fixture_removed = [
        workspace_event("focus", &second_output, Some(fixture_workspace.id)),
        workspace_event("empty", &fixture_workspace, None),
    ];
    assert!(verify_workspace_events(&fixture_removed, &expected).is_err());
}

fn completed(code: i32, stdout: &str, stderr: &str) -> CompletedProcess {
    CompletedProcess {
        status: ExitStatus::from_raw(code << 8),
        stdout: stdout.into(),
        stderr: stderr.into(),
    }
}

#[test]
fn the_report_lists_exactly_the_published_files_in_order() {
    let layout_names: Vec<String> = ["captured-all-001", "captured-all-002", "captured-all-003"]
        .map(str::to_owned)
        .to_vec();
    let paths = captured_file_paths(
        Path::new("/private/layouts"),
        Path::new("/private/profiles"),
        &layout_names,
    );
    assert_eq!(
        paths,
        [
            "/private/layouts/captured-all-001.yaml",
            "/private/layouts/captured-all-002.yaml",
            "/private/layouts/captured-all-003.yaml",
            "/private/profiles/captured-all.yaml",
        ]
        .map(PathBuf::from)
    );

    let report = "sway-stage: captured profile \"captured-all\". Command inference disabled; \
                  unresolved commands: 9. Manual editing required.\n  \
                  /private/layouts/captured-all-001.yaml\n  \
                  /private/layouts/captured-all-002.yaml\n  \
                  /private/layouts/captured-all-003.yaml\n  \
                  /private/profiles/captured-all.yaml\n";
    verify_capture_report(&completed(0, "", report), &paths, 9).unwrap();

    // Another count, order, or file set; output on stdout; or a failure is not the report.
    assert!(verify_capture_report(&completed(0, "", report), &paths, 8).is_err());
    let mut reordered = paths.clone();
    reordered.swap(0, 1);
    assert!(verify_capture_report(&completed(0, "", report), &reordered, 9).is_err());
    assert!(verify_capture_report(&completed(0, "", report), &paths[1..], 9).is_err());
    assert_contains(
        &verify_capture_report(&completed(0, "version: 1\n", report), &paths, 9).unwrap_err(),
        "stdout",
    );
    assert_contains(
        &verify_capture_report(&completed(1, "", "profile capture: failed\n"), &paths, 9)
            .unwrap_err(),
        "profile capture failed",
    );
    let inferred = report.replace(
        "Command inference disabled; unresolved commands: 9. Manual editing required.",
        "Commands inferred: 9; unresolved: 0. Review best-effort commands before use.",
    );
    assert!(verify_capture_report(&completed(0, "", &inferred), &paths, 9).is_err());
}

#[test]
fn both_layouts_record_launches_and_select_a_window_other_than_their_first() {
    let layouts = layouts(RUN_ID, FOOT, SLEEP, LAUNCH_SCRIPT, LAUNCH_RECORD);
    let mut app_ids = std::collections::HashSet::new();
    for layout in &layouts {
        let leaves = layout.layout.leaves();
        assert_ne!(
            layout.expected_focus, leaves[0].layout_id,
            "{}",
            layout.name
        );
        assert_eq!(layout.configured_focus, Some(layout.expected_focus));
        for leaf in leaves {
            assert_eq!(leaf.command[1], LAUNCH_SCRIPT);
            assert_eq!(
                leaf.command[2],
                format!("{}-{}", layout.name, leaf.layout_id)
            );
            assert_eq!(leaf.command[3], LAUNCH_RECORD);
            assert!(app_ids.insert(leaf.app_id.clone()), "{}", leaf.app_id);
        }
    }
    // Nine windows with the fixture's, each an unresolved command in an inference-disabled
    // capture.
    let window_count: usize = layouts
        .iter()
        .map(|layout| layout.layout.leaves().len())
        .sum();
    assert_eq!(window_count + 1, 9);
}
