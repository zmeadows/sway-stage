use super::*;
use crate::fixtures::{
    OTHER_WORKSPACE_ID, PREEXISTING_WINDOW_ID, SCRATCHPAD_WORKSPACE_ID, TARGET_WORKSPACE_ID,
    assert_contains, desktop, group, node_mut, parse_tree, rect, wayland_window, workspaces_reply,
};
use serde_json::{Value, json};

/// The target workspace holding one focused window.
fn focused_target() -> Value {
    desktop(
        "splith",
        vec![wayland_window(20, "app", rect(0, 0, 1920, 1080))],
        20,
    )
}

fn validate(
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
    let focused_id = validate(focused_target(), &workspaces_reply("9"), None).unwrap();
    assert_eq!(focused_id, TARGET_WORKSPACE_ID);

    let focused_id = validate(
        focused_target(),
        &workspaces_reply("9"),
        Some(&target_identity()),
    )
    .unwrap();
    assert_eq!(focused_id, TARGET_WORKSPACE_ID);

    // An empty workspace may itself hold focus.
    let empty = desktop("splith", Vec::new(), TARGET_WORKSPACE_ID);
    assert_eq!(
        validate(empty, &workspaces_reply("9"), None).unwrap(),
        TARGET_WORKSPACE_ID
    );
}

#[test]
fn snapshot_inconsistencies_are_rejected() {
    let mut cases: Vec<(Value, Vec<Workspace>, Option<WorkspaceIdentity>, &str)> = Vec::new();

    let mut duplicate_ids = focused_target();
    node_mut(&mut duplicate_ids, 20)["id"] = json!(PREEXISTING_WINDOW_ID);
    cases.push((
        duplicate_ids,
        workspaces_reply("9"),
        None,
        "duplicate container identities",
    ));

    let mut not_root = focused_target();
    not_root["type"] = json!("output");
    cases.push((not_root, workspaces_reply("9"), None, "invalid tree"));

    let mut duplicate_names = workspaces_reply("9");
    duplicate_names[0].name = "9".into();
    cases.push((
        focused_target(),
        duplicate_names,
        None,
        "ambiguous workspace identities",
    ));

    let mut no_focused_workspace = workspaces_reply("9");
    no_focused_workspace[1].focused = false;
    cases.push((
        focused_target(),
        no_focused_workspace,
        None,
        "exactly one focused workspace",
    ));

    let mut two_focused_workspaces = workspaces_reply("9");
    two_focused_workspaces[0].focused = true;
    cases.push((
        focused_target(),
        two_focused_workspaces,
        None,
        "exactly one focused workspace",
    ));

    let renamed = WorkspaceIdentity {
        id: TARGET_WORKSPACE_ID,
        name: "renamed".into(),
    };
    cases.push((
        focused_target(),
        workspaces_reply("9"),
        Some(renamed),
        "focus or identity changed",
    ));
    let elsewhere = WorkspaceIdentity {
        id: OTHER_WORKSPACE_ID,
        name: "1".into(),
    };
    cases.push((
        focused_target(),
        workspaces_reply("9"),
        Some(elsewhere),
        "focus or identity changed",
    ));

    cases.push((
        focused_target(),
        workspaces_reply("1"),
        None,
        "tree and workspace focus disagree",
    ));

    let mut two_focused_nodes = focused_target();
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
        focused_target(),
        missing_workspace,
        None,
        "identities disagree",
    ));

    let mut renamed_in_tree = focused_target();
    node_mut(&mut renamed_in_tree, OTHER_WORKSPACE_ID)["name"] = json!("one");
    cases.push((
        renamed_in_tree,
        workspaces_reply("9"),
        None,
        "names disagree",
    ));

    for (tree, workspaces, current, expected) in cases {
        let error = validate(tree, &workspaces, current.as_ref()).unwrap_err();
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
