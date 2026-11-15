use super::*;
use crate::fixtures::{
    OTHER_WORKSPACE_ID, PREEXISTING_WINDOW_ID, RUN_ID, SCRATCHPAD_WORKSPACE_ID,
    SECOND_OUTPUT_WORKSPACE_ID, TARGET_WORKSPACE_ID, accepted_windows, assert_contains,
    configured_app_id, container_ids, desktop, find, group, node_mut, parse_tree, rect,
    standard_scenario, two_output_desktop, two_output_outputs, two_output_workspaces_reply,
    wayland_window, window_event, workspaces_reply,
};
use crate::headless::FIRST_OUTPUT_INITIAL_WORKSPACE;
use serde_json::{Value, json};
use std::os::unix::fs::{PermissionsExt, symlink};

fn recorded_snapshot(tree: Value, focused_name: &str, focused_workspace_id: i64) -> Snapshot {
    Snapshot {
        tree: parse_tree(tree),
        workspaces: workspaces_reply(focused_name),
        focused_workspace_id,
        window_events: Vec::new(),
        workspace_events: Vec::new(),
    }
}

fn starting_snapshot() -> Snapshot {
    recorded_snapshot(
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

fn find_and_rename(tree: &mut TreeNode, id: i64, name: &str) {
    if tree.id == id {
        tree.name = Some(name.into());
    }
    for child in tree.nodes.iter_mut().chain(&mut tree.floating_nodes) {
        find_and_rename(child, id, name);
    }
}

/// Every registered scenario, in the order a run executes them.
const SUITE_ORDER: [&str; 34] = [
    "single-window",
    "horizontal-pair",
    "vertical-pair",
    "uneven-siblings",
    "mixed-nesting",
    "deep-nesting",
    "sibling-nested-groups",
    "process-execution",
    "default-focus",
    "occupied-workspace-refusal",
    "capture-reload-roundtrip",
    "root-tabbed",
    "root-stacked",
    "singleton-groups",
    "mixed-native-nesting",
    "independent-native-groups",
    "mixed-capture-reload",
    "initially-tabbed",
    "initially-stacked",
    "outside-focus-capture-reload",
    "duplicate-foot-ids",
    "profile-success",
    "profile-occupied-refusal",
    "profile-partial-failure",
    "profile-named-success",
    "profile-event-burst",
    "layout-exec-direct",
    "layout-exec-menu",
    "profile-exec-context",
    "profile-exec-ancestry-only",
    "profile-exec-token-only",
    "profile-exec-occupied-origin",
    "profile-exec-menu",
    "profile-cross-output",
];

#[test]
fn every_scenario_is_registered_once_and_selected_by_default_in_suite_order() {
    assert_eq!(suite_scenario_names(), SUITE_ORDER);
    assert_eq!(select_scenarios(None).unwrap(), SUITE_ORDER);

    let unique_names: HashSet<&str> = SUITE_ORDER.into_iter().collect();
    assert_eq!(unique_names.len(), SUITE_ORDER.len());
}

#[test]
fn filters_select_matching_standard_and_profile_scenarios_in_suite_order() {
    assert_eq!(
        select_scenarios(Some("single-window")).unwrap(),
        ["single-window"]
    );
    assert_eq!(
        select_scenarios(Some("pair")).unwrap(),
        ["horizontal-pair", "vertical-pair"]
    );
    assert_eq!(
        select_scenarios(Some("capture-reload")).unwrap(),
        [
            "capture-reload-roundtrip",
            "mixed-capture-reload",
            "outside-focus-capture-reload"
        ]
    );
    assert_eq!(
        select_scenarios(Some("profile")).unwrap(),
        [
            "profile-success",
            "profile-occupied-refusal",
            "profile-partial-failure",
            "profile-named-success",
            "profile-event-burst",
            "profile-exec-context",
            "profile-exec-ancestry-only",
            "profile-exec-token-only",
            "profile-exec-occupied-origin",
            "profile-exec-menu",
            "profile-cross-output"
        ]
    );
    assert_eq!(
        select_scenarios(Some("profile-exec")).unwrap(),
        [
            "profile-exec-context",
            "profile-exec-ancestry-only",
            "profile-exec-token-only",
            "profile-exec-occupied-origin",
            "profile-exec-menu"
        ]
    );
    // `process-execution` also contains "exec"; the hyphen leaves only the exec cases.
    assert_eq!(
        select_scenarios(Some("exec-")).unwrap(),
        [
            "layout-exec-direct",
            "layout-exec-menu",
            "profile-exec-context",
            "profile-exec-ancestry-only",
            "profile-exec-token-only",
            "profile-exec-occupied-origin",
            "profile-exec-menu"
        ]
    );
    assert_eq!(
        select_scenarios(Some("exec-menu")).unwrap(),
        ["layout-exec-menu", "profile-exec-menu"]
    );
    assert_eq!(
        select_scenarios(Some("refusal")).unwrap(),
        ["occupied-workspace-refusal", "profile-occupied-refusal"]
    );

    // Matching is a case-sensitive substring test; no match is an error naming the filter.
    for unmatched in ["PAIR", "no-such-scenario"] {
        let error = select_scenarios(Some(unmatched)).unwrap_err();
        assert_contains(&error, &format!("{unmatched:?} matches no scenario"));
        assert_contains(&error, "single-window");
    }
}

#[test]
fn only_the_cross_output_profile_needs_the_two_output_session() {
    for name in SUITE_ORDER {
        let expected = if name == "profile-cross-output" {
            OutputSetup::TwoOutputs
        } else {
            OutputSetup::OneOutput
        };
        assert_eq!(output_setup(name), expected, "{name}");
    }
    assert_eq!(
        select_scenarios(Some("cross-output")).unwrap(),
        ["profile-cross-output"]
    );
}

#[test]
fn workspace_plan_starts_on_the_fresh_workspace_and_uses_distinct_safe_names() {
    assert_eq!(FIXTURE_WORKSPACE, "1");
    // The two-output session loads the same fixture on its first output's workspace.
    assert_eq!(FIXTURE_WORKSPACE, FIRST_OUTPUT_INITIAL_WORKSPACE);

    let names = [FIXTURE_WORKSPACE, PRIMARY_WORKSPACE, PROFILE_WORKSPACE];
    let unique_names: HashSet<&str> = names.into_iter().collect();
    assert_eq!(unique_names.len(), names.len());
    for name in names {
        crate::ipc::validate_workspace_name(name).unwrap();
    }
}

/// A compositor nothing has used: only workspace "1", focused and empty, on one output.
fn fresh_compositor_tree() -> Value {
    let mut tree = desktop("splith", Vec::new(), OTHER_WORKSPACE_ID);
    // The real output, after Sway's hidden scratchpad output.
    let output_workspaces = tree["nodes"][1]["nodes"].as_array_mut().unwrap();
    output_workspaces.retain(|workspace| workspace["id"] == OTHER_WORKSPACE_ID);
    output_workspaces[0]["nodes"] = json!([]);
    tree
}

/// The snapshot of a `fresh_compositor_tree`, possibly modified.
fn fresh_compositor_snapshot(tree: Value) -> Snapshot {
    let mut workspaces = workspaces_reply("1");
    workspaces.retain(|workspace| workspace.id == OTHER_WORKSPACE_ID);
    Snapshot {
        tree: parse_tree(tree),
        workspaces,
        focused_workspace_id: OTHER_WORKSPACE_ID,
        window_events: Vec::new(),
        workspace_events: Vec::new(),
    }
}

#[test]
fn only_a_fresh_compositor_starts_a_run() {
    let fresh = fresh_compositor_snapshot(fresh_compositor_tree());
    assert_eq!(verify_fresh_compositor(&fresh).unwrap().name, "1");

    // Another workspace and a window: a used desktop, not a fresh compositor.
    assert_contains(
        &verify_fresh_compositor(&starting_snapshot()).unwrap_err(),
        r#"found ["1", "9"]"#,
    );

    for child_key in ["nodes", "floating_nodes"] {
        let mut tree = fresh_compositor_tree();
        // Even an empty structural container makes the workspace occupied.
        node_mut(&mut tree, OTHER_WORKSPACE_ID)[child_key] =
            json!([group(30, "splith", rect(0, 0, 1920, 1080), Vec::new())]);
        assert_contains(
            &verify_fresh_compositor(&fresh_compositor_snapshot(tree)).unwrap_err(),
            "occupied",
        );
    }

    let mut tree = fresh_compositor_tree();
    node_mut(&mut tree, SCRATCHPAD_WORKSPACE_ID)["floating_nodes"] =
        json!([wayland_window(40, "hidden", rect(0, 0, 100, 100))]);
    assert_contains(
        &verify_fresh_compositor(&fresh_compositor_snapshot(tree)).unwrap_err(),
        "found container 40",
    );

    let mut renamed = fresh_compositor_snapshot(fresh_compositor_tree());
    renamed.workspaces[0].name = "2".into();
    find_and_rename(&mut renamed.tree, OTHER_WORKSPACE_ID, "2");
    assert_contains(
        &verify_fresh_compositor(&renamed).unwrap_err(),
        r#"starts on workspace "1", found "2""#,
    );
}

fn fresh_two_output_snapshot(tree: Value, focused_name: &str, focused_id: i64) -> Snapshot {
    Snapshot {
        tree: parse_tree(tree),
        workspaces: two_output_workspaces_reply(focused_name),
        focused_workspace_id: focused_id,
        window_events: Vec::new(),
        workspace_events: Vec::new(),
    }
}

#[test]
fn a_fresh_two_output_compositor_shows_empty_initial_workspaces_on_both_outputs() {
    let fresh = || {
        fresh_two_output_snapshot(
            two_output_desktop(OTHER_WORKSPACE_ID),
            "1",
            OTHER_WORKSPACE_ID,
        )
    };
    let started = fresh();
    let focused = verify_fresh_two_output_compositor(&started, &two_output_outputs()).unwrap();
    assert_eq!(focused.name, "1");

    let mut other_shown = two_output_outputs();
    other_shown[1].current_workspace = Some("3".into());
    assert_contains(
        &verify_fresh_two_output_compositor(&started, &other_shown).unwrap_err(),
        r#"shows workspace "2" on HEADLESS-2"#,
    );

    let mut occupied = two_output_desktop(OTHER_WORKSPACE_ID);
    node_mut(&mut occupied, SECOND_OUTPUT_WORKSPACE_ID)["nodes"] =
        json!([group(40, "splith", rect(0, 0, 10, 10), Vec::new())]);
    let occupied = fresh_two_output_snapshot(occupied, "1", OTHER_WORKSPACE_ID);
    assert_contains(
        &verify_fresh_two_output_compositor(&occupied, &two_output_outputs()).unwrap_err(),
        "occupied",
    );

    let second_focused = fresh_two_output_snapshot(
        two_output_desktop(SECOND_OUTPUT_WORKSPACE_ID),
        "2",
        SECOND_OUTPUT_WORKSPACE_ID,
    );
    assert_contains(
        &verify_fresh_two_output_compositor(&second_focused, &two_output_outputs()).unwrap_err(),
        r#"starts on workspace "1", found "2""#,
    );

    let mut hidden_window = two_output_desktop(OTHER_WORKSPACE_ID);
    node_mut(&mut hidden_window, SCRATCHPAD_WORKSPACE_ID)["floating_nodes"] =
        json!([wayland_window(40, "hidden", rect(0, 0, 100, 100))]);
    let hidden_window = fresh_two_output_snapshot(hidden_window, "1", OTHER_WORKSPACE_ID);
    assert_contains(
        &verify_fresh_two_output_compositor(&hidden_window, &two_output_outputs()).unwrap_err(),
        "found container 40",
    );

    // The one-output desktop is not a two-output session.
    let one_output = fresh_compositor_snapshot(fresh_compositor_tree());
    assert_contains(
        &verify_fresh_two_output_compositor(&one_output, &two_output_outputs()).unwrap_err(),
        r#"found ["1"]"#,
    );
}

fn loaded_pair_snapshot(scenario: &Scenario) -> Snapshot {
    recorded_snapshot(
        desktop(
            "splith",
            vec![
                wayland_window(
                    20,
                    &configured_app_id(scenario, "window1"),
                    rect(0, 0, 1152, 1080),
                ),
                wayland_window(
                    21,
                    &configured_app_id(scenario, "window2"),
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
    let preexisting_container_ids = container_ids(&starting_snapshot().tree);
    let foreign = ForeignWindows::record(&loaded_pair_snapshot(&scenario), target.id).unwrap();
    let context = CleanupContext {
        foreign: &foreign,
        target: &target,
        layout: &scenario.layout,
        preexisting_ids: &preexisting_container_ids,
    };
    let snapshot = loaded_pair_snapshot(&scenario);
    let workspace_baseline = signature(snapshot.focused_workspace());
    let owned_windows = accepted_windows(&[(20, "window1"), (21, "window2")]);
    let close_plan = prepare_close(
        &snapshot,
        &context,
        &owned_windows,
        &workspace_baseline,
        20,
        true,
    )
    .unwrap();

    assert_eq!(close_plan.remaining, accepted_windows(&[(21, "window2")]));
    assert_eq!(
        close_plan.hierarchy,
        Some(shape(find(&snapshot.tree, 21)).unwrap())
    );

    let survivor = recorded_snapshot(
        desktop(
            "splith",
            vec![wayland_window(
                21,
                &configured_app_id(&scenario, "window2"),
                rect(0, 0, 1920, 1080),
            )],
            21,
        ),
        "9",
        TARGET_WORKSPACE_ID,
    );
    let final_close_plan = prepare_close(
        &survivor,
        &context,
        &close_plan.remaining,
        &signature(survivor.focused_workspace()),
        21,
        true,
    )
    .unwrap();
    assert!(final_close_plan.remaining.is_empty());
    assert_eq!(final_close_plan.hierarchy, None);
}

#[test]
fn cleanup_refuses_lost_history_activity_changed_state_and_unowned_ids() {
    let scenario = standard_scenario("horizontal-pair");
    let target = target_identity();
    let preexisting_container_ids = container_ids(&starting_snapshot().tree);
    let foreign = ForeignWindows::record(&loaded_pair_snapshot(&scenario), target.id).unwrap();
    let context = CleanupContext {
        foreign: &foreign,
        target: &target,
        layout: &scenario.layout,
        preexisting_ids: &preexisting_container_ids,
    };
    let snapshot = loaded_pair_snapshot(&scenario);
    let workspace_baseline = signature(snapshot.focused_workspace());
    let owned_windows = accepted_windows(&[(20, "window1"), (21, "window2")]);

    assert_contains(
        &prepare_close(
            &snapshot,
            &context,
            &owned_windows,
            &workspace_baseline,
            20,
            false,
        )
        .unwrap_err(),
        "trusted",
    );
    assert_contains(
        &prepare_close(
            &snapshot,
            &context,
            &owned_windows,
            &workspace_baseline,
            99,
            true,
        )
        .unwrap_err(),
        "individually verified",
    );
    let mut activity = loaded_pair_snapshot(&scenario);
    activity.window_events.push(window_event("focus", 20, None));
    assert_contains(
        &prepare_close(
            &activity,
            &context,
            &owned_windows,
            &workspace_baseline,
            20,
            true,
        )
        .unwrap_err(),
        "unexpected window activity",
    );

    let mut changed = loaded_pair_snapshot(&scenario);
    changed.tree.nodes[1].nodes[1].nodes[0].rect.width += 1;
    assert_contains(
        &prepare_close(
            &changed,
            &context,
            &owned_windows,
            &workspace_baseline,
            20,
            true,
        )
        .unwrap_err(),
        "changed before cleanup",
    );

    let mut foreign = loaded_pair_snapshot(&scenario);
    foreign.tree.nodes[1].nodes[1]
        .nodes
        .push(parse_tree(wayland_window(
            99,
            "foreign",
            rect(0, 0, 50, 50),
        )));
    assert_contains(
        &prepare_close(
            &foreign,
            &context,
            &owned_windows,
            &workspace_baseline,
            20,
            true,
        )
        .unwrap_err(),
        "ownership/placement uncertain",
    );

    let mut elsewhere = loaded_pair_snapshot(&scenario);
    elsewhere.focused_workspace_id = OTHER_WORKSPACE_ID;
    assert_contains(
        &prepare_close(
            &elsewhere,
            &context,
            &owned_windows,
            &workspace_baseline,
            20,
            true,
        )
        .unwrap_err(),
        "trusted target",
    );
}

#[test]
fn close_observation_allows_only_the_requested_disappearance_and_verified_survivors() {
    let scenario = standard_scenario("horizontal-pair");
    let target = target_identity();
    let preexisting_container_ids = container_ids(&starting_snapshot().tree);
    let foreign = ForeignWindows::record(&loaded_pair_snapshot(&scenario), target.id).unwrap();
    let context = CleanupContext {
        foreign: &foreign,
        target: &target,
        layout: &scenario.layout,
        preexisting_ids: &preexisting_container_ids,
    };
    let before = loaded_pair_snapshot(&scenario);
    let workspace_baseline = signature(before.focused_workspace());
    let owned_windows = accepted_windows(&[(20, "window1"), (21, "window2")]);
    let close_plan = prepare_close(
        &before,
        &context,
        &owned_windows,
        &workspace_baseline,
        20,
        true,
    )
    .unwrap();

    assert!(
        !verify_close_observation(
            &before,
            &context,
            &owned_windows,
            &close_plan,
            &workspace_baseline,
            20
        )
        .unwrap()
    );

    let survivor = || {
        let mut snapshot = recorded_snapshot(
            desktop(
                "splith",
                vec![wayland_window(
                    21,
                    &configured_app_id(&scenario, "window2"),
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
    assert!(
        verify_close_observation(
            &survivor(),
            &context,
            &owned_windows,
            &close_plan,
            &workspace_baseline,
            20
        )
        .unwrap()
    );

    let mut unexpected_event = survivor();
    unexpected_event
        .window_events
        .push(window_event("close", PREEXISTING_WINDOW_ID, None));
    assert_contains(
        &verify_close_observation(
            &unexpected_event,
            &context,
            &owned_windows,
            &close_plan,
            &workspace_baseline,
            20,
        )
        .unwrap_err(),
        "unexpected window activity",
    );

    let mut changed_hierarchy = survivor();
    changed_hierarchy.tree.nodes[1].nodes[1].nodes[0].pid = Some(5_999_999);
    assert_contains(
        &verify_close_observation(
            &changed_hierarchy,
            &context,
            &owned_windows,
            &close_plan,
            &workspace_baseline,
            20,
        )
        .unwrap_err(),
        "cleanup hierarchy changed",
    );

    let mut changed_while_waiting = loaded_pair_snapshot(&scenario);
    changed_while_waiting.tree.nodes[1].nodes[1].nodes[0]
        .rect
        .width -= 1;
    assert_contains(
        &verify_close_observation(
            &changed_while_waiting,
            &context,
            &owned_windows,
            &close_plan,
            &workspace_baseline,
            20,
        )
        .unwrap_err(),
        "while waiting for close",
    );

    let before_last = survivor();
    let last_baseline = signature(before_last.focused_workspace());
    // A fresh fence consumes the previous close/focus history before another close.
    let mut before_last = before_last;
    before_last.window_events.clear();
    let final_close_plan = prepare_close(
        &before_last,
        &context,
        &close_plan.remaining,
        &last_baseline,
        21,
        true,
    )
    .unwrap();
    let mut empty = recorded_snapshot(
        desktop("splith", Vec::new(), TARGET_WORKSPACE_ID),
        "9",
        TARGET_WORKSPACE_ID,
    );
    empty.window_events.push(window_event("close", 21, None));
    assert!(
        verify_close_observation(
            &empty,
            &context,
            &close_plan.remaining,
            &final_close_plan,
            &last_baseline,
            21
        )
        .unwrap()
    );
}

#[test]
fn operation_deadlines_keep_their_established_limits() {
    assert_eq!(CAPTURE_TIMEOUT, Duration::from_secs(15));
    assert_eq!(
        load_timeout(&standard_scenario("single-window")),
        Duration::from_secs(50)
    );
    assert_eq!(
        load_timeout(&standard_scenario("deep-nesting")),
        Duration::from_secs(130)
    );
}

#[test]
fn temporary_yaml_is_private_and_replaced_safely() {
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

    // Dropping it removes nothing: the launcher keeps the session directory after a failure.
    drop(directory);
    assert!(captured.is_file());
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn foot_and_sleep_keep_their_path_names_through_symlinks() {
    let scratch = env::temp_dir().join(format!(
        "sway-stage-integration-test-{}-unresolved-executables",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&scratch);
    let not_executable = scratch.join("not-executable");
    let multicall = scratch.join("multicall");
    fs::create_dir_all(&not_executable).unwrap();
    fs::create_dir_all(&multicall).unwrap();

    // A non-executable `sleep` earlier on PATH is passed over.
    fs::write(not_executable.join("sleep"), "").unwrap();
    let busybox = multicall.join("busybox");
    fs::write(&busybox, "").unwrap();
    fs::set_permissions(&busybox, fs::Permissions::from_mode(0o755)).unwrap();
    symlink("busybox", multicall.join("sleep")).unwrap();
    symlink("missing", multicall.join("foot")).unwrap();
    let search_path = env::join_paths([&not_executable, &multicall]).unwrap();

    let sleep = unresolved_executable_text("sleep", &search_path).unwrap();

    assert_eq!(sleep, multicall.join("sleep").to_str().unwrap());

    let missing_target = unresolved_executable_text("foot", &search_path).unwrap_err();
    assert_contains(&missing_target, "required executable not found");
    assert!(unresolved_executable_text("absent", &search_path).is_err());

    fs::remove_dir_all(&scratch).unwrap();
}

#[test]
fn private_process_directory_refuses_existing_entries_and_missing_parents() {
    let test_id = format!("{RUN_ID}-{}-process-cwd", std::process::id());
    let directory = TemporaryConfigurations::new(&env::temp_dir(), &test_id).unwrap();

    assert!(TemporaryConfigurations::new(&directory.path.join("missing"), "nested").is_err());
    directory.create_process_cwd().unwrap();
    assert_eq!(
        fs::metadata(directory.path.join("process cwd"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(
        directory
            .create_process_cwd()
            .unwrap_err()
            .contains("create private process cwd")
    );
    fs::remove_dir_all(&directory.path).unwrap();
}

fn refused_output(code: i32, stderr: &str) -> process::CompletedProcess {
    use std::os::unix::process::ExitStatusExt;
    process::CompletedProcess {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: String::new(),
        stderr: stderr.into(),
    }
}

fn occupied_diagnostic() -> String {
    format!(
        "sway-stage: prelaunch workspace={TARGET_WORKSPACE_ID}: layout \"single-window\": target workspace must be completely empty, including tiled/floating containers; choose an empty disposable workspace; log: /private/log\n"
    )
}

#[test]
fn occupied_refusal_requires_the_specific_prelaunch_error_and_unchanged_desktop() {
    let scenario = standard_scenario("occupied-workspace-refusal");
    let before = loaded_pair_snapshot(&scenario);
    let after = loaded_pair_snapshot(&scenario);
    verify_refusal(
        &refused_output(1, &occupied_diagnostic()),
        &before,
        &after,
        false,
    )
    .unwrap();
    for (code, diagnostic) in [
        (0, occupied_diagnostic()),
        (2, occupied_diagnostic()),
        (1, "sway-stage: validation: unknown layout".into()),
        (1, "sway-stage: subscribe: IPC disconnected".into()),
        (
            1,
            occupied_diagnostic().replace("workspace=7", "workspace=99"),
        ),
        (1, occupied_diagnostic().replace("prelaunch", "launch")),
    ] {
        verify_refusal(&refused_output(code, &diagnostic), &before, &after, false).unwrap_err();
    }
    let mut stdout = refused_output(1, &occupied_diagnostic());
    stdout.stdout = "unexpected data".into();
    verify_refusal(&stdout, &before, &after, false).unwrap_err();
    // Init/empty or focus events returning to the baseline are still evidence of activity.
    verify_refusal(
        &refused_output(1, &occupied_diagnostic()),
        &before,
        &after,
        true,
    )
    .unwrap_err();
}

#[test]
fn occupied_refusal_rejects_new_windows_and_changed_ids_hierarchy_focus_or_geometry() {
    let scenario = standard_scenario("occupied-workspace-refusal");
    let before = loaded_pair_snapshot(&scenario);
    let output = refused_output(1, &occupied_diagnostic());
    for mutation in [
        "new",
        "id",
        "parent",
        "order",
        "geometry",
        "focus",
        "foreign-geometry",
    ] {
        let mut after = loaded_pair_snapshot(&scenario);
        let workspace = &mut after.tree.nodes[1].nodes[1];
        match mutation {
            "new" => {
                workspace
                    .nodes
                    .push(parse_tree(wayland_window(99, "extra", rect(0, 0, 10, 10))))
            }
            "id" => workspace.nodes[0].id = 99,
            "parent" => {
                let children = std::mem::take(&mut workspace.nodes);
                let mut wrapper =
                    parse_tree(group(30, "splith", rect(0, 0, 1920, 1080), Vec::new()));
                wrapper.nodes = children;
                workspace.nodes.push(wrapper);
            }
            "order" => workspace.nodes.reverse(),
            "geometry" => workspace.nodes[0].rect.width -= 1,
            "focus" => {
                workspace.nodes[0].focused = true;
                workspace.nodes[1].focused = false;
            }
            "foreign-geometry" => after.tree.nodes[1].nodes[0].nodes[0].rect.width -= 1,
            _ => unreachable!(),
        }
        verify_refusal(&output, &before, &after, false).unwrap_err();
    }
    for change in [
        "new",
        "close",
        "focus",
        "move",
        "floating",
        "fullscreen_mode",
    ] {
        let mut after = loaded_pair_snapshot(&scenario);
        after.window_events.push(window_event(change, 99, None));
        assert_contains(
            &verify_refusal(&output, &before, &after, false).unwrap_err(),
            "unexpected window activity",
        );
    }
}

#[test]
fn foreign_fixture_requires_its_own_launch_evidence_for_cleanup() {
    use crate::fixtures::{FOOT, SLEEP};

    let scenario = foreign_fixture_scenario(RUN_ID, FOOT, SLEEP);
    let fixture_app_id = configured_app_id(&scenario, "window1");
    let preexisting_container_ids = container_ids(&starting_snapshot().tree);
    let mut loaded = recorded_snapshot(
        desktop(
            "splith",
            vec![wayland_window(20, &fixture_app_id, rect(0, 0, 1920, 1080))],
            20,
        ),
        "9",
        TARGET_WORKSPACE_ID,
    );
    loaded.window_events = vec![
        window_event("new", 20, Some(&fixture_app_id)),
        window_event("focus", 20, Some(&fixture_app_id)),
    ];
    let owned_windows = bind_launches(
        &loaded.window_events,
        &scenario.layout,
        &preexisting_container_ids,
    )
    .unwrap();
    verify_ownership(
        &loaded.tree,
        TARGET_WORKSPACE_ID,
        &scenario.layout,
        &preexisting_container_ids,
        &owned_windows,
    )
    .unwrap();
    verify_layout(loaded.focused_workspace(), &scenario, &owned_windows).unwrap();
    loaded.window_events.clear();
    let workspace_baseline = signature(loaded.focused_workspace());
    let foreign = ForeignWindows::record(&loaded, TARGET_WORKSPACE_ID).unwrap();
    let target = target_identity();
    let context = CleanupContext {
        target: &target,
        layout: &scenario.layout,
        preexisting_ids: &preexisting_container_ids,
        foreign: &foreign,
    };
    let close_plan = prepare_close(
        &loaded,
        &context,
        &owned_windows,
        &workspace_baseline,
        20,
        true,
    )
    .unwrap();
    for closing_window_id in [PREEXISTING_WINDOW_ID, 99] {
        prepare_close(
            &loaded,
            &context,
            &owned_windows,
            &workspace_baseline,
            closing_window_id,
            true,
        )
        .unwrap_err();
    }
    prepare_close(
        &loaded,
        &context,
        &AcceptedWindows::new(),
        &workspace_baseline,
        20,
        true,
    )
    .unwrap_err();
    prepare_close(
        &loaded,
        &context,
        &owned_windows,
        &workspace_baseline,
        20,
        false,
    )
    .unwrap_err();
    let mut changed_foreign = recorded_snapshot(
        desktop(
            "splith",
            vec![wayland_window(20, &fixture_app_id, rect(0, 0, 1920, 1080))],
            20,
        ),
        "9",
        TARGET_WORKSPACE_ID,
    );
    changed_foreign.tree.nodes[1].nodes[0].nodes[0].rect.width -= 1;
    assert_contains(
        &prepare_close(
            &changed_foreign,
            &context,
            &owned_windows,
            &workspace_baseline,
            20,
            true,
        )
        .unwrap_err(),
        "foreign window",
    );
    // Preservation is checked on the post-close boundary too; no later close is allowed.
    verify_close_observation(
        &changed_foreign,
        &context,
        &owned_windows,
        &close_plan,
        &workspace_baseline,
        20,
    )
    .unwrap_err();
    let mut cleaned = recorded_snapshot(
        desktop("splith", Vec::new(), TARGET_WORKSPACE_ID),
        "9",
        TARGET_WORKSPACE_ID,
    );
    cleaned.window_events.push(window_event("close", 20, None));
    assert!(
        verify_close_observation(
            &cleaned,
            &context,
            &owned_windows,
            &close_plan,
            &workspace_baseline,
            20
        )
        .unwrap()
    );
}

#[test]
fn interrupted_or_incomplete_second_invocation_never_counts_as_a_refusal() {
    refusal_completion(Ok(refused_output(1, &occupied_diagnostic())), false).unwrap();
    for interrupted in [false, true] {
        for reason in [
            "deadline expired",
            "stdout exceeded limit",
            "output stayed open",
            "start failed",
            "interrupted",
        ] {
            refusal_completion(Err(reason.into()), interrupted).unwrap_err();
        }
    }
    refusal_completion(Ok(refused_output(1, &occupied_diagnostic())), true).unwrap_err();
}

#[test]
fn fixture_creation_evidence_survives_expected_focus_changes_but_never_changed_state() {
    use crate::fixtures::{FOOT, SLEEP};
    let scenario = foreign_fixture_scenario(RUN_ID, FOOT, SLEEP);
    let fixture_app_id = configured_app_id(&scenario, "window1");
    let make_tree = |focus| {
        desktop(
            "splith",
            vec![wayland_window(20, &fixture_app_id, rect(0, 0, 1920, 1080))],
            focus,
        )
    };
    let loaded = recorded_snapshot(make_tree(20), "9", TARGET_WORKSPACE_ID);
    let workspace_baseline = signature(loaded.focused_workspace());
    let fixture = ForeignFixture {
        workspace: target_identity(),
        progress: ScenarioProgress {
            scenario,
            preexisting_ids: Some(container_ids(&starting_snapshot().tree)),
            accepted: accepted_windows(&[(20, "window1")]),
        },
        foreign: ForeignWindows::record(&loaded, TARGET_WORKSPACE_ID).unwrap(),
        loaded_signature: Some(workspace_baseline.clone()),
    };
    let elsewhere = recorded_snapshot(make_tree(PREEXISTING_WINDOW_ID), "1", OTHER_WORKSPACE_ID);
    verify_fixture_state(&elsewhere, &fixture, &workspace_baseline).unwrap();
    verify_fixture_state(&loaded, &fixture, &workspace_baseline).unwrap();
    let mut changed = recorded_snapshot(make_tree(20), "9", TARGET_WORKSPACE_ID);
    changed.tree.nodes[1].nodes[1].nodes[0].rect.height -= 1;
    verify_fixture_state(&changed, &fixture, &workspace_baseline).unwrap_err();
}

#[test]
fn replay_transition_requires_clean_empty_preserved_state_before_discarding_evidence() {
    let scenario = standard_scenario("horizontal-pair");
    let loaded = loaded_pair_snapshot(&scenario);
    let foreign = ForeignWindows::record(&loaded, TARGET_WORKSPACE_ID).unwrap();
    let make_progress = || ScenarioProgress {
        scenario: scenario.clone(),
        preexisting_ids: Some(container_ids(&starting_snapshot().tree)),
        accepted: accepted_windows(&[(20, "window1"), (21, "window2")]),
    };
    let empty = || {
        recorded_snapshot(
            desktop("splith", Vec::new(), TARGET_WORKSPACE_ID),
            "9",
            TARGET_WORKSPACE_ID,
        )
    };
    let mut activity = empty();
    activity
        .window_events
        .push(window_event("move", PREEXISTING_WINDOW_ID, None));
    let mut changed_foreign = empty();
    changed_foreign.tree.nodes[1].nodes[0].nodes[0].rect.width -= 1;
    // An incomplete cleanup (one survivor), unexpected history, or changed foreign
    // window cannot authorize the transition to another construction cycle.
    let mut survivor = loaded_pair_snapshot(&scenario);
    survivor.tree.nodes[1].nodes[1].nodes.remove(0);
    for incomplete in [loaded, survivor, activity, changed_foreign] {
        let mut progress = make_progress();
        let mut retired = HashSet::new();
        assert!(
            progress
                .prepare_replay(&incomplete, &foreign, scenario.clone(), &mut retired)
                .is_err()
        );
        assert_eq!(
            progress.accepted,
            accepted_windows(&[(20, "window1"), (21, "window2")])
        );
        assert!(progress.preexisting_ids.is_some());
        assert!(retired.is_empty());
    }
    let mut progress = make_progress();
    let mut retired = HashSet::new();
    progress
        .prepare_replay(&empty(), &foreign, scenario, &mut retired)
        .unwrap();

    assert_eq!(retired, HashSet::from([20, 21]));
    assert!(progress.accepted.is_empty());
    assert!(progress.preexisting_ids.is_none());
}

#[test]
fn replay_needs_a_fresh_baseline_and_cannot_cleanup_with_first_cycle_evidence() {
    let scenario = standard_scenario("horizontal-pair");
    let first = loaded_pair_snapshot(&scenario);
    let target = target_identity();
    let foreign = ForeignWindows::record(&first, target.id).unwrap();
    let mut progress = ScenarioProgress {
        scenario: scenario.clone(),
        preexisting_ids: Some(container_ids(&starting_snapshot().tree)),
        accepted: accepted_windows(&[(20, "window1"), (21, "window2")]),
    };
    let first_workspace_baseline = signature(first.focused_workspace());
    let first_context = CleanupContext {
        target: &target,
        foreign: &foreign,
        layout: &scenario.layout,
        preexisting_ids: progress.preexisting_ids.as_ref().unwrap(),
    };
    prepare_close(
        &first,
        &first_context,
        &progress.accepted,
        &first_workspace_baseline,
        20,
        true,
    )
    .unwrap();

    assert!(
        prepare_close(
            &first,
            &first_context,
            &progress.accepted,
            &first_workspace_baseline,
            40,
            true
        )
        .is_err()
    );

    let mut empty = recorded_snapshot(
        desktop("splith", Vec::new(), TARGET_WORKSPACE_ID),
        "9",
        TARGET_WORKSPACE_ID,
    );
    let mut retired = HashSet::new();
    progress
        .prepare_replay(&empty, &foreign, scenario.clone(), &mut retired)
        .unwrap();
    // A newly present foreign window must be included in the *fresh* identity set.
    empty.tree.nodes[1].nodes[0]
        .nodes
        .push(parse_tree(wayland_window(
            99,
            "new-foreign",
            rect(0, 0, 10, 10),
        )));
    progress.begin_cycle(&empty, &retired).unwrap();
    let preexisting_container_ids = progress.preexisting_ids.as_ref().unwrap();
    assert!(preexisting_container_ids.contains(&99));
    assert!(preexisting_container_ids.contains(&20) && preexisting_container_ids.contains(&21));
    assert!(progress.accepted.is_empty());

    let mut second = loaded_pair_snapshot(&scenario);
    second.tree.nodes[1].nodes[1].nodes[0].id = 40;
    second.tree.nodes[1].nodes[1].nodes[1].id = 41;
    let second_workspace_baseline = signature(second.focused_workspace());
    let context = CleanupContext {
        target: &target,
        foreign: &foreign,
        layout: &scenario.layout,
        preexisting_ids: preexisting_container_ids,
    };
    // Failed/incomplete second loads leave no accepted evidence, so no close is legal.
    assert!(
        prepare_close(
            &second,
            &context,
            &progress.accepted,
            &second_workspace_baseline,
            40,
            true
        )
        .is_err()
    );
    let new_events = |first, second| {
        vec![
            window_event("new", first, None),
            window_event("new", second, None),
        ]
    };
    assert!(
        bind_launches(
            &new_events(20, 21),
            &scenario.layout,
            preexisting_container_ids
        )
        .is_err()
    );
    assert!(
        bind_launches(
            &new_events(40, 99),
            &scenario.layout,
            preexisting_container_ids
        )
        .is_err()
    );
    assert!(
        bind_launches(
            &new_events(40, 40),
            &scenario.layout,
            preexisting_container_ids
        )
        .is_err()
    );
    let owned_windows = bind_launches(
        &new_events(40, 41),
        &scenario.layout,
        preexisting_container_ids,
    )
    .unwrap();
    verify_ownership(
        &second.tree,
        target.id,
        &scenario.layout,
        preexisting_container_ids,
        &owned_windows,
    )
    .unwrap();
    verify_layout(second.focused_workspace(), &scenario, &owned_windows).unwrap();
    prepare_close(
        &second,
        &context,
        &owned_windows,
        &second_workspace_baseline,
        40,
        true,
    )
    .unwrap();
    assert!(
        prepare_close(
            &second,
            &context,
            &owned_windows,
            &second_workspace_baseline,
            20,
            true
        )
        .is_err()
    );
    assert!(
        prepare_close(
            &second,
            &context,
            &owned_windows,
            &second_workspace_baseline,
            41,
            false
        )
        .is_err()
    );
    assert!(
        prepare_close(
            &second,
            &context,
            &owned_windows,
            &first_workspace_baseline,
            40,
            true
        )
        .is_err()
    );
}

#[test]
fn mixed_group_cleanup_requires_exact_surviving_headers_before_every_next_close() {
    use crate::fixtures::mixed_native_desktop;
    let scenario = standard_scenario("mixed-capture-reload");
    let mut tree = mixed_native_desktop(&scenario);
    let initial = recorded_snapshot(tree.clone(), "9", TARGET_WORKSPACE_ID);
    let target = target_identity();
    let foreign = ForeignWindows::record(&initial, target.id).unwrap();
    let preexisting_container_ids = container_ids(&starting_snapshot().tree);
    let context = CleanupContext {
        target: &target,
        layout: &scenario.layout,
        preexisting_ids: &preexisting_container_ids,
        foreign: &foreign,
    };
    let mut remaining = accepted_windows(&[
        (20, "window1"),
        (21, "window2"),
        (22, "window3"),
        (23, "window4"),
    ]);
    verify_layout(initial.focused_workspace(), &scenario, &remaining).unwrap();
    let mut workspace_baseline = signature(initial.focused_workspace());
    for closing_window_id in 20..=23 {
        let before = recorded_snapshot(tree.clone(), "9", TARGET_WORKSPACE_ID);
        let close_plan = prepare_close(
            &before,
            &context,
            &remaining,
            &workspace_baseline,
            closing_window_id,
            true,
        )
        .unwrap();
        // A foreign/preexisting window and group IDs never authorize a close.
        for unowned_id in [PREEXISTING_WINDOW_ID, 31, 32, 99] {
            assert!(
                prepare_close(
                    &before,
                    &context,
                    &remaining,
                    &workspace_baseline,
                    unowned_id,
                    true
                )
                .is_err()
            );
        }
        match closing_window_id {
            20 => {
                node_mut(&mut tree, 31)["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .remove(0);
                node_mut(&mut tree, 31)["focus"] = json!([33]);
            }
            21 => {
                node_mut(&mut tree, 33)["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .remove(0);
                node_mut(&mut tree, 33)["focus"] = json!([22]);
            }
            22 => {
                node_mut(&mut tree, TARGET_WORKSPACE_ID)["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .remove(0);
                node_mut(&mut tree, TARGET_WORKSPACE_ID)["focus"] = json!([23]);
            }
            23 => {
                node_mut(&mut tree, TARGET_WORKSPACE_ID)["nodes"] = json!([]);
                node_mut(&mut tree, TARGET_WORKSPACE_ID)["focus"] = json!([]);
            }
            _ => unreachable!(),
        }
        let focused_container_id = if closing_window_id < 22 {
            22
        } else if closing_window_id == 22 {
            23
        } else {
            TARGET_WORKSPACE_ID
        };
        crate::fixtures::set_focus(&mut tree, focused_container_id);
        let mut after = recorded_snapshot(tree.clone(), "9", TARGET_WORKSPACE_ID);
        after
            .window_events
            .push(window_event("close", closing_window_id, None));
        assert!(
            verify_close_observation(
                &after,
                &context,
                &remaining,
                &close_plan,
                &workspace_baseline,
                closing_window_id
            )
            .unwrap()
        );

        if closing_window_id == 20 {
            // A compositor that flattened the surviving tab group is not compatible.
            let mut flattened = tree.clone();
            let row = node_mut(&mut flattened, 31)["nodes"][0].take();
            node_mut(&mut flattened, TARGET_WORKSPACE_ID)["nodes"][0] = row;
            let mut changed = recorded_snapshot(flattened, "9", TARGET_WORKSPACE_ID);
            changed
                .window_events
                .push(window_event("close", closing_window_id, None));
            assert_contains(
                &verify_close_observation(
                    &changed,
                    &context,
                    &remaining,
                    &close_plan,
                    &workspace_baseline,
                    closing_window_id,
                )
                .unwrap_err(),
                "cleanup hierarchy changed",
            );
            // A different header layout is equally uncertain.
            for replacement in [json!("stacked"), json!("splith")] {
                let mut changed_tree = tree.clone();
                node_mut(&mut changed_tree, 31)["layout"] = replacement;
                let mut changed = recorded_snapshot(changed_tree, "9", TARGET_WORKSPACE_ID);
                changed
                    .window_events
                    .push(window_event("close", closing_window_id, None));
                assert!(
                    verify_close_observation(
                        &changed,
                        &context,
                        &remaining,
                        &close_plan,
                        &workspace_baseline,
                        closing_window_id
                    )
                    .is_err()
                );
            }
        }
        remaining = close_plan.remaining;
        workspace_baseline = signature(after.focused_workspace());
    }

    assert!(remaining.is_empty());
}

fn empty_target_snapshot(layout: &str) -> Snapshot {
    recorded_snapshot(
        desktop(layout, Vec::new(), TARGET_WORKSPACE_ID),
        "9",
        TARGET_WORKSPACE_ID,
    )
}

#[test]
fn initial_layout_preparation_verifies_state_before_beginning_normal_load_cycle() {
    let before = empty_target_snapshot("splith");
    let target = target_identity();
    let baseline = ForeignWindows::record(&before, target.id).unwrap();
    for (name, layout) in [
        ("initially-tabbed", "tabbed"),
        ("initially-stacked", "stacked"),
    ] {
        let scenario = standard_scenario(name);
        let mut progress = ScenarioProgress {
            scenario: scenario.clone(),
            preexisting_ids: None,
            accepted: AcceptedWindows::new(),
        };
        let prepared = prepare_initial_workspace_layout(
            &before,
            &target,
            &baseline,
            scenario.initial_workspace_layout.unwrap(),
            7,
            || Ok((empty_target_snapshot(layout), 7)),
        )
        .unwrap();
        assert!(progress.preexisting_ids.is_none());
        progress.begin_cycle(&prepared, &HashSet::new()).unwrap();
        assert_eq!(
            progress.preexisting_ids.unwrap(),
            container_ids(&prepared.tree)
        );
        assert!(progress.accepted.is_empty());
    }
}

#[test]
fn initial_layout_guards_refuse_mutation_on_unsafe_targets() {
    let before = empty_target_snapshot("splith");
    let target = target_identity();
    let baseline = ForeignWindows::record(&before, target.id).unwrap();
    for problem in [
        "tiled",
        "floating",
        "container",
        "focus",
        "identity",
        "output",
        "event",
    ] {
        let mut unsafe_target = empty_target_snapshot("splith");
        let workspace = &mut unsafe_target.tree.nodes[1].nodes[1];
        match problem {
            "tiled" => workspace.nodes.push(parse_tree(wayland_window(
                20,
                "foreign",
                rect(0, 0, 10, 10),
            ))),
            "floating" => workspace.floating_nodes.push(parse_tree(wayland_window(
                20,
                "foreign",
                rect(0, 0, 10, 10),
            ))),
            "container" => workspace.nodes.push(parse_tree(group(
                30,
                "tabbed",
                rect(0, 0, 10, 10),
                Vec::new(),
            ))),
            "focus" => unsafe_target = starting_snapshot(),
            "identity" => workspace.name = Some("renamed".into()),
            "output" => unsafe_target.tree.nodes[1].id += 100,
            "event" => {
                unsafe_target
                    .window_events
                    .push(window_event("move", PREEXISTING_WINDOW_ID, None))
            }
            _ => unreachable!(),
        }
        let result = prepare_initial_workspace_layout(
            &unsafe_target,
            &target,
            &baseline,
            InitialWorkspaceLayout::Tabbed,
            0,
            || panic!("must not mutate {problem}"),
        );
        assert!(result.is_err(), "{problem}");
    }
}

#[test]
fn initial_layout_failure_leaves_no_launch_or_cleanup_evidence() {
    let before = empty_target_snapshot("splith");
    let target = target_identity();
    let baseline = ForeignWindows::record(&before, target.id).unwrap();
    for initial_layout in [
        InitialWorkspaceLayout::Tabbed,
        InitialWorkspaceLayout::Stacked,
    ] {
        for problem in [
            "command",
            "snapshot",
            "wrong-layout",
            "occupied",
            "workspace-event",
            "window-event",
            "foreign-change",
        ] {
            let mut progress = ScenarioProgress {
                scenario: standard_scenario("initially-tabbed"),
                preexisting_ids: None,
                accepted: AcceptedWindows::new(),
            };
            let result =
                prepare_initial_workspace_layout(
                    &before,
                    &target,
                    &baseline,
                    initial_layout,
                    7,
                    || {
                        if matches!(problem, "command" | "snapshot") {
                            return Err(format!("failed {problem}"));
                        }
                        let layout = if initial_layout == InitialWorkspaceLayout::Tabbed {
                            "tabbed"
                        } else {
                            "stacked"
                        };
                        let mut prepared = empty_target_snapshot(layout);
                        let mut revision = 7;
                        match problem {
                            "wrong-layout" => {
                                prepared.tree.nodes[1].nodes[1].layout = crate::ipc::Layout::SplitV
                            }
                            "occupied" => prepared.tree.nodes[1].nodes[1].nodes.push(parse_tree(
                                wayland_window(20, "unexpected", rect(0, 0, 10, 10)),
                            )),
                            "workspace-event" => revision += 1,
                            "window-event" => {
                                prepared.window_events.push(window_event("new", 20, None))
                            }
                            "foreign-change" => {
                                prepared.tree.nodes[1].nodes[0].layout = crate::ipc::Layout::Tabbed
                            }
                            _ => unreachable!(),
                        }
                        Ok((prepared, revision))
                    },
                )
                .and_then(|prepared| progress.begin_cycle(&prepared, &HashSet::new()));
            assert!(result.is_err());
            assert!(progress.preexisting_ids.is_none());
            assert!(progress.accepted.is_empty());
        }
    }
}

#[test]
fn profile_refusal_requires_specific_diagnostics_and_an_unchanged_desktop() {
    let before = recorded_snapshot(
        desktop("splith", Vec::new(), TARGET_WORKSPACE_ID),
        "9",
        TARGET_WORKSPACE_ID,
    );
    let diagnostic = format!(
        "sway-stage: profile load: workspace 1, layout \"profile-second\"; completed 0 workspaces []; workspace preflight: profile: workspace 1 id={OTHER_WORKSPACE_ID} must be completely empty, including tiled/floating containers; no applications launched"
    );
    let valid = refused_output(1, &diagnostic);
    verify_profile_refusal(&valid, &before, &before, "1", OTHER_WORKSPACE_ID, false).unwrap();
    assert!(
        verify_profile_refusal(&valid, &before, &before, "1", OTHER_WORKSPACE_ID, true).is_err()
    );
    for bad in [
        refused_output(0, &diagnostic),
        refused_output(1, "unrelated error"),
        refused_output(1, &diagnostic.replace("completed 0", "completed 1")),
        refused_output(1, &diagnostic.replace("workspace preflight", "selection")),
    ] {
        assert!(
            verify_profile_refusal(&bad, &before, &before, "1", OTHER_WORKSPACE_ID, false).is_err()
        );
    }
    let mut changed = recorded_snapshot(
        desktop("splith", Vec::new(), TARGET_WORKSPACE_ID),
        "9",
        TARGET_WORKSPACE_ID,
    );
    changed.tree.nodes[1].nodes[0].nodes[0].rect.width += 1;
    assert!(
        verify_profile_refusal(&valid, &before, &changed, "1", OTHER_WORKSPACE_ID, false).is_err()
    );
    let mut changed = recorded_snapshot(
        desktop("splith", Vec::new(), TARGET_WORKSPACE_ID),
        "9",
        TARGET_WORKSPACE_ID,
    );
    changed
        .window_events
        .push(window_event("move", PREEXISTING_WINDOW_ID, None));
    assert!(
        verify_profile_refusal(&valid, &before, &changed, "1", OTHER_WORKSPACE_ID, false).is_err()
    );
}

#[test]
fn partial_profile_diagnostics_require_the_completed_and_failed_workspace() {
    let primary = target_identity();
    let diagnostic = format!(
        "profile load: workspace 10, layout \"profile-second\"; completed 1 workspaces [(9, {})]; wait: window timeout: elapsed=500ms; stopped without cleanup or relaunch",
        primary.id
    );
    verify_profile_partial_failure(&refused_output(1, &diagnostic), &primary, "10").unwrap();
    for bad in [
        refused_output(0, &diagnostic),
        refused_output(1, &diagnostic.replace("workspace 10", "workspace 8")),
        refused_output(1, &diagnostic.replace("completed 1", "completed 0")),
        refused_output(1, &diagnostic.replace("window timeout:", "spawn failed:")),
        refused_output(
            1,
            &diagnostic.replace("stopped without cleanup or relaunch", ""),
        ),
    ] {
        assert!(verify_profile_partial_failure(&bad, &primary, "10").is_err());
    }
}

#[test]
fn profile_launch_record_distinguishes_refusal_order_and_no_window_launches() {
    let test_id = format!("{RUN_ID}-{}-profile-launches", std::process::id());
    let directory = TemporaryConfigurations::new(&env::temp_dir(), &test_id).unwrap();
    let layouts_directory = directory.subdirectory("layouts").unwrap();
    let profiles_directory = directory.subdirectory("profiles").unwrap();
    let layouts = profile::layouts(
        profile::Case::Partial,
        RUN_ID,
        "/foot",
        "/sleep",
        "/script",
        "/record",
        &[],
    );
    layouts_directory
        .write("profile-first", &configuration_yaml(&layouts[0]))
        .unwrap();
    profiles_directory
        .write("partial", &profile::configuration("9", "10"))
        .unwrap();
    assert!(layouts_directory.path.join("profile-first.yaml").is_file());
    assert!(profiles_directory.path.join("partial.yaml").is_file());
    assert_eq!(
        fs::metadata(&profiles_directory.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    let record = directory.path.join("launches");
    verify_profile_launch_record(&record, &layouts, profile::Case::Occupied).unwrap();
    assert!(verify_profile_launch_record(&record, &layouts, profile::Case::Partial).is_err());
    // The short-lived command must be recorded too, despite producing no new-window event.
    let expected = "profile-first-window1\nprofile-first-window2\nprofile-second-window1\n";
    fs::write(&record, expected).unwrap();
    verify_profile_launch_record(&record, &layouts, profile::Case::Partial).unwrap();
    assert!(verify_profile_launch_record(&record, &layouts, profile::Case::Occupied).is_err());
    for incorrect in [
        "profile-first-window1\nprofile-first-window2\n",
        "profile-second-window1\nprofile-first-window1\nprofile-first-window2\n",
        "profile-first-window1\nprofile-first-window1\nprofile-second-window1\n",
    ] {
        fs::write(&record, incorrect).unwrap();
        assert!(verify_profile_launch_record(&record, &layouts, profile::Case::Partial).is_err());
    }
    fs::remove_dir_all(&directory.path).unwrap();
}

#[test]
fn returning_to_a_profile_target_requires_fresh_cleanup_evidence() {
    let scenario = standard_scenario("horizontal-pair");
    let primary = target_identity();
    let second = WorkspaceIdentity {
        id: 100,
        name: "10".into(),
    };
    let before = loaded_pair_snapshot(&scenario);
    let preexisting = container_ids(&starting_snapshot().tree);
    let accepted = accepted_windows(&[(20, "window1"), (21, "window2")]);
    let foreign = ForeignWindows::record(&before, primary.id).unwrap();
    let mut context = CleanupContext {
        target: &second,
        layout: &scenario.layout,
        preexisting_ids: &preexisting,
        foreign: &foreign,
    };
    let baseline = signature(before.focused_workspace());
    assert!(prepare_close(&before, &context, &accepted, &baseline, 20, true).is_err());
    context.target = &primary;
    prepare_close(&before, &context, &accepted, &baseline, 20, true).unwrap();
    assert!(prepare_close(&before, &context, &accepted, &baseline, 20, false).is_err());
    let focus = vec![window_event("focus", 21, None)];
    require_return_focus(&focus, &accepted).unwrap();
    assert!(
        require_return_focus(
            &[window_event("focus", PREEXISTING_WINDOW_ID, None)],
            &accepted
        )
        .is_err()
    );
    assert!(require_return_focus(&[window_event("close", 21, None)], &accepted).is_err());
    let mut returned = loaded_pair_snapshot(&scenario);
    returned.window_events = focus;
    assert!(prepare_close(&returned, &context, &accepted, &baseline, 20, true).is_err());
    // Only the next clean fence may authorize a close; switch events themselves cannot.
    returned.window_events.clear();
    prepare_close(
        &returned,
        &context,
        &accepted,
        &signature(returned.focused_workspace()),
        20,
        true,
    )
    .unwrap();
}

#[test]
fn sway_stage_comes_from_the_same_cargo_build_as_the_harness() {
    assert_eq!(
        sibling_sway_stage(Path::new(
            "/repo/target/x86_64-unknown-linux-gnu/release/examples/integration"
        ))
        .unwrap(),
        Path::new("/repo/target/x86_64-unknown-linux-gnu/release/sway-stage")
    );

    for refused in [
        "/repo/target/release/deps/integration-0123abcd",
        "integration",
        "/",
    ] {
        let error = sibling_sway_stage(Path::new(refused)).unwrap_err();
        assert!(error.contains("run.sh"), "{refused:?}: {error}");
    }
}
