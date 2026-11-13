use super::*;
use crate::fixtures::{
    OTHER_WORKSPACE_ID, PREEXISTING_WINDOW_ID, SCRATCHPAD_WORKSPACE_ID, TARGET_WORKSPACE_ID,
    assert_contains, desktop, group, node_mut, parse_tree, rect, wayland_window, workspaces_reply,
};
use serde_json::{Value, json};

#[test]
fn reporting_uncertainty_permanently_invalidates_history() {
    let mut history_trusted = true;

    assert_eq!(
        Failure::Check("sizing".into()).report_history(&mut history_trusted),
        "sizing"
    );
    assert!(history_trusted);
    assert_eq!(
        Failure::Uncertain("ownership".into()).report_history(&mut history_trusted),
        "ownership"
    );
    assert!(!history_trusted);
    Failure::Check("later failure".into()).report_history(&mut history_trusted);
    assert!(!history_trusted);
    Failure::Uncertain("interrupted".into()).report_history(&mut history_trusted);
    assert!(!history_trusted);
}

/// The target workspace holding one focused window.
fn tree_with_focused_target_window() -> Value {
    desktop(
        "splith",
        vec![wayland_window(20, "app", rect(0, 0, 1920, 1080))],
        20,
    )
}

fn validate_recorded_snapshot(
    tree: Value,
    workspaces: &[Workspace],
    current: Option<&WorkspaceIdentity>,
) -> Result<i64, String> {
    validate_snapshot(&parse_tree(tree), workspaces, current)
}

fn target_identity() -> WorkspaceIdentity {
    WorkspaceIdentity {
        id: TARGET_WORKSPACE_ID,
        name: "9".into(),
    }
}

#[test]
fn consistent_snapshot_returns_the_focused_workspace() {
    let focused_container_id = validate_recorded_snapshot(
        tree_with_focused_target_window(),
        &workspaces_reply("9"),
        None,
    )
    .unwrap();

    assert_eq!(focused_container_id, TARGET_WORKSPACE_ID);

    let focused_container_id = validate_recorded_snapshot(
        tree_with_focused_target_window(),
        &workspaces_reply("9"),
        Some(&target_identity()),
    )
    .unwrap();
    assert_eq!(focused_container_id, TARGET_WORKSPACE_ID);

    // An empty workspace may itself hold focus.
    let empty = desktop("splith", Vec::new(), TARGET_WORKSPACE_ID);
    assert_eq!(
        validate_recorded_snapshot(empty, &workspaces_reply("9"), None).unwrap(),
        TARGET_WORKSPACE_ID
    );
}

#[test]
fn snapshot_inconsistencies_are_rejected() {
    let mut cases: Vec<(Value, Vec<Workspace>, Option<WorkspaceIdentity>, &str)> = Vec::new();

    let mut duplicate_ids = tree_with_focused_target_window();
    node_mut(&mut duplicate_ids, 20)["id"] = json!(PREEXISTING_WINDOW_ID);
    cases.push((
        duplicate_ids,
        workspaces_reply("9"),
        None,
        "duplicate container identities",
    ));

    let mut not_root = tree_with_focused_target_window();
    not_root["type"] = json!("output");
    cases.push((not_root, workspaces_reply("9"), None, "invalid tree"));

    let mut duplicate_names = workspaces_reply("9");
    duplicate_names[0].name = "9".into();
    cases.push((
        tree_with_focused_target_window(),
        duplicate_names,
        None,
        "ambiguous workspace identities",
    ));

    let mut no_focused_workspace = workspaces_reply("9");
    no_focused_workspace[1].focused = false;
    cases.push((
        tree_with_focused_target_window(),
        no_focused_workspace,
        None,
        "exactly one focused workspace",
    ));

    let mut two_focused_workspaces = workspaces_reply("9");
    two_focused_workspaces[0].focused = true;
    cases.push((
        tree_with_focused_target_window(),
        two_focused_workspaces,
        None,
        "exactly one focused workspace",
    ));

    let renamed = WorkspaceIdentity {
        id: TARGET_WORKSPACE_ID,
        name: "renamed".into(),
    };
    cases.push((
        tree_with_focused_target_window(),
        workspaces_reply("9"),
        Some(renamed),
        "focus or identity changed",
    ));
    let elsewhere = WorkspaceIdentity {
        id: OTHER_WORKSPACE_ID,
        name: "1".into(),
    };
    cases.push((
        tree_with_focused_target_window(),
        workspaces_reply("9"),
        Some(elsewhere),
        "focus or identity changed",
    ));

    cases.push((
        tree_with_focused_target_window(),
        workspaces_reply("1"),
        None,
        "tree and workspace focus disagree",
    ));

    let mut two_focused_nodes = tree_with_focused_target_window();
    node_mut(&mut two_focused_nodes, TARGET_WORKSPACE_ID)["focused"] = json!(true);
    cases.push((
        two_focused_nodes,
        workspaces_reply("9"),
        None,
        "tree and workspace focus disagree",
    ));

    let mut missing_workspace = workspaces_reply("9");
    missing_workspace.remove(0);
    cases.push((
        tree_with_focused_target_window(),
        missing_workspace,
        None,
        "identities disagree",
    ));

    let mut renamed_in_tree = tree_with_focused_target_window();
    node_mut(&mut renamed_in_tree, OTHER_WORKSPACE_ID)["name"] = json!("one");
    cases.push((
        renamed_in_tree,
        workspaces_reply("9"),
        None,
        "names disagree",
    ));

    for (tree, workspaces, current, expected) in cases {
        let error = validate_recorded_snapshot(tree, &workspaces, current.as_ref()).unwrap_err();
        assert_contains(&error, expected);
    }
}

