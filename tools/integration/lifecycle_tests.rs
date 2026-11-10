use super::*;
use crate::fixtures::{
    OTHER_WORKSPACE_ID, PREEXISTING_WINDOW_ID, RUN_ID, TARGET_WORKSPACE_ID, accepted,
    assert_contains, container_ids, desktop, find, group, node_mut, parse_tree, rect,
    selector_value, standard_scenario, uncertain_message, wayland_window, window_event,
    workspaces_reply,
};
use serde_json::{Value, json};
use std::os::unix::fs::{PermissionsExt, symlink};

fn observed(tree: Value, focused_name: &str, focused_workspace_id: i64) -> Snapshot {
    Snapshot {
        tree: parse_tree(tree),
        workspaces: workspaces_reply(focused_name),
        focused_workspace_id,
        window_events: Vec::new(),
    }
}

fn starting_snapshot() -> Snapshot {
    observed(
        desktop("splith", Vec::new(), PREEXISTING_WINDOW_ID),
        "1",
        OTHER_WORKSPACE_ID,
    )
}

fn target_identity() -> WorkspaceIdentity {
    WorkspaceIdentity {
        id: TARGET_WORKSPACE_ID,
        name: "9".into(),
    }
}

fn original_identity() -> WorkspaceIdentity {
    WorkspaceIdentity {
        id: OTHER_WORKSPACE_ID,
        name: "1".into(),
    }
}

#[test]
fn cli_requires_an_explicit_workspace_and_preserves_optional_arguments() {
    assert!(Arguments::try_parse_from(["integration"]).is_err());
    for workspace in ["1", "9", "999999999"] {
        let arguments =
            Arguments::try_parse_from(["integration", "--workspace", workspace]).unwrap();
        assert_eq!(arguments.workspace, workspace);
        assert!(!arguments.extended);
        assert_eq!(arguments.firefox_app_id, None);
        assert_eq!(
            arguments.binary,
            concat!(env!("CARGO_MANIFEST_DIR"), "/target/release/sway-stage")
        );
    }
    let arguments = Arguments::try_parse_from([
        "integration",
        "--workspace",
        "9",
        "--extended",
        "--firefox-app-id",
        "firefox-esr",
        "--binary",
        "/a path/sway-stage",
    ])
    .unwrap();
    assert!(arguments.extended);
    assert_eq!(arguments.firefox_app_id.as_deref(), Some("firefox-esr"));
    assert_eq!(arguments.binary, "/a path/sway-stage");

    for invalid in [
        "",
        "0",
        "01",
        "+1",
        "-1",
        "1000000000",
        "1;kill",
        "9: test",
        " 9",
        "9\n",
        "９",
    ] {
        assert!(
            Arguments::try_parse_from(["integration", "--workspace", invalid]).is_err(),
            "{invalid:?}"
        );
    }
    for arguments in [
        vec!["integration", "--workspace", "9", "--workspace", "8"],
        vec!["integration", "--workspace", "9", "--unknown"],
        vec!["integration", "--workspace", "9", "extra"],
        vec!["integration", "--workspace"],
    ] {
        assert!(Arguments::try_parse_from(arguments).is_err());
    }
}

#[test]
fn preflight_accepts_only_an_empty_or_absent_designated_workspace() {
    let eligibility = preflight_eligibility(&starting_snapshot(), "9").unwrap();
    assert_eq!(eligibility.original, original_identity());
    assert_eq!(eligibility.designated_id, Some(TARGET_WORKSPACE_ID));

    let mut absent = starting_snapshot();
    absent.workspaces.pop();
    // Remove the target from the tree too, keeping a valid snapshot fixture.
    absent.tree.nodes[1].nodes.pop();
    assert_eq!(
        preflight_eligibility(&absent, "9").unwrap().designated_id,
        None
    );

    for floating in [false, true] {
        let mut tree = desktop("splith", Vec::new(), PREEXISTING_WINDOW_ID);
        let target = node_mut(&mut tree, TARGET_WORKSPACE_ID);
        let child_key = if floating { "floating_nodes" } else { "nodes" };
        // Even a structural container with no windows counts as occupancy.
        target[child_key] = json!([group(30, "splith", rect(0, 0, 1920, 1080), Vec::new())]);
        let snapshot = observed(tree, "1", OTHER_WORKSPACE_ID);
        assert_contains(
            &preflight_eligibility(&snapshot, "9").err().unwrap(),
            "occupied",
        );
    }
}

