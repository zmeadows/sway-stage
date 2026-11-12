use super::*;
use crate::fixtures::{
    OTHER_WORKSPACE_ID, PREEXISTING_WINDOW_ID, TARGET_WORKSPACE_ID, assert_contains, desktop,
    group, node_mut, parse_tree, rect, uncertain_message, wayland_window, window_event,
    workspaces_reply,
};
use crate::ownership::require_quiet;
use serde_json::{Value, json};

fn recorded_snapshot(tree: Value) -> Snapshot {
    Snapshot {
        tree: parse_tree(tree),
        workspaces: workspaces_reply("9"),
        focused_workspace_id: TARGET_WORKSPACE_ID,
        window_events: Vec::new(),
    }
}

fn foreign_window_baseline_tree() -> Value {
    let mut tree = desktop("splith", Vec::new(), TARGET_WORKSPACE_ID);
    node_mut(&mut tree, OTHER_WORKSPACE_ID)["nodes"] = json!([group(
        30,
        "splitv",
        rect(0, 0, 1920, 1080),
        vec![
            wayland_window(PREEXISTING_WINDOW_ID, "foreign", rect(0, 0, 1920, 540)),
            wayland_window(31, "foreign-sibling", rect(0, 540, 1920, 540)),
        ]
    )]);
    tree
}

fn foreign_window_baseline() -> ForeignWindows {
    ForeignWindows::record(
        &recorded_snapshot(foreign_window_baseline_tree()),
        TARGET_WORKSPACE_ID,
    )
    .unwrap()
}

#[test]
fn unchanged_foreign_windows_survive_target_construction_capture_and_cleanup() {
    let foreign_window_baseline = foreign_window_baseline();
    for target_windows in [
        Vec::new(),
        vec![wayland_window(20, "owned", rect(0, 0, 1920, 1080))],
    ] {
        let mut tree = foreign_window_baseline_tree();
        node_mut(&mut tree, TARGET_WORKSPACE_ID)["nodes"] = json!(target_windows);
        foreign_window_baseline
            .verify(&recorded_snapshot(tree))
            .unwrap();
    }
    let mut title_change = foreign_window_baseline_tree();
    node_mut(&mut title_change, PREEXISTING_WINDOW_ID)["name"] = json!("new title");
    let mut snapshot = recorded_snapshot(title_change);
    snapshot
        .window_events
        .push(window_event("title", PREEXISTING_WINDOW_ID, None));
    foreign_window_baseline.verify(&snapshot).unwrap();
    require_quiet(&snapshot.window_events, None).unwrap();
}

#[test]
fn foreign_geometry_state_identity_and_ancestor_changes_fail_conservatively() {
    let foreign_window_baseline = foreign_window_baseline();
    let changes = [
        ("rect", rect(1, 0, 1920, 540)),
        ("floating", json!("user_on")),
        ("fullscreen_mode", json!(1)),
        ("scratchpad_state", json!("fresh")),
        ("pid", json!(9999999)),
        ("app_id", json!("different")),
    ];
    for (field, changed) in changes {
        let mut tree = foreign_window_baseline_tree();
        node_mut(&mut tree, PREEXISTING_WINDOW_ID)[field] = changed;
        assert_contains(
            &uncertain_message(foreign_window_baseline.verify(&recorded_snapshot(tree))),
            "identity/geometry/state changed",
        );
    }
    for (ancestor, field, changed) in [
        (30, "layout", json!("splith")),
        (30, "fullscreen_mode", json!(1)),
        (OTHER_WORKSPACE_ID, "rect", rect(0, 0, 900, 1080)),
    ] {
        let mut tree = foreign_window_baseline_tree();
        node_mut(&mut tree, ancestor)[field] = changed;
        assert_contains(
            &uncertain_message(foreign_window_baseline.verify(&recorded_snapshot(tree))),
            "ancestry/parent/order/ancestor state changed",
        );
    }
}

