use super::*;
use crate::command_inference::{infer_host_command, infer_process_command};
use crate::config::{self, Config, GroupLayout, LayoutNode, Matcher, NodeKind};
use crate::observation::fixtures::{
    empty_workspace_tree, ipc_frame, node, read_request, send_message, socket_pair,
};
use serde_json::{Value, json};
use std::io::Read;
use std::thread;

/// A tiled Wayland view whose container rect is `width` by `height` pixels.
fn wayland_window(
    container_id: i64,
    app_id: Option<&str>,
    width_pixels: i32,
    height_pixels: i32,
) -> Value {
    let mut wayland_window = node(container_id, "con", false);
    wayland_window["shell"] = json!("xdg_shell");
    wayland_window["app_id"] = json!(app_id);
    wayland_window["rect"]["width"] = json!(width_pixels);
    wayland_window["rect"]["height"] = json!(height_pixels);
    wayland_window
}

/// A tiled Xwayland view: no app ID, an X11 window, and a class.
fn xwayland_window(container_id: i64, class: &str, width_pixels: i32, height_pixels: i32) -> Value {
    let mut wayland_window = wayland_window(container_id, None, width_pixels, height_pixels);
    wayland_window["shell"] = json!("xwayland");
    wayland_window["window"] = json!(4_000_000 + container_id);
    wayland_window["window_properties"] = json!({"class": class});
    wayland_window
}

fn group(
    container_id: i64,
    layout: &str,
    width_pixels: i32,
    height_pixels: i32,
    children: Vec<Value>,
) -> Value {
    let mut group = node(container_id, "con", false);
    group["layout"] = json!(layout);
    group["rect"]["width"] = json!(width_pixels);
    group["rect"]["height"] = json!(height_pixels);
    group["nodes"] = json!(children);
    group
}

/// A tree whose focused workspace (id 10) holds `children`, with only `focused_container_id` focused.
fn workspace_tree(layout: &str, children: Vec<Value>, focused_container_id: i64) -> Value {
    fn focus_only(value: &mut Value, focused_container_id: i64) {
        value["focused"] = json!(value["id"] == focused_container_id);
        for child in value["nodes"].as_array_mut().unwrap() {
            focus_only(child, focused_container_id);
        }
    }
    let mut tree = empty_workspace_tree();
    let workspace_tree = &mut tree["nodes"][0]["nodes"][0];
    workspace_tree["name"] = json!("1");
    workspace_tree["layout"] = json!(layout);
    workspace_tree["nodes"] = json!(children);
    focus_only(&mut tree, focused_container_id);
    tree
}

fn left_window() -> Value {
    wayland_window(20, Some("left"), 960, 1080)
}

fn right_window() -> Value {
    wayland_window(21, Some("right"), 960, 1080)
}

/// The server accepts exactly one GET_TREE, then verifies that capture disconnects.
fn read_recorded_snapshot(tree: Value) -> Result<Snapshot, String> {
    let (client, mut server) = socket_pair();
    let worker = thread::spawn(move || {
        assert!(read_request(&mut server, 4).is_empty());
        send_message(&mut server, 4, tree);
        assert_eq!(server.read(&mut [0]).unwrap(), 0);
    });

    let result = snapshot_from_socket(client, Instant::now() + IPC_TIMEOUT);
    worker.join().unwrap();

    result
}

fn capture_recorded_tree(tree: Value) -> Result<String, String> {
    read_recorded_snapshot(tree)?
        .render_with(false, &mut |_| panic!("default reads no processes"))
        .map(|draft| draft.yaml)
}

/// The YAML after the explanatory header, which itself mentions both placeholders.
fn draft_body(draft: &str) -> &str {
    draft.strip_prefix(DRAFT_HEADER).expect("draft header")
}

/// Fills the placeholders as a user would, then applies the strict validator `check` uses.
fn parse_filled_draft(draft: &str) -> Config {
    let edited = draft
        .replace("command: null", "command: [app]")
        .replace("match: null", "match: {app_id: filled}");
    config::parse(&edited).unwrap()
}

/// Groups as `h(...)`, `v(...)`, `t(...)`, or `s(...)`, with explicit child weights
/// after `@`; windows as `id app_id=value` or `id class=value`.
fn layout_outline(layout: &LayoutNode) -> String {
    match &layout.kind {
        NodeKind::Window {
            id,
            matcher: Matcher::AppId(app_id),
            ..
        } => format!("{id} app_id={app_id}"),
        NodeKind::Window {
            id,
            matcher: Matcher::Class(class),
            ..
        } => format!("{id} class={class}"),
        NodeKind::Group { layout, children } => {
            let group = match layout {
                GroupLayout::SplitH => "h",
                GroupLayout::SplitV => "v",
                GroupLayout::Tabbed => "t",
                GroupLayout::Stacked => "s",
            };
            let children: Vec<_> = children
                .iter()
                .map(|child| {
                    let mut description = layout_outline(&child.value);
                    if let Some(weight) = child.value.weight {
                        description.push_str(&format!(" @{weight}"));
                    }
                    description
                })
                .collect();
            format!("{group}({})", children.join(", "))
        }
    }
}

/// Captures a saved workspace and summarizes the filled, validated draft as (focus, outline).
fn capture_outline(tree: Value) -> (String, String) {
    let config = parse_filled_draft(&capture_recorded_tree(tree).unwrap());
    assert!(config.layout.value.weight.is_none());
    (config.focus.unwrap(), layout_outline(&config.layout.value))
}

#[test]
fn single_window_is_a_root_leaf_with_explicit_focus() {
    let tree = workspace_tree(
        "splith",
        vec![wayland_window(20, Some("foot"), 1920, 1080)],
        20,
    );
    // As in Sway, the fixture workspace reports fullscreen_mode=1 without any fullscreen window.

    assert_eq!(tree["nodes"][0]["nodes"][0]["fullscreen_mode"], 1);

    let expected = "\
version: 1
focus: window1
layout:
  id: window1
  command: null
  match:
    app_id: foot
";
    assert_eq!(
        capture_recorded_tree(tree).unwrap(),
        format!("{DRAFT_HEADER}{expected}")
    );
}

