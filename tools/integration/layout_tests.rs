use super::*;
use crate::fixtures::{
    TARGET_WORKSPACE_ID, accepted_windows, assert_contains, check_message, desktop, find, group,
    node_mut, parse_tree, rect, selector_value, set_focus, standard_scenario, uncertain_message,
    wayland_window, xwayland_window,
};
use serde_json::{Value, json};

/// deep-nesting as sway-stage builds it on a 1920x1080 workspace: a workspace wrapper
/// around the vertical root, and a horizontal group nested in another horizontal group.
/// Windows 20..=24 are window1..=window5; window4 is focused.
fn deep_nesting_desktop(scenario: &Scenario) -> Value {
    let window = |id: i64, layout_id: &str, rect: Value| {
        wayland_window(id, &selector_value(scenario, layout_id), rect)
    };
    let column = group(
        33,
        "splitv",
        rect(1229, 0, 691, 648),
        vec![
            window(22, "window3", rect(1229, 0, 691, 389)),
            window(23, "window4", rect(1229, 389, 691, 259)),
        ],
    );
    let inner_row = group(
        32,
        "splith",
        rect(768, 0, 1152, 648),
        vec![window(21, "window2", rect(768, 0, 461, 648)), column],
    );
    let outer_row = group(
        31,
        "splith",
        rect(0, 0, 1920, 648),
        vec![window(20, "window1", rect(0, 0, 768, 648)), inner_row],
    );
    let root = group(
        30,
        "splitv",
        rect(0, 0, 1920, 1080),
        vec![outer_row, window(24, "window5", rect(0, 648, 1920, 432))],
    );
    desktop("splith", vec![root], 23)
}

fn deep_nesting_accepted() -> AcceptedWindows {
    accepted_windows(&[
        (20, "window1"),
        (21, "window2"),
        (22, "window3"),
        (23, "window4"),
        (24, "window5"),
    ])
}

/// horizontal-pair (3:2) on a workspace 1000 pixels wide, so widths are tenths of percent.
fn horizontal_pair_desktop(
    scenario: &Scenario,
    widths: (i32, i32),
    focused_container_id: i64,
) -> Value {
    let (first_width_pixels, second_width_pixels) = widths;
    desktop(
        "splith",
        vec![
            wayland_window(
                20,
                &selector_value(scenario, "window1"),
                rect(0, 0, first_width_pixels, 1080),
            ),
            wayland_window(
                21,
                &selector_value(scenario, "window2"),
                rect(first_width_pixels, 0, second_width_pixels, 1080),
            ),
        ],
        focused_container_id,
    )
}

fn verify_recorded_layout(
    tree: Value,
    scenario: &Scenario,
    accepted_windows: &AcceptedWindows,
) -> Result<LayoutMeasurements, Failure> {
    let tree = parse_tree(tree);
    verify_layout(find(&tree, TARGET_WORKSPACE_ID), scenario, accepted_windows)
}

#[test]
fn nested_hierarchy_through_a_wrapper_verifies_with_measured_shares() {
    let scenario = standard_scenario("deep-nesting");
    let measurements = verify_recorded_layout(
        deep_nesting_desktop(&scenario),
        &scenario,
        &deep_nesting_accepted(),
    )
    .unwrap();

    assert!(
        measurements.sizing_notes.is_empty(),
        "{:?}",
        measurements.sizing_notes
    );
    let observed_paths: Vec<&Vec<usize>> = measurements.shares.keys().collect();
    let expected_paths: [&[usize]; 8] = [
        &[0],
        &[0, 0],
        &[0, 1],
        &[0, 1, 0],
        &[0, 1, 1],
        &[0, 1, 1, 0],
        &[0, 1, 1, 1],
        &[1],
    ];
    assert_eq!(observed_paths, expected_paths);
    let measured_share = |path: &[usize]| measurements.shares[path];
    assert_eq!(measured_share(&[0]), 60.0);
    assert_eq!(measured_share(&[1]), 40.0);
    assert_eq!(measured_share(&[0, 0]), 40.0);
    assert!((measured_share(&[0, 1, 0]) - 100.0 * 461.0 / 1152.0).abs() < 1e-9);
    assert!((measured_share(&[0, 1, 1, 0]) - 100.0 * 389.0 / 648.0).abs() < 1e-9);
}

