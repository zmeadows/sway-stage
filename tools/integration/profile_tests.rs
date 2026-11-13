use super::*;
use crate::fixtures::{
    FOOT, OTHER_WORKSPACE_ID, PREEXISTING_WINDOW_ID, RUN_ID, SLEEP, TARGET_WORKSPACE_ID,
    accepted_windows, container_ids, desktop, parse_tree, uncertain_message, window_event,
    workspaces_reply,
};
use crate::scenarios::configuration_yaml;

fn fixtures(case: Case) -> [Scenario; 2] {
    layouts(
        case,
        RUN_ID,
        FOOT,
        SLEEP,
        "/private path/record.sh",
        "/private path/launches",
    )
}

#[test]
fn workspace_numbers_are_distinct_and_obey_existing_restrictions() {
    workspace_eligibility("8", "9", "10").unwrap();
    for numbers in [
        ("8", "8", "10"),
        ("8", "9", "8"),
        ("8", "9", "9"),
        ("8", "9", "01"),
        ("8", "9", "0"),
        ("8", "9", "1;kill"),
        ("8", "9", "1000000000"),
    ] {
        assert!(workspace_eligibility(numbers.0, numbers.1, numbers.2).is_err());
    }
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
                assert!(selectors.insert(leaf.selector.value().to_owned()));
                assert!(leaf.selector.value().contains(RUN_ID));
                assert_eq!(&leaf.command[..2], &["/bin/sh", "/private path/record.sh"]);
                assert_eq!(leaf.command[3], "/private path/launches");
                if case == Case::Partial && scenario.name == "profile-second" {
                    assert_eq!(&leaf.command[4..], &[SLEEP, "0"]);
                    assert_eq!(scenario.window_timeout_ms, 500);
                } else {
                    assert_eq!(leaf.command[4], FOOT);
                    assert_eq!(
                        leaf.command[5],
                        format!("--app-id={}", leaf.selector.value())
                    );
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
        events.push(window_event("new", id, Some(leaf.selector.value())));
        events.push(window_event("focus", id, Some(leaf.selector.value())));
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

fn empty_target() -> Snapshot {
    Snapshot {
        tree: parse_tree(desktop("splith", Vec::new(), PREEXISTING_WINDOW_ID)),
        workspaces: workspaces_reply("1"),
        focused_workspace_id: OTHER_WORKSPACE_ID,
        window_events: Vec::new(),
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
        uncertain_message(target.verify(&changed));
    }
    let mut renamed = empty_target();
    renamed.workspaces[1].name = "9: other".into();
    assert!(Target::record(&renamed, "9").is_err());
    let mut absent = empty_target();
    absent.workspaces.pop();
    absent.tree.nodes[1].nodes.pop();
    assert!(Target::record(&absent, "9").unwrap().is_none());
    uncertain_message(target.verify(&absent));
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
        verify_captured_references(&yaml, primary, "10").unwrap();
        for changed in [
            yaml.replace("captured-001", "wrong"),
            yaml.replace("workspace: 10", "workspace: 9"),
            yaml.replace("version: 1", "version: 2"),
        ] {
            assert!(verify_captured_references(&changed, primary, "10").is_err());
        }
    }
}
