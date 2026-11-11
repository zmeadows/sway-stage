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
        let arguments = Arguments::try_parse_from([
            "integration",
            "--workspace",
            workspace,
            "--foreign-workspace",
            "8",
        ])
        .unwrap();
        assert_eq!(arguments.workspace, workspace);
        assert_eq!(arguments.foreign_workspace, "8");
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
        "--foreign-workspace",
        "8",
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
            Arguments::try_parse_from([
                "integration",
                "--workspace",
                invalid,
                "--foreign-workspace",
                "8"
            ])
            .is_err(),
            "{invalid:?}"
        );
    }
    for arguments in [
        vec!["integration", "--workspace", "9", "--workspace", "8"],
        vec!["integration", "--workspace", "9", "--unknown"],
        vec!["integration", "--workspace", "9", "extra"],
        vec!["integration", "--workspace"],
    ] {
        let mut arguments = arguments;
        arguments.extend(["--foreign-workspace", "8"]);
        assert!(Arguments::try_parse_from(arguments).is_err());
    }
    assert!(Arguments::try_parse_from(["integration", "--workspace", "9"]).is_err());
    assert!(
        Arguments::try_parse_from([
            "integration",
            "--workspace",
            "9",
            "--foreign-workspace",
            "08"
        ])
        .is_err()
    );
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
    let foreign = ForeignWindows::record(&loaded_pair(&scenario), target.id).unwrap();
    let context = CleanupContext {
        foreign: &foreign,
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
    let foreign = ForeignWindows::record(&loaded_pair(&scenario), target.id).unwrap();
    let context = CleanupContext {
        foreign: &foreign,
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
    let foreign = ForeignWindows::record(&loaded_pair(&scenario), target.id).unwrap();
    let context = CleanupContext {
        foreign: &foreign,
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
    let path = directory.path.clone();
    drop(directory);
    assert!(!path.exists());
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
    let before = loaded_pair(&scenario);
    let after = loaded_pair(&scenario);
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
        uncertain_message(verify_refusal(
            &refused_output(code, &diagnostic),
            &before,
            &after,
            false,
        ));
    }
    let mut stdout = refused_output(1, &occupied_diagnostic());
    stdout.stdout = "unexpected data".into();
    uncertain_message(verify_refusal(&stdout, &before, &after, false));
    // Init/empty or focus events returning to the baseline are still evidence of activity.
    uncertain_message(verify_refusal(
        &refused_output(1, &occupied_diagnostic()),
        &before,
        &after,
        true,
    ));
}

#[test]
fn occupied_refusal_rejects_new_windows_and_changed_ids_hierarchy_focus_or_geometry() {
    let scenario = standard_scenario("occupied-workspace-refusal");
    let before = loaded_pair(&scenario);
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
        let mut after = loaded_pair(&scenario);
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
        uncertain_message(verify_refusal(&output, &before, &after, false));
    }
    for change in [
        "new",
        "close",
        "focus",
        "move",
        "floating",
        "fullscreen_mode",
    ] {
        let mut after = loaded_pair(&scenario);
        after.window_events.push(window_event(change, 99, None));
        assert_contains(
            &uncertain_message(verify_refusal(&output, &before, &after, false)),
            "unexpected window activity",
        );
    }
}