#[test]
fn flattened_or_reoriented_groups_are_hierarchy_mismatches() {
    let scenario = standard_scenario("deep-nesting");

    // Sway merged the nested horizontal group into its horizontal parent.
    let mut flattened = deep_nesting_desktop(&scenario);
    let inner_row = node_mut(&mut flattened, 32).take();
    let inner_children = inner_row["nodes"].as_array().unwrap().clone();
    let outer_row = node_mut(&mut flattened, 31);
    let mut outer_children = vec![outer_row["nodes"][0].take()];
    outer_children.extend(inner_children);
    outer_row["nodes"] = json!(outer_children);
    let message = uncertain_message(verify_recorded_layout(
        flattened,
        &scenario,
        &deep_nesting_accepted(),
    ));

    assert_contains(
        &message,
        "layout.children[0]: expected splith with 2 children",
    );
    assert_contains(&message, "Expected layout:\nversion: 1\n");
    assert_contains(&message, "Observed: 7:splith(30:splitv(");

    let mut reoriented = deep_nesting_desktop(&scenario);
    node_mut(&mut reoriented, 33)["layout"] = json!("splith");
    let message = uncertain_message(verify_recorded_layout(
        reoriented,
        &scenario,
        &deep_nesting_accepted(),
    ));
    assert_contains(
        &message,
        "layout.children[0].children[1].children[1]: expected splitv",
    );
}

#[test]
fn identities_order_and_focus_must_match_exactly() {
    let scenario = standard_scenario("horizontal-pair");
    let owned_windows = accepted_windows(&[(20, "window1"), (21, "window2")]);

    let swapped = accepted_windows(&[(20, "window2"), (21, "window1")]);
    let message = uncertain_message(verify_recorded_layout(
        horizontal_pair_desktop(&scenario, (600, 400), 21),
        &scenario,
        &swapped,
    ));

    assert_contains(&message, "window order differs");

    let mut extra = horizontal_pair_desktop(&scenario, (600, 400), 21);
    let target = node_mut(&mut extra, TARGET_WORKSPACE_ID);
    target["nodes"][1] = group(
        30,
        "splitv",
        rect(600, 0, 400, 1080),
        vec![
            target["nodes"][1].take(),
            wayland_window(99, "unrelated", rect(600, 540, 400, 540)),
        ],
    );
    let message = uncertain_message(verify_recorded_layout(extra, &scenario, &owned_windows));
    assert_contains(&message, "unexpected windows");

    let missing = accepted_windows(&[(20, "window1"), (21, "window2"), (22, "window3")]);
    let message = uncertain_message(verify_recorded_layout(
        horizontal_pair_desktop(&scenario, (600, 400), 21),
        &scenario,
        &missing,
    ));
    assert_contains(&message, "unexpected windows");

    for focused_container_id in [20, TARGET_WORKSPACE_ID] {
        let message = uncertain_message(verify_recorded_layout(
            horizontal_pair_desktop(&scenario, (600, 400), focused_container_id),
            &scenario,
            &owned_windows,
        ));
        assert_contains(&message, "final focus differs: expected window2 id=21");
    }

    let mut vertical = horizontal_pair_desktop(&scenario, (600, 400), 21);
    node_mut(&mut vertical, TARGET_WORKSPACE_ID)["layout"] = json!("splitv");
    let message = uncertain_message(verify_recorded_layout(vertical, &scenario, &owned_windows));
    assert_contains(&message, "layout: expected splith with 2 children");
}

