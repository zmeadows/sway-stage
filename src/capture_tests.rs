use super::*;
use crate::config::{self, Config, LayoutNode, Matcher, NodeKind, Orientation};
use crate::observation::fixtures::{frame, node, pair, request, send, tree};
use serde_json::{Value, json};
use std::path::Path;
use std::thread;

/// A tiled Wayland view whose container rect is `width` by `height` pixels.
fn view(id: i64, app_id: Option<&str>, width: i32, height: i32) -> Value {
    let mut view = node(id, "con", false);
    view["shell"] = json!("xdg_shell");
    view["app_id"] = json!(app_id);
    view["rect"]["width"] = json!(width);
    view["rect"]["height"] = json!(height);
    view
}

/// A tiled Xwayland view: no app ID, an X11 window, and a class.
fn xwayland(id: i64, class: &str, width: i32, height: i32) -> Value {
    let mut view = view(id, None, width, height);
    view["shell"] = json!("xwayland");
    view["window"] = json!(4_000_000 + id);
    view["window_properties"] = json!({"class": class});
    view
}

fn group(id: i64, layout: &str, width: i32, height: i32, children: Vec<Value>) -> Value {
    let mut group = node(id, "con", false);
    group["layout"] = json!(layout);
    group["rect"]["width"] = json!(width);
    group["rect"]["height"] = json!(height);
    group["nodes"] = json!(children);
    group
}

/// A tree whose focused workspace (id 10) holds `children`, with only `focused_id` focused.
fn workspace(layout: &str, children: Vec<Value>, focused_id: i64) -> Value {
    fn focus_only(value: &mut Value, focused_id: i64) {
        value["focused"] = json!(value["id"] == focused_id);
        for child in value["nodes"].as_array_mut().unwrap() {
            focus_only(child, focused_id);
        }
    }
    let mut tree = tree();
    let workspace = &mut tree["nodes"][0]["nodes"][0];
    workspace["name"] = json!("1");
    workspace["layout"] = json!(layout);
    workspace["nodes"] = json!(children);
    focus_only(&mut tree, focused_id);
    tree
}

fn left() -> Value {
    view(20, Some("left"), 960, 1080)
}

fn right() -> Value {
    view(21, Some("right"), 960, 1080)
}

/// The server accepts exactly one GET_TREE, then verifies that capture disconnects.
fn saved_snapshot(tree: Value) -> Result<Snapshot, String> {
    let (client, mut server) = pair();
    let worker = thread::spawn(move || {
        assert!(request(&mut server, 4).is_empty());
        send(&mut server, 4, tree);
        assert_eq!(server.read(&mut [0]).unwrap(), 0);
    });
    let result = snapshot_from_socket(client, Instant::now() + IPC_TIMEOUT);
    worker.join().unwrap();
    result
}

fn capture_recorded(tree: Value) -> Result<String, String> {
    saved_snapshot(tree)?.render_with(false, &mut |_| panic!("default reads no processes"))
}

/// The YAML after the explanatory header, which itself mentions both placeholders.
fn body(draft: &str) -> &str {
    draft.strip_prefix(DRAFT_HEADER).expect("draft header")
}

/// Fills the placeholders as a user would, then applies the strict validator `check` uses.
fn filled(draft: &str) -> Config {
    let edited = draft
        .replace("command: null", "command: [app]")
        .replace("match: null", "match: {app_id: filled}");
    config::parse(&edited, Path::new("draft.yaml")).unwrap()
}

/// Splits as `h(...)` or `v(...)` with each child's weight after `@`; windows as
/// `id app_id=value` or `id class=value`.
fn outline(layout: &LayoutNode) -> String {
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
        NodeKind::Split {
            orientation,
            children,
        } => {
            let axis = match orientation {
                Orientation::Horizontal => "h",
                Orientation::Vertical => "v",
            };
            let children: Vec<_> = children
                .iter()
                .map(|child| format!("{} @{}", outline(&child.value), child.value.weight))
                .collect();
            format!("{axis}({})", children.join(", "))
        }
    }
}

