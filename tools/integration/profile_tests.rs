use super::*;
use crate::fixtures::{
    FOOT, OTHER_WORKSPACE_ID, PREEXISTING_WINDOW_ID, RUN_ID, SECOND_OUTPUT_WORKSPACE_ID, SLEEP,
    TARGET_WORKSPACE_ID, accepted_windows, container_ids, desktop, node_mut, parse_tree, set_focus,
    two_output_desktop, two_output_outputs, two_output_workspaces_reply, wayland_window,
    window_event, workspaces_reply,
};
use crate::scenarios::configuration_yaml;
use serde_json::{Value, json};

const SWAYMSG: &str = "/usr/bin/swaymsg";
const PRIVATE_SOCKET: &str = "/private path/runtime/sway-ipc.sock";

fn tick_burst() -> Vec<String> {
    tick_burst_command(
        "/private path/tick-burst.sh",
        SWAYMSG,
        PRIVATE_SOCKET,
        SLEEP,
    )
}

fn fixtures(case: Case) -> [Scenario; 2] {
    layouts(
        case,
        RUN_ID,
        FOOT,
        SLEEP,
        "/private path/record.sh",
        "/private path/launches",
        &tick_burst(),
    )
}

#[test]
fn fixtures_use_unique_selectors_preserve_argv_and_render_ordered_profiles() {
    let mut selectors = HashSet::new();
    for case in CASES {
        let layouts = fixtures(case);
        for scenario in &layouts {
            let yaml = configuration_yaml(scenario);
            assert!(yaml.contains("/private path/record.sh"));
            for leaf in scenario.layout.leaves() {
                assert!(selectors.insert(leaf.app_id.clone()));
                assert!(leaf.app_id.as_str().contains(RUN_ID));
                assert_eq!(&leaf.command[..2], &["/bin/sh", "/private path/record.sh"]);
                assert_eq!(leaf.command[3], "/private path/launches");
                let is_burst_leaf = case == Case::EventBurst
                    && scenario.name == "profile-first"
                    && leaf.layout_id == "window1";
                let application = if is_burst_leaf {
                    // The recorder stays outermost; the burst then execs the window's command.
                    assert_eq!(leaf.command[4..10], tick_burst());
                    &leaf.command[10..]
                } else {
                    &leaf.command[4..]
                };
                if case == Case::Partial && scenario.name == "profile-second" {
                    assert_eq!(application, &[SLEEP, "0"]);
                    assert_eq!(scenario.window_timeout_ms, 500);
                } else {
                    assert_eq!(application[0], FOOT);
                    assert_eq!(application[1], format!("--app-id={}", leaf.app_id.as_str()));
                }
            }
        }
    }
    assert_eq!(
        configuration("10", "8"),
        "version: 1\nworkspaces:\n  - workspace: 10\n    layout: profile-first\n  - workspace: 8\n    layout: profile-second\n"
    );
}

fn launch_events(scenario: &Scenario, first_id: i64) -> Vec<WindowEvent> {
    let mut events = Vec::new();
    for (index, leaf) in scenario.layout.leaves().iter().enumerate() {
        let id = first_id + index as i64;
        events.push(window_event("new", id, Some(leaf.app_id.as_str())));
        events.push(window_event("focus", id, Some(leaf.app_id.as_str())));
    }
    let focused_index = scenario
        .layout
        .leaves()
        .iter()
        .position(|leaf| leaf.layout_id == scenario.expected_focus)
        .unwrap();
    events.push(window_event("focus", first_id + focused_index as i64, None));
    events
}

#[test]
fn launch_binding_requires_complete_first_layout_before_the_switch() {
    let layouts = fixtures(Case::Success);
    let first = launch_events(&layouts[0], 20);
    let mut events = launch_events(&layouts[0], 20);
    events.extend(launch_events(&layouts[1], 30));
    let accepted =
        bind_ordered_launches(&events, first.len(), &layouts, &HashSet::new(), false).unwrap();
    assert_eq!(
        accepted[0],
        layouts[0]
            .layout
            .leaves()
            .iter()
            .enumerate()
            .map(|(index, leaf)| (20 + index as i64, leaf.layout_id.to_owned()))
            .collect()
    );
    assert_eq!(
        accepted[1],
        accepted_windows(&[(30, "window1"), (31, "window2")])
    );

    // An early/late switch, cross-target activity, reused ID, or wrong final focus is fatal.
    for boundary in [0, 2, first.len() + 1, events.len(), events.len() + 1] {
        assert!(
            bind_ordered_launches(&events, boundary, &layouts, &HashSet::new(), false).is_err()
        );
    }
    assert!(
        bind_ordered_launches(&events, first.len(), &layouts, &HashSet::from([20]), false).is_err()
    );
    let mut wrong_focus = launch_events(&layouts[0], 20);
    wrong_focus.extend(launch_events(&layouts[1], 30));
    wrong_focus[first.len() - 1] = window_event("focus", 20, None);
    assert!(
        bind_ordered_launches(&wrong_focus, first.len(), &layouts, &HashSet::new(), false).is_err()
    );
    events.push(window_event("close", 20, None));
    assert!(bind_ordered_launches(&events, first.len(), &layouts, &HashSet::new(), false).is_err());
}