#[test]
fn sizing_is_noted_above_one_point_and_fails_above_five() {
    let scenario = standard_scenario("horizontal-pair");
    let owned_windows = accepted_windows(&[(20, "window1"), (21, "window2")]);
    let measure = |widths| {
        verify_recorded_layout(
            horizontal_pair_desktop(&scenario, widths, 21),
            &scenario,
            &owned_windows,
        )
    };

    // Requested shares are 60:40.
    for exact_or_within_target in [(600, 400), (610, 390), (590, 410)] {
        let measurements = measure(exact_or_within_target).unwrap();
        assert!(
            measurements.sizing_notes.is_empty(),
            "{exact_or_within_target:?}"
        );
    }

    for within_tolerance in [(611, 389), (650, 350), (550, 450)] {
        let measurements = measure(within_tolerance).unwrap();
        let [note] = measurements.sizing_notes.as_slice() else {
            panic!("expected one note for {within_tolerance:?}");
        };
        assert_contains(note, "layout splith: expected [60.00, 40.00], observed [");
        assert_contains(note, "container=7");
    }

    let message = check_message(measure((651, 349)));

    assert_contains(
        &message,
        "sizing exceeds 5 percentage points (check application minimum sizes): \
         layout splith: expected [60.00, 40.00], observed [65.10, 34.90] percent",
    );
    assert_contains(&message, "Expected layout:\n");
}

#[test]
fn unusable_geometry_fails_ordinarily_but_overlap_is_uncertain() {
    let scenario = standard_scenario("horizontal-pair");
    let owned_windows = accepted_windows(&[(20, "window1"), (21, "window2")]);

    let message = check_message(verify_recorded_layout(
        horizontal_pair_desktop(&scenario, (1000, 0), 21),
        &scenario,
        &owned_windows,
    ));

    assert_contains(&message, "layout: unusable rectangles");

    let mut overlapping = horizontal_pair_desktop(&scenario, (600, 400), 21);
    node_mut(&mut overlapping, 21)["rect"]["x"] = json!(599);
    let message = uncertain_message(verify_recorded_layout(
        overlapping,
        &scenario,
        &owned_windows,
    ));
    assert_contains(&message, "layout: visual order/overlap differs");

    // Gaps between siblings are expected and excluded from shares.
    let mut gapped = horizontal_pair_desktop(&scenario, (600, 400), 21);
    node_mut(&mut gapped, 21)["rect"]["x"] = json!(620);
    assert!(
        verify_recorded_layout(gapped, &scenario, &owned_windows)
            .unwrap()
            .sizing_notes
            .is_empty()
    );
}

#[test]
fn failure_context_omits_launch_arguments_and_environment() {
    let mut scenario = standard_scenario("horizontal-pair");
    let leaf = scenario.layout.first_leaf_mut();
    leaf.command.push("sensitive-launch-argument".into());
    leaf.env
        .push(("PRIVATE_VALUE".into(), "sensitive-environment-value".into()));
    let message = check_message(verify_recorded_layout(
        horizontal_pair_desktop(&scenario, (700, 300), 21),
        &scenario,
        &accepted_windows(&[(20, "window1"), (21, "window2")]),
    ));

    assert_contains(&message, "Expected layout:");
    assert!(!message.contains("sensitive-") && !message.contains("PRIVATE_VALUE"));
}

#[test]
fn floating_fullscreen_scratchpad_and_unsupported_containers_are_uncertain() {
    let scenario = standard_scenario("horizontal-pair");
    let owned_windows = accepted_windows(&[(20, "window1"), (21, "window2")]);
    let state_message = "floating/fullscreen/scratchpad state";

    let mut cases: Vec<(Value, &str)> = Vec::new();

    let mut floating_child = horizontal_pair_desktop(&scenario, (600, 400), 21);
    node_mut(&mut floating_child, TARGET_WORKSPACE_ID)["floating_nodes"] =
        json!([container_with(40, "floating_con")]);
    cases.push((floating_child, state_message));

    for (field, value) in [
        ("floating", json!("user_on")),
        ("floating", json!("auto_on")),
        ("fullscreen_mode", json!(1)),
        ("fullscreen_mode", json!(2)),
        ("fullscreen_mode", Value::Null),
        ("scratchpad_state", json!("fresh")),
        ("scratchpad_state", json!("changed")),
    ] {
        let mut tree = horizontal_pair_desktop(&scenario, (600, 400), 21);
        node_mut(&mut tree, 20)[field] = value;
        cases.push((tree, state_message));
    }

    // Header wrappers are meaningful even with one child; a split-only layout refuses them.
    for layout in ["tabbed", "stacked"] {
        let mut tree = horizontal_pair_desktop(&scenario, (600, 400), 21);
        let target = node_mut(&mut tree, TARGET_WORKSPACE_ID);
        target["nodes"][0] = group(
            30,
            layout,
            rect(0, 0, 600, 1080),
            vec![target["nodes"][0].take()],
        );
        cases.push((tree, "layout.children[0]: expected window1"));
    }

    let mut window_with_children = horizontal_pair_desktop(&scenario, (600, 400), 21);
    node_mut(&mut window_with_children, 20)["nodes"] = json!([container_with(41, "con")]);
    cases.push((window_with_children, "malformed window: 20:"));

    let mut unexpected_type = horizontal_pair_desktop(&scenario, (600, 400), 21);
    node_mut(&mut unexpected_type, 20)["type"] = json!("floating_con");
    cases.push((unexpected_type, "unexpected container type: 20:"));

    for (tree, expected) in cases {
        assert_contains(
            &uncertain_message(verify_recorded_layout(tree, &scenario, &owned_windows)),
            expected,
        );
    }
}

