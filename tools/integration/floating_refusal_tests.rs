use super::*;
use crate::fixtures::{
    FOOT, OTHER_WORKSPACE_ID, PREEXISTING_WINDOW_ID, RUN_ID, SLEEP, TARGET_WORKSPACE_ID, desktop,
    node_mut, parse_tree, rect, set_focus, wayland_window, window_event, workspaces_reply,
};
use serde_json::{Value, json};

const LAUNCH_SCRIPT: &str = "/private path/record-launch.sh";
const LAUNCH_RECORD: &str = "/private path/launches";
const FLOATING_WINDOW_ID: i64 = 20;

/// The floating window's leaf, without process evidence: fixture PIDs name no process.
fn floating_leaf() -> ConfiguredWindow {
    let mut leaf = floating_window(Case::Layout, RUN_ID, FOOT, SLEEP)
        .layout
        .leaves()[0]
        .clone();
    leaf.requires_process_evidence = false;
    leaf
}

/// The target workspace holding only `floating_window`, which is focused.
fn floating_only_tree(floating_window: Value) -> Value {
    let mut tree = desktop("splith", Vec::new(), TARGET_WORKSPACE_ID);
    node_mut(&mut tree, TARGET_WORKSPACE_ID)["floating_nodes"] = json!([floating_window]);
    set_focus(&mut tree, FLOATING_WINDOW_ID);
    tree
}

/// This run's window as Sway reports it after `floating enable`.
fn user_floating_window(app_id: &str) -> Value {
    let mut window = wayland_window(FLOATING_WINDOW_ID, app_id, rect(600, 300, 720, 480));
    window["type"] = json!("floating_con");
    window["floating"] = json!("user_on");
    window
}

fn recorded_snapshot(tree: Value) -> Snapshot {
    Snapshot {
        tree: parse_tree(tree),
        workspaces: workspaces_reply("9"),
        focused_workspace_id: TARGET_WORKSPACE_ID,
        window_events: Vec::new(),
        workspace_events: Vec::new(),
    }
}

#[test]
fn the_cases_are_independently_selectable() {
    let names = CASES.map(Case::name);
    assert_eq!(
        names,
        ["layout-floating-refusal", "profile-floating-refusal"]
    );
    assert!(!names[0].contains(names[1]) && !names[1].contains(names[0]));
}

#[test]
fn the_floating_window_is_unique_to_its_case_and_requires_launch_evidence() {
    let layout_window = floating_window(Case::Layout, RUN_ID, FOOT, SLEEP);
    let profile_window = floating_window(Case::Profile, RUN_ID, FOOT, SLEEP);
    let leaves = layout_window.layout.leaves();
    let [leaf] = leaves.as_slice() else {
        panic!("the floating window is one window");
    };

    assert!(leaf.requires_process_evidence);
    assert!(leaf.app_id.contains(RUN_ID) && leaf.app_id.contains("layout-floating-refusal"));
    assert_eq!(leaf.command[0], FOOT);
    assert!(leaf.command.contains(&format!("--app-id={}", leaf.app_id)));
    assert_ne!(leaf.app_id, profile_window.layout.leaves()[0].app_id);
}

#[test]
fn every_refused_launch_would_be_recorded() {
    let standalone = refused_layouts(
        Case::Layout,
        RUN_ID,
        FOOT,
        SLEEP,
        LAUNCH_SCRIPT,
        LAUNCH_RECORD,
    );
    let profile = refused_layouts(
        Case::Profile,
        RUN_ID,
        FOOT,
        SLEEP,
        LAUNCH_SCRIPT,
        LAUNCH_RECORD,
    );
    let names: Vec<&str> = standalone
        .iter()
        .chain(&profile)
        .map(|layout| layout.name)
        .collect();
    assert_eq!(
        names,
        ["layout-floating-refusal", "profile-first", "profile-second"]
    );

    let floating_app_id = floating_window(Case::Layout, RUN_ID, FOOT, SLEEP)
        .layout
        .leaves()[0]
        .app_id
        .clone();
    for layout in standalone.iter().chain(&profile) {
        for leaf in layout.layout.leaves() {
            assert_eq!(
                leaf.command[..4],
                [
                    "/bin/sh".to_owned(),
                    LAUNCH_SCRIPT.to_owned(),
                    format!("{}-{}", layout.name, leaf.layout_id),
                    LAUNCH_RECORD.to_owned(),
                ]
            );
            assert_eq!(leaf.command[4], FOOT);
            assert_ne!(leaf.app_id, floating_app_id);
        }
    }
}

