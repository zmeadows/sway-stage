//! Independent verification of an observed workspace against a configured scenario:
//! window identities, visual order, split hierarchy, focus, and rectangle-based shares.
//!
//! The checks read raw `get_tree` data and never use sway-stage's own hierarchy, sizing,
//! or capture code, so a defect there cannot confirm itself.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::ipc::{Floating, Layout, NodeType, Rect, ScratchpadState, Shell, TreeNode};
use crate::observation::{Failure, walk};
use crate::ownership::AcceptedWindows;
use crate::scenarios::{
    ConfiguredNode, Scenario, Selector, SplitAxis, configuration_yaml, describe_path,
};

/// Deviations above this many percentage points are reported: sway-stage's sizing target.
const SIZING_TARGET_POINTS: f64 = 1.0;
/// Deviations above this many percentage points fail: sway-stage's sizing tolerance.
const SIZING_TOLERANCE_POINTS: f64 = 5.0;
/// Keeps floating-point error in share arithmetic from crossing either threshold.
const SHARE_EPSILON: f64 = 1e-9;

/// Sway describes every view with a shell and, depending on it, an app ID or X11 window.
pub fn is_window(node: &TreeNode) -> bool {
    node.app_id.is_some() || node.shell.is_some() || node.window.is_some()
}

/// The Xwayland `WM_CLASS` class, when Sway reports one.
pub fn class(node: &TreeNode) -> Option<&str> {
    node.window_properties.as_ref()?.class.as_deref()
}

/// The identifier of the selector's own kind: an app ID never satisfies a class selector,
/// nor a class an app ID selector.
pub fn observed_identifier<'a>(node: &'a TreeNode, selector: &Selector) -> Option<&'a str> {
    match selector {
        Selector::AppId(_) => node.app_id.as_deref(),
        Selector::Class(_) => class(node),
    }
}

/// A compact diagnostic outline: `id:app_id`, `id:class`, or `id:layout`, then tiled and
/// floating children in parentheses. Titles and process arguments are never included.
pub fn outline(node: &TreeNode) -> String {
    let label = [node.app_id.as_deref(), class(node)]
        .into_iter()
        .flatten()
        .find(|identifier| !identifier.is_empty())
        .unwrap_or(layout_name(node.layout));
    let children: Vec<String> = node
        .nodes
        .iter()
        .chain(&node.floating_nodes)
        .map(outline)
        .collect();

    if children.is_empty() {
        format!("{}:{label}", node.id)
    } else {
        format!("{}:{label}({})", node.id, children.join(", "))
    }
}

/// Sway's own name for a layout, as `swaymsg` prints it.
fn layout_name(layout: Layout) -> &'static str {
    match layout {
        Layout::None => "none",
        Layout::SplitH => "splith",
        Layout::SplitV => "splitv",
        Layout::Stacked => "stacked",
        Layout::Tabbed => "tabbed",
        Layout::Output => "output",
    }
}

/// The identity-bearing hierarchy of a workspace. Windows keep their identities; groups
/// keep their container IDs and axes. Only validated single-child wrappers disappear.
#[derive(Debug, PartialEq, Eq)]
pub enum Shape {
    Window {
        container_id: i64,
        app_id: Option<String>,
        class: Option<String>,
        pid: Option<i32>,
    },
    Group {
        container_id: i64,
        layout: Layout,
        children: Vec<Shape>,
    },
}