#[test]
fn partial_failure_accepts_no_second_target_windows_or_corrective_activity() {
    let layouts = fixtures(Case::Partial);
    let mut events = launch_events(&layouts[0], 20);
    let boundary = events.len();
    let accepted =
        bind_ordered_launches(&events, boundary, &layouts, &HashSet::new(), true).unwrap();
    assert_eq!(accepted[0].len(), 2);
    assert!(accepted[1].is_empty());
    for change in ["new", "close", "move", "focus"] {
        events.truncate(boundary);
        events.push(window_event(change, 30, None));
        assert!(bind_ordered_launches(&events, boundary, &layouts, &HashSet::new(), true).is_err());
    }
}

/// The session round-trip's four layouts, each launched after its own switch: the window
/// events and each switch's boundary among them.
fn sequential_session_events() -> ([Scenario; 4], Vec<WindowEvent>, Vec<usize>) {
    let layouts = crate::scenarios::session_roundtrip_layouts(RUN_ID, FOOT, SLEEP);
    let mut events = Vec::new();
    let mut switch_indexes = Vec::new();
    for (layout_index, layout) in layouts.iter().enumerate() {
        switch_indexes.push(events.len());
        events.extend(launch_events(layout, 20 + 10 * layout_index as i64));
    }
    (layouts, events, switch_indexes)
}

#[test]
fn sequential_binding_gives_each_target_only_the_windows_after_its_own_switch() {
    let (layouts, events, switch_indexes) = sequential_session_events();
    let accepted =
        bind_sequential_launches(&events, &switch_indexes, &layouts, &HashSet::new()).unwrap();
    for (layout_index, accepted_windows_of_layout) in accepted.iter().enumerate() {
        let first_id = 20 + 10 * layout_index as i64;
        assert_eq!(
            *accepted_windows_of_layout,
            accepted_windows(&[
                (first_id, "window1"),
                (first_id + 1, "window2"),
                (first_id + 2, "window3")
            ])
        );
    }

    // Every window shares Foot's default app ID, so only the boundaries tell targets apart:
    // a shifted, missing, or reordered boundary is fatal.
    let shifted_later = vec![0, 8, 14, 21];
    let shifted_earlier = vec![0, 6, 14, 21];
    let missing = vec![0, 7, 14];
    let reordered = vec![0, 14, 7, 21];
    let past_the_end = vec![0, 7, 14, events.len() + 1];
    for boundaries in [
        shifted_later,
        shifted_earlier,
        missing,
        reordered,
        past_the_end,
    ] {
        assert!(
            bind_sequential_launches(&events, &boundaries, &layouts, &HashSet::new()).is_err(),
            "{boundaries:?}"
        );
    }
}

#[test]
fn sequential_binding_rejects_early_launches_reuse_and_unselected_focus() {
    let (layouts, events, switch_indexes) = sequential_session_events();
    let bind = |events: &[WindowEvent], switch_indexes: &[usize], preexisting: &HashSet<i64>| {
        bind_sequential_launches(events, switch_indexes, &layouts, preexisting)
    };

    // A window before the first switch belongs to no target.
    let (_, mut early_window, _) = sequential_session_events();
    early_window.insert(0, window_event("new", 90, Some("foot")));
    let shifted: Vec<usize> = switch_indexes.iter().map(|index| index + 1).collect();
    assert!(bind(&early_window, &shifted, &HashSet::new()).is_err());

    // An earlier target's window cannot be launched again for a later one.
    let (_, mut reused, _) = sequential_session_events();
    reused[switch_indexes[1]] = window_event("new", 20, Some("foot"));
    assert!(bind(&reused, &switch_indexes, &HashSet::new()).is_err());
    assert!(bind(&events, &switch_indexes, &HashSet::from([40])).is_err());

    // A completed layout must have selected its own final focus before the next switch.
    let (_, mut unselected, _) = sequential_session_events();
    unselected[switch_indexes[2] - 1] = window_event("focus", 30, None);
    assert!(bind(&unselected, &switch_indexes, &HashSet::new()).is_err());

    // Activity of an earlier target's window during a later target's construction is fatal.
    let (_, mut late_activity, _) = sequential_session_events();
    late_activity.push(window_event("move", 21, None));
    assert!(bind(&late_activity, &switch_indexes, &HashSet::new()).is_err());
}