#[test]
fn walk_inherits_workspaces_and_unsafe_placement() {
    let mut fullscreen_group = group(
        30,
        "splitv",
        rect(960, 0, 960, 1080),
        vec![wayland_window(
            21,
            "inside-fullscreen",
            rect(960, 0, 960, 1080),
        )],
    );
    fullscreen_group["fullscreen_mode"] = json!(1);
    let mut tree = desktop(
        "splith",
        vec![
            wayland_window(20, "tiled", rect(0, 0, 960, 1080)),
            fullscreen_group,
        ],
        20,
    );

    let mut floating_container = group(
        40,
        "splith",
        rect(100, 100, 400, 300),
        vec![wayland_window(22, "floating", rect(100, 100, 400, 300))],
    );
    floating_container["type"] = json!("floating_con");
    node_mut(&mut tree, TARGET_WORKSPACE_ID)["floating_nodes"] = json!([floating_container]);
    node_mut(&mut tree, SCRATCHPAD_WORKSPACE_ID)["floating_nodes"] =
        json!([wayland_window(23, "hidden", rect(0, 0, 400, 300))]);

    let tree = parse_tree(tree);
    let placements: Vec<(i64, Option<i64>, bool)> = walk(&tree)
        .iter()
        .map(|placed| {
            let workspace_id = placed.workspace.map(|workspace| workspace.id);
            (placed.node.id, workspace_id, placed.unsafe_state)
        })
        .collect();
    let expected = [
        (1, None, false),
        (2, None, false),
        (SCRATCHPAD_WORKSPACE_ID, Some(SCRATCHPAD_WORKSPACE_ID), true),
        (23, Some(SCRATCHPAD_WORKSPACE_ID), true),
        (4, None, false),
        (OTHER_WORKSPACE_ID, Some(OTHER_WORKSPACE_ID), false),
        (PREEXISTING_WINDOW_ID, Some(OTHER_WORKSPACE_ID), false),
        // The workspace's own fullscreen_mode 1 is not window state.
        (TARGET_WORKSPACE_ID, Some(TARGET_WORKSPACE_ID), false),
        (20, Some(TARGET_WORKSPACE_ID), false),
        (30, Some(TARGET_WORKSPACE_ID), true),
        (21, Some(TARGET_WORKSPACE_ID), true),
        (40, Some(TARGET_WORKSPACE_ID), true),
        (22, Some(TARGET_WORKSPACE_ID), true),
    ];

    assert_eq!(placements, expected);
}

// Workspace events within one fence. As in the lifecycle, the harness starts on workspace
// "1" and switches to the disposable workspace "9"; focus must never reach workspace "3".