fn container_with(id: i64, node_type: &str) -> Value {
    let mut node = group(id, "splith", rect(0, 0, 100, 100), Vec::new());
    node["type"] = json!(node_type);
    node
}

#[test]
fn shape_keeps_identities_and_removal_collapses_like_sway() {
    let row = group(
        31,
        "splith",
        rect(0, 0, 1920, 1080),
        vec![
            wayland_window(20, "first", rect(0, 0, 640, 1080)),
            group(
                32,
                "splitv",
                rect(640, 0, 1280, 1080),
                vec![
                    wayland_window(21, "second", rect(640, 0, 1280, 540)),
                    xwayland_window(22, "Third", rect(640, 540, 1280, 540)),
                ],
            ),
        ],
    );
    let wrapper = group(30, "splitv", rect(0, 0, 1920, 1080), vec![row]);
    let tree = parse_tree(desktop("splith", vec![wrapper], 22));
    let observed = shape(find(&tree, TARGET_WORKSPACE_ID)).unwrap();

    let window = |id: i64, app_id: Option<&str>, class: Option<&str>| Shape::Window {
        container_id: id,
        app_id: app_id.map(str::to_owned),
        class: class.map(str::to_owned),
        pid: Some(5_000_000 + id as i32),
    };
    let column = Shape::Group {
        container_id: 32,
        layout: Layout::SplitV,
        children: vec![
            window(21, Some("second"), None),
            window(22, None, Some("Third")),
        ],
    };
    let expected = Shape::Group {
        container_id: 31,
        layout: Layout::SplitH,
        children: vec![window(20, Some("first"), None), column],
    };

    assert_eq!(observed, expected);

    let without_second = remove_from_shape(observed, 21).unwrap();
    let expected = Shape::Group {
        container_id: 31,
        layout: Layout::SplitH,
        children: vec![
            window(20, Some("first"), None),
            window(22, None, Some("Third")),
        ],
    };
    assert_eq!(without_second, expected);

    let without_first = remove_from_shape(without_second, 20).unwrap();
    assert_eq!(without_first, window(22, None, Some("Third")));
    assert_eq!(remove_from_shape(without_first, 22), None);
}

#[test]
fn an_empty_workspace_has_an_empty_shape() {
    let tree = parse_tree(desktop("splith", Vec::new(), TARGET_WORKSPACE_ID));
    let observed = shape(find(&tree, TARGET_WORKSPACE_ID)).unwrap();

    assert_eq!(
        observed,
        Shape::Group {
            container_id: TARGET_WORKSPACE_ID,
            layout: Layout::SplitH,
            children: Vec::new(),
        }
    );
}

#[test]
fn signature_ignores_titles_and_marks_but_not_geometry_or_focus() {
    let scenario = standard_scenario("horizontal-pair");
    let original = horizontal_pair_desktop(&scenario, (600, 400), 21);
    let signature_of = |tree: Value| {
        let tree = parse_tree(tree);
        signature(find(&tree, TARGET_WORKSPACE_ID))
    };
    let baseline = signature_of(original.clone());

    let mut cosmetic = original.clone();
    node_mut(&mut cosmetic, 20)["name"] = json!("a new title");
    node_mut(&mut cosmetic, 20)["marks"] = json!(["mark"]);
    node_mut(&mut cosmetic, 20)["urgent"] = json!(true);

    assert_eq!(signature_of(cosmetic), baseline);

    let mut resized = original.clone();
    node_mut(&mut resized, 20)["rect"]["width"] = json!(601);
    assert_ne!(signature_of(resized), baseline);

    let mut refocused = original.clone();
    set_focus(&mut refocused, 20);
    assert_ne!(signature_of(refocused), baseline);

    let mut new_pid = original;
    node_mut(&mut new_pid, 21)["pid"] = json!(1);
    assert_ne!(signature_of(new_pid), baseline);
}