#[test]
fn a_shared_layout_is_one_layout_referenced_by_both_entries() {
    let entries = sequential_entries(SequentialCase::SharedLayout, "2", "3", RUN_ID, FOOT, SLEEP);
    let [first, second] = entries.as_slice() else {
        panic!("two entries");
    };
    assert_eq!(
        (first.workspace.as_str(), second.workspace.as_str()),
        ("2", "3")
    );
    assert_eq!(first.layout.name, "profile-shared");
    // The same file, so the same selectors and literal commands for both workspaces.
    assert_eq!(
        configuration_yaml(&first.layout),
        configuration_yaml(&second.layout)
    );
    assert_eq!(
        sequential_configuration(&entries),
        "version: 1\nworkspaces:\n  - workspace: 2\n    layout: profile-shared\n  - workspace: 3\n    layout: profile-shared\n"
    );
}

#[test]
fn the_recreated_origin_profile_targets_the_absent_workspace_before_the_focused_one() {
    let entries = sequential_entries(
        SequentialCase::RecreatedOrigin,
        "2",
        "3",
        RUN_ID,
        FOOT,
        SLEEP,
    );
    assert_eq!(
        sequential_configuration(&entries),
        "version: 1\nworkspaces:\n  - workspace: 3\n    layout: profile-first\n  - workspace: 2\n    layout: profile-second\n"
    );
    let selectors: HashSet<String> = entries
        .iter()
        .flat_map(|entry| entry.layout.layout.leaves())
        .map(|leaf| leaf.app_id.clone())
        .collect();
    assert_eq!(selectors.len(), 4);
    for case in SEQUENTIAL_CASES {
        assert!(!CASES.iter().any(|other| other.name() == case.name()));
    }
}

#[test]
fn sequential_selection_events_follow_focus_and_remove_only_an_empty_origin() {
    let origin = WorkspaceIdentity {
        id: 20,
        name: "2".into(),
    };
    let created = WorkspaceIdentity {
        id: 30,
        name: "3".into(),
    };
    let recreated = WorkspaceIdentity {
        id: 40,
        name: "2".into(),
    };
    let init = |id, name| workspace_event("init", id, name, name.parse().unwrap(), None);
    let focus =
        |id, name: &str, old| workspace_event("focus", id, name, name.parse().unwrap(), Some(old));
    let empty = |id, name: &str| workspace_event("empty", id, name, name.parse().unwrap(), None);

    // The focused origin is the first target: only the second is created and selected, and
    // the origin keeps its windows.
    let kept = [origin.clone(), created.clone()];
    let expected = sequential_selection_events(&origin, &kept);
    verify_workspace_events(&[init(30, "3"), focus(30, "3", 20)], &expected).unwrap();
    assert!(
        verify_workspace_events(
            &[init(30, "3"), focus(30, "3", 20), empty(20, "2")],
            &expected
        )
        .is_err()
    );

    // Leaving the empty origin first removes it; its name is then created anew.
    let reordered = [created.clone(), recreated.clone()];
    let expected = sequential_selection_events(&origin, &reordered);
    let observed = [
        init(30, "3"),
        focus(30, "3", 20),
        empty(20, "2"),
        init(40, "2"),
        focus(40, "2", 30),
    ];
    verify_workspace_events(&observed, &expected).unwrap();
    for (case, observed) in [
        (
            "origin kept",
            vec![init(30, "3"), focus(30, "3", 20), focus(20, "2", 30)],
        ),
        (
            "origin reused",
            vec![
                init(30, "3"),
                focus(30, "3", 20),
                empty(20, "2"),
                init(20, "2"),
                focus(20, "2", 30),
            ],
        ),
        (
            "first target removed",
            vec![
                init(30, "3"),
                focus(30, "3", 20),
                empty(20, "2"),
                init(40, "2"),
                focus(40, "2", 30),
                empty(30, "3"),
            ],
        ),
    ] {
        assert!(
            verify_workspace_events(&observed, &expected).is_err(),
            "{case}"
        );
    }
}

#[test]
fn constructed_layouts_have_only_new_containers_of_their_own() {
    let workspace = |id, name: &str, children| {
        let mut workspace = crate::fixtures::container(
            id,
            "workspace",
            "splith",
            crate::fixtures::rect(0, 0, 2560, 1440),
            children,
        );
        workspace["name"] = json!(name);
        parse_tree(workspace)
    };
    let window = |id| wayland_window(id, "app", crate::fixtures::rect(0, 0, 10, 10));
    let group = |id, children| {
        crate::fixtures::group(id, "splitv", crate::fixtures::rect(0, 0, 10, 10), children)
    };

    let first = workspace(20, "2", vec![window(41), group(42, vec![window(43)])]);
    let second = workspace(30, "3", vec![window(51), window(52)]);
    // The kept origin's own ID may predate the load.
    let preexisting = HashSet::from([6, 20]);
    require_distinct_new_containers(&[&first, &second], &preexisting).unwrap();

    let preexisting_window = workspace(30, "3", vec![window(6), window(52)]);
    let shared_group = workspace(30, "3", vec![group(42, vec![window(52)])]);
    for second in [preexisting_window, shared_group] {
        assert!(require_distinct_new_containers(&[&first, &second], &preexisting).is_err());
    }
}