#[test]
fn foreign_fixture_requires_two_safe_workspaces_and_its_own_launch_evidence_for_cleanup() {
    use crate::fixtures::{FOOT, SLEEP};
    assert!(fixture_eligibility("9", "8").is_ok());
    assert!(fixture_eligibility("9", "9").is_err());
    assert!(fixture_eligibility("9", "8;kill").is_err());
    // Both numbered targets independently use the existing preflight rules: occupied,
    // original, and differently named workspaces never become fixtures.
    assert!(preflight_eligibility(&starting_snapshot(), "1").is_err());
    assert!(
        preflight_eligibility(&loaded_pair(&standard_scenario("horizontal-pair")), "9").is_err()
    );

    let scenario = foreign_fixture_scenario(RUN_ID, FOOT, SLEEP);
    let selector = selector_value(&scenario, "window1");
    let preexisting = container_ids(&starting_snapshot().tree);
    let mut loaded = observed(
        desktop(
            "splith",
            vec![wayland_window(20, &selector, rect(0, 0, 1920, 1080))],
            20,
        ),
        "9",
        TARGET_WORKSPACE_ID,
    );
    loaded.window_events = vec![
        window_event("new", 20, Some(&selector)),
        window_event("focus", 20, Some(&selector)),
    ];
    let owned = bind_launches(&loaded.window_events, &scenario.layout, &preexisting).unwrap();
    verify_ownership(
        &loaded.tree,
        TARGET_WORKSPACE_ID,
        &scenario.layout,
        &preexisting,
        &owned,
    )
    .unwrap();
    verify_layout(loaded.focused_workspace(), &scenario, &owned).unwrap();
    loaded.window_events.clear();
    let baseline = signature(loaded.focused_workspace());
    let foreign = ForeignWindows::record(&loaded, TARGET_WORKSPACE_ID).unwrap();
    let target = target_identity();
    let context = CleanupContext {
        target: &target,
        layout: &scenario.layout,
        preexisting_ids: &preexisting,
        foreign: &foreign,
    };
    let plan = prepare_close(&loaded, &context, &owned, &baseline, 20, true).unwrap();
    for closing_id in [PREEXISTING_WINDOW_ID, 99] {
        uncertain_message(prepare_close(
            &loaded, &context, &owned, &baseline, closing_id, true,
        ));
    }
    uncertain_message(prepare_close(
        &loaded,
        &context,
        &AcceptedWindows::new(),
        &baseline,
        20,
        true,
    ));
    uncertain_message(prepare_close(
        &loaded, &context, &owned, &baseline, 20, false,
    ));
    let mut changed_foreign = observed(
        desktop(
            "splith",
            vec![wayland_window(20, &selector, rect(0, 0, 1920, 1080))],
            20,
        ),
        "9",
        TARGET_WORKSPACE_ID,
    );
    changed_foreign.tree.nodes[1].nodes[0].nodes[0].rect.width -= 1;
    assert_contains(
        &uncertain_message(prepare_close(
            &changed_foreign,
            &context,
            &owned,
            &baseline,
            20,
            true,
        )),
        "foreign window",
    );
    // Preservation is checked on the post-close boundary too; no later close is allowed.
    uncertain_message(verify_close_observation(
        &changed_foreign,
        &context,
        &owned,
        &plan,
        &baseline,
        20,
    ));
    let mut cleaned = observed(
        desktop("splith", Vec::new(), TARGET_WORKSPACE_ID),
        "9",
        TARGET_WORKSPACE_ID,
    );
    cleaned.window_events.push(window_event("close", 20, None));
    assert!(verify_close_observation(&cleaned, &context, &owned, &plan, &baseline, 20).unwrap());
}

#[test]
fn interrupted_or_incomplete_second_invocation_never_authorizes_cleanup_or_restoration() {
    refusal_completion(Ok(refused_output(1, &occupied_diagnostic())), false).unwrap();
    for interrupted in [false, true] {
        for reason in [
            "deadline expired",
            "stdout exceeded limit",
            "output stayed open",
            "start failed",
            "interrupted",
        ] {
            uncertain_message(refusal_completion(Err(reason.into()), interrupted));
        }
    }
    uncertain_message(refusal_completion(
        Ok(refused_output(1, &occupied_diagnostic())),
        true,
    ));
}

#[test]
fn fixture_creation_evidence_survives_expected_focus_changes_but_never_changed_state() {
    use crate::fixtures::{FOOT, SLEEP};
    let scenario = foreign_fixture_scenario(RUN_ID, FOOT, SLEEP);
    let selector = selector_value(&scenario, "window1");
    let make_tree = |focus| {
        desktop(
            "splith",
            vec![wayland_window(20, &selector, rect(0, 0, 1920, 1080))],
            focus,
        )
    };
    let loaded = observed(make_tree(20), "9", TARGET_WORKSPACE_ID);
    let baseline = signature(loaded.focused_workspace());
    let fixture = ForeignFixture {
        workspace: target_identity(),
        progress: ScenarioProgress {
            scenario,
            preexisting_ids: Some(container_ids(&starting_snapshot().tree)),
            accepted: accepted(&[(20, "window1")]),
        },
        foreign: ForeignWindows::record(&loaded, TARGET_WORKSPACE_ID).unwrap(),
        loaded_signature: Some(baseline.clone()),
    };
    let elsewhere = observed(make_tree(PREEXISTING_WINDOW_ID), "1", OTHER_WORKSPACE_ID);
    verify_fixture_state(&elsewhere, &fixture, &baseline).unwrap();
    verify_fixture_state(&loaded, &fixture, &baseline).unwrap();
    let mut changed = observed(make_tree(20), "9", TARGET_WORKSPACE_ID);
    changed.tree.nodes[1].nodes[1].nodes[0].rect.height -= 1;
    uncertain_message(verify_fixture_state(&changed, &fixture, &baseline));
}

