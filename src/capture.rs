//! Read-only capture of the focused workspace's tiled arrangement as an editable YAML draft.
//!
//! Capture retains one complete GET_TREE response and renders only that saved state.
//! It never sends a Sway command, launches anything, or writes a file, and it records
//! arrangement and window selectors, with optional best-effort process arguments.

use crate::command_inference::read_process_command;
use crate::config::{MAX_LAYOUT_DEPTH, MAX_LAYOUT_NODES};
use crate::observation::{self, app_id, class, proportions, sibling_dimensions};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use swayipc::{Connection, Floating, Node, NodeLayout, NodeType, ScratchpadState};

// Bounds the complete tree query, including stalled or fragmented replies.
const IPC_TIMEOUT: Duration = Duration::from_secs(2);

const INFERENCE_HEADER: &str = "\
# Review recovered commands before executing or sharing: process arguments are
# best-effort, may contain secrets, and may not reproduce the original application.
";

const DRAFT_HEADER: &str = "\
# Layout draft from `sway-stage capture`. Before it can run, replace every
# `command: null` with the executable and arguments that open that window, and any
# `match: null` with exactly one exact `app_id` or `class`. Weights are percentages of
# the enclosing split. Validate the edited file with `sway-stage check NAME`.
";

/// YAML and command counts from the same generated layout.
#[derive(Debug, PartialEq)]
pub struct RenderedDraft {
    pub yaml: String,
    pub commands: CommandCounts,
}

#[derive(Debug, PartialEq)]
pub struct CommandCounts {
    pub total: usize,
    pub inferred: usize,
    pub inference_requested: bool,
}

impl CommandCounts {
    pub fn unresolved(&self) -> usize {
        self.total - self.inferred
    }

    /// Contains only counts and guidance, never recovered arguments or process metadata.
    pub fn summary(&self) -> String {
        let unresolved = self.unresolved();
        if !self.inference_requested {
            format!(
                "Command inference disabled; unresolved commands: {unresolved}. Manual editing required."
            )
        } else if unresolved == 0 {
            format!(
                "Commands inferred: {}; unresolved: 0. Review best-effort commands before use.",
                self.inferred
            )
        } else {
            format!(
                "Commands inferred: {}; unresolved: {unresolved}. Manual editing required.",
                self.inferred
            )
        }
    }
}

/// Complete read-only Sway state, including output ancestry, window PIDs, and focus.
/// Acquiring it does not require a nonempty or representable layout.
pub struct Snapshot {
    tree: Node,
    target: i64,
}

impl Snapshot {
    /// Focused workspace identity established from the saved tree, before any menu opens.
    pub fn workspace_id(&self) -> i64 {
        self.target
    }

    fn from_tree(tree: Node) -> Result<Self, String> {
        if tree.node_type != NodeType::Root {
            return Err("snapshot: invalid tree root".into());
        }

        let mut container_ids = HashSet::new();
        let mut focused_workspace = None;
        let mut pending = vec![(&tree, None)];
        while let Some((container, workspace_id)) = pending.pop() {
            if !container_ids.insert(container.id) {
                return Err("snapshot: duplicate container identities".into());
            }
            let workspace_id = if container.node_type == NodeType::Workspace {
                Some(container.id)
            } else {
                workspace_id
            };
            if container.focused {
                let target = workspace_id.ok_or("snapshot: focus is outside a workspace")?;
                if focused_workspace.replace(target).is_some() {
                    return Err("snapshot: expected exactly one focused container".into());
                }
            }
            for child in container.nodes.iter().chain(&container.floating_nodes) {
                pending.push((child, workspace_id));
            }
        }
        let target = focused_workspace.ok_or("snapshot: no focused container")?;
        Ok(Self { tree, target })
    }

    /// Original workspace's containing output, when Sway supplied a nonempty name.
    pub fn output_name(&self) -> Option<&str> {
        let output = self.tree.nodes.iter().find(|container| {
            container.node_type == NodeType::Output
                && container
                    .find_as_ref(|node| node.id == self.target)
                    .is_some()
        })?;
        output.name.as_deref().filter(|name| !name.is_empty())
    }

    /// Validates and renders only saved Sway state. Process arguments remain best-effort.
    pub fn render(&self, infer_commands: bool) -> Result<RenderedDraft, String> {
        self.render_with(infer_commands, &mut read_process_command)
    }

    fn render_with(
        &self,
        infer_commands: bool,
        read_command: &mut dyn FnMut(i32) -> Option<Vec<String>>,
    ) -> Result<RenderedDraft, String> {
        let captured = capture_workspace(self)?;
        let mut command_for = |container_id| {
            if !infer_commands {
                return None;
            }
            let window = self.tree.find_as_ref(|node| node.id == container_id)?;
            let pid = window.pid.filter(|pid| *pid > 0)?;
            // The process may exit, replace its argv, or have its PID reused after GET_TREE.
            read_command(pid)
        };
        render_draft(&captured, infer_commands, &mut command_for)
    }
}

/// Obtains one tree through the explicit SWAYSOCK path, without subscribing to events.
pub fn snapshot() -> Result<Snapshot, String> {
    let socket = observation::sway_socket()?;
    snapshot_from_socket(observation::connect(&socket)?, Instant::now() + IPC_TIMEOUT)
}

fn snapshot_from_socket(socket: UnixStream, deadline: Instant) -> Result<Snapshot, String> {
    let control = socket
        .try_clone()
        .map_err(|error| format!("clone query socket: {error}"))?;
    let (sender, receiver) = mpsc::channel();
    let reader = thread::Builder::new()
        .name("sway-capture".into())
        .spawn(move || {
            let result = Connection::from(socket)
                .get_tree()
                .map_err(|error| format!("get tree: {error}"));
            let _ = sender.send(result);
        })
        .map_err(|error| format!("start capture reader: {error}"))?;

    let result = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    // Cancel even a partial frame read before joining; this connection is never reused.
    let _ = control.shutdown(Shutdown::Both);
    reader.join().map_err(|_| "capture reader panicked")?;
    let tree = result.map_err(|error| format!("tree deadline/channel failure: {error}"))??;
    Snapshot::from_tree(tree)
}