fn empty_target() -> Snapshot {
    Snapshot {
        tree: parse_tree(desktop("splith", Vec::new(), PREEXISTING_WINDOW_ID)),
        workspaces: workspaces_reply("1"),
        focused_workspace_id: OTHER_WORKSPACE_ID,
        window_events: Vec::new(),
        workspace_events: Vec::new(),
    }
}

#[test]
fn target_evidence_freezes_existing_identity_and_output_and_allows_absence() {
    let before = empty_target();
    let target = Target::record(&before, "9").unwrap().unwrap();
    target.verify(&before).unwrap();
    assert!(Target::record(&before, "10").unwrap().is_none());
    assert!(container_ids(&before.tree).contains(&TARGET_WORKSPACE_ID));
    for changed_field in ["id", "name", "output-id", "output-name"] {
        let mut changed = empty_target();
        match changed_field {
            "id" => changed.tree.nodes[1].nodes[1].id = 100,
            "name" => changed.tree.nodes[1].nodes[1].name = Some("renamed".into()),
            "output-id" => changed.tree.nodes[1].id = 100,
            "output-name" => changed.tree.nodes[1].name = Some("different".into()),
            _ => unreachable!(),
        }
        target.verify(&changed).unwrap_err();
    }
    let mut renamed = empty_target();
    renamed.workspaces[1].name = "9: other".into();
    assert!(Target::record(&renamed, "9").is_err());
    let mut absent = empty_target();
    absent.workspaces.pop();
    absent.tree.nodes[1].nodes.pop();
    assert!(Target::record(&absent, "9").unwrap().is_none());
    target.verify(&absent).unwrap_err();
    let mut duplicate = empty_target();
    duplicate.workspaces.push(crate::ipc::Workspace {
        id: 80,
        num: 9,
        name: "9: duplicate".into(),
        focused: false,
    });
    assert!(Target::record(&duplicate, "9").is_err());
}

#[test]
fn workspace_return_preserves_exact_layout_state_apart_from_focus() {
    let before = parse_tree(desktop("splith", Vec::new(), PREEXISTING_WINDOW_ID));
    let mut after = parse_tree(desktop("splith", Vec::new(), TARGET_WORKSPACE_ID));
    require_preserved_layout(&before, &after).unwrap();
    after.nodes[1].nodes[0].nodes[0].rect.width += 1;
    assert!(require_preserved_layout(&before, &after).is_err());
    let mut after = parse_tree(desktop("splith", Vec::new(), TARGET_WORKSPACE_ID));
    after.nodes[1].nodes[0].nodes[0].id = 200;
    assert!(require_preserved_layout(&before, &after).is_err());
}

fn named_target() -> Snapshot {
    let mut observed = empty_target();
    observed.focused_workspace_id = TARGET_WORKSPACE_ID;
    observed.workspaces[0].focused = false;
    observed.workspaces[1].focused = true;
    observed.tree.nodes[1].nodes[0].nodes[0].focused = false;
    observed.tree.nodes[1].nodes[1].focused = true;
    observed
}

#[test]
fn disposable_rename_requires_empty_focus_exact_identity_and_unused_destination() {
    let before = named_target();
    let primary = WorkspaceIdentity {
        id: TARGET_WORKSPACE_ID,
        name: "9".into(),
    };
    before.validate().unwrap();
    verify_rename_preparation(&before, &primary, "stage-test music").unwrap();
    for destination in ["1", "9", "bad;exit"] {
        assert!(verify_rename_preparation(&before, &primary, destination).is_err());
    }
    for scenario in ["focus", "name", "number", "floating", "structural"] {
        let mut changed = named_target();
        match scenario {
            "focus" => changed.focused_workspace_id = OTHER_WORKSPACE_ID,
            "name" => changed.tree.nodes[1].nodes[1].name = Some("other".into()),
            "number" => changed.tree.nodes[1].nodes[1].num = Some(-1),
            "floating" => changed.tree.nodes[1].nodes[1]
                .floating_nodes
                .push(parse_tree(crate::fixtures::container(
                    80,
                    "floating_con",
                    "splith",
                    serde_json::json!({"x": 0, "y": 0, "width": 100, "height": 100}),
                    vec![],
                ))),
            _ => changed.tree.nodes[1].nodes[1]
                .nodes
                .push(parse_tree(crate::fixtures::container(
                    80,
                    "con",
                    "splith",
                    serde_json::json!({"x": 0, "y": 0, "width": 100, "height": 100}),
                    vec![],
                ))),
        }
        assert!(
            verify_rename_preparation(&changed, &primary, "stage-test music").is_err(),
            "{scenario}"
        );
    }
}

fn renamed_target() -> Snapshot {
    let mut after = named_target();
    after.tree.nodes[1].nodes[1].name = Some("stage-test music".into());
    after.tree.nodes[1].nodes[1].num = Some(-1);
    after.workspaces[0].focused = false;
    after.workspaces[1].focused = true;
    after.workspaces[1].name = "stage-test music".into();
    after.workspaces[1].num = -1;
    after
}