#[test]
fn outline_names_identities_and_layouts_without_titles() {
    let tree = parse_tree(desktop(
        "splith",
        vec![
            wayland_window(20, "app", rect(0, 0, 960, 1080)),
            group(
                30,
                "splitv",
                rect(960, 0, 960, 1080),
                vec![
                    xwayland_window(21, "Class", rect(960, 0, 960, 540)),
                    wayland_window(22, "", rect(960, 540, 960, 540)),
                ],
            ),
        ],
        20,
    ));
    let outline = outline(find(&tree, TARGET_WORKSPACE_ID));

    assert_eq!(outline, "7:splith(20:app, 30:splitv(21:Class, 22:none))");
    assert!(!outline.contains("title"));
}

#[test]
fn sibling_groups_verify_both_levels_and_default_focus_uses_first_leaf() {
    let scenario = standard_scenario("sibling-nested-groups");
    let window =
        |id, name: &str, rectangle| wayland_window(id, &selector_value(&scenario, name), rectangle);
    let left = group(
        30,
        "splitv",
        rect(0, 0, 600, 1000),
        vec![
            window(20, "window1", rect(0, 0, 600, 400)),
            window(21, "window2", rect(0, 400, 600, 600)),
        ],
    );
    let right = group(
        31,
        "splitv",
        rect(600, 0, 400, 1000),
        vec![
            window(22, "window3", rect(600, 0, 400, 750)),
            window(23, "window4", rect(600, 750, 400, 250)),
        ],
    );
    let tree = desktop("splith", vec![left, right], 22);
    let owned_windows = accepted_windows(&[
        (20, "window1"),
        (21, "window2"),
        (22, "window3"),
        (23, "window4"),
    ]);
    let measurements = verify_recorded_layout(tree.clone(), &scenario, &owned_windows).unwrap();

    assert_eq!(
        measurements.shares,
        BTreeMap::from([
            (vec![0], 60.0),
            (vec![1], 40.0),
            (vec![0, 0], 40.0),
            (vec![0, 1], 60.0),
            (vec![1, 0], 75.0),
            (vec![1, 1], 25.0),
        ])
    );
    let draft = crate::scenarios::expected_draft(&scenario, &measurements.shares).unwrap();
    crate::capture::verify_capture(&draft, &scenario, &measurements).unwrap();
    let mut swapped = tree.clone();
    node_mut(&mut swapped, 31)["nodes"]
        .as_array_mut()
        .unwrap()
        .swap(0, 1);
    assert!(verify_recorded_layout(swapped, &scenario, &owned_windows).is_err());
    let mut wrong_size = tree;
    node_mut(&mut wrong_size, 22)["rect"]["height"] = json!(400);
    assert!(
        check_message(verify_recorded_layout(
            wrong_size,
            &scenario,
            &owned_windows
        ))
        .contains("sizing exceeds")
    );

    let scenario = standard_scenario("default-focus");
    let column = group(
        30,
        "splitv",
        rect(0, 0, 600, 1000),
        vec![
            wayland_window(
                20,
                &selector_value(&scenario, "window1"),
                rect(0, 0, 600, 400),
            ),
            wayland_window(
                21,
                &selector_value(&scenario, "window2"),
                rect(0, 400, 600, 600),
            ),
        ],
    );
    let mut tree = desktop(
        "splith",
        vec![
            column,
            wayland_window(
                22,
                &selector_value(&scenario, "window3"),
                rect(600, 0, 400, 1000),
            ),
        ],
        20,
    );
    let owned_windows = accepted_windows(&[(20, "window1"), (21, "window2"), (22, "window3")]);
    let measurements = verify_recorded_layout(tree.clone(), &scenario, &owned_windows).unwrap();
    let draft = crate::scenarios::expected_draft(&scenario, &measurements.shares).unwrap();
    assert!(draft.contains("focus: window1"));
    crate::capture::verify_capture(&draft, &scenario, &measurements).unwrap();
    set_focus(&mut tree, 22);
    assert!(
        uncertain_message(verify_recorded_layout(tree, &scenario, &owned_windows))
            .contains("final focus differs")
    );
}