/// Validates every container under a workspace and returns its shape. Every failure is
/// uncertain: a floating, fullscreen, scratchpad, tabbed, stacked, or malformed container
/// means something other than this run has changed the workspace.
pub fn shape(node: &TreeNode) -> Result<Shape, Failure> {
    let is_workspace = node.node_type == NodeType::Workspace;
    if !is_workspace && node.node_type != NodeType::Con {
        return Err(Failure::Uncertain(format!(
            "unexpected container type: {}",
            outline(node)
        )));
    }

    let floating = matches!(node.floating, Some(Floating::AutoOn | Floating::UserOn));
    let in_scratchpad = !matches!(node.scratchpad_state, None | Some(ScratchpadState::None));
    // Sway reports fullscreen_mode 1 on every workspace, so only containers carry it.
    let fullscreen = !is_workspace && node.fullscreen_mode != Some(0);
    if !node.floating_nodes.is_empty() || floating || in_scratchpad || fullscreen {
        return Err(Failure::Uncertain(format!(
            "floating/fullscreen/scratchpad state: {}",
            outline(node)
        )));
    }

    if is_window(node) {
        if is_workspace || !node.nodes.is_empty() {
            return Err(Failure::Uncertain(format!(
                "malformed window: {}",
                outline(node)
            )));
        }
        return Ok(Shape::Window {
            container_id: node.id,
            app_id: node.app_id.clone(),
            class: class(node).map(str::to_owned),
            pid: node.pid,
        });
    }

    if !matches!(node.layout, Layout::SplitH | Layout::SplitV) {
        return Err(Failure::Uncertain(format!(
            "unsupported group: {}",
            outline(node)
        )));
    }
    let mut children = Vec::new();
    for child in &node.nodes {
        children.push(shape(child)?);
    }

    // A single child fills its container, which therefore has no visible boundary.
    if children.len() == 1 {
        return Ok(children.remove(0));
    }
    Ok(Shape::Group {
        container_id: node.id,
        layout: node.layout,
        children,
    })
}

/// The shape Sway should leave after closing window `closing_id`: a group left with one
/// child is replaced by that child, and a group left empty disappears.
pub fn remove_from_shape(shape: Shape, closing_id: i64) -> Option<Shape> {
    match shape {
        Shape::Window { container_id, .. } if container_id == closing_id => None,
        Shape::Window { .. } => Some(shape),
        Shape::Group {
            container_id,
            layout,
            children,
        } => {
            let mut remaining_children: Vec<Shape> = children
                .into_iter()
                .filter_map(|child| remove_from_shape(child, closing_id))
                .collect();
            match remaining_children.len() {
                0 => None,
                1 => Some(remaining_children.remove(0)),
                _ => Some(Shape::Group {
                    container_id,
                    layout,
                    children: remaining_children,
                }),
            }
        }
    }
}

/// Everything about a workspace that capture must leave unchanged: identities, geometry,
/// actual wrappers, state flags, and current focus. Titles, urgency, marks, and focus
/// history are not modeled, so they may change.
#[derive(Debug, PartialEq, Eq)]
pub struct Signature {
    container_id: i64,
    node_type: NodeType,
    layout: Layout,
    app_id: Option<String>,
    class: Option<String>,
    pid: Option<i32>,
    x11_window: Option<i64>,
    shell: Option<Shell>,
    rect: Rect,
    focused: bool,
    floating: Option<Floating>,
    fullscreen_mode: Option<u8>,
    scratchpad_state: Option<ScratchpadState>,
    nodes: Vec<Signature>,
    floating_nodes: Vec<Signature>,
}

pub fn signature(node: &TreeNode) -> Signature {
    Signature {
        container_id: node.id,
        node_type: node.node_type,
        layout: node.layout,
        app_id: node.app_id.clone(),
        class: class(node).map(str::to_owned),
        pid: node.pid,
        x11_window: node.window,
        shell: node.shell,
        rect: node.rect,
        focused: node.focused,
        floating: node.floating,
        fullscreen_mode: node.fullscreen_mode,
        scratchpad_state: node.scratchpad_state,
        nodes: node.nodes.iter().map(signature).collect(),
        floating_nodes: node.floating_nodes.iter().map(signature).collect(),
    }
}