#[test]
fn two_windows_record_axis_order_proportions_and_non_first_focus() {
    let horizontal = workspace_tree(
        "splith",
        vec![
            wayland_window(20, Some("foot"), 960, 1080),
            xwayland_window(21, "Firefox", 960, 1080),
        ],
        21,
    );
    let expected = "\
version: 1
focus: window2
layout:
  group: splith
  children:
    - id: window1
      weight: 50.0
      command: null
      match:
        app_id: foot
    - id: window2
      weight: 50.0
      command: null
      match:
        class: Firefox
";

    assert_eq!(
        capture_recorded_tree(horizontal).unwrap(),
        format!("{DRAFT_HEADER}{expected}")
    );

    // Vertical groups are measured by height; widths are irrelevant.
    let vertical = workspace_tree(
        "splitv",
        vec![
            wayland_window(20, Some("top"), 1920, 810),
            wayland_window(21, Some("bottom"), 700, 270),
        ],
        20,
    );
    assert_eq!(
        capture_outline(vertical),
        (
            "window1".into(),
            "v(window1 app_id=top @75, window2 app_id=bottom @25)".into()
        )
    );
}

#[test]
fn tabbed_and_stacked_roots_keep_order_focus_and_singletons_without_weights() {
    for (layout, symbol) in [("tabbed", "t"), ("stacked", "s")] {
        let desktop = workspace_tree(layout, vec![left_window(), right_window()], 21);
        let draft = capture_recorded_tree(desktop.clone()).unwrap();
        assert!(!draft_body(&draft).contains("weight:"), "{draft}");
        assert!(draft_body(&draft).contains(&format!("layout:\n  group: {layout}\n")));
        assert_eq!(
            capture_outline(desktop),
            (
                "window2".into(),
                format!("{symbol}(window1 app_id=left, window2 app_id=right)")
            )
        );
        assert_eq!(
            capture_outline(workspace_tree(layout, vec![left_window()], 20)),
            ("window1".into(), format!("{symbol}(window1 app_id=left)"))
        );
    }
}

#[test]
fn split_parents_weight_tabbed_and_stacked_groups_by_the_parent_axis() {
    for (layout, symbol, tab_weight, stack_weight) in
        [("splith", "h", 25, 75), ("splitv", "v", 75, 25)]
    {
        let desktop = workspace_tree(
            layout,
            vec![
                group(30, "tabbed", 400, 750, vec![left_window(), right_window()]),
                group(
                    31,
                    "stacked",
                    1200,
                    250,
                    vec![wayland_window(22, Some("c"), 0, 0)],
                ),
            ],
            21,
        );
        assert_eq!(
            capture_outline(desktop),
            (
                "window2".into(),
                format!(
                    "{symbol}(t(window1 app_id=left, window2 app_id=right) @{tab_weight}, \
                     s(window3 app_id=c) @{stack_weight})"
                )
            )
        );
    }
}

/// Mixed ancestry exercises weights on groups, omitted weights on nested groups, and
/// split sizing below both header layouts. Header children need no split-axis geometry.
fn mixed_native_groups() -> Value {
    let inner_split = group(
        33,
        "splith",
        0,
        0,
        vec![
            xwayland_window(23, "D", 300, 0),
            wayland_window(24, Some("e"), 100, 0),
        ],
    );
    let stack = group(
        32,
        "stacked",
        100,
        250,
        vec![wayland_window(22, None, 0, 0), inner_split],
    );
    let column = group(
        31,
        "splitv",
        0,
        0,
        vec![wayland_window(21, Some("b"), 100, 750), stack],
    );
    let tabs = group(
        30,
        "tabbed",
        600,
        1080,
        vec![wayland_window(20, Some("a"), 0, 0), column],
    );
    let singleton_stack = group(
        34,
        "stacked",
        1400,
        1080,
        vec![wayland_window(25, Some("f"), 0, 0)],
    );
    workspace_tree("splith", vec![tabs, singleton_stack], 24)
}

#[test]
fn mixed_native_groups_round_trip_through_the_configuration_validator() {
    assert_eq!(
        capture_outline(mixed_native_groups()),
        (
            "window5".into(),
            "h(t(window1 app_id=a, v(window2 app_id=b @75, \
             s(window3 app_id=filled, h(window4 class=D @75, window5 app_id=e @25)) @25)) @30, \
             s(window6 app_id=f) @70)"
                .into()
        )
    );
}

#[test]
fn singleton_split_wrappers_preserve_header_groups_and_enclosing_weights() {
    for (layout, symbol) in [("tabbed", "t"), ("stacked", "s")] {
        let meaningful_group = group(32, layout, 0, 0, vec![left_window()]);
        let wrapper = group(31, "splitv", 300, 100, vec![meaningful_group]);
        let outer_wrapper = group(30, "splith", 400, 100, vec![wrapper]);
        assert_eq!(
            capture_outline(workspace_tree("splitv", vec![outer_wrapper.clone()], 20)),
            ("window1".into(), format!("{symbol}(window1 app_id=left)"))
        );
        assert_eq!(
            capture_outline(workspace_tree(
                "splith",
                vec![outer_wrapper, right_window()],
                21
            )),
            (
                "window2".into(),
                format!("h({symbol}(window1 app_id=left) @29.41, window2 app_id=right @70.59)")
            )
        );
    }
}

#[test]
fn uneven_siblings_and_mixed_nested_groups_keep_their_proportions() {
    // The four-window reference arrangement: 44:32:24 widths and a 77:23 right column.
    let tree = workspace_tree(
        "splith",
        vec![
            wayland_window(20, Some("browser"), 1268, 1080),
            wayland_window(21, Some("middle"), 920, 1080),
            group(
                30,
                "splitv",
                692,
                1080,
                vec![
                    wayland_window(22, Some("top"), 692, 832),
                    wayland_window(23, Some("bottom"), 692, 248),
                ],
            ),
        ],
        20,
    );

    assert_eq!(
        capture_outline(tree),
        (
            "window1".into(),
            "h(window1 app_id=browser @44.03, window2 app_id=middle @31.94, \
             v(window3 app_id=top @77.04, window4 app_id=bottom @22.96) @24.03)"
                .into()
        )
    );
}