#[test]
fn replay_geometry_is_judged_against_captured_weights_instead_of_original_weights() {
    let scenario = standard_scenario("horizontal-pair");
    let measurements = LayoutMeasurements {
        shares: BTreeMap::from([(vec![0], 70.0), (vec![1], 30.0)]),
        sizing_notes: Vec::new(),
    };
    let draft = crate::scenarios::expected_draft(&scenario, &measurements.shares).unwrap();
    let (_, replay) = crate::capture::executable_capture(&draft, &scenario, &measurements).unwrap();
    let owned_windows = accepted_windows(&[(20, "window1"), (21, "window2")]);
    let tree = horizontal_pair_desktop(&scenario, (700, 300), 21);

    assert!(verify_recorded_layout(tree.clone(), &scenario, &owned_windows).is_err());
    verify_recorded_layout(tree, &replay, &owned_windows).unwrap();
    // Small differences from captured geometry retain the ordinary five-point tolerance.
    verify_recorded_layout(
        horizontal_pair_desktop(&scenario, (710, 290), 21),
        &replay,
        &owned_windows,
    )
    .unwrap();
    assert!(
        verify_recorded_layout(
            horizontal_pair_desktop(&scenario, (760, 240), 21),
            &replay,
            &owned_windows
        )
        .is_err()
    );
}

#[test]
fn root_header_groups_check_selection_without_split_geometry() {
    for (name, layout, focused_container_id, history) in [
        ("root-tabbed", "tabbed", 20, json!([20, 22, 21])),
        ("root-stacked", "stacked", 21, json!([21, 22, 20])),
    ] {
        let scenario = standard_scenario(name);
        let windows = (20..=22)
            .map(|id| {
                wayland_window(
                    id,
                    &selector_value(&scenario, &format!("window{}", id - 19)),
                    rect(0, 0, 0, 0),
                )
            })
            .collect();
        let mut tree = desktop(layout, windows, focused_container_id);
        node_mut(&mut tree, TARGET_WORKSPACE_ID)["focus"] = history;
        let owned_windows = accepted_windows(&[(20, "window1"), (21, "window2"), (22, "window3")]);
        assert!(
            verify_recorded_layout(tree.clone(), &scenario, &owned_windows)
                .unwrap()
                .shares
                .is_empty()
        );
        for invalid in [json!([]), json!([99]), json!([22, 21, 20])] {
            let mut changed = tree.clone();
            node_mut(&mut changed, TARGET_WORKSPACE_ID)["focus"] = invalid;
            assert_contains(
                &uncertain_message(verify_recorded_layout(changed, &scenario, &owned_windows)),
                "selected child differs",
            );
        }
        node_mut(&mut tree, TARGET_WORKSPACE_ID)["layout"] = json!("splith");
        assert_contains(
            &uncertain_message(verify_recorded_layout(tree, &scenario, &owned_windows)),
            "expected",
        );
    }
}