/// Measurements of a verified arrangement.
#[derive(Debug)]
pub struct LayoutMeasurements {
    /// Each non-root node's measured percentage of its sibling group's extent, keyed by
    /// child-index path from the root: `[2, 0]` is the first child of the third child.
    pub shares: BTreeMap<Vec<usize>, f64>,
    /// Splits more than 1 percentage point from their weights, but within 5.
    pub sizing_notes: Vec<String>,
}

/// Verifies that `workspace` holds exactly the accepted windows in the scenario's visual
/// order, split hierarchy, and axes, focused on the configured window, with every sibling
/// share within 5 percentage points of its weight. Unusable geometry and sizing are
/// `Check` failures; every other failure is `Uncertain`.
pub fn verify_layout(
    workspace: &TreeNode,
    scenario: &Scenario,
    accepted: &AcceptedWindows,
) -> Result<LayoutMeasurements, Failure> {
    // Validate every container, including wrappers the comparison below steps through.
    shape(workspace)?;

    let placed_nodes = walk(workspace);
    let observed_window_ids: Vec<i64> = placed_nodes
        .iter()
        .map(|placed| placed.node)
        .filter(|node| is_window(node))
        .map(|node| node.id)
        .collect();
    let observed_id_set: BTreeSet<i64> = observed_window_ids.iter().copied().collect();
    let accepted_id_set: BTreeSet<i64> = accepted.keys().copied().collect();
    if observed_id_set != accepted_id_set {
        return Err(Failure::Uncertain(format!(
            "unexpected windows: {}",
            outline(workspace)
        )));
    }

    let container_ids: HashMap<&str, i64> = accepted
        .iter()
        .map(|(container_id, layout_id)| (layout_id.as_str(), *container_id))
        .collect();
    let mut expected_window_ids = Vec::new();
    for leaf in scenario.layout.leaves() {
        let Some(container_id) = container_ids.get(leaf.layout_id.as_str()) else {
            return Err(Failure::Uncertain(format!(
                "no accepted window for {}",
                leaf.layout_id
            )));
        };
        expected_window_ids.push(*container_id);
    }
    if observed_window_ids != expected_window_ids {
        return Err(Failure::Uncertain(format!(
            "window order differs: {}",
            outline(workspace)
        )));
    }

    let Some(&focus_id) = container_ids.get(scenario.focus) else {
        return Err(Failure::Uncertain(format!(
            "no accepted window for focus {}",
            scenario.focus
        )));
    };
    let focused_ids: Vec<i64> = placed_nodes
        .iter()
        .filter(|placed| placed.node.focused)
        .map(|placed| placed.node.id)
        .collect();
    if focused_ids != [focus_id] {
        return Err(Failure::Uncertain(format!(
            "final focus differs: expected {} id={focus_id}, observed {}",
            scenario.focus,
            outline(workspace)
        )));
    }

    let mut measurements = LayoutMeasurements {
        shares: BTreeMap::new(),
        sizing_notes: Vec::new(),
    };
    let comparison = compare_node(
        &scenario.layout,
        workspace,
        &mut Vec::new(),
        &container_ids,
        &mut measurements,
    );

    if let Err(failure) = comparison {
        let context = format!(
            "\nExpected layout:\n{}Observed: {}",
            configuration_yaml(scenario),
            outline(workspace)
        );
        return Err(match failure {
            Failure::Check(message) => Failure::Check(message + &context),
            Failure::Uncertain(message) => Failure::Uncertain(message + &context),
        });
    }
    Ok(measurements)
}