#[test]
fn preflight_refuses_the_starting_workspace_ambiguous_numbers_and_unsafe_names() {
    let focused_target = observed(
        desktop("splith", Vec::new(), TARGET_WORKSPACE_ID),
        "9",
        TARGET_WORKSPACE_ID,
    );
    assert_contains(
        &preflight_eligibility(&focused_target, "9").err().unwrap(),
        "starting workspace",
    );

    let mut numbered_start = starting_snapshot();
    numbered_start.workspaces[0].num = 9;
    numbered_start.workspaces[0].name = "9: original".into();
    find_and_rename(&mut numbered_start.tree, OTHER_WORKSPACE_ID, "9: original");
    assert_contains(
        &preflight_eligibility(&numbered_start, "9").err().unwrap(),
        "starting workspace",
    );

    let mut differently_named = starting_snapshot();
    differently_named.workspaces[1].name = "9: other".into();
    find_and_rename(&mut differently_named.tree, TARGET_WORKSPACE_ID, "9: other");
    assert_contains(
        &preflight_eligibility(&differently_named, "9")
            .err()
            .unwrap(),
        "differently named",
    );

    let mut ambiguous = starting_snapshot();
    ambiguous.workspaces.push(crate::ipc::Workspace {
        id: 40,
        num: 9,
        name: "9: another".into(),
        focused: false,
    });
    assert_contains(
        &preflight_eligibility(&ambiguous, "9").err().unwrap(),
        "ambiguous",
    );

    for unsafe_name in ["", "-start", "start\"", "start;kill", "start\n", "スタート"] {
        let mut unsafe_start = starting_snapshot();
        unsafe_start.workspaces[0].name = unsafe_name.into();
        find_and_rename(&mut unsafe_start.tree, OTHER_WORKSPACE_ID, unsafe_name);
        assert_contains(
            &preflight_eligibility(&unsafe_start, "9").err().unwrap(),
            "safely represented",
        );
    }
}

fn find_and_rename(tree: &mut TreeNode, id: i64, name: &str) {
    if tree.id == id {
        tree.name = Some(name.into());
    }
    for child in tree.nodes.iter_mut().chain(&mut tree.floating_nodes) {
        find_and_rename(child, id, name);
    }
}

fn loaded_pair(scenario: &Scenario) -> Snapshot {
    observed(
        desktop(
            "splith",
            vec![
                wayland_window(
                    20,
                    &selector_value(scenario, "window1"),
                    rect(0, 0, 1152, 1080),
                ),
                wayland_window(
                    21,
                    &selector_value(scenario, "window2"),
                    rect(1152, 0, 768, 1080),
                ),
            ],
            21,
        ),
        "9",
        TARGET_WORKSPACE_ID,
    )
}

#[test]
fn cleanup_plan_revalidates_ownership_and_calculates_the_remaining_hierarchy() {
    let scenario = standard_scenario("horizontal-pair");
    let target = target_identity();
    let preexisting = container_ids(&starting_snapshot().tree);
    let context = CleanupContext {
        target: &target,
        layout: &scenario.layout,
        preexisting_ids: &preexisting,
    };
    let snapshot = loaded_pair(&scenario);
    let baseline = signature(snapshot.focused_workspace());
    let owned = accepted(&[(20, "window1"), (21, "window2")]);
    let plan = prepare_close(&snapshot, &context, &owned, &baseline, 20, true).unwrap();
    assert_eq!(plan.remaining, accepted(&[(21, "window2")]));
    assert_eq!(
        plan.hierarchy,
        Some(shape(find(&snapshot.tree, 21)).unwrap())
    );

    let survivor = observed(
        desktop(
            "splith",
            vec![wayland_window(
                21,
                &selector_value(&scenario, "window2"),
                rect(0, 0, 1920, 1080),
            )],
            21,
        ),
        "9",
        TARGET_WORKSPACE_ID,
    );
    let last = prepare_close(
        &survivor,
        &context,
        &plan.remaining,
        &signature(survivor.focused_workspace()),
        21,
        true,
    )
    .unwrap();
    assert!(last.remaining.is_empty());
    assert_eq!(last.hierarchy, None);
}