#[test]
fn named_evidence_checks_true_number_identity_output_and_restoration() {
    let before = named_target();
    let renamed = WorkspaceIdentity {
        id: TARGET_WORKSPACE_ID,
        name: "stage-test music".into(),
    };
    let after = renamed_target();
    verify_rename_result(&before, &after, &renamed).unwrap();
    let target = Target::record(&after, &renamed.name).unwrap().unwrap();
    target.verify(&after).unwrap();
    let original = WorkspaceIdentity {
        id: TARGET_WORKSPACE_ID,
        name: "9".into(),
    };
    verify_rename_preparation(&after, &renamed, "9").unwrap();
    verify_rename_result(&after, &before, &original).unwrap();
    assert!(configuration(&renamed.name, "10").contains("workspace: \"stage-test music\""));
    for scenario in ["id", "num", "name", "output", "focus"] {
        let mut changed = renamed_target();
        match scenario {
            "id" => changed.tree.nodes[1].nodes[1].id = 88,
            "num" => changed.tree.nodes[1].nodes[1].num = Some(9),
            "name" => changed.tree.nodes[1].nodes[1].name = Some("other".into()),
            "output" => changed.tree.nodes[1].id = 88,
            _ => changed.focused_workspace_id = OTHER_WORKSPACE_ID,
        }
        assert!(
            verify_rename_result(&before, &changed, &renamed).is_err(),
            "{scenario}"
        );
        assert!(target.verify(&changed).is_err() || scenario == "focus");
    }
}

#[test]
fn captured_profile_references_preserve_targets_and_ordinal_order() {
    for primary in ["8", "stage-test music"] {
        let target = if primary == "8" {
            "8"
        } else {
            "\"stage-test music\""
        };
        let yaml = format!(
            "version: 1\nworkspaces:\n  - workspace: {target}\n    layout: captured-001\n  - workspace: 10\n    layout: captured-002\n"
        );
        verify_captured_references(&yaml, "captured", &[primary, "10"]).unwrap();
        for changed in [
            yaml.replace("captured-001", "wrong"),
            yaml.replace("workspace: 10", "workspace: 9"),
            yaml.replace("version: 1", "version: 2"),
        ] {
            assert!(verify_captured_references(&changed, "captured", &[primary, "10"]).is_err());
        }
    }
}

#[test]
fn captured_profile_references_name_each_target_in_capture_order() {
    let yaml = "version: 1\nworkspaces:\n  - workspace: 5\n    layout: session-001\n  \
                - workspace: 6\n    layout: session-002\n  - workspace: 7\n    layout: \
                session-003\n  - workspace: 8\n    layout: session-004\n";
    let targets = ["5", "6", "7", "8"];
    verify_captured_references(yaml, "session", &targets).unwrap();

    assert!(verify_captured_references(yaml, "other", &targets).is_err());
    assert!(verify_captured_references(yaml, "session", &["5", "6", "8", "7"]).is_err());
    assert!(verify_captured_references(yaml, "session", &["5", "6", "7"]).is_err());
    let swapped_layouts = yaml
        .replace("session-003", "swap")
        .replace("session-004", "session-003")
        .replace("swap", "session-004");
    assert!(verify_captured_references(&swapped_layouts, "session", &targets).is_err());
    assert_eq!(captured_layout_name("session", 0), "session-001");
    assert_eq!(captured_layout_name("session", 3), "session-004");
}

#[test]
fn tick_burst_checks_every_tick_through_the_private_socket_before_starting_the_window() {
    assert_eq!(
        tick_burst(),
        [
            "/bin/sh",
            "/private path/tick-burst.sh",
            SWAYMSG,
            PRIVATE_SOCKET,
            SLEEP,
            "400"
        ]
    );
    for required in [
        "\"$swaymsg\" -s \"$socket\" -t send_tick",
        "|| exit 1\n    remaining=",
        "\"$sleep\" 0.1 || exit 1",
        "exec \"$@\"",
    ] {
        assert!(TICK_BURST_SCRIPT.contains(required), "{required}");
    }
}

/// A workspace event as Sway reports it; `num` is separate so that it can disagree.
fn workspace_event(
    change: &str,
    workspace_id: i64,
    name: &str,
    num: i32,
    old_id: Option<i64>,
) -> WorkspaceEvent {
    let old = old_id.map(|id| json!({"type": "workspace", "id": id, "name": "old", "num": -1}));
    serde_json::from_value(json!({
        "change": change,
        "current": {"type": "workspace", "id": workspace_id, "name": name, "num": num},
        "old": old,
    }))
    .expect("a workspace event in Sway's format")
}