#[test]
fn making_the_window_floating_allows_only_its_own_floating_and_focus_events() {
    let accepted = [
        window_event("floating", FLOATING_WINDOW_ID, None),
        window_event("focus", FLOATING_WINDOW_ID, None),
        window_event("title", PREEXISTING_WINDOW_ID, None),
    ];
    assert!(require_float_events(&accepted, FLOATING_WINDOW_ID).is_ok());

    for unexpected in [
        window_event("floating", PREEXISTING_WINDOW_ID, None),
        window_event("focus", PREEXISTING_WINDOW_ID, None),
        window_event("move", FLOATING_WINDOW_ID, None),
        window_event("new", 21, None),
    ] {
        let error = require_float_events(&[unexpected], FLOATING_WINDOW_ID).unwrap_err();
        assert!(error.contains("unexpected window activity"), "{error}");
    }
}

#[test]
fn only_the_window_floating_alone_on_its_workspace_is_accepted() {
    let leaf = floating_leaf();
    let floating_only = recorded_snapshot(floating_only_tree(user_floating_window(&leaf.app_id)));
    verify_floating_only(
        &floating_only,
        TARGET_WORKSPACE_ID,
        FLOATING_WINDOW_ID,
        &leaf,
    )
    .unwrap();

    // Any tiled container, even beside the floating window, is other occupancy.
    let mut with_tiled = floating_only_tree(user_floating_window(&leaf.app_id));
    node_mut(&mut with_tiled, TARGET_WORKSPACE_ID)["nodes"] =
        json!([wayland_window(21, "other", rect(0, 0, 1920, 1080))]);
    set_focus(&mut with_tiled, FLOATING_WINDOW_ID);

    let mut tiled_state = user_floating_window(&leaf.app_id);
    tiled_state["floating"] = json!("auto_off");
    let mut fullscreen = user_floating_window(&leaf.app_id);
    fullscreen["fullscreen_mode"] = json!(1);
    let mut in_scratchpad = user_floating_window(&leaf.app_id);
    in_scratchpad["scratchpad_state"] = json!("fresh");

    let mut same_app_id_elsewhere = floating_only_tree(user_floating_window(&leaf.app_id));
    node_mut(&mut same_app_id_elsewhere, PREEXISTING_WINDOW_ID)["app_id"] = json!(leaf.app_id);

    for (tree, expected) in [
        (with_tiled, "must hold only floating window"),
        (
            floating_only_tree(user_floating_window("another-app")),
            "identity or state differs",
        ),
        (floating_only_tree(tiled_state), "identity or state differs"),
        (floating_only_tree(fullscreen), "identity or state differs"),
        (
            floating_only_tree(in_scratchpad),
            "identity or state differs",
        ),
        (same_app_id_elsewhere, "another window has"),
    ] {
        let error = verify_floating_only(
            &recorded_snapshot(tree),
            TARGET_WORKSPACE_ID,
            FLOATING_WINDOW_ID,
            &leaf,
        )
        .unwrap_err();
        assert!(error.contains(expected), "{expected}: {error}");
    }

    // The window must be on the named workspace, not merely somewhere.
    let error = verify_floating_only(
        &floating_only,
        OTHER_WORKSPACE_ID,
        FLOATING_WINDOW_ID,
        &leaf,
    )
    .unwrap_err();
    assert!(error.contains("must hold only floating window"), "{error}");
}

#[test]
fn a_required_launch_argument_check_refuses_an_unprovable_process() {
    let mut leaf = floating_leaf();
    leaf.requires_process_evidence = true;
    let floating_only = recorded_snapshot(floating_only_tree(user_floating_window(&leaf.app_id)));

    let error = verify_floating_only(
        &floating_only,
        TARGET_WORKSPACE_ID,
        FLOATING_WINDOW_ID,
        &leaf,
    )
    .unwrap_err();
    assert!(error.contains("cannot verify process argv"), "{error}");
}