#[test]
fn cleanup_refuses_lost_history_activity_changed_state_and_unowned_ids() {
    let scenario = standard_scenario("horizontal-pair");
    let target = target_identity();
    let preexisting = container_ids(&starting_snapshot().tree);
    let context = CleanupContext {
        target: &target,
        layout: &scenario.layout,
        preexisting_ids: &preexisting,
    };
    let snapshot = loaded_pair(&scenario);
    let baseline = signature(snapshot.focused_workspace());
    let owned = accepted(&[(20, "window1"), (21, "window2")]);

    assert_contains(
        &uncertain_message(prepare_close(
            &snapshot, &context, &owned, &baseline, 20, false,
        )),
        "trusted",
    );
    assert_contains(
        &uncertain_message(prepare_close(
            &snapshot, &context, &owned, &baseline, 99, true,
        )),
        "individually verified",
    );
    let mut activity = loaded_pair(&scenario);
    activity.window_events.push(window_event("focus", 20, None));
    assert_contains(
        &uncertain_message(prepare_close(
            &activity, &context, &owned, &baseline, 20, true,
        )),
        "unexpected window activity",
    );

    let mut changed = loaded_pair(&scenario);
    changed.tree.nodes[1].nodes[1].nodes[0].rect.width += 1;
    assert_contains(
        &uncertain_message(prepare_close(
            &changed, &context, &owned, &baseline, 20, true,
        )),
        "changed before cleanup",
    );

    let mut foreign = loaded_pair(&scenario);
    foreign.tree.nodes[1].nodes[1]
        .nodes
        .push(parse_tree(wayland_window(
            99,
            "foreign",
            rect(0, 0, 50, 50),
        )));
    assert_contains(
        &uncertain_message(prepare_close(
            &foreign, &context, &owned, &baseline, 20, true,
        )),
        "ownership/placement uncertain",
    );

    let mut elsewhere = loaded_pair(&scenario);
    elsewhere.focused_workspace_id = OTHER_WORKSPACE_ID;
    assert_contains(
        &uncertain_message(prepare_close(
            &elsewhere, &context, &owned, &baseline, 20, true,
        )),
        "trusted target",
    );
}

#[test]
fn close_observation_allows_only_the_requested_disappearance_and_verified_survivors() {
    let scenario = standard_scenario("horizontal-pair");
    let target = target_identity();
    let preexisting = container_ids(&starting_snapshot().tree);
    let context = CleanupContext {
        target: &target,
        layout: &scenario.layout,
        preexisting_ids: &preexisting,
    };
    let before = loaded_pair(&scenario);
    let baseline = signature(before.focused_workspace());
    let owned = accepted(&[(20, "window1"), (21, "window2")]);
    let plan = prepare_close(&before, &context, &owned, &baseline, 20, true).unwrap();
    assert!(!verify_close_observation(&before, &context, &owned, &plan, &baseline, 20).unwrap());

    let survivor = || {
        let mut snapshot = observed(
            desktop(
                "splith",
                vec![wayland_window(
                    21,
                    &selector_value(&scenario, "window2"),
                    rect(0, 0, 1920, 1080),
                )],
                21,
            ),
            "9",
            TARGET_WORKSPACE_ID,
        );
        snapshot.window_events = vec![
            window_event("close", 20, None),
            window_event("focus", 21, None),
        ];
        snapshot
    };
    assert!(verify_close_observation(&survivor(), &context, &owned, &plan, &baseline, 20).unwrap());

    let mut unexpected_event = survivor();
    unexpected_event
        .window_events
        .push(window_event("close", PREEXISTING_WINDOW_ID, None));
    assert_contains(
        &uncertain_message(verify_close_observation(
            &unexpected_event,
            &context,
            &owned,
            &plan,
            &baseline,
            20,
        )),
        "unexpected window activity",
    );

    let mut changed_hierarchy = survivor();
    changed_hierarchy.tree.nodes[1].nodes[1].nodes[0].pid = Some(5_999_999);
    assert_contains(
        &uncertain_message(verify_close_observation(
            &changed_hierarchy,
            &context,
            &owned,
            &plan,
            &baseline,
            20,
        )),
        "cleanup hierarchy changed",
    );

    let mut changed_while_waiting = loaded_pair(&scenario);
    changed_while_waiting.tree.nodes[1].nodes[1].nodes[0]
        .rect
        .width -= 1;
    assert_contains(
        &uncertain_message(verify_close_observation(
            &changed_while_waiting,
            &context,
            &owned,
            &plan,
            &baseline,
            20,
        )),
        "while waiting for close",
    );

    let before_last = survivor();
    let last_baseline = signature(before_last.focused_workspace());
    // A fresh fence consumes the previous close/focus history before another close.
    let mut before_last = before_last;
    before_last.window_events.clear();
    let last = prepare_close(
        &before_last,
        &context,
        &plan.remaining,
        &last_baseline,
        21,
        true,
    )
    .unwrap();
    let mut empty = observed(
        desktop("splith", Vec::new(), TARGET_WORKSPACE_ID),
        "9",
        TARGET_WORKSPACE_ID,
    );
    empty.window_events.push(window_event("close", 21, None));
    assert!(
        verify_close_observation(&empty, &context, &plan.remaining, &last, &last_baseline, 21)
            .unwrap()
    );
}

