use std::env;

use serde_json::json;

use super::*;
use crate::fixtures::{
    FOOT, RUN_ID, SLEEP, TARGET_WORKSPACE_ID, accepted_windows, desktop, parse_tree,
    workspaces_reply,
};
use crate::ipc::{InitialWorkspaceLayout, WorkspaceEvent};
use crate::profile::verify_workspace_events;
use crate::scenarios::{
    ConfiguredNode, FOOT_DEFAULT_APP_ID, GroupLayout, session_roundtrip_layouts,
};

#[test]
fn layouts_reproduce_the_reported_workspaces_with_default_foot_app_ids() {
    let layouts = session_roundtrip_layouts(RUN_ID, FOOT, SLEEP);
    let expected_groups = [
        (GroupLayout::Horizontal, 1, InitialWorkspaceLayout::SplitH),
        (GroupLayout::Vertical, 1, InitialWorkspaceLayout::SplitV),
        (GroupLayout::Tabbed, 3, InitialWorkspaceLayout::Tabbed),
        (GroupLayout::Stacked, 3, InitialWorkspaceLayout::Stacked),
    ];

    let mut titles = HashSet::new();
    for (layout, (expected_group, expected_active, expected_initial_layout)) in
        layouts.iter().zip(expected_groups)
    {
        let ConfiguredNode::Group {
            layout: group_layout,
            active,
            children,
            ..
        } = &layout.layout
        else {
            panic!("{} has a group root", layout.name);
        };
        assert_eq!(*group_layout, expected_group, "{}", layout.name);
        assert_eq!(*active, expected_active, "{}", layout.name);
        assert_eq!(children.len(), 3, "{}", layout.name);
        assert_eq!(
            layout.initial_workspace_layout,
            Some(expected_initial_layout)
        );
        assert_eq!(layout.expected_focus, "window3");
        assert_eq!(layout.configured_focus, Some("window3"));

        for (leaf_index, leaf) in layout.layout.leaves().iter().enumerate() {
            assert_eq!(leaf.layout_id, format!("window{}", leaf_index + 1));
            assert_eq!(leaf.weight, 1);
            assert_eq!(leaf.app_id, FOOT_DEFAULT_APP_ID);
            assert!(leaf.requires_process_evidence);
            // No `--app-id`: only the per-window title argument tells the launches apart.
            let title = format!(
                "--title=sway-stage-it-{RUN_ID}-{}-{}",
                layout.name, leaf.layout_id
            );
            assert_eq!(leaf.command, [FOOT, &title, SLEEP, "300"]);
            assert!(titles.insert(title));
        }
    }
}

#[test]
fn window_launch_scripts_replace_themselves_with_the_literal_command() {
    let command = vec![
        "/usr/bin/foot".to_owned(),
        "--title=it's $HOME".to_owned(),
        "/usr/bin/sleep".to_owned(),
        "300".to_owned(),
    ];
    assert_eq!(
        window_launch_script(&command),
        "#!/bin/sh\nexec '/usr/bin/foot' '--title=it'\\''s $HOME' '/usr/bin/sleep' '300'\n"
    );
}

#[test]
fn the_menu_offers_both_captures_then_the_captured_profile() {
    assert_eq!(
        menu_rows(),
        [
            "Capture workspace layout…",
            "Capture session profile…",
            "Profile: session"
        ]
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
            "type": "workspace", "id": workspace.id, "name": workspace.name,
            "num": workspace.name.parse::<i32>().unwrap(),
        },
        "old": old,
    }))
    .expect("a workspace event in Sway's format")
}

#[test]
fn opening_the_original_workspaces_removes_only_the_empty_primary() {
    let primary = identity(10, "2");
    let first = identity(50, "5");
    let second = identity(60, "6");

    let from_primary = [
        workspace_event("init", &first, None),
        workspace_event("focus", &first, Some(10)),
        workspace_event("empty", &primary, None),
    ];
    verify_workspace_events(&from_primary, &selection_events(&primary, &first, true)).unwrap();
    assert!(
        verify_workspace_events(&from_primary, &selection_events(&primary, &first, false)).is_err()
    );

    let from_populated = || {
        vec![
            workspace_event("init", &second, None),
            workspace_event("focus", &second, Some(50)),
        ]
    };
    let expected = selection_events(&first, &second, false);
    verify_workspace_events(&from_populated(), &expected).unwrap();
    let mut removed_populated = from_populated();
    removed_populated.push(workspace_event("empty", &first, None));
    assert!(verify_workspace_events(&removed_populated, &expected).is_err());
}

/// The reload's selections from the fixture's workspace through the four targets.
fn reload_selections(
    origin: &WorkspaceIdentity,
    targets: &[WorkspaceIdentity],
) -> Vec<WorkspaceEvent> {
    let mut events = Vec::new();
    let mut previous_id = origin.id;
    for target in targets {
        events.push(workspace_event("init", target, None));
        events.push(workspace_event("focus", target, Some(previous_id)));
        previous_id = target.id;
    }
    events
}