#[test]
fn workspace_events_must_match_the_expected_sequence_exactly() {
    let created = WorkspaceIdentity {
        id: 30,
        name: "3".into(),
    };
    let expected = [
        ExpectedWorkspaceEvent::Init(&created),
        ExpectedWorkspaceEvent::Focus {
            from_id: 10,
            to: &created,
        },
    ];
    let init = || workspace_event("init", 30, "3", 3, None);
    let focus = || workspace_event("focus", 30, "3", 3, Some(10));
    verify_workspace_events(&[init(), focus()], &expected).unwrap();

    // A cross-output selection must also report removing exactly the hidden workspace.
    let hidden = WorkspaceIdentity {
        id: 20,
        name: "2".into(),
    };
    let with_removal = [
        ExpectedWorkspaceEvent::Init(&created),
        ExpectedWorkspaceEvent::Focus {
            from_id: 10,
            to: &created,
        },
        ExpectedWorkspaceEvent::Empty(&hidden),
    ];
    let empty = || workspace_event("empty", 20, "2", 2, None);
    verify_workspace_events(&[init(), focus(), empty()], &with_removal).unwrap();
    for (case, observed) in [
        ("missing removal", vec![init(), focus()]),
        (
            "other workspace removed",
            vec![init(), focus(), workspace_event("empty", 21, "2", 2, None)],
        ),
        (
            "renamed removal",
            vec![
                init(),
                focus(),
                workspace_event("empty", 20, "2: x", 2, None),
            ],
        ),
        (
            "renumbered removal",
            vec![init(), focus(), workspace_event("empty", 20, "2", -1, None)],
        ),
        (
            "removal with old",
            vec![
                init(),
                focus(),
                workspace_event("empty", 20, "2", 2, Some(10)),
            ],
        ),
        ("removal before focus", vec![init(), empty(), focus()]),
        ("duplicate removal", vec![init(), focus(), empty(), empty()]),
    ] {
        assert!(
            verify_workspace_events(&observed, &with_removal).is_err(),
            "{case}"
        );
    }

    for (case, observed) in [
        ("missing focus", vec![init()]),
        (
            "extra destruction",
            vec![init(), focus(), workspace_event("empty", 20, "2", 2, None)],
        ),
        ("reordered", vec![focus(), init()]),
        (
            "other workspace created",
            vec![workspace_event("init", 31, "3", 3, None), focus()],
        ),
        (
            "renamed creation",
            vec![workspace_event("init", 30, "3: x", 3, None), focus()],
        ),
        (
            "renumbered creation",
            vec![workspace_event("init", 30, "3", -1, None), focus()],
        ),
        (
            "creation with old",
            vec![workspace_event("init", 30, "3", 3, Some(10)), focus()],
        ),
        (
            "focus from elsewhere",
            vec![init(), workspace_event("focus", 30, "3", 3, Some(11))],
        ),
        (
            "move instead of creation",
            vec![workspace_event("move", 30, "3", 3, None), focus()],
        ),
    ] {
        assert!(
            verify_workspace_events(&observed, &expected).is_err(),
            "{case}"
        );
    }
}

#[test]
fn the_cross_output_profile_targets_the_assigned_workspace_on_the_second_output() {
    assert_eq!(
        cross_output_configuration(),
        "version: 1\nworkspaces:\n  - workspace: 3\n    layout: profile-cross-output\n    output: HEADLESS-2\n"
    );
    let layout = cross_output_layout(
        RUN_ID,
        FOOT,
        SLEEP,
        "/private path/record.sh",
        "/private path/launches",
    );
    assert_eq!(layout.name, CROSS_OUTPUT_SCENARIO);
    for leaf in layout.layout.leaves() {
        assert!(leaf.app_id.contains(RUN_ID));
        assert!(leaf.app_id.contains(CROSS_OUTPUT_SCENARIO));
        assert_eq!(
            &leaf.command[..4],
            &[
                "/bin/sh",
                "/private path/record.sh",
                &format!("profile-cross-output-{}", leaf.layout_id),
                "/private path/launches"
            ]
        );
        assert_eq!(leaf.command[4], FOOT);
    }
}

const FIXTURE_WINDOW_ID: i64 = 40;
const CREATED_WORKSPACE_ID: i64 = 13;
/// HEADLESS-2's container in `two_output_desktop`.
const SECOND_OUTPUT_ID: i64 = 12;

fn second_output_workspace() -> WorkspaceIdentity {
    WorkspaceIdentity {
        id: SECOND_OUTPUT_WORKSPACE_ID,
        name: "2".into(),
    }
}

fn fixture_workspace() -> WorkspaceIdentity {
    WorkspaceIdentity {
        id: OTHER_WORKSPACE_ID,
        name: "1".into(),
    }
}

/// The two-output desktop after the foreign fixture loaded on workspace "1".
fn cross_output_before_tree() -> Value {
    let mut tree = two_output_desktop(FIXTURE_WINDOW_ID);
    node_mut(&mut tree, OTHER_WORKSPACE_ID)["nodes"] = json!([wayland_window(
        FIXTURE_WINDOW_ID,
        "fixture",
        crate::fixtures::rect(0, 0, 2560, 1440)
    )]);
    set_focus(&mut tree, FIXTURE_WINDOW_ID);
    tree
}

