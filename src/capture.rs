//! Read-only capture of the focused workspace's tiled arrangement as an editable YAML draft.
//!
//! Capture observes only through `Observer`: workspace and tree queries plus tick fences.
//! It never sends a Sway command, launches anything, or writes a file, and it records
//! arrangement and window selectors, never launch commands.

use crate::config::{MAX_LAYOUT_DEPTH, MAX_LAYOUT_NODES};
use crate::observation::{
    self, Observer, Snapshot, app_id, class, proportions, sibling_dimensions,
};
use serde::Serialize;
use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use swayipc::{Event, Floating, Node, NodeLayout, NodeType, WindowChange};

// Bounds the subscription and each fenced observation, as for load.
const IPC_TIMEOUT: Duration = Duration::from_secs(2);

const DRAFT_HEADER: &str = "\
# Layout draft from `sway-stage capture`. Before it can run, replace every
# `command: null` with the executable and arguments that open that window, and any
# `match: null` with exactly one exact `app_id` or `class`. Weights are percentages of
# the enclosing split. Validate the edited file with `sway-stage check NAME`.
";

/// Observes the focused workspace and returns the complete draft document. Nothing is
/// printed here, so a failed capture cannot leave partial YAML on stdout.
pub fn capture() -> Result<String, String> {
    let socket = observation::sway_socket()?;
    let mut connect = || observation::connect(&socket);
    capture_with(&mut connect)
}

// Concrete socket injection keeps all fixture traffic disconnected from socket discovery.
fn capture_with(connect: &mut dyn FnMut() -> Result<UnixStream, String>) -> Result<String, String> {
    let mut observer = Observer::subscribe(connect()?, Instant::now() + IPC_TIMEOUT)?;
    let initial = observer.snapshot(connect()?, Instant::now() + IPC_TIMEOUT)?;
    reject_window_activity(&initial)?;
    let captured = capture_workspace(&initial)?;

    // Sway has no transactional read. A second fenced observation must find the same
    // arrangement with no window activity since subscribing. The observer itself fails if
    // workspace focus left the target at any point, even if it has since returned.
    let confirming = observer.snapshot(connect()?, Instant::now() + IPC_TIMEOUT)?;
    reject_window_activity(&confirming)?;
    if capture_workspace(&confirming)? != captured {
        return Err("the workspace changed during capture; retry once it is idle".into());
    }

    render_draft(&captured)
}

/// The observed facts a draft is generated from, which both observations must agree on.
/// Distinct from the configured `LayoutNode`: it holds Sway container IDs and pixel extents.
#[derive(Debug, PartialEq)]
struct CapturedWorkspace {
    root: CapturedNode,
    /// Sway's focused container, which must be one of the captured windows.
    focused_id: i64,
}