#[test]
fn the_reload_selects_every_target_in_order_and_recreates_nothing() {
    let fixture_workspace = identity(5, "1");
    let targets = [
        identity(70, "5"),
        identity(71, "6"),
        identity(72, "7"),
        identity(73, "8"),
    ];
    let expected = reload_events(&fixture_workspace, &targets);
    verify_workspace_events(&reload_selections(&fixture_workspace, &targets), &expected).unwrap();

    // A misplaced window would make Sway recreate or keep a workspace for it: any extra
    // creation or removal differs, as does a reordered or skipped selection.
    let mut recreated = reload_selections(&fixture_workspace, &targets);
    recreated.insert(2, workspace_event("init", &identity(74, "1"), None));
    let mut removed_origin = reload_selections(&fixture_workspace, &targets);
    removed_origin.push(workspace_event("empty", &fixture_workspace, None));
    let reordered_targets = [
        targets[0].clone(),
        targets[2].clone(),
        targets[1].clone(),
        targets[3].clone(),
    ];
    let reordered = reload_selections(&fixture_workspace, &reordered_targets);
    let skipped = reload_selections(&fixture_workspace, &targets[1..]);
    for observed in [recreated, removed_origin, reordered, skipped] {
        assert!(verify_workspace_events(&observed, &expected).is_err());
    }
}

#[test]
fn returning_from_a_cleaned_workspace_removes_only_that_workspace() {
    let cleaned = identity(73, "8");
    let destination = identity(72, "7");
    let expected = return_events(&cleaned, &destination);
    let observed = [
        workspace_event("focus", &destination, Some(73)),
        workspace_event("empty", &cleaned, None),
    ];
    verify_workspace_events(&observed, &expected).unwrap();
    assert!(verify_workspace_events(&observed[..1], &expected).is_err());
}

#[test]
fn ownership_passes_over_the_other_workspaces_windows_and_preexisting_ones() {
    let layouts = session_roundtrip_layouts(RUN_ID, FOOT, SLEEP);
    let workspaces: Vec<SessionWorkspace> = layouts
        .into_iter()
        .enumerate()
        .map(|(index, layout)| {
            let first_id = 20 + 10 * index as i64;
            let observed = Snapshot {
                tree: parse_tree(desktop("splith", Vec::new(), TARGET_WORKSPACE_ID)),
                workspaces: workspaces_reply("9"),
                focused_workspace_id: TARGET_WORKSPACE_ID,
                window_events: Vec::new(),
                workspace_events: Vec::new(),
            };
            SessionWorkspace {
                target: Target::record(&observed, "9").unwrap().unwrap(),
                layout,
                accepted: accepted_windows(&[
                    (first_id, "window1"),
                    (first_id + 1, "window2"),
                    (first_id + 2, "window3"),
                ]),
            }
        })
        .collect();

    let preexisting = HashSet::from([6]);
    let known_ids = known_ids_besides(&workspaces, 1, &preexisting);
    assert_eq!(
        known_ids,
        HashSet::from([6, 20, 21, 22, 40, 41, 42, 50, 51, 52])
    );
}

#[test]
fn only_the_expected_workspaces_may_remain() {
    let observed = Snapshot {
        tree: parse_tree(desktop("splith", Vec::new(), TARGET_WORKSPACE_ID)),
        workspaces: workspaces_reply("9"),
        focused_workspace_id: TARGET_WORKSPACE_ID,
        window_events: Vec::new(),
        workspace_events: Vec::new(),
    };
    require_workspace_names(&observed, &["9", "1"]).unwrap();
    assert!(require_workspace_names(&observed, &["1"]).is_err());
    assert!(require_workspace_names(&observed, &["1", "9", "5"]).is_err());
}

#[test]
fn capture_directories_must_hold_exactly_the_expected_files() {
    let directory = env::temp_dir().join(format!(
        "sway-stage-session-roundtrip-test-{}",
        std::process::id()
    ));
    fs::create_dir(&directory).unwrap();
    let expected = vec!["session-001.yaml".to_owned(), "session-002.yaml".to_owned()];

    fs::write(directory.join("session-001.yaml"), "").unwrap();
    assert!(require_directory_files(&directory, &expected).is_err());
    fs::write(directory.join("session-002.yaml"), "").unwrap();
    require_directory_files(&directory, &expected).unwrap();
    fs::write(directory.join("session-003.yaml"), "").unwrap();
    assert!(require_directory_files(&directory, &expected).is_err());
    fs::remove_file(directory.join("session-003.yaml")).unwrap();
    fs::create_dir(directory.join("nested")).unwrap();
    assert!(require_directory_files(&directory, &expected).is_err());

    fs::remove_dir_all(&directory).unwrap();
}