#[test]
fn deeper_and_same_axis_groups_stay_separate_meaningful_groups() {
    let innermost = group(
        32,
        "splitv",
        660,
        600,
        vec![
            wayland_window(22, Some("c"), 660, 300),
            wayland_window(23, Some("d"), 660, 300),
        ],
    );
    let same_axis = group(
        31,
        "splith",
        1320,
        600,
        vec![wayland_window(21, Some("b"), 660, 600), innermost],
    );
    let tree = workspace_tree(
        "splitv",
        vec![
            group(
                30,
                "splith",
                1920,
                600,
                vec![wayland_window(20, Some("a"), 600, 600), same_axis],
            ),
            wayland_window(24, Some("e"), 1920, 480),
        ],
        23,
    );

    assert_eq!(
        capture_outline(tree),
        (
            "window4".into(),
            "v(h(window1 app_id=a @31.25, h(window2 app_id=b @50, \
             v(window3 app_id=c @50, window4 app_id=d @50) @50) @68.75) @55.56, \
             window5 app_id=e @44.44)"
                .into()
        )
    );
}

#[test]
fn single_child_wrappers_collapse_and_keep_their_measured_extent() {
    // Wrapper 31's extent (1320) sizes the group it holds, whatever that group reports.
    let wrapped_group = group(
        31,
        "splitv",
        1320,
        1080,
        vec![group(
            32,
            "splith",
            1000,
            1080,
            vec![
                wayland_window(21, Some("b"), 660, 1080),
                wayland_window(22, Some("c"), 660, 1080),
            ],
        )],
    );
    let wrapped_window = group(
        30,
        "splitv",
        600,
        1080,
        vec![wayland_window(20, Some("a"), 600, 1080)],
    );

    assert_eq!(
        capture_outline(workspace_tree(
            "splith",
            vec![wrapped_window, wrapped_group],
            22
        )),
        (
            "window3".into(),
            "h(window1 app_id=a @31.25, h(window2 app_id=b @50, window3 app_id=c @50) @68.75)"
                .into()
        )
    );

    // A workspace's single child is the root, and a chain of wrappers can hold one window.
    let only_group = group(
        30,
        "splith",
        1920,
        1080,
        vec![
            wayland_window(20, Some("a"), 1000, 1080),
            wayland_window(21, Some("b"), 920, 1080),
        ],
    );
    assert_eq!(
        capture_outline(workspace_tree("splitv", vec![only_group], 21)).1,
        "h(window1 app_id=a @52.08, window2 app_id=b @47.92)"
    );
    let chain = group(
        30,
        "splitv",
        1920,
        1080,
        vec![group(
            31,
            "splith",
            1920,
            1080,
            vec![wayland_window(20, Some("a"), 1920, 1080)],
        )],
    );
    assert_eq!(
        capture_outline(workspace_tree("splith", vec![chain], 20)),
        ("window1".into(), "window1 app_id=a".into())
    );
}

#[test]
fn missing_identifiers_become_explicit_match_placeholders() {
    // Empty identifiers cannot select a window.
    let tree = workspace_tree(
        "splith",
        vec![
            wayland_window(20, Some("foot"), 480, 1080),
            xwayland_window(21, "Firefox", 480, 1080),
            wayland_window(22, Some(""), 480, 1080),
            xwayland_window(23, "", 480, 1080),
        ],
        20,
    );
    let draft = capture_recorded_tree(tree).unwrap();

    assert_eq!(draft_body(&draft).matches("command: null").count(), 4);
    assert_eq!(draft_body(&draft).matches("match: null").count(), 2);
    assert_eq!(
        layout_outline(&parse_filled_draft(&draft).layout.value),
        "h(window1 app_id=foot @25, window2 class=Firefox @25, \
         window3 app_id=filled @25, window4 app_id=filled @25)"
    );

    // check and load reject unfilled drafts with placeholder instructions.
    let error = config::parse(&draft).unwrap_err();
    assert!(
        error.contains("command: null is a capture placeholder"),
        "{error}"
    );
    let commands_only = draft.replace("command: null", "command: [app]");
    let error = config::parse(&commands_only).unwrap_err();
    assert!(
        error.contains("match: null is a capture placeholder"),
        "{error}"
    );
}

#[test]
fn unusual_identifiers_survive_serialization_and_strict_parsing_exactly() {
    let long_words = "word ".repeat(30);
    let identifiers = [
        "true",
        "False",
        "null",
        "~",
        "1",
        "-1.5e3",
        "0x1F",
        "0o17",
        ".inf",
        "NaN",
        "yes",
        "on",
        "2026-11-09",
        "1:20",
        "- x",
        "-",
        "? q",
        "a: b",
        "trailing:",
        "#c",
        "x #y",
        "[x]",
        "{y}",
        "a,b",
        "*a",
        "&b",
        "!t",
        "|",
        ">",
        "%p",
        "@",
        "`",
        "'q'",
        "\"dq\"",
        "---",
        "...",
        "<<",
        "=",
        " lead",
        "trail ",
        "tab\there",
        "new\nline",
        "cr\rlf",
        "nul\0",
        "del\u{7f}",
        "nel\u{85}",
        "ls\u{2028}",
        "bom\u{feff}",
        "back\\slash",
        "é雪🦀",
        long_words.as_str(),
    ];
    let windows = identifiers
        .iter()
        .enumerate()
        .map(|(index, identifier)| {
            let id = 100 + index as i64;
            if index % 2 == 0 {
                wayland_window(id, Some(*identifier), 10, 1080)
            } else {
                xwayland_window(id, identifier, 10, 1080)
            }
        })
        .collect();
    let draft = capture_recorded_tree(workspace_tree("splith", windows, 100)).unwrap();

    assert!(!draft_body(&draft).contains("match: null"), "{draft}");

    let config = parse_filled_draft(&draft);
    let NodeKind::Group { children, .. } = &config.layout.value.kind else {
        panic!("expected a root split");
    };
    let parsed: Vec<_> = children
        .iter()
        .map(|child| match &child.value.kind {
            NodeKind::Window {
                matcher: Matcher::AppId(value) | Matcher::Class(value),
                ..
            } => value.as_str(),
            NodeKind::Group { .. } => panic!("expected only windows"),
        })
        .collect();
    assert_eq!(parsed, identifiers);
}

#[test]
fn weights_are_positive_hundredths_that_reconstruct_shares_within_tolerance() {
    assert_eq!(
        percentage_weights(&[1268.0, 920.0, 692.0]),
        [44.03, 31.94, 24.03]
    );
    assert_eq!(percentage_weights(&[1.0, 1.0, 1.0]), [33.33, 33.33, 33.33]);
    assert_eq!(percentage_weights(&[1.0, 30_000.0]), [0.01, 100.0]);

    // Worst accumulated rounding: the most siblings a layout allows, all slivers but one.
    let mut extents = vec![1.0; MAX_LAYOUT_NODES - 2];
    extents.push(30_000.0);
    let reconstructed = proportions(&percentage_weights(&extents));
    for (observed, reconstructed) in proportions(&extents).iter().zip(reconstructed) {
        assert!(
            (observed - reconstructed).abs() <= 0.05,
            "{observed} -> {reconstructed}"
        );
    }
}