#[test]
fn changed_foreign_parent_order_workspace_output_or_disappearance_fails() {
    let foreign_window_baseline = foreign_window_baseline();
    let mut reordered = foreign_window_baseline_tree();
    node_mut(&mut reordered, 30)["nodes"]
        .as_array_mut()
        .unwrap()
        .reverse();
    let mut reparented = foreign_window_baseline_tree();
    let children = node_mut(&mut reparented, 30)["nodes"].take();
    node_mut(&mut reparented, OTHER_WORKSPACE_ID)["nodes"] = children;
    let mut moved_to_target = foreign_window_baseline_tree();
    let window = node_mut(&mut moved_to_target, 30)["nodes"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    node_mut(&mut moved_to_target, TARGET_WORKSPACE_ID)["nodes"] = json!([window]);
    let mut moved_to_scratchpad = foreign_window_baseline_tree();
    let window = node_mut(&mut moved_to_scratchpad, 30)["nodes"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    node_mut(
        &mut moved_to_scratchpad,
        crate::fixtures::SCRATCHPAD_WORKSPACE_ID,
    )["floating_nodes"] = json!([window]);
    let mut floating_list = foreign_window_baseline_tree();
    let window = node_mut(&mut floating_list, 30)["nodes"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    node_mut(&mut floating_list, OTHER_WORKSPACE_ID)["floating_nodes"] = json!([window]);
    let mut disappeared = foreign_window_baseline_tree();
    node_mut(&mut disappeared, 30)["nodes"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    let mut output_moved = foreign_window_baseline_tree();
    // A different output now contains the foreign workspace, while the target stays put.
    let workspace = node_mut(&mut output_moved, 4)["nodes"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    let mut output = crate::fixtures::container(
        40,
        "output",
        "output",
        rect(0, 0, 1920, 1080),
        vec![workspace],
    );
    output["name"] = json!("DP-2");
    output_moved["nodes"].as_array_mut().unwrap().push(output);
    for tree in [
        reordered,
        reparented,
        moved_to_target,
        moved_to_scratchpad,
        floating_list,
        disappeared,
        output_moved,
    ] {
        assert_contains(
            &uncertain_message(foreign_window_baseline.verify(&recorded_snapshot(tree))),
            "foreign window id=6",
        );
    }
    let mut target_output_changed = foreign_window_baseline_tree();
    node_mut(&mut target_output_changed, 4)["id"] = json!(41);

    assert_contains(
        &uncertain_message(
            foreign_window_baseline.verify(&recorded_snapshot(target_output_changed)),
        ),
        "target workspace id=7 changed output ancestry",
    );
}

#[test]
fn contradictory_or_missing_snapshot_evidence_never_establishes_preservation() {
    let foreign_window_baseline = foreign_window_baseline();
    let mut missing_workspace = recorded_snapshot(foreign_window_baseline_tree());
    missing_workspace.workspaces.remove(0);
    let mut contradictory_focus = recorded_snapshot(foreign_window_baseline_tree());
    contradictory_focus.focused_workspace_id = OTHER_WORKSPACE_ID;
    let mut duplicate = foreign_window_baseline_tree();
    node_mut(&mut duplicate, 31)["id"] = json!(PREEXISTING_WINDOW_ID);
    let mut missing_target = recorded_snapshot(foreign_window_baseline_tree());
    missing_target.tree.nodes[1].nodes.pop();
    for snapshot in [
        missing_workspace,
        contradictory_focus,
        recorded_snapshot(duplicate),
        missing_target,
    ] {
        uncertain_message(foreign_window_baseline.verify(&snapshot));
        uncertain_message(ForeignWindows::record(&snapshot, TARGET_WORKSPACE_ID));
    }
}

#[test]
fn structural_events_fail_even_when_a_foreign_window_returns_to_its_baseline() {
    let foreign_window_baseline = foreign_window_baseline();
    for change in [
        "move",
        "floating",
        "fullscreen_mode",
        "close",
        "new",
        "focus",
    ] {
        let mut snapshot = recorded_snapshot(foreign_window_baseline_tree());
        snapshot
            .window_events
            .push(window_event(change, PREEXISTING_WINDOW_ID, None));
        foreign_window_baseline.verify(&snapshot).unwrap();
        uncertain_message(require_quiet(&snapshot.window_events, None));
    }
}

#[test]
fn absent_window_state_fields_cannot_establish_a_preservation_baseline() {
    for field in ["floating", "fullscreen_mode", "scratchpad_state"] {
        let mut tree = foreign_window_baseline_tree();
        node_mut(&mut tree, PREEXISTING_WINDOW_ID)
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert_contains(
            &uncertain_message(ForeignWindows::record(
                &recorded_snapshot(tree),
                TARGET_WORKSPACE_ID,
            )),
            "lacks floating/fullscreen/scratchpad evidence",
        );
    }
}