/// Compares a configured node with the observed container in its place, first stepping
/// through single-child wrappers, which `shape` has already validated. `path` holds the
/// configured node's child indexes from the root.
fn compare_node(
    configured: &ConfiguredNode,
    observed: &TreeNode,
    path: &mut Vec<usize>,
    container_ids: &HashMap<&str, i64>,
    measurements: &mut LayoutMeasurements,
) -> Result<(), Failure> {
    let mut observed = observed;
    while !is_window(observed) && observed.nodes.len() == 1 {
        observed = &observed.nodes[0];
    }
    let location = describe_path(path);

    let (axis, configured_children) = match configured {
        ConfiguredNode::Window(window) => {
            let expected_id = container_ids.get(window.layout_id.as_str());
            if !is_window(observed) || expected_id != Some(&observed.id) {
                return Err(Failure::Uncertain(format!(
                    "{location}: expected {}, observed {}",
                    window.layout_id,
                    outline(observed)
                )));
            }
            return Ok(());
        }
        ConfiguredNode::Split { axis, children, .. } => (*axis, children),
    };

    let expected_layout = axis.sway_layout();
    if is_window(observed)
        || observed.layout != expected_layout
        || observed.nodes.len() != configured_children.len()
    {
        return Err(Failure::Uncertain(format!(
            "{location}: expected {} with {} children, observed {}",
            layout_name(expected_layout),
            configured_children.len(),
            outline(observed)
        )));
    }

    let rects: Vec<Rect> = observed.nodes.iter().map(|child| child.rect).collect();
    let spans: Vec<(i32, i32)> = rects.iter().map(|rect| span(rect, axis)).collect();
    if spans.iter().any(|&(_, extent)| extent <= 0) {
        return Err(Failure::Check(format!(
            "{location}: unusable rectangles {rects:?}"
        )));
    }
    for adjacent in spans.windows(2) {
        let (previous_position, previous_extent) = adjacent[0];
        let (next_position, _) = adjacent[1];
        if i64::from(previous_position) + i64::from(previous_extent) > i64::from(next_position) {
            return Err(Failure::Uncertain(format!(
                "{location}: visual order/overlap differs: {rects:?}"
            )));
        }
    }

    // Shares are percentages of the sibling sums, independently of gaps and borders.
    let total_weight: f64 = configured_children
        .iter()
        .map(|child| f64::from(child.weight()))
        .sum();
    let total_extent: f64 = spans.iter().map(|&(_, extent)| f64::from(extent)).sum();
    let requested_shares: Vec<f64> = configured_children
        .iter()
        .map(|child| 100.0 * f64::from(child.weight()) / total_weight)
        .collect();
    let measured_shares: Vec<f64> = spans
        .iter()
        .map(|&(_, extent)| 100.0 * f64::from(extent) / total_extent)
        .collect();
    let max_deviation = requested_shares
        .iter()
        .zip(&measured_shares)
        .map(|(requested, measured)| (measured - requested).abs())
        .fold(0.0, f64::max);

    let detail = format!(
        "{location} {}: expected [{}], observed [{}] percent; container={}",
        layout_name(expected_layout),
        format_shares(&requested_shares),
        format_shares(&measured_shares),
        observed.id
    );
    if max_deviation > SIZING_TOLERANCE_POINTS + SHARE_EPSILON {
        return Err(Failure::Check(format!(
            "sizing exceeds 5 percentage points (check application minimum sizes): {detail}"
        )));
    }
    if max_deviation > SIZING_TARGET_POINTS + SHARE_EPSILON {
        measurements.sizing_notes.push(detail);
    }

    let child_pairs = configured_children.iter().zip(&observed.nodes);
    for (child_index, (configured_child, observed_child)) in child_pairs.enumerate() {
        path.push(child_index);
        measurements
            .shares
            .insert(path.clone(), measured_shares[child_index]);
        compare_node(
            configured_child,
            observed_child,
            path,
            container_ids,
            measurements,
        )?;
        path.pop();
    }
    Ok(())
}

/// A rectangle's position and extent along a split axis.
fn span(rect: &Rect, axis: SplitAxis) -> (i32, i32) {
    match axis {
        SplitAxis::Horizontal => (rect.x, rect.width),
        SplitAxis::Vertical => (rect.y, rect.height),
    }
}

fn format_shares(shares: &[f64]) -> String {
    let formatted: Vec<String> = shares.iter().map(|share| format!("{share:.2}")).collect();
    formatted.join(", ")
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