/// Splits nested so the captured layout is `levels` deep, focusing the deepest window.
fn nested(levels: usize) -> Value {
    let mut subtree = wayland_window(2000, Some("deepest"), 100, 100);
    for level in 1..levels as i64 {
        let sibling = wayland_window(2000 + level, Some("sibling"), 100, 100);
        subtree = group(1000 + level, "splith", 200, 100, vec![subtree, sibling]);
    }
    workspace_tree("splith", vec![subtree], 2000)
}

/// `count` windows side by side in the workspace's root split.
fn side_by_side(count: usize) -> Value {
    let windows = (0..count as i64)
        .map(|index| wayland_window(2000 + index, Some("tile"), 10, 100))
        .collect();
    workspace_tree("splith", windows, 2000)
}

#[test]
fn layout_format_limits_bound_captures() {
    assert!(capture_recorded_tree(nested(MAX_LAYOUT_DEPTH)).is_ok());
    let error = capture_recorded_tree(nested(MAX_LAYOUT_DEPTH + 1)).unwrap_err();
    assert!(
        error.contains("deeper than the layout format's 32"),
        "{error}"
    );

    assert!(capture_recorded_tree(side_by_side(MAX_LAYOUT_NODES - 1)).is_ok());
    let error = capture_recorded_tree(side_by_side(MAX_LAYOUT_NODES)).unwrap_err();
    assert!(
        error.contains("more than the layout format's 256"),
        "{error}"
    );
}

#[test]
fn preserved_header_groups_count_toward_depth_and_node_limits() {
    for levels in [MAX_LAYOUT_DEPTH, MAX_LAYOUT_DEPTH + 1] {
        let mut subtree = left_window();
        for level in 1..levels {
            let layout = if level % 2 == 0 { "tabbed" } else { "stacked" };
            subtree = group(1000 + level as i64, layout, 0, 0, vec![subtree]);
        }
        let result = capture_recorded_tree(workspace_tree("splith", vec![subtree], 20));
        if levels == MAX_LAYOUT_DEPTH {
            parse_filled_draft(&result.unwrap());
        } else {
            assert!(
                result
                    .unwrap_err()
                    .contains("deeper than the layout format")
            );
        }
    }

    let mut children = Vec::new();
    for child_index in 0..(MAX_LAYOUT_NODES - 2) / 2 {
        children.push(group(
            1000 + child_index as i64,
            "tabbed",
            0,
            0,
            vec![wayland_window(
                2000 + child_index as i64,
                Some("tile"),
                0,
                0,
            )],
        ));
    }
    children.push(left_window());
    parse_filled_draft(
        &capture_recorded_tree(workspace_tree("stacked", children.clone(), 20)).unwrap(),
    );
    children.push(right_window());
    let error = capture_recorded_tree(workspace_tree("stacked", children, 20)).unwrap_err();

    assert!(error.contains("more than the layout format's 256 windows and groups"));
}

#[test]
fn unrepresentable_arrangements_fail_without_a_draft() {
    let mut floating_child = workspace_tree("splith", vec![left_window()], 20);
    floating_child["nodes"][0]["nodes"][0]["floating_nodes"] =
        json!([node(30, "floating_con", false)]);
    let mut flagged = left_window();
    flagged["floating"] = json!("user_on");
    let mut fullscreen = left_window();
    fullscreen["fullscreen_mode"] = json!(1);
    let mut global_fullscreen = left_window();
    global_fullscreen["fullscreen_mode"] = json!(2);
    let mut unknown_fullscreen = left_window();
    unknown_fullscreen["fullscreen_mode"] = Value::Null;
    let mut fullscreen_group = group(
        30,
        "splitv",
        1920,
        1080,
        vec![left_window(), right_window()],
    );
    fullscreen_group["fullscreen_mode"] = json!(1);
    let mut window_with_children = left_window();
    window_with_children["nodes"] = json!([wayland_window(22, Some("inner"), 960, 1080)]);
    let column = group(30, "splitv", 960, 1080, vec![left_window(), right_window()]);

    #[rustfmt::skip]
    let cases = [
        (workspace_tree("splith", vec![], 10), "empty"),
        (floating_child, "1 floating container"),
        (workspace_tree("splith", vec![flagged, right_window()], 21), "floating"),
        (workspace_tree("splith", vec![node(30, "floating_con", true), right_window()], 30), "floating"),
        (workspace_tree("splith", vec![fullscreen, right_window()], 21), "fullscreen"),
        (workspace_tree("splith", vec![global_fullscreen, right_window()], 21), "fullscreen"),
        (workspace_tree("splith", vec![unknown_fullscreen, right_window()], 21), "fullscreen"),
        (workspace_tree("splith", vec![fullscreen_group], 20), "id=30 is fullscreen"),
        (workspace_tree("none", vec![left_window(), right_window()], 20), "unsupported layout"),
        (workspace_tree("splith", vec![node(30, "output", false), right_window()], 21), "unsupported Sway node type"),
        (workspace_tree("splith", vec![window_with_children, right_window()], 21), "holds child containers"),
        (workspace_tree("splith", vec![node(30, "con", false), right_window()], 21), "neither a window nor child"),
        (workspace_tree("splith", vec![wayland_window(20, Some("left"), 0, 1080), right_window()], 21), "unusable geometry"),
        (workspace_tree("splitv", vec![wayland_window(20, Some("top"), 1920, -1), right_window()], 21), "unusable geometry"),
        // `focus parent` selects a group, or eventually the workspace, rather than a window.
        (workspace_tree("splith", vec![column, wayland_window(22, Some("x"), 960, 1080)], 30), "focus is on container id=30"),
        (workspace_tree("splith", vec![left_window(), right_window()], 10), "focus is on container id=10"),
    ];
    for (tree, expected) in cases {
        let error = capture_recorded_tree(tree).unwrap_err();
        assert!(error.contains(expected), "expected {expected:?}: {error}");
    }
}