#[test]
fn refusal_notification_stub_is_private_and_scoped_to_the_refusal_command() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let test_id = format!("{RUN_ID}-{}-refusal-notification", std::process::id());
    let directory = TemporaryConfigurations::new(&env::temp_dir(), &test_id).unwrap();
    let original_path = env::var_os("PATH");
    let inherited_path = OsString::from_vec(b"/tools with spaces:/non-utf8-\xff:/usr/bin".to_vec());
    let binary = Path::new("/a path/sway-stage");
    let refused = directory.refusal_command(binary, &inherited_path).unwrap();

    let notification_bin = directory.path.join("refusal-notification-bin");
    let stub = notification_bin.join("notify-send");
    assert_eq!(fs::read(&stub).unwrap(), b"#!/bin/sh\nexit 0\n");
    for path in [&notification_bin, &stub] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    let overrides: Vec<_> = refused.get_envs().collect();
    let [(key, Some(child_path))] = overrides.as_slice() else {
        panic!("expected only one child PATH override: {overrides:?}");
    };
    assert_eq!(*key, OsStr::new("PATH"));
    assert_eq!(
        env::split_paths(child_path).collect::<Vec<_>>(),
        [
            notification_bin,
            PathBuf::from("/tools with spaces"),
            PathBuf::from(OsString::from_vec(b"/non-utf8-\xff".to_vec())),
            PathBuf::from("/usr/bin"),
        ]
    );
    assert_eq!(env::var_os("PATH"), original_path);

    let ordinary = configured_command(binary, &directory.path, "load", "single-window");
    assert_eq!(refused.get_program(), ordinary.get_program());
    assert_eq!(
        refused.get_args().collect::<Vec<_>>(),
        ordinary.get_args().collect::<Vec<_>>()
    );
    assert_eq!(ordinary.get_envs().count(), 0);
    // An existing stub directory is never reused or overwritten.
    assert!(directory.refusal_command(binary, &inherited_path).is_err());
    let path = directory.path.clone();
    drop(directory);
    assert!(!path.exists());
}