/// Returns a complete draft in memory, so a failed capture cannot print partial YAML.
pub fn capture(infer_commands: bool) -> Result<RenderedDraft, String> {
    snapshot()?.render(infer_commands)
}

/// The saved facts a draft is generated from.
/// Distinct from the configured `LayoutNode`: it holds Sway container IDs and pixel extents.
struct CapturedWorkspace {
    root: CapturedNode,
    /// Sway's focused container, which must be one of the captured windows.
    focused_id: i64,
}

enum CapturedNode {
    Window {
        container_id: i64,
        selector: Option<Selector>,
    },
    Split {
        axis: SplitAxis,
        /// Child extents along `axis` in pixels, parallel to `children`.
        extents: Vec<f64>,
        children: Vec<CapturedNode>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
enum SplitAxis {
    #[serde(rename = "splith")]
    Horizontal,
    #[serde(rename = "splitv")]
    Vertical,
}

/// An exact window selector, serialized as the single key of a `match` mapping.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Selector {
    AppId(String),
    Class(String),
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

    // Snapshot acquisition verified exactly one focused node and its workspace ancestry.
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
    if matches!(
        container.scratchpad_state,
        Some(ScratchpadState::Fresh | ScratchpadState::Changed)
    ) {
        return Err(format!(
            "container id={id} belongs to the scratchpad; remove it from the scratchpad and retry"
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
        group: SplitAxis,
        #[serde(skip_serializing_if = "Option::is_none")]
        weight: Option<f64>,
        children: Vec<DraftNode>,
    },
    Window {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        weight: Option<f64>,
        command: Option<DraftCommand>,
        #[serde(rename = "match")]
        selector: Option<Selector>,
    },
}

/// Only launch arguments may use flow style; layout children remain ordinary sequences.
struct DraftCommand {
    arguments: Vec<String>,
    indent: usize,
}

impl Serialize for DraftCommand {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let flow = serde_saphyr::FlowSeq(&self.arguments);
        let flow_yaml = serde_saphyr::to_string_with_options(&flow, draft_options())
            .map_err(serde::ser::Error::custom)?;
        let sequence = flow_yaml.trim_end_matches('\n');
        let line_length = self.indent + "command: ".len() + sequence.chars().count();

        // Measure YAML quoting and escapes, not raw argv. Unexpected multiline output
        // also falls back to the established block representation.
        if !sequence.contains('\n') && line_length <= 120 {
            flow.serialize(serializer)
        } else {
            self.arguments.serialize(serializer)
        }
    }
}

fn draft_options() -> serde_saphyr::SerializerOptions {
    // Keep every scalar on one line and indent sequences like the example layouts.
    serde_saphyr::ser_options! {
        prefer_block_scalars: false,
        compact_list_indent: false,
    }
}

/// Names windows `window1`, `window2`, ... in depth-first (visual and launch) order and
/// serializes the whole document in memory.
fn render_draft(
    captured: &CapturedWorkspace,
    infer_commands: bool,
    command_for: &mut dyn FnMut(i64) -> Option<Vec<String>>,
) -> Result<RenderedDraft, String> {
    let mut draft_ids = HashMap::new();
    let mut commands = CommandCounts {
        total: 0,
        inferred: 0,
        inference_requested: infer_commands,
    };
    let layout = draft_node(
        &captured.root,
        None,
        2,
        &mut draft_ids,
        command_for,
        &mut commands,
    );
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
    let yaml = serde_saphyr::to_string_with_options(&draft, draft_options()).map_err(|error| {
        // Serializer errors must not echo recovered arguments into notifications.
        if infer_commands {
            "serialize draft: YAML serialization failed".to_owned()
        } else {
            format!("serialize draft: {error}")
        }
    })?;
    let inference_header = if infer_commands { INFERENCE_HEADER } else { "" };
    Ok(RenderedDraft {
        yaml: format!("{DRAFT_HEADER}{inference_header}{yaml}"),
        commands,
    })
}

/// `weight` is `None` only for the root, whose weight would have no effect. `draft_ids`
/// maps each captured window's container ID to its draft ID.
fn draft_node(
    captured: &CapturedNode,
    weight: Option<f64>,
    indent: usize,
    draft_ids: &mut HashMap<i64, String>,
    command_for: &mut dyn FnMut(i64) -> Option<Vec<String>>,
    commands: &mut CommandCounts,
) -> DraftNode {
    match captured {
        CapturedNode::Window {
            container_id,
            selector,
        } => {
            let id = format!("window{}", draft_ids.len() + 1);
            draft_ids.insert(*container_id, id.clone());
            let command = command_for(*container_id);
            commands.total += 1;
            if command.is_some() {
                commands.inferred += 1;
            }
            DraftNode::Window {
                id,
                weight,
                command: command.map(|arguments| DraftCommand { arguments, indent }),
                selector: selector.clone(),
            }
        }
        CapturedNode::Split {
            axis,
            extents,
            children,
        } => {
            let child_weights = percentage_weights(extents);
            let children = children
                .iter()
                .zip(child_weights)
                .map(|(child, child_weight)| {
                    // Each child adds a two-space sequence indent and a two-space dash.
                    draft_node(
                        child,
                        Some(child_weight),
                        indent + 4,
                        draft_ids,
                        command_for,
                        commands,
                    )
                })
                .collect();
            DraftNode::Split {
                group: *axis,
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
