//! Constructed Sway data for the offline tests, in the shape `swaymsg -r` reports it. Tree
//! nodes carry the fields Sway serializes, including ones the harness ignores.

use std::collections::HashSet;
use std::fmt::Debug;

use serde_json::{Value, json};

use crate::ipc::{TreeNode, WindowEvent, Workspace};
use crate::observation::{Failure, walk};
use crate::ownership::AcceptedWindows;
use crate::scenarios::{Scenario, extended_scenarios, standard_scenarios};

pub const RUN_ID: &str = "0123456789abcdef01234567";
pub const FOOT: &str = "/usr/bin/foot";
pub const SLEEP: &str = "/usr/bin/sleep";

pub const ROOT_ID: i64 = 1;
pub const SCRATCHPAD_WORKSPACE_ID: i64 = 3;
pub const OTHER_WORKSPACE_ID: i64 = 5;
/// A window on the other workspace that existed before the run, such as the user's browser.
pub const PREEXISTING_WINDOW_ID: i64 = 6;
pub const TARGET_WORKSPACE_ID: i64 = 7;

pub fn standard_scenario(name: &str) -> Scenario {
    standard_scenarios(RUN_ID, FOOT, SLEEP)
        .into_iter()
        .find(|scenario| scenario.name == name)
        .expect("a standard scenario name")
}

pub fn extended_scenario(name: &str) -> Scenario {
    extended_scenarios(RUN_ID, FOOT, SLEEP)
        .into_iter()
        .find(|scenario| scenario.name == name)
        .expect("an extended scenario name")
}

/// The selector value configured for one of the scenario's windows.
pub fn selector_value(scenario: &Scenario, layout_id: &str) -> String {
    let leaves = scenario.layout.leaves();
    let leaf = leaves
        .iter()
        .find(|leaf| leaf.layout_id == layout_id)
        .expect("a layout ID in the scenario");
    leaf.selector.value().to_owned()
}

pub fn accepted(windows: &[(i64, &str)]) -> AcceptedWindows {
    windows
        .iter()
        .map(|&(container_id, layout_id)| (container_id, layout_id.to_owned()))
        .collect()
}

pub fn rect(x: i32, y: i32, width: i32, height: i32) -> Value {
    json!({"x": x, "y": y, "width": width, "height": height})
}

/// A structural container with every field Sway serializes for one.
pub fn container(
    id: i64,
    node_type: &str,
    layout: &str,
    rect: Value,
    children: Vec<Value>,
) -> Value {
    let empty = json!({"x": 0, "y": 0, "width": 0, "height": 0});
    json!({
        "id": id, "type": node_type, "name": null, "layout": layout,
        "orientation": if layout == "splitv" { "vertical" } else { "horizontal" },
        "percent": null, "urgent": false, "marks": [], "focused": false, "focus": [],
        "border": "none", "current_border_width": 0, "sticky": false,
        "rect": rect, "window_rect": empty, "deco_rect": empty, "geometry": empty,
        "window": null, "nodes": children, "floating_nodes": [],
        // Sway reports 1 on every workspace, independently of window fullscreen state.
        "fullscreen_mode": if node_type == "workspace" { 1 } else { 0 },
    })
}

pub fn group(id: i64, layout: &str, rect: Value, children: Vec<Value>) -> Value {
    container(id, "con", layout, rect, children)
}

/// Fixture PIDs exceed Linux's maximum PID, 2^22, so they never name a real process.
pub const UNUSED_PID_BASE: i64 = 5_000_000;

/// A tiled native Wayland view.
pub fn wayland_window(id: i64, app_id: &str, rect: Value) -> Value {
    let mut window = container(id, "con", "none", rect, Vec::new());
    window["name"] = json!("a title, which is never ownership evidence");
    window["app_id"] = json!(app_id);
    window["shell"] = json!("xdg_shell");
    window["pid"] = json!(UNUSED_PID_BASE + id);
    window["visible"] = json!(true);
    window["inhibit_idle"] = json!(false);
    window["idle_inhibitors"] = json!({"user": "none", "application": "none"});
    window["max_render_time"] = json!(0);
    window["floating"] = json!("auto_off");
    window["scratchpad_state"] = json!("none");
    window
}

/// A tiled Xwayland view: no app ID, an X11 window, and a `WM_CLASS` class.
pub fn xwayland_window(id: i64, class: &str, rect: Value) -> Value {
    let mut window = wayland_window(id, "", rect);
    window["app_id"] = Value::Null;
    window["shell"] = json!("xwayland");
    window["window"] = json!(8_000_000 + id);
    window["window_properties"] = json!({
        "class": class, "instance": "xterm", "title": "xterm", "transient_for": null
    });
    window
}

fn workspace(id: i64, name: &str, layout: &str, children: Vec<Value>) -> Value {
    let mut workspace = container(id, "workspace", layout, rect(0, 0, 1920, 1080), children);
    workspace["name"] = json!(name);
    workspace["num"] = json!(name.parse::<i32>().unwrap_or(-1));
    workspace["output"] = json!("DP-1");
    workspace["representation"] = Value::Null;
    workspace
}