fn cross_output_before() -> Snapshot {
    Snapshot {
        tree: parse_tree(cross_output_before_tree()),
        workspaces: two_output_workspaces_reply("1"),
        focused_workspace_id: OTHER_WORKSPACE_ID,
        window_events: Vec::new(),
        workspace_events: Vec::new(),
    }
}

/// After the profile: the created "3" is focused on HEADLESS-2, and Sway removed "2" unless
/// `keep_hidden_workspace` models it surviving.
fn cross_output_loaded(keep_hidden_workspace: bool) -> Snapshot {
    let mut tree = two_output_desktop(CREATED_WORKSPACE_ID);
    node_mut(&mut tree, OTHER_WORKSPACE_ID)["nodes"] = json!([wayland_window(
        FIXTURE_WINDOW_ID,
        "fixture",
        crate::fixtures::rect(0, 0, 2560, 1440)
    )]);
    let mut created = node_mut(&mut tree, SECOND_OUTPUT_WORKSPACE_ID).clone();
    created["id"] = json!(CREATED_WORKSPACE_ID);
    created["name"] = json!("3");
    created["num"] = json!(3);
    let second_output_workspaces = node_mut(&mut tree, SECOND_OUTPUT_ID)["nodes"]
        .as_array_mut()
        .unwrap();
    if !keep_hidden_workspace {
        second_output_workspaces.clear();
    }
    second_output_workspaces.push(created);
    set_focus(&mut tree, CREATED_WORKSPACE_ID);

    let mut workspaces = two_output_workspaces_reply("none");
    if !keep_hidden_workspace {
        workspaces.pop();
    }
    workspaces.push(crate::ipc::Workspace {
        id: CREATED_WORKSPACE_ID,
        num: 3,
        name: "3".into(),
        focused: true,
    });
    Snapshot {
        tree: parse_tree(tree),
        workspaces,
        focused_workspace_id: CREATED_WORKSPACE_ID,
        window_events: Vec::new(),
        workspace_events: Vec::new(),
    }
}

#[test]
fn two_output_baseline_requires_the_original_empty_initial_workspace_shown_on_the_second_output() {
    let before = cross_output_before();
    let baseline = TwoOutputBaseline::record(
        &before,
        &two_output_outputs(),
        &fixture_workspace(),
        &second_output_workspace(),
    )
    .unwrap();
    assert_eq!(
        baseline.second_output_workspace,
        WorkspaceIdentity {
            id: SECOND_OUTPUT_WORKSPACE_ID,
            name: "2".into(),
        }
    );

    let mut occupied = cross_output_before();
    occupied.tree.nodes[2].nodes[0].nodes = vec![parse_tree(wayland_window(
        41,
        "other",
        crate::fixtures::rect(0, 0, 10, 10),
    ))];
    assert!(
        TwoOutputBaseline::record(
            &occupied,
            &two_output_outputs(),
            &fixture_workspace(),
            &second_output_workspace()
        )
        .err()
        .unwrap()
        .contains("must be empty")
    );

    let mut not_shown = two_output_outputs();
    not_shown[1].current_workspace = Some("4".into());
    assert!(
        TwoOutputBaseline::record(
            &before,
            &not_shown,
            &fixture_workspace(),
            &second_output_workspace()
        )
        .err()
        .unwrap()
        .contains("shown on HEADLESS-2")
    );

    let other_fixture = WorkspaceIdentity {
        id: SECOND_OUTPUT_WORKSPACE_ID,
        name: "2".into(),
    };
    assert!(
        TwoOutputBaseline::record(
            &before,
            &two_output_outputs(),
            &other_fixture,
            &second_output_workspace()
        )
        .err()
        .unwrap()
        .contains("start on the fixture workspace")
    );

    // A workspace "2" with another identity replaced the original one.
    let replaced = WorkspaceIdentity {
        id: CREATED_WORKSPACE_ID,
        name: "2".into(),
    };
    assert!(
        TwoOutputBaseline::record(
            &before,
            &two_output_outputs(),
            &fixture_workspace(),
            &replaced
        )
        .err()
        .unwrap()
        .contains("initial workspace was replaced")
    );

    // No other workspace: in particular, a target that a load must create is still absent.
    let mut created_early_tree = cross_output_before_tree();
    let mut created_early_workspace =
        node_mut(&mut created_early_tree, SECOND_OUTPUT_WORKSPACE_ID).clone();
    created_early_workspace["id"] = json!(CREATED_WORKSPACE_ID);
    created_early_workspace["name"] = json!("3");
    created_early_workspace["num"] = json!(3);
    node_mut(&mut created_early_tree, SECOND_OUTPUT_ID)["nodes"]
        .as_array_mut()
        .unwrap()
        .push(created_early_workspace);
    let mut created_early_workspaces = two_output_workspaces_reply("1");
    created_early_workspaces.push(crate::ipc::Workspace {
        id: CREATED_WORKSPACE_ID,
        num: 3,
        name: "3".into(),
        focused: false,
    });
    let created_early = Snapshot {
        tree: parse_tree(created_early_tree),
        workspaces: created_early_workspaces,
        focused_workspace_id: OTHER_WORKSPACE_ID,
        window_events: Vec::new(),
        workspace_events: Vec::new(),
    };
    assert!(
        TwoOutputBaseline::record(
            &created_early,
            &two_output_outputs(),
            &fixture_workspace(),
            &second_output_workspace()
        )
        .err()
        .unwrap()
        .contains("exists beside the two initial workspaces")
    );
}