/// Captures a saved workspace and summarizes the filled, validated draft as (focus, outline).
fn capture_outline(tree: Value) -> (String, String) {
    let config = filled(&capture_recorded(tree).unwrap());
    (config.focus.unwrap(), outline(&config.layout.value))
}

#[test]
fn single_window_is_a_root_leaf_with_explicit_focus() {
    let tree = workspace("splith", vec![view(20, Some("foot"), 1920, 1080)], 20);
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
        capture_recorded(tree).unwrap(),
        format!("{DRAFT_HEADER}{expected}")
    );
}

#[test]
fn two_windows_record_axis_order_proportions_and_non_first_focus() {
    let horizontal = workspace(
        "splith",
        vec![
            view(20, Some("foot"), 960, 1080),
            xwayland(21, "Firefox", 960, 1080),
        ],
        21,
    );
    let expected = "\
version: 1
focus: window2
layout:
  split: horizontal
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
        capture_recorded(horizontal).unwrap(),
        format!("{DRAFT_HEADER}{expected}")
    );

    // Vertical groups are measured by height; widths are irrelevant.
    let vertical = workspace(
        "splitv",
        vec![
            view(20, Some("top"), 1920, 810),
            view(21, Some("bottom"), 700, 270),
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
fn uneven_siblings_and_mixed_nested_groups_keep_their_proportions() {
    // The four-window reference arrangement: 44:32:24 widths and a 77:23 right column.
    let tree = workspace(
        "splith",
        vec![
            view(20, Some("browser"), 1268, 1080),
            view(21, Some("middle"), 920, 1080),
            group(
                30,
                "splitv",
                692,
                1080,
                vec![
                    view(22, Some("top"), 692, 832),
                    view(23, Some("bottom"), 692, 248),
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
        vec![view(22, Some("c"), 660, 300), view(23, Some("d"), 660, 300)],
    );
    let same_axis = group(
        31,
        "splith",
        1320,
        600,
        vec![view(21, Some("b"), 660, 600), innermost],
    );
    let tree = workspace(
        "splitv",
        vec![
            group(
                30,
                "splith",
                1920,
                600,
                vec![view(20, Some("a"), 600, 600), same_axis],
            ),
            view(24, Some("e"), 1920, 480),
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
                view(21, Some("b"), 660, 1080),
                view(22, Some("c"), 660, 1080),
            ],
        )],
    );
    let wrapped_window = group(
        30,
        "splitv",
        600,
        1080,
        vec![view(20, Some("a"), 600, 1080)],
    );
    assert_eq!(
        capture_outline(workspace("splith", vec![wrapped_window, wrapped_group], 22)),
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
            view(20, Some("a"), 1000, 1080),
            view(21, Some("b"), 920, 1080),
        ],
    );
    assert_eq!(
        capture_outline(workspace("splitv", vec![only_group], 21)).1,
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
            vec![view(20, Some("a"), 1920, 1080)],
        )],
    );
    assert_eq!(
        capture_outline(workspace("splith", vec![chain], 20)),
        ("window1".into(), "window1 app_id=a".into())
    );
}