/// A workspace absent from the fixture desktop.
const ELSEWHERE_WORKSPACE_ID: i64 = 8;

fn original_identity() -> WorkspaceIdentity {
    WorkspaceIdentity {
        id: OTHER_WORKSPACE_ID,
        name: "1".into(),
    }
}

fn elsewhere_identity() -> WorkspaceIdentity {
    WorkspaceIdentity {
        id: ELSEWHERE_WORKSPACE_ID,
        name: "3".into(),
    }
}

/// The lifecycle's switch from the original workspace to the disposable workspace.
fn switch_to_target() -> ExpectedSwitch {
    ExpectedSwitch {
        from_id: OTHER_WORKSPACE_ID,
        destination_name: "9".into(),
    }
}

/// A workspace event as `swaymsg -m` prints it, reduced to workspace identities. An absent
/// workspace is reported as null.
fn workspace_event(
    change: &str,
    old: Option<&WorkspaceIdentity>,
    current: Option<&WorkspaceIdentity>,
) -> WorkspaceEvent {
    let describe = |workspace: Option<&WorkspaceIdentity>| {
        workspace.map(
            |workspace| json!({"id": workspace.id, "type": "workspace", "name": workspace.name}),
        )
    };
    let event = json!({"change": change, "old": describe(old), "current": describe(current)});
    serde_json::from_value(event).expect("a workspace event in Sway's format")
}

fn focus_event(old: &WorkspaceIdentity, current: &WorkspaceIdentity) -> WorkspaceEvent {
    workspace_event("focus", Some(old), Some(current))
}

/// The state that `read_through_fence` carries between workspace events: the established
/// workspace, which persists across fences, and this fence's expected switch and whether it
/// has occurred.
struct FenceState {
    current_workspace: Option<WorkspaceIdentity>,
    expected_switch: Option<ExpectedSwitch>,
    switch_window_index: Option<usize>,
}

impl FenceState {
    /// A fence that has not yet read any workspace event.
    fn new(
        current_workspace: Option<WorkspaceIdentity>,
        expected_switch: Option<ExpectedSwitch>,
    ) -> Self {
        Self {
            current_workspace,
            expected_switch,
            switch_window_index: None,
        }
    }

    fn apply(&mut self, workspace_event: &WorkspaceEvent) -> Result<(), String> {
        apply_workspace_event(
            workspace_event,
            self.expected_switch.as_ref(),
            &mut self.switch_window_index,
            0,
            &mut self.current_workspace,
        )
    }

    fn assert_accepts(&mut self, workspace_event: &WorkspaceEvent) {
        if let Err(error) = self.apply(workspace_event) {
            panic!("rejected {workspace_event:?}: {error}");
        }
    }

    /// Requires a rejection that leaves the fence's state unchanged.
    fn assert_rejects(&mut self, workspace_event: &WorkspaceEvent, expected_error: &str) {
        let workspace_before = self.current_workspace.clone();
        let boundary_before = self.switch_window_index;

        let Err(error) = self.apply(workspace_event) else {
            panic!("accepted {workspace_event:?}");
        };
        assert_contains(&error, expected_error);
        assert_eq!(self.current_workspace, workspace_before);
        assert_eq!(self.switch_window_index, boundary_before);
    }
}

#[test]
fn refocusing_the_established_workspace_is_accepted() {
    let original = original_identity();
    let mut fence = FenceState::new(Some(original.clone()), None);

    fence.assert_accepts(&focus_event(&original, &original));
    // Without an `old` workspace, focusing the established one is equally harmless.
    fence.assert_accepts(&workspace_event("focus", None, Some(&original)));

    assert_eq!(fence.current_workspace, Some(original));
    assert_eq!(fence.switch_window_index, None);
}