#[test]
fn header_groups_still_validate_their_contents_focus_and_enclosing_split_geometry() {
    for layout in ["tabbed", "stacked"] {
        let mut floating = left_window();
        floating["floating"] = json!("user_on");
        let mut fullscreen = group(30, layout, 100, 100, vec![left_window()]);
        fullscreen["fullscreen_mode"] = json!(1);
        let mut scratchpad = group(30, layout, 100, 100, vec![left_window()]);
        scratchpad["scratchpad_state"] = json!("changed");
        let malformed_window = {
            let mut window = left_window();
            window["nodes"] = json!([right_window()]);
            window
        };
        let invalid_split = group(
            30,
            "splitv",
            0,
            0,
            vec![wayland_window(20, Some("a"), 100, 0), right_window()],
        );

        #[rustfmt::skip]
        let cases = [
            (workspace_tree(layout, vec![group(30, layout, 0, 0, vec![])], 30), "neither a window nor child"),
            (workspace_tree(layout, vec![floating], 20), "floating"),
            (workspace_tree(layout, vec![fullscreen], 20), "fullscreen"),
            (workspace_tree(layout, vec![scratchpad], 20), "scratchpad"),
            (workspace_tree(layout, vec![node(30, "output", false)], 30), "unsupported Sway node type"),
            (workspace_tree(layout, vec![malformed_window], 20), "holds child containers"),
            (workspace_tree(layout, vec![invalid_split], 21), "unusable geometry"),
            (workspace_tree("splith", vec![group(30, layout, 0, 100, vec![left_window()]), right_window()], 21), "unusable geometry"),
            (workspace_tree(layout, vec![left_window(), right_window()], 10), "focus is on container id=10"),
            (workspace_tree("splith", vec![group(30, layout, 100, 100, vec![left_window()])], 30), "focus is on container id=30"),
            (workspace_tree(layout, vec![], 10), "empty"),
        ];
        for (desktop, expected) in cases {
            let error = capture_recorded_tree(desktop).unwrap_err();
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }
    }
}

#[test]
fn scratchpad_windows_and_groups_are_refused_even_when_visible_and_tiled() {
    for state in ["fresh", "changed"] {
        let mut scratchpad_window = left_window();
        scratchpad_window["scratchpad_state"] = json!(state);
        let mut scratchpad_group = group(
            30,
            "splith",
            1920,
            1080,
            vec![left_window(), right_window()],
        );
        scratchpad_group["scratchpad_state"] = json!(state);

        for container in [scratchpad_window, scratchpad_group] {
            let error =
                capture_recorded_tree(workspace_tree("splith", vec![container], 20)).unwrap_err();
            assert!(error.contains("belongs to the scratchpad"), "{error}");
        }
    }
}

#[test]
fn saved_snapshot_survives_desktop_changes_and_retains_output_ancestry() {
    let mut desktop = workspace_tree(
        "splith",
        vec![view_with_pid(20, json!(101)), view_with_pid(21, json!(102))],
        20,
    );
    desktop["nodes"][0]["name"] = json!("DP-1");
    desktop["nodes"][0]["nodes"][0]["name"] = json!("named-workspace");
    let saved = read_recorded_snapshot(desktop.clone()).unwrap();

    assert_eq!(saved.workspace_id(), 10);
    assert_eq!(saved.tree.nodes[0].name.as_deref(), Some("DP-1"));
    let original = saved.render(false).unwrap().yaml;

    // Simulate focus/workspace/output changes and closed, moved, or newly opened windows.
    // Rendering has no connection to ask for this later state, even during inference.
    desktop["nodes"] = json!([]);
    let mut requested_pids = Vec::new();
    let inferred = saved
        .render_with(true, &mut |pid| {
            requested_pids.push(pid);
            desktop = workspace_tree(
                "splith",
                vec![wayland_window(99, Some("new"), 100, 100)],
                99,
            );
            None
        })
        .unwrap();
    let inferred = inferred.yaml;
    assert_eq!(requested_pids, [101, 102]);
    assert_eq!(desktop["nodes"][0]["nodes"][0]["nodes"][0]["id"], 99);
    assert_eq!(saved.render(false).unwrap().yaml, original);
    assert_eq!(
        inferred,
        original.replacen(
            DRAFT_HEADER,
            &format!("{DRAFT_HEADER}{INFERENCE_HEADER}"),
            1
        )
    );
    assert!(original.contains("focus: window1"));
    assert!(original.contains("app_id: shared-app"));
}

#[test]
fn workspace_selection_uses_focused_ancestry_across_outputs_and_floating_nodes() {
    let mut desktop = workspace_tree("splith", vec![left_window()], 999);
    let mut second_workspace = node(77, "workspace", false);
    second_workspace["name"] = json!("named-workspace");
    let mut focused_window = right_window();
    focused_window["focused"] = json!(true);
    second_workspace["nodes"] = json!([focused_window]);
    let mut second_output = node(3, "output", false);
    second_output["name"] = json!("HDMI-A-1");
    second_output["nodes"] = json!([second_workspace]);
    desktop["nodes"].as_array_mut().unwrap().push(second_output);

    let saved = read_recorded_snapshot(desktop.clone()).unwrap();

    assert_eq!(saved.workspace_id(), 77);
    let draft = saved.render(false).unwrap().yaml;
    assert!(draft.contains("app_id: right"));
    assert!(!draft.contains("app_id: left"));

    let workspace_tree = &mut desktop["nodes"][1]["nodes"][0];
    workspace_tree["floating_nodes"] = workspace_tree["nodes"].take();
    workspace_tree["nodes"] = json!([]);
    let saved = read_recorded_snapshot(desktop).unwrap();
    assert_eq!(saved.workspace_id(), 77);
    assert!(saved.render(false).unwrap_err().contains("floating"));
}

#[test]
fn acquisition_allows_empty_and_unsupported_layouts_until_rendering() {
    for desktop in [
        workspace_tree("splith", vec![], 10),
        workspace_tree("none", vec![left_window(), right_window()], 20),
    ] {
        let saved = read_recorded_snapshot(desktop).unwrap();
        assert_eq!(saved.workspace_id(), 10);
        assert!(saved.render(false).is_err());
    }
}