#[derive(Debug, PartialEq)]
enum CapturedNode {
    Window {
        container_id: i64,
        selector: Option<Selector>,
    },
    Split {
        container_id: i64,
        axis: SplitAxis,
        /// Child extents along `axis` in pixels, parallel to `children`.
        extents: Vec<f64>,
        children: Vec<CapturedNode>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum SplitAxis {
    Horizontal,
    Vertical,
}

/// An exact window selector, serialized as the single key of a `match` mapping.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Selector {
    AppId(String),
    Class(String),
}

/// Window events carry no workspace ancestry, so any change that could alter what capture
/// records makes the observation uncertain. Titles, urgency hints, and marks are not
/// captured; a shell setting its terminal title must not prevent a capture.
fn reject_window_activity(snapshot: &Snapshot) -> Result<(), String> {
    for event in &snapshot.events {
        if let Event::Window(window) = event
            && !matches!(
                window.change,
                WindowChange::Title | WindowChange::Urgent | WindowChange::Mark
            )
        {
            return Err(format!(
                "window activity ({:?} id={}) during capture; retry once windows are idle",
                window.change, window.container.id
            ));
        }
    }
    Ok(())
}

/// Converts the target workspace's tiled hierarchy, refusing anything the layout format
/// cannot represent rather than approximating it.
fn capture_workspace(snapshot: &Snapshot) -> Result<CapturedWorkspace, String> {
    let workspace = snapshot
        .tree
        .find_as_ref(|node| node.id == snapshot.target)
        .ok_or("target workspace missing from tree")?;
    let name = workspace.name.as_deref().unwrap_or_default();
    let in_workspace = |error: String| format!("workspace {name:?}: {error}");

    if workspace.nodes.is_empty() && workspace.floating_nodes.is_empty() {
        return Err(in_workspace("empty; there is nothing to capture".into()));
    }
    if !workspace.floating_nodes.is_empty() {
        return Err(in_workspace(format!(
            "{} floating container(s); the layout format has only tiled windows, so tile or move them and retry",
            workspace.floating_nodes.len()
        )));
    }

    // Sway reports fullscreen_mode=1 on every workspace for i3 compatibility. That is not
    // window state; capture_node rejects fullscreen windows and groups individually.
    let mut node_count = 0;
    let root = capture_group(workspace, 1, &mut node_count).map_err(in_workspace)?;

    // The observer verified that exactly one node in the target workspace is focused.
    let focused = workspace
        .find_as_ref(|node| node.focused)
        .ok_or("no focused container in target workspace")?;
    Ok(CapturedWorkspace {
        root,
        focused_id: focused.id,
    })
}

/// Captures the workspace or a structural container from its tiled children. A group with
/// one child has no visible boundary of its own, since the child fills it, so the child
/// replaces it; the group's extent was already measured among its own siblings.
fn capture_group(
    group: &Node,
    depth: usize,
    node_count: &mut usize,
) -> Result<CapturedNode, String> {
    // Checked even for a single child: a one-tab group still shows its tab bar.
    let axis = match group.layout {
        NodeLayout::SplitH => SplitAxis::Horizontal,
        NodeLayout::SplitV => SplitAxis::Vertical,
        NodeLayout::Tabbed | NodeLayout::Stacked => {
            return Err(format!(
                "container id={} is tabbed or stacked, which the layout format cannot represent; switch it to `layout splith` or `layout splitv` and retry",
                group.id
            ));
        }
        layout => {
            return Err(format!(
                "container id={} has unsupported layout {layout:?}",
                group.id
            ));
        }
    };

    match group.nodes.as_slice() {
        [] => Err(format!(
            "container id={} holds neither a window nor child containers",
            group.id
        )),
        [only_child] => capture_node(only_child, depth, node_count),
        children => {
            count_layout_node(depth, node_count)?;
            let extents = sibling_dimensions(group).map_err(|error| {
                format!("container id={} has unusable geometry: {error}", group.id)
            })?;

            let mut captured_children = Vec::new();
            for child in children {
                captured_children.push(capture_node(child, depth + 1, node_count)?);
            }
            Ok(CapturedNode::Split {
                container_id: group.id,
                axis,
                extents,
                children: captured_children,
            })
        }
    }
}

/// Captures one tiled `con`: a window, or a structural group of further containers.
fn capture_node(
    container: &Node,
    depth: usize,
    node_count: &mut usize,
) -> Result<CapturedNode, String> {
    let id = container.id;
    match container.node_type {
        NodeType::Con => {}
        NodeType::FloatingCon => {
            return Err(format!(
                "container id={id} is floating; the layout format has only tiled windows"
            ));
        }
        node_type => {
            return Err(format!(
                "unsupported Sway node type {node_type:?} at id={id}"
            ));
        }
    }
    let floating = matches!(
        container.floating,
        Some(Floating::AutoOn | Floating::UserOn)
    );
    if floating || !container.floating_nodes.is_empty() {
        return Err(format!(
            "container id={id} is or holds a floating window; the layout format has only tiled windows"
        ));
    }
    if container.fullscreen_mode != Some(0) {
        return Err(format!(
            "container id={id} is fullscreen or reports no fullscreen state; exit fullscreen and retry"
        ));
    }

    // Sway describes every view with a shell and, depending on it, an app ID or X11 window.
    let is_window =
        container.shell.is_some() || container.app_id.is_some() || container.window.is_some();
    if !is_window {
        return capture_group(container, depth, node_count);
    }
    if !container.nodes.is_empty() {
        return Err(format!(
            "window id={id} unexpectedly holds child containers"
        ));
    }

    count_layout_node(depth, node_count)?;
    let selector = match (app_id(container), class(container)) {
        (Some(app_id), _) => Some(Selector::AppId(app_id.to_owned())),
        (None, Some(class)) => Some(Selector::Class(class.to_owned())),
        (None, None) => None,
    };
    Ok(CapturedNode::Window {
        container_id: id,
        selector,
    })
}

/// Counts one emitted layout node against the limits `check` and `load` enforce, so a
/// filled-in draft can always be loaded. Collapsed wrappers are not layout nodes.
fn count_layout_node(depth: usize, node_count: &mut usize) -> Result<(), String> {
    *node_count += 1;
    if depth > MAX_LAYOUT_DEPTH {
        return Err(format!(
            "the arrangement nests deeper than the layout format's {MAX_LAYOUT_DEPTH} levels"
        ));
    }
    if *node_count > MAX_LAYOUT_NODES {
        return Err(format!(
            "the arrangement has more than the layout format's {MAX_LAYOUT_NODES} windows and splits"
        ));
    }
    Ok(())
}

/// The emitted document. Field order is document order, matching the examples.
#[derive(Serialize)]
struct Draft {
    version: u32,
    focus: String,
    layout: DraftNode,
}

#[derive(Serialize)]
#[serde(untagged)]
enum DraftNode {
    Split {
        split: SplitAxis,
        #[serde(skip_serializing_if = "Option::is_none")]
        weight: Option<f64>,
        children: Vec<DraftNode>,
    },
    Window {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        weight: Option<f64>,
        /// Always `null`: capture never infers how an application was launched.
        command: (),
        #[serde(rename = "match")]
        selector: Option<Selector>,
    },
}

/// Names windows `window1`, `window2`, ... in depth-first (visual and launch) order and
/// serializes the whole document in memory.
fn render_draft(captured: &CapturedWorkspace) -> Result<String, String> {
    let mut draft_ids = HashMap::new();
    let layout = draft_node(&captured.root, None, &mut draft_ids);
    let focus = draft_ids.remove(&captured.focused_id).ok_or_else(|| {
        format!(
            "focus is on container id={} rather than a window, for example after `focus parent`; focus a window and retry",
            captured.focused_id
        )
    })?;

    let draft = Draft {
        version: 1,
        focus,
        layout,
    };
    // Keep every scalar on one line and indent sequences like the example layouts.
    let options = serde_saphyr::ser_options! {
        prefer_block_scalars: false,
        compact_list_indent: false,
    };
    let yaml = serde_saphyr::to_string_with_options(&draft, options)
        .map_err(|error| format!("serialize draft: {error}"))?;
    Ok(format!("{DRAFT_HEADER}{yaml}"))
}

/// `weight` is `None` only for the root, whose weight would have no effect. `draft_ids`
/// maps each captured window's container ID to its draft ID.
fn draft_node(
    captured: &CapturedNode,
    weight: Option<f64>,
    draft_ids: &mut HashMap<i64, String>,
) -> DraftNode {
    match captured {
        CapturedNode::Window {
            container_id,
            selector,
        } => {
            let id = format!("window{}", draft_ids.len() + 1);
            draft_ids.insert(*container_id, id.clone());
            DraftNode::Window {
                id,
                weight,
                command: (),
                selector: selector.clone(),
            }
        }
        CapturedNode::Split {
            axis,
            extents,
            children,
            ..
        } => {
            let child_weights = percentage_weights(extents);
            let children = children
                .iter()
                .zip(child_weights)
                .map(|(child, child_weight)| draft_node(child, Some(child_weight), draft_ids))
                .collect();
            DraftNode::Split {
                split: *axis,
                weight,
                children,
            }
        }
    }
}

/// Each extent's percentage of the sibling sum, rounded to hundredths. Rounding, and the
/// minimum that keeps a sliver's weight positive, shift each reconstructed share by less
/// than 3 percentage points even across the maximum 255 siblings: within the 5-point
/// sizing tolerance.
fn percentage_weights(extents: &[f64]) -> Vec<f64> {
    proportions(extents)
        .into_iter()
        .map(|share| ((share * 10_000.0).round() / 100.0).max(0.01))
        .collect()
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