#[test]
fn expected_switch_is_accepted_and_establishes_its_destination() {
    let original = original_identity();
    let target = target_identity();
    let mut fence = FenceState::new(Some(original.clone()), Some(switch_to_target()));

    // Creating the destination is not the switch.
    fence.assert_accepts(&workspace_event("init", None, Some(&target)));

    assert_eq!(fence.switch_window_index, None);
    assert_eq!(fence.current_workspace, Some(original.clone()));

    fence.assert_accepts(&focus_event(&original, &target));
    assert_eq!(fence.switch_window_index, Some(0));
    assert_eq!(fence.current_workspace, Some(target.clone()));

    // Later events in the same fence are judged against the destination.
    fence.assert_accepts(&focus_event(&target, &target));
    fence.assert_rejects(
        &workspace_event("empty", None, Some(&target)),
        "focused workspace was destroyed",
    );
    assert_eq!(fence.current_workspace, Some(target));
}

#[test]
fn restoration_switch_ignores_the_emptied_disposable_workspace() {
    let original = original_identity();
    let target = target_identity();
    let restoration = ExpectedSwitch {
        from_id: TARGET_WORKSPACE_ID,
        destination_name: "1".into(),
    };
    let mut fence = FenceState::new(Some(target.clone()), Some(restoration));

    fence.assert_accepts(&focus_event(&target, &original));
    // Once focus has left it, the empty disposable workspace's destruction is unrelated.
    fence.assert_accepts(&workspace_event("empty", None, Some(&target)));

    assert_eq!(fence.switch_window_index, Some(0));
    assert_eq!(fence.current_workspace, Some(original));
}

#[test]
fn no_second_switch_is_accepted_after_the_expected_one() {
    let original = original_identity();
    let target = target_identity();
    let elsewhere = elsewhere_identity();
    let second_switches = [
        focus_event(&target, &elsewhere),
        // Ending on the expected destination does not make another transition expected.
        focus_event(&original, &target),
        focus_event(&elsewhere, &target),
    ];

    for second_switch in &second_switches {
        let mut fence = FenceState::new(Some(original.clone()), Some(switch_to_target()));
        fence.assert_accepts(&focus_event(&original, &target));
        fence.assert_rejects(second_switch, "user control takes precedence");
    }
}

#[test]
fn unrelated_workspace_events_are_ignored() {
    let original = original_identity();
    let elsewhere = elsewhere_identity();
    let unrelated_events = [
        workspace_event("init", None, Some(&elsewhere)),
        workspace_event("urgent", None, Some(&original)),
        workspace_event("urgent", None, Some(&elsewhere)),
        workspace_event("empty", None, Some(&elsewhere)),
    ];

    for current_workspace in [None, Some(original)] {
        for unrelated_event in &unrelated_events {
            let mut fence = FenceState::new(current_workspace.clone(), None);
            fence.assert_accepts(unrelated_event);
            assert_eq!(fence.current_workspace, current_workspace);
            assert_eq!(fence.switch_window_index, None);
        }
    }
}

#[test]
fn focus_changes_before_initial_preflight_are_rejected() {
    let original = original_identity();
    let elsewhere = elsewhere_identity();

    // Until preflight establishes a workspace, even a refocus is a focus change.
    for focus_change in [
        focus_event(&original, &elsewhere),
        focus_event(&original, &original),
    ] {
        FenceState::new(None, None).assert_rejects(&focus_change, "during initial preflight");
    }
}

#[test]
fn focus_changes_involving_another_workspace_are_rejected() {
    let original = original_identity();
    let elsewhere = elsewhere_identity();
    let away = focus_event(&original, &elsewhere);
    let back = focus_event(&elsewhere, &original);
    let elsewhere_without_old = workspace_event("focus", None, Some(&elsewhere));

    // A fence stops at the first rejected event, so focus returning later in the same fence
    // cannot conceal the departure. The return alone is also rejected, because it leaves
    // another workspace.
    for focus_change in [away, back, elsewhere_without_old] {
        let mut fence = FenceState::new(Some(original.clone()), None);
        fence.assert_rejects(&focus_change, "user control takes precedence");
    }
}

#[test]
fn mismatched_expected_switches_are_rejected() {
    let original = original_identity();
    let target = target_identity();
    let elsewhere = elsewhere_identity();
    let mismatches = [
        // The wrong origin, including arrival at the destination from another workspace.
        focus_event(&elsewhere, &target),
        // The wrong destination.
        focus_event(&original, &elsewhere),
        // No reported origin.
        workspace_event("focus", None, Some(&target)),
        // While the switch is pending, even a refocus of the origin is unexpected.
        focus_event(&original, &original),
    ];

    for mismatch in &mismatches {
        let mut fence = FenceState::new(Some(original.clone()), Some(switch_to_target()));
        fence.assert_rejects(mismatch, "unexpected workspace switch");
    }
}