#[test]
fn malformed_focus_and_container_identities_are_rejected() {
    let mut absent = workspace_tree("splith", vec![left_window()], 999);
    let mut ambiguous = workspace_tree("splith", vec![left_window(), right_window()], 20);
    ambiguous["nodes"][0]["nodes"][0]["nodes"][1]["focused"] = json!(true);
    let mut outside = workspace_tree("splith", vec![left_window()], 1);
    let mut duplicate = workspace_tree("splith", vec![left_window(), right_window()], 20);
    duplicate["nodes"][0]["nodes"][0]["nodes"][1]["id"] = json!(20);
    let mut invalid_root = workspace_tree("splith", vec![left_window()], 20);
    invalid_root["type"] = json!("output");
    for desktop in [
        &mut absent,
        &mut ambiguous,
        &mut outside,
        &mut duplicate,
        &mut invalid_root,
    ] {
        assert!(read_recorded_snapshot(desktop.clone()).is_err());
    }
}

#[test]
fn tree_query_deadline_cancels_stalled_and_fragmented_replies() {
    use std::io::Write;
    for partial in [false, true] {
        let (client, mut server) = socket_pair();
        let worker = thread::spawn(move || {
            read_request(&mut server, 4);
            if partial {
                let response = ipc_frame(4, b"{}");
                server.write_all(&response[..15]).unwrap();
            }
            // Wait for cancellation instead of completing the frame.
            assert_eq!(server.read(&mut [0]).unwrap(), 0);
        });
        let started = Instant::now();
        let error = snapshot_from_socket(client, started + Duration::from_millis(100))
            .err()
            .unwrap();
        assert!(error.contains("deadline"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
        worker.join().unwrap();
    }
}

#[test]
fn failed_tree_read_produces_no_draft() {
    let (client, mut server) = socket_pair();
    let worker = thread::spawn(move || {
        read_request(&mut server, 4);
    });
    let error = snapshot_from_socket(client, Instant::now() + IPC_TIMEOUT)
        .err()
        .unwrap();

    assert!(error.contains("get tree"), "{error}");
    worker.join().unwrap();
}

fn view_with_pid(id: i64, pid: Value) -> Value {
    let mut window = wayland_window(id, Some("shared-app"), 960, 1080);
    window["pid"] = pid;
    window
}

/// Remove only launch commands and comments when comparing inferred and ordinary drafts.
fn without_commands(draft: &str) -> String {
    let mut lines = Vec::new();
    let mut in_command = false;
    for line in draft.lines().filter(|line| !line.starts_with('#')) {
        if line.trim_start().starts_with("command:") {
            lines.push(format!(
                "{}command: null",
                " ".repeat(line.len() - line.trim_start().len())
            ));
            in_command = true;
        } else if in_command && line.trim_start().starts_with("- ") {
            continue;
        } else {
            in_command = false;
            lines.push(line.to_owned());
        }
    }
    lines.join("\n")
}

#[test]
fn inference_keeps_mixed_group_structure_weights_selectors_and_global_focus() {
    fn set_window_pids(container: &mut Value) {
        if container["shell"].is_string() {
            container["pid"] = json!(container["id"].as_i64().unwrap() + 100);
        }
        for child in container["nodes"].as_array_mut().unwrap() {
            set_window_pids(child);
        }
    }

    let mut desktop = mixed_native_groups();
    set_window_pids(&mut desktop);
    let saved = read_recorded_snapshot(desktop).unwrap();
    let ordinary = saved
        .render_with(false, &mut |_| {
            panic!("disabled inference reads no processes")
        })
        .unwrap();
    let mut requested_pids = Vec::new();
    let inferred = saved
        .render_with(true, &mut |pid| {
            requested_pids.push(pid);
            (pid != 122).then(|| vec!["app".into(), format!("pid-{pid}")])
        })
        .unwrap();

    assert_eq!(requested_pids, [120, 121, 122, 123, 124, 125]);
    assert_eq!(inferred.commands.total, 6);
    assert_eq!(inferred.commands.inferred, 5);
    assert_eq!(
        without_commands(&ordinary.yaml),
        without_commands(&inferred.yaml)
    );

    let ordinary_config = parse_filled_draft(&ordinary.yaml);
    let inferred_config = parse_filled_draft(&inferred.yaml);
    assert_eq!(inferred_config.focus, ordinary_config.focus);
    assert_eq!(
        layout_outline(&inferred_config.layout.value),
        layout_outline(&ordinary_config.layout.value)
    );

    let mut pending = vec![&inferred_config.layout.value];
    while let Some(layout_node) = pending.pop() {
        match &layout_node.kind {
            NodeKind::Group { children, .. } => {
                pending.extend(children.iter().map(|child| &child.value));
            }
            NodeKind::Window { id, command, .. } => {
                let window_number: i32 = id.strip_prefix("window").unwrap().parse().unwrap();
                let mut expected = vec!["app".to_owned()];
                if window_number != 3 {
                    expected.push(format!("pid-{}", 119 + window_number));
                }
                assert_eq!(command, &expected);
            }
        }
    }
}

#[test]
fn inference_preserves_window_association_structure_and_yaml_arguments() {
    let windows = [
        view_with_pid(20, json!(101)),
        view_with_pid(21, json!(102)),
        view_with_pid(22, json!(103)),
    ];
    let tree = workspace_tree(
        "splith",
        vec![
            windows[0].clone(),
            group(30, "splitv", 960, 1080, windows[1..].to_vec()),
        ],
        22,
    );
    let default = capture_recorded_tree(tree.clone()).unwrap();
    let commands = [
        vec![
            "foot",
            "--app-id",
            "one",
            "two words",
            "",
            "雪",
            "#x: [y]",
            "'\"",
            "\n",
        ],
        vec!["other", "--app-id=two", "null", "*alias"],
    ];
    let mut requested = Vec::new();
    let inferred = read_recorded_snapshot(tree)
        .unwrap()
        .render_with(true, &mut |pid| {
            requested.push(pid);
            match pid {
                101 | 102 => Some(
                    commands[(pid - 101) as usize]
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                ),
                103 => None,
                _ => panic!("unexpected PID"),
            }
        })
        .unwrap();
    let inferred = inferred.yaml;

    assert_eq!(requested, [101, 102, 103]);
    assert!(inferred.contains(INFERENCE_HEADER));
    assert!(inferred.contains("  children:\n    - id: window1\n"));
    assert!(inferred.contains("      children:\n        - id: window2\n"));
    assert!(!inferred.contains("children: ["));
    assert_eq!(inferred.matches("command: null").count(), 2); // Header plus failed leaf.

    let inferred_config = parse_filled_draft(&inferred);
    let default_config = parse_filled_draft(&default);
    assert_eq!(inferred_config.focus, default_config.focus);
    let NodeKind::Group { children, .. } = &inferred_config.layout.value.kind else {
        panic!("expected horizontal root");
    };
    let NodeKind::Group {
        children: nested, ..
    } = &children[1].value.kind
    else {
        panic!("expected nested vertical split");
    };
    for (child, arguments) in [(&children[0], &commands[0]), (&nested[0], &commands[1])] {
        let NodeKind::Window {
            command, matcher, ..
        } = &child.value.kind
        else {
            panic!("expected window");
        };
        assert_eq!(command, arguments);
        assert!(matches!(matcher, Matcher::AppId(app_id) if app_id == "shared-app"));
    }
    assert_eq!(without_commands(&inferred), without_commands(&default));
}

#[test]
fn absent_or_invalid_saved_pids_leave_null_without_reading_processes() {
    for pid in [Value::Null, json!(0), json!(-1)] {
        let desktop = workspace_tree("splith", vec![view_with_pid(20, pid)], 20);
        let draft = read_recorded_snapshot(desktop)
            .unwrap()
            .render_with(true, &mut |_| panic!("invalid PID must not be read"))
            .unwrap()
            .yaml;
        assert!(draft.contains("  command: null"));
    }
}

#[test]
fn desktop_launcher_command_serializes_as_three_literal_arguments() {
    let saved = read_recorded_snapshot(workspace_tree(
        "splith",
        vec![view_with_pid(20, json!(101))],
        20,
    ))
    .unwrap();
    let expected = vec![
        "gio".to_owned(),
        "launch".to_owned(),
        "/private/path with spaces/application.desktop".to_owned(),
    ];
    let draft = saved
        .render_with(true, &mut |pid| {
            assert_eq!(pid, 101);
            Some(expected.clone())
        })
        .unwrap();
    let parsed = parse_filled_draft(&draft.yaml);
    let NodeKind::Window { command, .. } = &parsed.layout.value.kind else {
        panic!("expected window");
    };

    assert_eq!(command, &expected);
    assert!(draft.yaml.contains("  command: [gio, launch,"));
    assert_eq!(draft.commands.inferred, 1);
    assert!(!draft.commands.summary().contains("/private"));
}

#[test]
fn command_styles_preserve_special_arguments_through_strict_parsing() {
    let saved = read_recorded_snapshot(workspace_tree(
        "splith",
        vec![view_with_pid(20, json!(101))],
        20,
    ))
    .unwrap();
    let special_arguments = [
        "",
        "two words",
        " lead",
        "trail ",
        "line\nbreak",
        "tab\there",
        "a,b",
        "[x]",
        "a: b",
        "#comment",
        "x #y",
        "'quote'",
        "\"quote\"",
        "back\\slash",
        "null",
        "true",
        "123",
        "*alias",
        "!tag",
        "$(touch /tmp/no); $HOME | & > * ? `cmd`",
        "雪🦀",
    ];
    for special in special_arguments {
        for use_block in [false, true] {
            let mut expected = vec!["app".to_owned(), special.to_owned()];
            if use_block {
                expected.push("long argument ".repeat(12));
            }
            let draft = saved
                .render_with(true, &mut |_| Some(expected.clone()))
                .unwrap();
            if use_block {
                assert!(
                    draft.yaml.contains("  command:\n    - app\n"),
                    "{}",
                    draft.yaml
                );
            } else {
                assert!(draft.yaml.contains("  command: [app, "), "{}", draft.yaml);
            }

            let parsed = config::parse(&draft.yaml).unwrap();
            let NodeKind::Window { command, .. } = &parsed.layout.value.kind else {
                panic!("expected window");
            };
            assert_eq!(command, &expected);
        }
    }
}

#[test]
fn command_line_limit_counts_yaml_escaping_and_indentation() {
    // A root command has two spaces of indentation; each nested child adds four more.
    for (levels, argument_length) in [(1, 102), (2, 98), (3, 94)] {
        let saved = read_recorded_snapshot(nested(levels)).unwrap();
        let captured = capture_workspace(&saved).unwrap();
        for extra in [0, 1] {
            let expected = vec!["app".to_owned(), "x".repeat(argument_length + extra)];
            let draft = render_draft(&captured, true, &mut |id| {
                (id == 2000).then(|| expected.clone())
            })
            .unwrap();
            let command_line = draft
                .yaml
                .lines()
                .find(|line| line.trim_start().starts_with("command:"))
                .unwrap();
            if extra == 0 {
                assert!(command_line.trim_start().starts_with("command: ["));
                assert_eq!(command_line.chars().count(), 120);
            } else {
                assert_eq!(command_line.trim(), "command:");
            }
            parse_filled_draft(&draft.yaml);
        }
    }

    let saved = read_recorded_snapshot(workspace_tree(
        "splith",
        vec![view_with_pid(20, json!(101))],
        20,
    ))
    .unwrap();
    let expected = vec!["app".to_owned(), "\n".repeat(60)];
    let draft = saved
        .render_with(true, &mut |_| Some(expected.clone()))
        .unwrap();

    assert!(draft.yaml.contains("  command:\n    - app\n"));
    let parsed = config::parse(&draft.yaml).unwrap();
    let NodeKind::Window { command, .. } = &parsed.layout.value.kind else {
        panic!("expected window");
    };
    assert_eq!(command, &expected);
}

#[test]
fn package_commands_serialize_literally_alongside_unresolved_commands() {
    let saved = read_recorded_snapshot(workspace_tree(
        "splith",
        vec![view_with_pid(20, json!(101)), view_with_pid(21, json!(102))],
        20,
    ))
    .unwrap();
    // The filesystem checks and single-element result are covered by AppImage fixtures.
    // Exercise serialization here with a path whose contents must stay entirely literal.
    let image_command = vec!["/private/雪 app: [one] '$HOME'.AppImage".to_owned()];
    let draft = saved
        .render_with(true, &mut |pid| {
            if pid == 101 {
                return Some(image_command.clone());
            }
            let environment = b"SNAP=unsupported\0SECRET=environment-only-secret\0BINARY=\xff\0";
            infer_host_command(
                pid,
                Ok(environment.as_slice()),
                || panic!("unsupported Snap needs no executable inspection"),
                || panic!("must not fall back to sandbox argv"),
            )
        })
        .unwrap();

    let parsed = parse_filled_draft(&draft.yaml);
    let NodeKind::Group { children, .. } = &parsed.layout.value.kind else {
        panic!("expected split");
    };
    for (child, expected) in children.iter().zip([
        image_command,
        vec!["app".into()], // filled() replaces only the unresolved placeholder.
    ]) {
        let NodeKind::Window {
            command, matcher, ..
        } = &child.value.kind
        else {
            panic!("expected window");
        };
        assert_eq!(command, &expected);
        assert!(matches!(matcher, Matcher::AppId(app_id) if app_id == "shared-app"));
    }

    assert_eq!(draft.yaml.matches("  command: null").count(), 1);
    assert_eq!(
        draft.commands,
        CommandCounts {
            total: 2,
            inferred: 1,
            inference_requested: true,
        }
    );
    assert_eq!(
        draft.commands.summary(),
        "Commands inferred: 1; unresolved: 1. Manual editing required."
    );
    assert!(!draft.commands.summary().contains("/private"));
    assert!(!draft.yaml.contains("SECRET"));
    assert!(!draft.yaml.contains("environment-only-secret"));
    assert!(!draft.commands.summary().contains("environment-only-secret"));
}

#[test]
fn flatpak_inference_preserves_pid_association_and_window_matchers_in_yaml() {
    let desktop = workspace_tree(
        "splith",
        vec![view_with_pid(20, json!(101)), view_with_pid(21, json!(102))],
        20,
    );
    let saved = read_recorded_snapshot(desktop).unwrap();
    let draft = saved
        .render_with(true, &mut |pid| {
            let metadata = match pid {
                101 => "[Application]\nname=com.mastermindzh.tidal-hifi",
                102 => {
                    "[Application]\nname=org.example.Other\n[Instance]\nbranch=beta\narch=aarch64"
                }
                _ => panic!("unexpected PID"),
            };
            infer_process_command(Ok(metadata.as_bytes()), || {
                panic!("sandbox argv must not be read")
            })
        })
        .unwrap();
    let parsed = parse_filled_draft(&draft.yaml);
    let NodeKind::Group { children, .. } = &parsed.layout.value.kind else {
        panic!("expected split")
    };
    for (child, expected) in children.iter().zip([
        vec!["flatpak", "run", "com.mastermindzh.tidal-hifi"],
        vec![
            "flatpak",
            "run",
            "--branch=beta",
            "--arch=aarch64",
            "org.example.Other",
        ],
    ]) {
        let NodeKind::Window {
            command, matcher, ..
        } = &child.value.kind
        else {
            panic!("expected window")
        };
        assert_eq!(*command, expected);
        assert!(matches!(matcher, Matcher::AppId(app_id) if app_id == "shared-app"));
    }
    let unresolved = saved
        .render_with(true, &mut |_| {
            infer_process_command(Ok("[Application]\nname=partial".as_bytes()), || None)
        })
        .unwrap()
        .yaml;

    assert_eq!(unresolved.matches("  command: null").count(), 2);
    let default = saved
        .render_with(false, &mut |_| {
            panic!("default must not inspect Flatpak metadata")
        })
        .unwrap()
        .yaml;
    assert_eq!(default.matches("  command: null").count(), 2);
}

#[test]
fn command_counts_follow_generated_leaves_independently_of_match_placeholders() {
    let mut unidentified = view_with_pid(22, json!(103));
    unidentified["app_id"] = Value::Null;
    let desktop = workspace_tree(
        "splith",
        vec![
            view_with_pid(20, json!(101)),
            view_with_pid(21, json!(102)),
            unidentified,
        ],
        20,
    );
    let saved = read_recorded_snapshot(desktop).unwrap();
    for (inference_requested, inferred_count) in [(false, 0), (true, 2), (true, 3)] {
        let rendered = saved
            .render_with(inference_requested, &mut |pid| {
                assert!(
                    inference_requested,
                    "disabled inference must not inspect processes"
                );
                if pid == 103 && inferred_count == 2 {
                    None
                } else {
                    Some(vec![
                        "sensitive-executable".into(),
                        "secret-argument".into(),
                    ])
                }
            })
            .unwrap();
        assert_eq!(
            rendered.commands,
            CommandCounts {
                total: 3,
                inferred: inferred_count,
                inference_requested,
            }
        );
        assert_eq!(rendered.commands.unresolved(), 3 - inferred_count);
        assert_eq!(
            rendered
                .yaml
                .lines()
                .filter(|line| line.trim() == "match: null")
                .count(),
            1
        );
        let summary = rendered.commands.summary();
        assert!(!summary.contains("sensitive-executable"));
        assert!(!summary.contains("secret-argument"));
        let expected = match inferred_count {
            0 => "Command inference disabled; unresolved commands: 3. Manual editing required.",
            2 => "Commands inferred: 2; unresolved: 1. Manual editing required.",
            3 => "Commands inferred: 3; unresolved: 0. Review best-effort commands before use.",
            _ => unreachable!(),
        };
        assert_eq!(summary, expected);
    }
}

#[test]
fn unusable_launcher_evidence_produces_a_valid_null_draft_and_unresolved_count() {
    let saved = read_recorded_snapshot(workspace_tree(
        "splith",
        vec![view_with_pid(20, json!(101))],
        20,
    ))
    .unwrap();
    let default = saved.render(false).unwrap();
    let rendered = saved
        .render_with(true, &mut |pid| {
            infer_host_command(
                pid,
                Ok(
                    b"GIO_LAUNCHED_DESKTOP_FILE_PID=101\0SECRET=environment-only-secret\0"
                        .as_slice(),
                ),
                || panic!("known unusable launcher blocks package inference"),
                || panic!("secret argv must not be consulted"),
            )
        })
        .unwrap();

    assert_eq!(
        rendered.yaml,
        default.yaml.replacen(
            DRAFT_HEADER,
            &format!("{DRAFT_HEADER}{INFERENCE_HEADER}"),
            1
        )
    );
    assert_eq!(rendered.commands.total, 1);
    assert_eq!(rendered.commands.inferred, 0);
    assert_eq!(rendered.commands.unresolved(), 1);
    assert_eq!(
        rendered.commands.summary(),
        "Commands inferred: 0; unresolved: 1. Manual editing required."
    );
    assert!(!rendered.yaml.contains("environment-only-secret"));
}