#[test]
fn replay_transition_requires_clean_empty_preserved_state_before_discarding_evidence() {
    let scenario = standard_scenario("horizontal-pair");
    let loaded = loaded_pair(&scenario);
    let foreign = ForeignWindows::record(&loaded, TARGET_WORKSPACE_ID).unwrap();
    let make_progress = || ScenarioProgress {
        scenario: scenario.clone(),
        preexisting_ids: Some(container_ids(&starting_snapshot().tree)),
        accepted: accepted(&[(20, "window1"), (21, "window2")]),
    };
    let empty = || {
        observed(
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
    let mut survivor = loaded_pair(&scenario);
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
            accepted(&[(20, "window1"), (21, "window2")])
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
    let first = loaded_pair(&scenario);
    let target = target_identity();
    let foreign = ForeignWindows::record(&first, target.id).unwrap();
    let mut progress = ScenarioProgress {
        scenario: scenario.clone(),
        preexisting_ids: Some(container_ids(&starting_snapshot().tree)),
        accepted: accepted(&[(20, "window1"), (21, "window2")]),
    };
    let first_baseline = signature(first.focused_workspace());
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
        &first_baseline,
        20,
        true,
    )
    .unwrap();
    assert!(
        prepare_close(
            &first,
            &first_context,
            &progress.accepted,
            &first_baseline,
            40,
            true
        )
        .is_err()
    );

    let mut empty = observed(
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
    let preexisting = progress.preexisting_ids.as_ref().unwrap();
    assert!(preexisting.contains(&99));
    assert!(preexisting.contains(&20) && preexisting.contains(&21));
    assert!(progress.accepted.is_empty());

    let mut second = loaded_pair(&scenario);
    second.tree.nodes[1].nodes[1].nodes[0].id = 40;
    second.tree.nodes[1].nodes[1].nodes[1].id = 41;
    let second_baseline = signature(second.focused_workspace());
    let context = CleanupContext {
        target: &target,
        foreign: &foreign,
        layout: &scenario.layout,
        preexisting_ids: preexisting,
    };
    // Failed/incomplete second loads leave no accepted evidence, so no close is legal.
    assert!(
        prepare_close(
            &second,
            &context,
            &progress.accepted,
            &second_baseline,
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
    assert!(bind_launches(&new_events(20, 21), &scenario.layout, preexisting).is_err());
    assert!(bind_launches(&new_events(40, 99), &scenario.layout, preexisting).is_err());
    assert!(bind_launches(&new_events(40, 40), &scenario.layout, preexisting).is_err());
    let owned = bind_launches(&new_events(40, 41), &scenario.layout, preexisting).unwrap();
    verify_ownership(
        &second.tree,
        target.id,
        &scenario.layout,
        preexisting,
        &owned,
    )
    .unwrap();
    verify_layout(second.focused_workspace(), &scenario, &owned).unwrap();
    prepare_close(&second, &context, &owned, &second_baseline, 40, true).unwrap();
    assert!(prepare_close(&second, &context, &owned, &second_baseline, 20, true).is_err());
    assert!(prepare_close(&second, &context, &owned, &second_baseline, 41, false).is_err());
    assert!(prepare_close(&second, &context, &owned, &first_baseline, 40, true).is_err());
}

#[test]
fn unavailable_launcher_is_restored_before_returning_capture_success_or_failure() {
    for outcome in [0, 1, 2] {
        let test_id = format!("{RUN_ID}-{}-hidden-{outcome}", std::process::id());
        let directory = TemporaryConfigurations::new(&env::temp_dir(), &test_id).unwrap();
        let mut scenario = crate::fixtures::extended_scenario("unresolved-launcher");
        configure_desktop_launcher(&mut scenario, &directory.path).unwrap();
        let fixture = scenario.layout.leaves()[0]
            .desktop_launcher
            .as_ref()
            .unwrap();
        let original = fs::metadata(&fixture.path).unwrap();
        let hidden = fixture.path.with_extension("desktop.unavailable");
        let result = directory.with_unavailable_launcher(fixture, || {
            assert!(!fixture.path.exists());
            assert!(hidden.exists());
            if outcome == 2 {
                Err("simulated bounded runner failure or interruption".into())
            } else {
                Ok(refused_output(outcome, "private captured stderr"))
            }
        });
        assert_eq!(result.is_err(), outcome == 2);
        assert!(private_launcher_matches(fixture));
        assert_eq!(fs::metadata(&fixture.path).unwrap().ino(), original.ino());
        assert!(!hidden.exists());
        assert!(!directory.retain.get());
    }
}

#[test]
fn failed_launcher_restoration_retains_evidence_and_never_overwrites_a_replacement() {
    for failure in [
        "occupied",
        "missing-backup",
        "changed-backup",
        "replaced-backup",
    ] {
        let test_id = format!("{RUN_ID}-{}-restore-{failure}", std::process::id());
        let directory = TemporaryConfigurations::new(&env::temp_dir(), &test_id).unwrap();
        let mut scenario = crate::fixtures::extended_scenario("unresolved-launcher");
        configure_desktop_launcher(&mut scenario, &directory.path).unwrap();
        let fixture = scenario.layout.leaves()[0]
            .desktop_launcher
            .as_ref()
            .unwrap();
        let hidden = fixture.path.with_extension("desktop.unavailable");
        let result = directory.with_unavailable_launcher(fixture, || {
            match failure {
                "occupied" => fs::write(&fixture.path, "replacement").unwrap(),
                "missing-backup" => fs::remove_file(&hidden).unwrap(),
                "changed-backup" => fs::write(&hidden, "changed").unwrap(),
                "replaced-backup" => {
                    // Keep the original inode alive to make replacement deterministic.
                    fs::rename(&hidden, hidden.with_extension("retired")).unwrap();
                    fs::write(&hidden, &fixture.contents).unwrap();
                }
                _ => unreachable!(),
            }
            Ok(refused_output(0, ""))
        });
        let message = uncertain_message(result.map(|_| ()));
        assert!(message.contains("restoration failed"));
        assert!(message.contains("files retained at"));
        if failure == "occupied" {
            assert_eq!(fs::read(&fixture.path).unwrap(), b"replacement");
            assert_eq!(fs::read(&hidden).unwrap(), fixture.contents.as_bytes());
        }
        let path = directory.path.clone();
        drop(directory);
        assert!(path.exists());
        fs::remove_dir_all(path).unwrap();
    }
}

#[test]
fn hiding_a_launcher_refuses_unverified_or_out_of_scope_files_before_capture() {
    let test_id = format!("{RUN_ID}-{}-hide-preconditions", std::process::id());
    let directory = TemporaryConfigurations::new(&env::temp_dir(), &test_id).unwrap();
    let mut scenario = crate::fixtures::extended_scenario("unresolved-launcher");
    configure_desktop_launcher(&mut scenario, &directory.path).unwrap();
    let fixture = scenario.layout.leaves()[0]
        .desktop_launcher
        .as_ref()
        .unwrap();
    let mut outside = fixture.clone();
    outside.path = directory.path.join("outside.desktop");
    assert!(
        directory
            .with_unavailable_launcher(&outside, || panic!("outside file"))
            .is_err()
    );
    let hidden = fixture.path.with_extension("desktop.unavailable");
    fs::write(&hidden, "occupied").unwrap();
    assert!(
        directory
            .with_unavailable_launcher(fixture, || panic!("occupied backup"))
            .is_err()
    );
    fs::remove_file(&hidden).unwrap();
    fs::write(&fixture.path, "changed").unwrap();
    assert!(
        directory
            .with_unavailable_launcher(fixture, || panic!("unverified file"))
            .is_err()
    );
}