#[test]
fn mixed_groups_preserve_singletons_and_measure_only_nested_splits() {
    let scenario = standard_scenario("mixed-capture-reload");
    let tree = crate::fixtures::mixed_native_desktop(&scenario);
    let owned_windows = accepted_windows(&[
        (20, "window1"),
        (21, "window2"),
        (22, "window3"),
        (23, "window4"),
    ]);
    let measurements = verify_recorded_layout(tree.clone(), &scenario, &owned_windows).unwrap();

    assert_eq!(
        measurements.shares,
        BTreeMap::from([
            (vec![0], 60.0),
            (vec![1], 40.0),
            (vec![0, 1, 0], 60.0),
            (vec![0, 1, 1], 40.0),
        ])
    );
    assert!(measurements.sizing_notes.is_empty());

    let mut collapsed = tree.clone();
    let leaf = node_mut(&mut collapsed, 32)["nodes"][0].take();
    node_mut(&mut collapsed, 31)["nodes"][0] = leaf;
    assert_contains(
        &uncertain_message(verify_recorded_layout(collapsed, &scenario, &owned_windows)),
        "expected stacked with 1 children",
    );

    for (id, layout) in [(31, "stacked"), (32, "tabbed"), (33, "splitv")] {
        let mut changed = tree.clone();
        node_mut(&mut changed, id)["layout"] = json!(layout);
        assert!(verify_recorded_layout(changed, &scenario, &owned_windows).is_err());
    }
    let mut reordered = tree.clone();
    node_mut(&mut reordered, 31)["nodes"]
        .as_array_mut()
        .unwrap()
        .swap(0, 1);
    assert_contains(
        &uncertain_message(verify_recorded_layout(reordered, &scenario, &owned_windows)),
        "window order differs",
    );

    let mut bad_nested_size = tree;
    node_mut(&mut bad_nested_size, 21)["rect"]["width"] = json!(800);
    node_mut(&mut bad_nested_size, 22)["rect"]["x"] = json!(800);
    node_mut(&mut bad_nested_size, 22)["rect"]["width"] = json!(200);
    assert_contains(
        &check_message(verify_recorded_layout(
            bad_nested_size,
            &scenario,
            &owned_windows,
        )),
        "layout.children[0].children[1] splith",
    );
}

#[test]
fn nested_and_independent_non_first_selections_survive_capture_replay() {
    let scenario = standard_scenario("independent-native-groups");
    let window = |id, layout_id: &str| {
        wayland_window(
            id,
            &selector_value(&scenario, layout_id),
            rect(0, 0, 600, 1000),
        )
    };
    let mut inner = group(
        31,
        "stacked",
        rect(0, 0, 600, 1000),
        vec![window(21, "window2"), window(22, "window3")],
    );
    inner["focus"] = json!([22, 21]);
    let mut left = group(
        30,
        "tabbed",
        rect(0, 0, 600, 1000),
        vec![window(20, "window1"), inner],
    );
    left["focus"] = json!([31, 20]);
    let mut inner_tabs = group(
        33,
        "tabbed",
        rect(600, 0, 400, 1000),
        vec![window(23, "window4"), window(24, "window5")],
    );
    inner_tabs["focus"] = json!([24, 23]);
    let mut right = group(
        32,
        "stacked",
        rect(600, 0, 400, 1000),
        vec![inner_tabs, window(25, "window6")],
    );
    right["focus"] = json!([25, 33]);
    let tree = desktop("splith", vec![left, right], 22);
    let owned_windows = accepted_windows(&[
        (20, "window1"),
        (21, "window2"),
        (22, "window3"),
        (23, "window4"),
        (24, "window5"),
        (25, "window6"),
    ]);
    let measurements = verify_recorded_layout(tree.clone(), &scenario, &owned_windows).unwrap();
    let draft = crate::scenarios::expected_draft(&scenario, &measurements.shares).unwrap();
    assert_eq!(draft.matches("active: 2").count(), 4);
    let (yaml, replay) =
        crate::capture::executable_capture(&draft, &scenario, &measurements).unwrap();
    assert_eq!(yaml.matches("active: 2").count(), 4);
    verify_recorded_layout(tree.clone(), &replay, &owned_windows).unwrap();
    for (id, bad_history) in [
        (30, json!([20, 31])),
        (31, json!([21, 22])),
        (32, json!([33, 25])),
        (33, json!([23, 24])),
    ] {
        let mut changed = tree.clone();
        node_mut(&mut changed, id)["focus"] = bad_history;
        assert_contains(
            &uncertain_message(verify_recorded_layout(changed, &scenario, &owned_windows)),
            "selected child differs",
        );
    }
    // Moving global focus to the other branch leaves both inactive left selections intact.
    let mut scenario = scenario;
    scenario.expected_focus = "window6";
    let mut elsewhere = tree;
    set_focus(&mut elsewhere, 25);
    node_mut(&mut elsewhere, 32)["focus"] = json!([25, 33]);
    verify_recorded_layout(elsewhere.clone(), &scenario, &owned_windows).unwrap();
    node_mut(&mut elsewhere, 31)["focus"] = json!([21, 22]);

    assert_contains(
        &uncertain_message(verify_recorded_layout(elsewhere, &scenario, &owned_windows)),
        "selected child differs",
    );
}