#[test]
fn restoration_requires_exact_original_identity_and_trusted_disposable_focus() {
    let original = original_identity();
    let target = target_identity();
    let mut snapshot = observed(
        desktop("splith", Vec::new(), TARGET_WORKSPACE_ID),
        "9",
        TARGET_WORKSPACE_ID,
    );
    assert!(controls_target(true, Some(&target), &target));
    assert!(!controls_target(false, Some(&target), &target));
    assert!(!controls_target(true, Some(&original), &target));
    assert!(!controls_target(true, None, &target));
    assert_eq!(
        restoration_refusal(&snapshot, &original, &target, true),
        None
    );
    assert_eq!(
        restoration_refusal(&snapshot, &original, &target, false),
        Some(LOST_CONTROL)
    );
    assert_eq!(
        restoration_refusal(&starting_snapshot(), &original, &target, true),
        Some(LOST_CONTROL)
    );

    snapshot.workspaces[0].id = 100;
    assert_eq!(
        restoration_refusal(&snapshot, &original, &target, true),
        Some(ORIGINAL_GONE)
    );
    snapshot.workspaces[0].id = original.id;
    snapshot.workspaces[0].name = "renamed".into();
    assert_eq!(
        restoration_refusal(&snapshot, &original, &target, true),
        Some(ORIGINAL_GONE)
    );
    snapshot.workspaces.remove(0);
    assert_eq!(
        restoration_refusal(&snapshot, &original, &target, true),
        Some(ORIGINAL_GONE)
    );
}

#[test]
fn operation_deadlines_keep_the_reference_limits() {
    assert_eq!(CAPTURE_TIMEOUT, Duration::from_secs(15));
    assert_eq!(
        load_timeout(&standard_scenario("single-window")),
        Duration::from_secs(50)
    );
    assert_eq!(
        load_timeout(&standard_scenario("deep-nesting")),
        Duration::from_secs(130)
    );
    let firefox = crate::fixtures::extended_scenario("nested-firefox-foot");
    assert_eq!(load_timeout(&firefox), Duration::from_secs(270));
}

#[test]
fn temporary_yaml_is_private_replaced_safely_and_automatically_removed() {
    let test_id = format!("{RUN_ID}-{}-private-yaml", std::process::id());
    let directory = TemporaryConfigurations::new(&env::temp_dir(), &test_id).unwrap();
    let path = directory.path.clone();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(TemporaryConfigurations::new(&env::temp_dir(), &test_id).is_err());
    directory.write("captured", "first draft").unwrap();
    let captured = path.join("captured.yaml");
    assert_eq!(
        fs::metadata(&captured).unwrap().permissions().mode() & 0o777,
        0o600
    );
    directory.write("captured", "second").unwrap();
    assert_eq!(fs::read_to_string(&captured).unwrap(), "second");
    for name in ["../escape", "", "/absolute", "two words"] {
        assert!(directory.write(name, "unused").is_err());
    }
    symlink(&captured, path.join("symlink.yaml")).unwrap();
    assert!(directory.write("symlink", "changed").is_err());
    assert_eq!(fs::read_to_string(&captured).unwrap(), "second");
    drop(directory);
    assert!(!path.exists());
}