#[test]
fn destroying_or_restructuring_workspaces_is_rejected() {
    let original = original_identity();
    let elsewhere = elsewhere_identity();
    let cases = [
        (
            workspace_event("empty", None, Some(&original)),
            "focused workspace was destroyed",
        ),
        (
            workspace_event("reload", None, None),
            "workspace Reload during harness execution",
        ),
        // Renaming or moving any workspace, not only the established one, is rejected.
        (
            workspace_event("rename", None, Some(&original)),
            "workspace Rename during harness execution",
        ),
        (
            workspace_event("rename", None, Some(&elsewhere)),
            "workspace Rename during harness execution",
        ),
        (
            workspace_event("move", None, Some(&original)),
            "workspace Move during harness execution",
        ),
        (
            workspace_event("move", None, Some(&elsewhere)),
            "workspace Move during harness execution",
        ),
    ];

    for (workspace_change, expected_error) in &cases {
        let mut fence = FenceState::new(Some(original.clone()), None);
        fence.assert_rejects(workspace_change, expected_error);
    }
}

#[test]
fn profile_switch_records_the_window_event_boundary_once() {
    let original = original_identity();
    let target = target_identity();
    let mut current = Some(original.clone());
    let mut boundary = None;
    let expected = switch_to_target();
    apply_workspace_event(
        &workspace_event("init", None, Some(&target)),
        Some(&expected),
        &mut boundary,
        3,
        &mut current,
    )
    .unwrap();
    assert_eq!(boundary, None);
    apply_workspace_event(
        &focus_event(&original, &target),
        Some(&expected),
        &mut boundary,
        5,
        &mut current,
    )
    .unwrap();
    assert_eq!(boundary, Some(5));
    apply_workspace_event(
        &focus_event(&target, &target),
        Some(&expected),
        &mut boundary,
        9,
        &mut current,
    )
    .unwrap();
    assert_eq!(boundary, Some(5));
    assert!(
        apply_workspace_event(
            &focus_event(&target, &original),
            Some(&expected),
            &mut boundary,
            10,
            &mut current
        )
        .is_err()
    );
    assert_eq!(boundary, Some(5));
}

#[test]
fn disposable_rename_accepts_only_exact_focused_identity_and_true_workspace_number() {
    let original = WorkspaceIdentity {
        id: TARGET_WORKSPACE_ID,
        name: "9".into(),
    };
    let renamed = WorkspaceIdentity {
        id: TARGET_WORKSPACE_ID,
        name: "stage-test music".into(),
    };
    let event_value = json!({"change": "rename", "old": null, "current": {
        "type": "workspace", "id": TARGET_WORKSPACE_ID, "name": "stage-test music", "num": -1
    }});
    let mut focused = Some(original.clone());
    let event = serde_json::from_value(event_value.clone()).unwrap();
    accept_disposable_rename(&event, &renamed, &mut focused).unwrap();
    assert_eq!(focused, Some(renamed.clone()));
    for scenario in [
        "id",
        "name",
        "num",
        "missing-num",
        "type",
        "old",
        "change",
        "unfocused",
    ] {
        let mut value = event_value.clone();
        let mut focused = Some(original.clone());
        match scenario {
            "id" => value["current"]["id"] = json!(80),
            "name" => value["current"]["name"] = json!("other"),
            "num" => value["current"]["num"] = json!(9),
            "missing-num" => {
                value["current"].as_object_mut().unwrap().remove("num");
            }
            "type" => value["current"]["type"] = json!("con"),
            "old" => value["old"] = value["current"].clone(),
            "change" => value["change"] = json!("focus"),
            _ => focused = None,
        }
        let event = serde_json::from_value(value).unwrap();
        assert!(
            accept_disposable_rename(&event, &renamed, &mut focused).is_err(),
            "{scenario}"
        );
    }
}