#[test]
fn signature_preserves_child_focus_history_across_capture() {
    let scenario = standard_scenario("mixed-capture-reload");
    let mut tree = crate::fixtures::mixed_native_desktop(&scenario);
    let before = parse_tree(tree.clone());
    let baseline = signature(find(&before, TARGET_WORKSPACE_ID));
    node_mut(&mut tree, 31)["focus"] = json!([32, 33]);
    let after = parse_tree(tree);

    assert_ne!(signature(find(&after, TARGET_WORKSPACE_ID)), baseline);
}

#[test]
fn cleanup_shape_retains_singleton_headers_and_reaps_only_empty_ancestors() {
    let scenario = standard_scenario("mixed-capture-reload");
    let tree = crate::fixtures::mixed_native_desktop(&scenario);
    let shape_of = |tree: Value| {
        let tree = parse_tree(tree);
        shape(find(&tree, TARGET_WORKSPACE_ID)).unwrap()
    };
    let mut expected = tree.clone();
    node_mut(&mut expected, 31)["nodes"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    let after_first = remove_from_shape(shape_of(tree), 20).unwrap();

    assert_eq!(after_first, shape_of(expected.clone()));
    // Removing window2 leaves a redundant split, but the tabbed parent remains.
    node_mut(&mut expected, 33)["nodes"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    let after_second = remove_from_shape(after_first, 21).unwrap();
    assert_eq!(after_second, shape_of(expected.clone()));
    let mut flattened = expected.clone();
    let survivor = node_mut(&mut flattened, 33)["nodes"][0].take();
    node_mut(&mut flattened, TARGET_WORKSPACE_ID)["nodes"][0] = survivor;
    assert_ne!(after_second, shape_of(flattened));
    node_mut(&mut expected, TARGET_WORKSPACE_ID)["nodes"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    let after_third = remove_from_shape(after_second, 22).unwrap();
    assert_eq!(after_third, shape_of(expected));
    assert_eq!(remove_from_shape(after_third, 23), None);
}

#[test]
fn inactive_profile_layout_requires_the_saved_focus_path_and_full_layout() {
    let scenario = standard_scenario("horizontal-pair");
    let accepted = accepted_windows(&[(20, "window1"), (21, "window2")]);
    let make_tree = || {
        let mut tree = horizontal_pair_desktop(&scenario, (600, 400), 6);
        node_mut(&mut tree, TARGET_WORKSPACE_ID)["focus"] = json!([21, 20]);
        tree
    };
    let tree = parse_tree(make_tree());
    let workspace = find(&tree, TARGET_WORKSPACE_ID);
    verify_inactive_layout(workspace, &scenario, &accepted).unwrap();
    assert!(verify_layout(workspace, &scenario, &accepted).is_err());

    for changed_evidence in [
        "missing-history",
        "wrong-selection",
        "global-focus",
        "geometry",
        "order",
    ] {
        let mut changed = make_tree();
        match changed_evidence {
            "missing-history" => node_mut(&mut changed, TARGET_WORKSPACE_ID)["focus"] = json!([]),
            "wrong-selection" => {
                node_mut(&mut changed, TARGET_WORKSPACE_ID)["focus"] = json!([20, 21])
            }
            "global-focus" => set_focus(&mut changed, 21),
            "geometry" => node_mut(&mut changed, 20)["rect"]["width"] = json!(100),
            "order" => node_mut(&mut changed, TARGET_WORKSPACE_ID)["nodes"]
                .as_array_mut()
                .unwrap()
                .swap(0, 1),
            _ => unreachable!(),
        }
        let changed = parse_tree(changed);
        assert!(
            verify_inactive_layout(find(&changed, TARGET_WORKSPACE_ID), &scenario, &accepted)
                .is_err(),
            "{changed_evidence}"
        );
    }
}