#[test]
fn cross_output_result_requires_removal_placement_focus_and_an_unmoved_fixture() {
    let baseline = TwoOutputBaseline::record(
        &cross_output_before(),
        &two_output_outputs(),
        &fixture_workspace(),
        &second_output_workspace(),
    )
    .unwrap();
    let created = WorkspaceIdentity {
        id: CREATED_WORKSPACE_ID,
        name: "3".into(),
    };
    baseline
        .verify_cross_output_result(&cross_output_loaded(false), &created, OTHER_WORKSPACE_ID)
        .unwrap();

    assert!(
        baseline
            .verify_cross_output_result(&cross_output_loaded(true), &created, OTHER_WORKSPACE_ID)
            .unwrap_err()
            .contains("still exists")
    );

    // The created workspace on the first output, beside the fixture.
    let mut wrong_output = cross_output_loaded(false);
    let target = wrong_output.tree.nodes[2].nodes.remove(0);
    wrong_output.tree.nodes[1].nodes.push(target);
    assert!(
        baseline
            .verify_cross_output_result(&wrong_output, &created, OTHER_WORKSPACE_ID)
            .unwrap_err()
            .contains("not on the second output")
    );

    // The fixture workspace moved to the second output.
    let mut moved_fixture = cross_output_loaded(false);
    let fixture = moved_fixture.tree.nodes[1].nodes.remove(0);
    moved_fixture.tree.nodes[2].nodes.push(fixture);
    assert!(
        baseline
            .verify_cross_output_result(&moved_fixture, &created, OTHER_WORKSPACE_ID)
            .unwrap_err()
            .contains("fixture workspace changed output")
    );

    let unfocused = WorkspaceIdentity {
        id: OTHER_WORKSPACE_ID,
        name: "1".into(),
    };
    assert!(
        baseline
            .verify_cross_output_result(&cross_output_loaded(false), &unfocused, OTHER_WORKSPACE_ID)
            .is_err()
    );
}

#[test]
fn two_output_baseline_names_each_output_and_keeps_the_second_outputs_workspace() {
    let before = cross_output_before();
    let outputs = two_output_outputs();
    let baseline = TwoOutputBaseline::record(
        &before,
        &outputs,
        &fixture_workspace(),
        &second_output_workspace(),
    )
    .unwrap();

    baseline
        .require_on_output(&before, OTHER_WORKSPACE_ID, "HEADLESS-1")
        .unwrap();
    baseline
        .require_on_output(&before, SECOND_OUTPUT_WORKSPACE_ID, "HEADLESS-2")
        .unwrap();
    for (workspace_id, output_name) in [
        (OTHER_WORKSPACE_ID, "HEADLESS-2"),
        (SECOND_OUTPUT_WORKSPACE_ID, "HEADLESS-1"),
        (OTHER_WORKSPACE_ID, "HEADLESS-9"),
    ] {
        assert!(
            baseline
                .require_on_output(&before, workspace_id, output_name)
                .is_err(),
            "{workspace_id} on {output_name}"
        );
    }
    baseline
        .require_second_output_workspace_kept(&before, &outputs)
        .unwrap();

    let mut occupied = cross_output_before();
    occupied.tree.nodes[2].nodes[0].nodes = vec![parse_tree(wayland_window(
        41,
        "other",
        crate::fixtures::rect(0, 0, 10, 10),
    ))];
    assert!(
        baseline
            .require_second_output_workspace_kept(&occupied, &outputs)
            .is_err()
    );

    let mut hidden = two_output_outputs();
    hidden[1].current_workspace = Some("3".into());
    assert!(
        baseline
            .require_second_output_workspace_kept(&before, &hidden)
            .is_err()
    );

    // The cross-output profile's result: the initial workspace is gone.
    assert!(
        baseline
            .require_second_output_workspace_kept(&cross_output_loaded(false), &outputs)
            .unwrap_err()
            .contains("disappeared")
    );

    // The same name with another identity.
    let mut replaced = cross_output_before();
    replaced.workspaces[1].id = CREATED_WORKSPACE_ID;
    replaced.tree.nodes[2].nodes[0].id = CREATED_WORKSPACE_ID;
    assert!(
        baseline
            .require_second_output_workspace_kept(&replaced, &outputs)
            .is_err()
    );
}