/// A desktop of one 1920x1080 output with workspace "1", holding a preexisting Firefox
/// window, and target workspace "9" with `target_layout` and `target_children`; plus
/// Sway's hidden scratchpad output. Only the container `focused_id` is focused.
pub fn desktop(target_layout: &str, target_children: Vec<Value>, focused_id: i64) -> Value {
    let mut scratchpad = workspace(
        SCRATCHPAD_WORKSPACE_ID,
        "__i3_scratch",
        "splith",
        Vec::new(),
    );
    scratchpad["rect"] = rect(0, 0, 0, 0);
    let mut scratchpad_output = container(
        2,
        "output",
        "output",
        rect(0, 0, 1920, 1080),
        vec![scratchpad],
    );
    scratchpad_output["name"] = json!("__i3");

    let preexisting = wayland_window(PREEXISTING_WINDOW_ID, "firefox-esr", rect(0, 0, 1920, 1080));
    let other = workspace(OTHER_WORKSPACE_ID, "1", "splith", vec![preexisting]);
    let target = workspace(TARGET_WORKSPACE_ID, "9", target_layout, target_children);
    let mut output = container(
        4,
        "output",
        "output",
        rect(0, 0, 1920, 1080),
        vec![other, target],
    );
    output["name"] = json!("DP-1");

    let mut root = container(
        ROOT_ID,
        "root",
        "splith",
        rect(0, 0, 1920, 1080),
        vec![scratchpad_output, output],
    );
    root["name"] = json!("root");
    set_focus(&mut root, focused_id);
    root
}

/// Marks only the container `focused_id` as focused.
pub fn set_focus(node: &mut Value, focused_id: i64) {
    node["focused"] = json!(node["id"] == focused_id);
    for children in ["nodes", "floating_nodes"] {
        for child in node[children].as_array_mut().expect("a child list") {
            set_focus(child, focused_id);
        }
    }
}

/// The node with `id` anywhere in a JSON tree, for modifying a desktop fixture.
pub fn node_mut(node: &mut Value, id: i64) -> &mut Value {
    try_node_mut(node, id).expect("a container ID in the fixture")
}

fn try_node_mut(node: &mut Value, id: i64) -> Option<&mut Value> {
    if node["id"] == id {
        return Some(node);
    }
    // One borrow of the object covers both child lists.
    let fields = node.as_object_mut()?;
    fields
        .iter_mut()
        .filter(|(key, _)| matches!(key.as_str(), "nodes" | "floating_nodes"))
        .flat_map(|(_, children)| children.as_array_mut().into_iter().flatten())
        .find_map(|child| try_node_mut(child, id))
}

pub fn parse_tree(tree: Value) -> TreeNode {
    serde_json::from_value(tree).expect("a fixture tree in Sway's format")
}

pub fn find(tree: &TreeNode, id: i64) -> &TreeNode {
    walk(tree)
        .into_iter()
        .map(|placed| placed.node)
        .find(|node| node.id == id)
        .expect("a container ID in the tree")
}

/// Every container ID in a tree: what a snapshot before launching would record.
pub fn container_ids(tree: &TreeNode) -> HashSet<i64> {
    walk(tree)
        .into_iter()
        .map(|placed| placed.node.id)
        .collect()
}

/// The `get_workspaces` reply that agrees with `desktop`, focused on `focused_name`.
pub fn workspaces_reply(focused_name: &str) -> Vec<Workspace> {
    let reply = json!([
        {"id": OTHER_WORKSPACE_ID, "num": 1, "name": "1", "focused": focused_name == "1",
         "visible": focused_name == "1", "urgent": false, "output": "DP-1"},
        {"id": TARGET_WORKSPACE_ID, "num": 9, "name": "9", "focused": focused_name == "9",
         "visible": focused_name == "9", "urgent": false, "output": "DP-1"},
    ]);
    serde_json::from_value(reply).expect("a workspace reply in Sway's format")
}

/// A window event as `swaymsg -m` prints it. An app ID may be absent until the client
/// sets it.
pub fn window_event(change: &str, container_id: i64, app_id: Option<&str>) -> WindowEvent {
    let event = json!({
        "change": change,
        "container": {
            "id": container_id, "type": "con", "name": "title", "app_id": app_id,
            "pid": 4321, "shell": "xdg_shell", "focused": false, "nodes": [],
        },
    });
    serde_json::from_value(event).expect("a window event in Sway's format")
}

/// The message of a failure that must be `Uncertain`.
pub fn uncertain_message<T: Debug>(result: Result<T, Failure>) -> String {
    match result {
        Err(Failure::Uncertain(message)) => message,
        other => panic!("expected an uncertain failure, got {other:?}"),
    }
}

/// The message of a failure that must be an ordinary `Check` failure.
pub fn check_message<T: Debug>(result: Result<T, Failure>) -> String {
    match result {
        Err(Failure::Check(message)) => message,
        other => panic!("expected an ordinary check failure, got {other:?}"),
    }
}

pub fn assert_contains(text: &str, expected: &str) {
    assert!(text.contains(expected), "expected {expected:?} in:\n{text}");
}