#[test]
fn missing_identifiers_become_explicit_match_placeholders() {
    // Empty identifiers cannot select a window.
    let tree = workspace(
        "splith",
        vec![
            view(20, Some("foot"), 480, 1080),
            xwayland(21, "Firefox", 480, 1080),
            view(22, Some(""), 480, 1080),
            xwayland(23, "", 480, 1080),
        ],
        20,
    );
    let draft = capture_recorded(tree).unwrap();
    assert_eq!(body(&draft).matches("command: null").count(), 4);
    assert_eq!(body(&draft).matches("match: null").count(), 2);
    assert_eq!(
        outline(&filled(&draft).layout.value),
        "h(window1 app_id=foot @25, window2 class=Firefox @25, \
         window3 app_id=filled @25, window4 app_id=filled @25)"
    );

    // check and load reject unfilled drafts with placeholder instructions.
    let error = config::parse(&draft, Path::new("draft.yaml")).unwrap_err();
    assert!(
        error.contains("command: null is a capture placeholder"),
        "{error}"
    );
    let commands_only = draft.replace("command: null", "command: [app]");
    let error = config::parse(&commands_only, Path::new("draft.yaml")).unwrap_err();
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
                view(id, Some(*identifier), 10, 1080)
            } else {
                xwayland(id, identifier, 10, 1080)
            }
        })
        .collect();
    let draft = capture_recorded(workspace("splith", windows, 100)).unwrap();
    assert!(!body(&draft).contains("match: null"), "{draft}");

    let config = filled(&draft);
    let NodeKind::Split { children, .. } = &config.layout.value.kind else {
        panic!("expected a root split");
    };
    let parsed: Vec<_> = children
        .iter()
        .map(|child| match &child.value.kind {
            NodeKind::Window {
                matcher: Matcher::AppId(value) | Matcher::Class(value),
                ..
            } => value.as_str(),
            NodeKind::Split { .. } => panic!("expected only windows"),
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
    let mut subtree = view(2000, Some("deepest"), 100, 100);
    for level in 1..levels as i64 {
        let sibling = view(2000 + level, Some("sibling"), 100, 100);
        subtree = group(1000 + level, "splith", 200, 100, vec![subtree, sibling]);
    }
    workspace("splith", vec![subtree], 2000)
}

/// `count` windows side by side in the workspace's root split.
fn side_by_side(count: usize) -> Value {
    let windows = (0..count as i64)
        .map(|index| view(2000 + index, Some("tile"), 10, 100))
        .collect();
    workspace("splith", windows, 2000)
}

#[test]
fn layout_format_limits_bound_captures() {
    assert!(capture_recorded(nested(MAX_LAYOUT_DEPTH)).is_ok());
    let error = capture_recorded(nested(MAX_LAYOUT_DEPTH + 1)).unwrap_err();
    assert!(
        error.contains("deeper than the layout format's 32"),
        "{error}"
    );

    assert!(capture_recorded(side_by_side(MAX_LAYOUT_NODES - 1)).is_ok());
    let error = capture_recorded(side_by_side(MAX_LAYOUT_NODES)).unwrap_err();
    assert!(
        error.contains("more than the layout format's 256"),
        "{error}"
    );
}

#[test]
fn unrepresentable_arrangements_fail_without_a_draft() {
    let mut floating_child = workspace("splith", vec![left()], 20);
    floating_child["nodes"][0]["nodes"][0]["floating_nodes"] =
        json!([node(30, "floating_con", false)]);
    let mut flagged = left();
    flagged["floating"] = json!("user_on");
    let mut fullscreen = left();
    fullscreen["fullscreen_mode"] = json!(1);
    let mut global_fullscreen = left();
    global_fullscreen["fullscreen_mode"] = json!(2);
    let mut unknown_fullscreen = left();
    unknown_fullscreen["fullscreen_mode"] = Value::Null;
    let mut fullscreen_group = group(30, "splitv", 1920, 1080, vec![left(), right()]);
    fullscreen_group["fullscreen_mode"] = json!(1);
    let mut window_with_children = left();
    window_with_children["nodes"] = json!([view(22, Some("inner"), 960, 1080)]);
    let stacked_group = group(30, "stacked", 960, 1080, vec![left(), right()]);
    // A single tab still shows a tab bar, so even a one-child tabbed group is refused.
    let single_tab = group(30, "tabbed", 1920, 1080, vec![left()]);
    let column = group(30, "splitv", 960, 1080, vec![left(), right()]);

    #[rustfmt::skip]
    let cases = [
        (workspace("splith", vec![], 10), "empty"),
        (floating_child, "1 floating container"),
        (workspace("splith", vec![flagged, right()], 21), "floating"),
        (workspace("splith", vec![node(30, "floating_con", true), right()], 30), "floating"),
        (workspace("splith", vec![fullscreen, right()], 21), "fullscreen"),
        (workspace("splith", vec![global_fullscreen, right()], 21), "fullscreen"),
        (workspace("splith", vec![unknown_fullscreen, right()], 21), "fullscreen"),
        (workspace("splith", vec![fullscreen_group], 20), "id=30 is fullscreen"),
        (workspace("tabbed", vec![left(), right()], 20), "tabbed or stacked"),
        (workspace("splith", vec![stacked_group, view(22, Some("x"), 960, 1080)], 20), "tabbed or stacked"),
        (workspace("splith", vec![single_tab], 20), "tabbed or stacked"),
        (workspace("splith", vec![node(30, "output", false), right()], 21), "unsupported Sway node type"),
        (workspace("splith", vec![window_with_children, right()], 21), "holds child containers"),
        (workspace("splith", vec![node(30, "con", false), right()], 21), "neither a window nor child"),
        (workspace("splith", vec![view(20, Some("left"), 0, 1080), right()], 21), "unusable geometry"),
        (workspace("splitv", vec![view(20, Some("top"), 1920, -1), right()], 21), "unusable geometry"),
        // `focus parent` selects a group, or eventually the workspace, rather than a window.
        (workspace("splith", vec![column, view(22, Some("x"), 960, 1080)], 30), "focus is on container id=30"),
        (workspace("splith", vec![left(), right()], 10), "focus is on container id=10"),
    ];
    for (tree, expected) in cases {
        let error = capture_recorded(tree).unwrap_err();
        assert!(error.contains(expected), "expected {expected:?}: {error}");
    }
}

#[test]
fn scratchpad_windows_and_groups_are_refused_even_when_visible_and_tiled() {
    for state in ["fresh", "changed"] {
        let mut scratchpad_window = left();
        scratchpad_window["scratchpad_state"] = json!(state);
        let mut scratchpad_group = group(30, "splith", 1920, 1080, vec![left(), right()]);
        scratchpad_group["scratchpad_state"] = json!(state);

        for container in [scratchpad_window, scratchpad_group] {
            let error = capture_recorded(workspace("splith", vec![container], 20)).unwrap_err();
            assert!(error.contains("belongs to the scratchpad"), "{error}");
        }
    }
}

#[test]
fn saved_snapshot_survives_desktop_changes_and_retains_output_ancestry() {
    let mut desktop = workspace(
        "splith",
        vec![view_with_pid(20, json!(101)), view_with_pid(21, json!(102))],
        20,
    );
    desktop["nodes"][0]["name"] = json!("DP-1");
    desktop["nodes"][0]["nodes"][0]["name"] = json!("named-workspace");
    let saved = saved_snapshot(desktop.clone()).unwrap();
    assert_eq!(saved.workspace_id(), 10);
    assert_eq!(saved.tree.nodes[0].name.as_deref(), Some("DP-1"));
    let original = saved.render(false).unwrap();

    // Simulate focus/workspace/output changes and closed, moved, or newly opened windows.
    // Rendering has no connection to ask for this later state, even during inference.
    desktop["nodes"] = json!([]);
    let mut requested_pids = Vec::new();
    let inferred = saved
        .render_with(true, &mut |pid| {
            requested_pids.push(pid);
            desktop = workspace("splith", vec![view(99, Some("new"), 100, 100)], 99);
            None
        })
        .unwrap();
    assert_eq!(requested_pids, [101, 102]);
    assert_eq!(desktop["nodes"][0]["nodes"][0]["nodes"][0]["id"], 99);
    assert_eq!(saved.render(false).unwrap(), original);
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
    let mut desktop = workspace("splith", vec![left()], 999);
    let mut second_workspace = node(77, "workspace", false);
    second_workspace["name"] = json!("named-workspace");
    let mut focused_window = right();
    focused_window["focused"] = json!(true);
    second_workspace["nodes"] = json!([focused_window]);
    let mut second_output = node(3, "output", false);
    second_output["name"] = json!("HDMI-A-1");
    second_output["nodes"] = json!([second_workspace]);
    desktop["nodes"].as_array_mut().unwrap().push(second_output);

    let saved = saved_snapshot(desktop.clone()).unwrap();
    assert_eq!(saved.workspace_id(), 77);
    let draft = saved.render(false).unwrap();
    assert!(draft.contains("app_id: right"));
    assert!(!draft.contains("app_id: left"));

    let workspace = &mut desktop["nodes"][1]["nodes"][0];
    workspace["floating_nodes"] = workspace["nodes"].take();
    workspace["nodes"] = json!([]);
    let saved = saved_snapshot(desktop).unwrap();
    assert_eq!(saved.workspace_id(), 77);
    assert!(saved.render(false).unwrap_err().contains("floating"));
}

#[test]
fn acquisition_allows_empty_and_unsupported_layouts_until_rendering() {
    for desktop in [
        workspace("splith", vec![], 10),
        workspace("tabbed", vec![left(), right()], 20),
    ] {
        let saved = saved_snapshot(desktop).unwrap();
        assert_eq!(saved.workspace_id(), 10);
        assert!(saved.render(false).is_err());
    }
}

#[test]
fn malformed_focus_and_container_identities_are_rejected() {
    let mut absent = workspace("splith", vec![left()], 999);
    let mut ambiguous = workspace("splith", vec![left(), right()], 20);
    ambiguous["nodes"][0]["nodes"][0]["nodes"][1]["focused"] = json!(true);
    let mut outside = workspace("splith", vec![left()], 1);
    let mut duplicate = workspace("splith", vec![left(), right()], 20);
    duplicate["nodes"][0]["nodes"][0]["nodes"][1]["id"] = json!(20);
    let mut invalid_root = workspace("splith", vec![left()], 20);
    invalid_root["type"] = json!("output");
    for desktop in [
        &mut absent,
        &mut ambiguous,
        &mut outside,
        &mut duplicate,
        &mut invalid_root,
    ] {
        assert!(saved_snapshot(desktop.clone()).is_err());
    }
}

#[test]
fn tree_query_deadline_cancels_stalled_and_fragmented_replies() {
    use std::io::Write;
    for partial in [false, true] {
        let (client, mut server) = pair();
        let worker = thread::spawn(move || {
            request(&mut server, 4);
            if partial {
                let response = frame(4, b"{}");
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
    let (client, mut server) = pair();
    let worker = thread::spawn(move || {
        request(&mut server, 4);
    });
    let error = snapshot_from_socket(client, Instant::now() + IPC_TIMEOUT)
        .err()
        .unwrap();
    assert!(error.contains("get tree"), "{error}");
    worker.join().unwrap();
}

#[test]
fn command_line_reader_preserves_literal_arguments_and_empty_arguments() {
    let arguments = [
        "foot",
        "--app-id",
        "two words",
        "'quotes'\"",
        "雪🦀",
        "#secret: [x]",
        "*alias",
        "true",
        "null",
        "line\nbreak",
        "",
        "",
    ];
    let bytes = arguments.join("\0") + "\0";
    assert_eq!(read_command_line(bytes.as_bytes()).unwrap(), arguments);
    assert_eq!(read_command_line(&b"foot\0"[..]).unwrap(), ["foot"]);
}

#[test]
fn command_line_reader_rejects_unusable_or_incomplete_data_and_bounds_reads() {
    for bytes in [
        &b""[..],
        &b"\0"[..],
        &b"\0argument\0"[..],
        &b"foot"[..],
        &b"foot\0unfinished"[..],
        &b"foot\0\xff\0"[..],
        &b"\xff\0"[..],
    ] {
        assert!(read_command_line(bytes).is_none());
    }

    let mut maximum = vec![b'x'; MAX_CMDLINE_BYTES];
    maximum[MAX_CMDLINE_BYTES - 1] = 0;
    assert!(read_command_line(maximum.as_slice()).is_some());
    maximum.push(0);
    assert!(read_command_line(maximum.as_slice()).is_none());

    let mut endless = std::io::repeat(b'x').take(MAX_CMDLINE_BYTES as u64 + 10);
    assert!(read_command_line(&mut endless).is_none());
    // The bounded reader stops even if the source has no EOF.
    assert_eq!(endless.limit(), 9);
}

#[test]
fn process_read_failures_are_silent_inference_misses() {
    struct Unavailable(std::io::ErrorKind);
    impl Read for Unavailable {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                self.0,
                "sensitive argument must not escape",
            ))
        }
    }
    for kind in [
        std::io::ErrorKind::NotFound,
        std::io::ErrorKind::PermissionDenied,
    ] {
        assert!(read_command_line(Unavailable(kind)).is_none());
    }
    for pid in [i32::MIN, -1, 0] {
        assert!(read_process_command(pid).is_none());
    }
}

fn view_with_pid(id: i64, pid: Value) -> Value {
    let mut window = view(id, Some("shared-app"), 960, 1080);
    window["pid"] = pid;
    window
}

#[test]
fn inference_preserves_window_association_structure_and_yaml_arguments() {
    let windows = [
        view_with_pid(20, json!(101)),
        view_with_pid(21, json!(102)),
        view_with_pid(22, json!(103)),
    ];
    let tree = workspace(
        "splith",
        vec![
            windows[0].clone(),
            group(30, "splitv", 960, 1080, windows[1..].to_vec()),
        ],
        22,
    );
    let default = capture_recorded(tree.clone()).unwrap();
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
    let inferred = saved_snapshot(tree)
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
    assert_eq!(requested, [101, 102, 103]);
    assert!(inferred.contains(INFERENCE_HEADER));
    assert_eq!(inferred.matches("command: null").count(), 2); // Header plus failed leaf.

    let inferred_config = filled(&inferred);
    let default_config = filled(&default);
    assert_eq!(inferred_config.focus, default_config.focus);
    let NodeKind::Split { children, .. } = &inferred_config.layout.value.kind else {
        panic!("expected horizontal root");
    };
    let NodeKind::Split {
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
    // Ignore only the command values and added comment in a structural comparison.
    let normalize = |draft: &str| {
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
    };
    assert_eq!(normalize(&inferred), normalize(&default));
}

#[test]
fn absent_or_invalid_saved_pids_leave_null_without_reading_processes() {
    for pid in [Value::Null, json!(0), json!(-1)] {
        let desktop = workspace("splith", vec![view_with_pid(20, pid)], 20);
        let draft = saved_snapshot(desktop)
            .unwrap()
            .render_with(true, &mut |_| panic!("invalid PID must not be read"))
            .unwrap();
        assert!(draft.contains("  command: null"));
    }
}

#[test]
fn flatpak_metadata_builds_host_commands_from_application_and_instance_fields() {
    let application = "# generated by Flatpak\n[Application]\nname=com.mastermindzh.tidal-hifi\n\
                       runtime=org.freedesktop.Platform/x86_64/24.08\n";
    assert_eq!(
        flatpak_command(application).unwrap(),
        ["flatpak", "run", "com.mastermindzh.tidal-hifi"]
    );
    let instance = format!(
        "{application}[Instance]\nbranch=beta\narch=x86_64\n\
         extra-args=--filesystem=host;--env=EXAMPLE=value;\n"
    );
    assert_eq!(
        flatpak_command(&instance).unwrap(),
        [
            "flatpak",
            "run",
            "--branch=beta",
            "--arch=x86_64",
            "com.mastermindzh.tidal-hifi"
        ]
    );
    for (field, option) in [
        ("branch=stable", "--branch=stable"),
        ("arch=aarch64", "--arch=aarch64"),
    ] {
        assert_eq!(
            flatpak_command(&format!("{application}[Instance]\n{field}\n")).unwrap(),
            ["flatpak", "run", option, "com.mastermindzh.tidal-hifi"]
        );
    }
    // Old or unrelated fields must not be confused with the current [Instance] fields.
    assert_eq!(
        flatpak_command(&format!("{application}branch=ignored\narch=ignored\n")).unwrap(),
        ["flatpak", "run", "com.mastermindzh.tidal-hifi"]
    );
}

#[test]
fn flatpak_metadata_rejects_missing_ambiguous_and_malformed_identity() {
    for metadata in [
        "",
        "[Instance]\narch=x86_64",
        "[Runtime]\nname=org.example.Runtime",
        "[Application]\nruntime=org.example.Runtime/x86_64/stable",
        "[Application]\nname=",
        "[Application\nname=org.example.App",
        "name=org.example.App",
        "[Application]\nname=org.example.App\ninvalid line",
        "[Application]\nname=org.example.App\nname=org.example.Other",
        "[Application]\nname=org.example.App\n[Application]\nname=org.example.Other",
        "[Application]\nname=org.example.App\n[Runtime]\nname=org.example.Runtime",
        "[Application]\nname=org.example.App\0",
    ] {
        assert!(flatpak_command(metadata).is_none(), "{metadata:?}");
    }
    for application_id in [
        "org.example",
        "org..App",
        ".org.example.App",
        "org.example.App.",
        "org.1example.App",
        "org.example.1App",
        "org.ex-ample.App",
        "--command=sh",
        "org.example.App;sh",
        "org.example.$(sh)",
        "org.example.App/extra",
        "org.example.App --command=sh",
        "org.example.App\\sargument",
        "org.example.雪",
    ] {
        assert!(flatpak_command(&format!("[Application]\nname={application_id}")).is_none());
    }
    assert!(!valid_flatpak_id(&format!(
        "org.example.{}",
        "x".repeat(244)
    )));
    assert!(valid_flatpak_id(&format!(
        "org.example.{}",
        "x".repeat(243)
    )));
    assert!(valid_flatpak_id("org.example._App-2"));

    for field in ["branch", "arch"] {
        for invalid in ["", "--command=sh", "x/y", "x y", "x;sh", "x\\sy", "$(sh)"] {
            let metadata =
                format!("[Application]\nname=org.example.App\n[Instance]\n{field}={invalid}");
            assert!(flatpak_command(&metadata).is_none());
        }
        let duplicate =
            format!("[Application]\nname=org.example.App\n[Instance]\n{field}=a\n{field}=b");
        assert!(flatpak_command(&duplicate).is_none());
    }
}

#[test]
fn flatpak_metadata_reads_are_bounded_and_fail_closed() {
    let mut metadata = b"[Application]\nname=org.example.App\n#".to_vec();
    metadata.resize(MAX_FLATPAK_INFO_BYTES, b'x');
    assert!(read_flatpak_command(metadata.as_slice()).is_some());
    metadata.push(b'x');
    assert!(read_flatpak_command(metadata.as_slice()).is_none());
    assert!(read_flatpak_command(&b"[Application]\nname=org.example.\xff"[..]).is_none());

    let mut endless = io::repeat(b'x').take(MAX_FLATPAK_INFO_BYTES as u64 + 10);
    assert!(read_flatpak_command(&mut endless).is_none());
    assert_eq!(endless.limit(), 9);
    struct Unreadable;
    impl Read for Unreadable {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::PermissionDenied.into())
        }
    }
    assert!(read_flatpak_command(Unreadable).is_none());
    for kind in [
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::NotADirectory,
        io::ErrorKind::Other,
    ] {
        let metadata: io::Result<&[u8]> = Err(kind.into());
        assert!(
            infer_process_command(metadata, || panic!("uncertain identity must not read argv"))
                .is_none()
        );
    }
    let missing: io::Result<&[u8]> = Err(io::ErrorKind::NotFound.into());
    assert_eq!(
        infer_process_command(missing, || read_command_line(
            &b"host-app\0two words\0\0"[..]
        ))
        .unwrap(),
        ["host-app", "two words", ""]
    );
    for metadata in [
        "[Application]\nname=org.example.App",
        "[Application]\nname=",
        "[Runtime]\nname=org.example.Runtime",
    ] {
        // Even unusable metadata must suppress sandbox-internal /app argv.
        let _ = infer_process_command(Ok(metadata.as_bytes()), || {
            panic!("must not reuse /app argv")
        });
    }
}

#[test]
fn flatpak_inference_preserves_pid_association_and_window_matchers_in_yaml() {
    let desktop = workspace(
        "splith",
        vec![view_with_pid(20, json!(101)), view_with_pid(21, json!(102))],
        20,
    );
    let saved = saved_snapshot(desktop).unwrap();
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
    let parsed = filled(&draft);
    let NodeKind::Split { children, .. } = &parsed.layout.value.kind else {
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
            flatpak_command("[Application]\nname=partial")
        })
        .unwrap();
    assert_eq!(unresolved.matches("  command: null").count(), 2);
    let default = saved
        .render_with(false, &mut |_| {
            panic!("default must not inspect Flatpak metadata")
        })
        .unwrap();
    assert_eq!(default.matches("  command: null").count(), 2);
}
