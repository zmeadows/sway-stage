//! Read-only capture of saved workspaces as editable YAML drafts.
//!
//! Capture retains one complete GET_TREE response and renders only that saved state.
//! It never sends a Sway command, launches anything, or writes a file, and it records
//! arrangement and window selectors, with optional best-effort process arguments.

use crate::command_inference::read_process_command;
use crate::config::{GroupLayout, MAX_LAYOUT_DEPTH, MAX_LAYOUT_NODES};
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
    target_workspace_id: i64,
}

impl Snapshot {
    /// Focused workspace identity established from the saved tree, before any menu opens.
    pub fn workspace_id(&self) -> i64 {
        self.target_workspace_id
    }

    /// Uses the loader's strict emptiness rule, including empty structural containers.
    pub fn workspace_is_empty(&self) -> bool {
        let workspace = self
            .tree
            .find_as_ref(|node| node.id == self.target_workspace_id)
            .expect("snapshot verified the focused workspace");
        workspace.nodes.is_empty() && workspace.floating_nodes.is_empty()
    }

    fn from_tree(tree: Node) -> Result<Self, String> {
        if tree.node_type != NodeType::Root {
            return Err("snapshot: invalid tree root".into());
        }

        let mut container_ids = HashSet::new();
        let mut focused_workspace_id = None;
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
                let target_workspace_id =
                    workspace_id.ok_or("snapshot: focus is outside a workspace")?;
                if focused_workspace_id.replace(target_workspace_id).is_some() {
                    return Err("snapshot: expected exactly one focused container".into());
                }
            }

            for child in container.nodes.iter().chain(&container.floating_nodes) {
                pending.push((child, workspace_id));
            }
        }

        let target_workspace_id = focused_workspace_id.ok_or("snapshot: no focused container")?;
        Ok(Self {
            tree,
            target_workspace_id,
        })
    }

    /// Select ordinary workspace identities from this tree only. No output or traversal
    /// order is used to determine the generated profile's order.
    pub fn select_workspaces(
        &self,
        targets: &[crate::profile::WorkspaceTarget],
        all: bool,
    ) -> Result<Vec<(crate::profile::WorkspaceTarget, i64)>, String> {
        use crate::profile::{MAX_WORKSPACES, WorkspaceTarget};

        let mut ordinary = Vec::new();
        let mut names = HashSet::new();
        let mut numbers = HashSet::new();
        let mut pending = vec![(&self.tree, None, false)];
        while let Some((container, parent_type, internal)) = pending.pop() {
            let internal = internal
                || (container.node_type == NodeType::Output
                    && container.name.as_deref() == Some("__i3"));
            if container.node_type == NodeType::Output
                && !internal
                && (parent_type != Some(NodeType::Root)
                    || container.id <= 0
                    || container.name.as_deref().is_none_or(str::is_empty))
            {
                return Err("workspace snapshot: missing or inconsistent output identity".into());
            }
            if container.node_type == NodeType::Workspace && !internal {
                let name = container
                    .name
                    .as_deref()
                    .filter(|name| !name.is_empty())
                    .ok_or("workspace snapshot: missing workspace name")?;
                let number = container
                    .num
                    .ok_or("workspace snapshot: missing workspace number")?;
                if parent_type != Some(NodeType::Output) || container.id <= 0 || number < -1 {
                    return Err(format!(
                        "workspace {name:?}: inconsistent IPC identity or output ancestry"
                    ));
                }
                if !names.insert(name) || (number >= 0 && !numbers.insert(number)) {
                    return Err("workspace snapshot: ambiguous workspace numbers or names".into());
                }
                // Sway derives num from a leading decimal number in the actual name.
                let numeric_prefix: String =
                    name.chars().take_while(char::is_ascii_digit).collect();
                let expected_number = if numeric_prefix.is_empty() {
                    Some(-1)
                } else {
                    numeric_prefix.parse::<i32>().ok()
                };
                if expected_number != Some(number) {
                    return Err(format!(
                        "workspace {name:?}: inconsistent IPC number {number}"
                    ));
                }
                ordinary.push(container);
            }
            for child in container.nodes.iter().chain(&container.floating_nodes) {
                pending.push((child, Some(container.node_type), internal));
            }
        }

        let mut selected = Vec::new();
        if all {
            for workspace in &ordinary {
                if workspace.nodes.is_empty() && workspace.floating_nodes.is_empty() {
                    continue;
                }
                let target = match workspace.num.expect("verified number") {
                    -1 => WorkspaceTarget::Name(workspace.name.clone().expect("verified name")),
                    number => WorkspaceTarget::Number(number as u64),
                };
                target
                    .validate()
                    .map_err(|error| format!("workspace {target}: {error}"))?;
                selected.push((target, workspace.id));
            }
            selected.sort_by(|(left, _), (right, _)| match (left, right) {
                (WorkspaceTarget::Number(left), WorkspaceTarget::Number(right)) => left.cmp(right),
                (WorkspaceTarget::Number(_), WorkspaceTarget::Name(_)) => std::cmp::Ordering::Less,
                (WorkspaceTarget::Name(_), WorkspaceTarget::Number(_)) => {
                    std::cmp::Ordering::Greater
                }
                (WorkspaceTarget::Name(left), WorkspaceTarget::Name(right)) => left.cmp(right),
            });
        } else {
            for target in targets {
                let workspace = ordinary
                    .iter()
                    .find(|workspace| {
                        target.matches(
                            workspace.num.expect("verified number"),
                            workspace.name.as_deref().expect("verified name"),
                        )
                    })
                    .ok_or_else(|| format!("workspace {target}: not found in snapshot"))?;
                selected.push((target.clone(), workspace.id));
            }
        }
        if selected.is_empty() || selected.len() > MAX_WORKSPACES {
            return Err(format!(
                "capture requires 1 to {MAX_WORKSPACES} populated workspaces"
            ));
        }
        Ok(selected)
    }

    /// Original workspace's containing output, when Sway supplied a nonempty name.
    pub fn output_name(&self) -> Option<&str> {
        let output = self.tree.nodes.iter().find(|container| {
            container.node_type == NodeType::Output
                && container
                    .find_as_ref(|node| node.id == self.target_workspace_id)
                    .is_some()
        })?;
        output.name.as_deref().filter(|name| !name.is_empty())
    }

    /// Validates and renders only saved Sway state. Process arguments remain best-effort.
    pub fn render(&self, infer_commands: bool) -> Result<RenderedDraft, String> {
        self.render_workspace(self.target_workspace_id, infer_commands)
    }

    /// Renders an existing workspace by container identity without further IPC observations.
    pub fn render_workspace(
        &self,
        workspace_id: i64,
        infer_commands: bool,
    ) -> Result<RenderedDraft, String> {
        self.render_workspace_with(workspace_id, infer_commands, &mut read_process_command)
    }

    #[cfg(test)]
    fn render_with(
        &self,
        infer_commands: bool,
        read_command: &mut dyn FnMut(i32) -> Option<Vec<String>>,
    ) -> Result<RenderedDraft, String> {
        self.render_workspace_with(self.target_workspace_id, infer_commands, read_command)
    }

    fn render_workspace_with(
        &self,
        workspace_id: i64,
        infer_commands: bool,
        read_command: &mut dyn FnMut(i32) -> Option<Vec<String>>,
    ) -> Result<RenderedDraft, String> {
        let captured = capture_workspace(self, workspace_id)?;
        let mut infer_window_command = |container_id| {
            if !infer_commands {
                return None;
            }
            let window = self.tree.find_as_ref(|node| node.id == container_id)?;
            let pid = window.pid.filter(|pid| *pid > 0)?;
            // The process may exit, replace its argv, or have its PID reused after GET_TREE.
            read_command(pid)
        };

        render_draft(&captured, infer_commands, &mut infer_window_command)
    }
}

/// Obtains one tree through the explicit SWAYSOCK path, without subscribing to events.
pub fn snapshot() -> Result<Snapshot, String> {
    let socket = observation::sway_socket()?;
    snapshot_from_socket(observation::connect(&socket)?, Instant::now() + IPC_TIMEOUT)
}

fn snapshot_from_socket(socket: UnixStream, deadline: Instant) -> Result<Snapshot, String> {
    let shutdown_socket = socket
        .try_clone()
        .map_err(|error| format!("clone query socket: {error}"))?;

    let (sender, receiver) = mpsc::channel();
    let reader = thread::Builder::new()
        .name("sway-capture".into())
        .spawn(move || {
            let tree_result = Connection::from(socket)
                .get_tree()
                .map_err(|error| format!("get tree: {error}"));
            let _ = sender.send(tree_result);
        })
        .map_err(|error| format!("start capture reader: {error}"))?;

    let tree_result = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    // Cancel even a partial frame read before joining; this connection is never reused.
    let _ = shutdown_socket.shutdown(Shutdown::Both);
    reader.join().map_err(|_| "capture reader panicked")?;

    let tree = tree_result.map_err(|error| format!("tree deadline/channel failure: {error}"))??;
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
    focused_container_id: i64,
}

enum CapturedNode {
    Window {
        container_id: i64,
        selector: Option<Selector>,
    },
    Group {
        layout: GroupLayout,
        /// One-based direct-child selection, present only for tabbed/stacked groups.
        active: Option<usize>,
        /// Present only for splits: child extents along the split axis, parallel to `children`.
        split_extents_pixels: Option<Vec<f64>>,
        children: Vec<CapturedNode>,
    },
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
fn capture_workspace(snapshot: &Snapshot, workspace_id: i64) -> Result<CapturedWorkspace, String> {
    let workspace = snapshot
        .tree
        .find_as_ref(|node| node.id == workspace_id && node.node_type == NodeType::Workspace)
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
    let mut layout_node_count = 0;
    let root = capture_group(workspace, 1, &mut layout_node_count).map_err(in_workspace)?;

    // Snapshot acquisition verified exactly one focused node and its workspace ancestry.
    let focused = if workspace_id == snapshot.target_workspace_id {
        workspace
            .find_as_ref(|node| node.focused)
            .ok_or("no focused container in target workspace")?
    } else {
        // Follow only verified direct tiled children. Snapshot validation already rejects
        // duplicate identities, so neither stale references nor cycles can be followed.
        let mut selected = workspace;
        while !selected.nodes.is_empty() {
            let child_index = selected_tiled_child(selected).map_err(in_workspace)?;
            selected = &selected.nodes[child_index - 1];
        }
        selected
    };

    Ok(CapturedWorkspace {
        root,
        focused_container_id: focused.id,
    })
}

/// Captures the workspace or a structural container from its tiled children. Singleton
/// splits collapse; tabbed/stacked groups retain their headers even with only one child.
/// The enclosing split already measured this container's extent before any collapse.
fn capture_group(
    group: &Node,
    layout_depth: usize,
    layout_node_count: &mut usize,
) -> Result<CapturedNode, String> {
    let layout = match group.layout {
        NodeLayout::SplitH => GroupLayout::SplitH,
        NodeLayout::SplitV => GroupLayout::SplitV,
        NodeLayout::Tabbed => GroupLayout::Tabbed,
        NodeLayout::Stacked => GroupLayout::Stacked,
        layout => {
            return Err(format!(
                "container id={} has unsupported layout {layout:?}",
                group.id
            ));
        }
    };

    let is_split = matches!(layout, GroupLayout::SplitH | GroupLayout::SplitV);
    match group.nodes.as_slice() {
        [] => Err(format!(
            "container id={} holds neither a window nor child containers",
            group.id
        )),
        [only_child] if is_split => capture_node(only_child, layout_depth, layout_node_count),
        children => {
            count_layout_node(layout_depth, layout_node_count)?;
            let split_extents_pixels = if is_split {
                let extents = sibling_dimensions(group).map_err(|error| {
                    format!("container id={} has unusable geometry: {error}", group.id)
                })?;
                Some(extents)
            } else {
                None
            };

            let mut captured_children = Vec::new();
            for child in children {
                captured_children.push(capture_node(child, layout_depth + 1, layout_node_count)?);
            }

            // Each direct child produces exactly one captured child, even when singleton
            // split wrappers collapse inside it, so selection keeps the emitted order.
            let active = if is_split {
                None
            } else {
                Some(capture_active_child(group)?)
            };

            Ok(CapturedNode::Group {
                layout,
                active,
                split_extents_pixels,
                children: captured_children,
            })
        }
    }
}

/// Uses direct-child history even when keyboard focus is outside this group.
fn capture_active_child(group: &Node) -> Result<usize, String> {
    if group.nodes.len() == 1 {
        return Ok(1);
    }

    selected_tiled_child(group)
}

/// Shared tab/stack selection and inactive-workspace focus descent. A singleton needs
/// no ordering information, but any supplied references must still identify its child.
fn selected_tiled_child(group: &Node) -> Result<usize, String> {
    let unusable_history = || {
        format!(
            "container id={} has unusable child-focus history; cannot determine the selected tiled child",
            group.id
        )
    };
    if group.nodes.len() == 1 && group.focus.is_empty() {
        return Ok(1);
    }

    let selected_child_id = group.focus.first().ok_or_else(unusable_history)?;
    let selected_child_index = group
        .nodes
        .iter()
        .position(|child| child.id == *selected_child_id)
        .ok_or_else(unusable_history)?;

    let mut seen_child_ids = HashSet::new();
    for child_id in &group.focus {
        if !seen_child_ids.insert(*child_id)
            || !group.nodes.iter().any(|child| child.id == *child_id)
        {
            return Err(unusable_history());
        }
    }

    for (child_index, child) in group.nodes.iter().enumerate() {
        if child_index != selected_child_index && child.find_as_ref(|node| node.focused).is_some() {
            return Err(format!(
                "container id={} has child-focus history contradicting keyboard focus",
                group.id
            ));
        }
    }

    Ok(selected_child_index + 1)
}

/// Captures one tiled `con`: a window, or a structural group of further containers.
fn capture_node(
    container: &Node,
    layout_depth: usize,
    layout_node_count: &mut usize,
) -> Result<CapturedNode, String> {
    let container_id = container.id;
    match container.node_type {
        NodeType::Con => {}
        NodeType::FloatingCon => {
            return Err(format!(
                "container id={container_id} is floating; the layout format has only tiled windows"
            ));
        }
        node_type => {
            return Err(format!(
                "unsupported Sway node type {node_type:?} at id={container_id}"
            ));
        }
    }

    let floating = matches!(
        container.floating,
        Some(Floating::AutoOn | Floating::UserOn)
    );
    if floating || !container.floating_nodes.is_empty() {
        return Err(format!(
            "container id={container_id} is or holds a floating window; the layout format has only tiled windows"
        ));
    }

    if container.fullscreen_mode != Some(0) {
        return Err(format!(
            "container id={container_id} is fullscreen or reports no fullscreen state; exit fullscreen and retry"
        ));
    }

    if matches!(
        container.scratchpad_state,
        Some(ScratchpadState::Fresh | ScratchpadState::Changed)
    ) {
        return Err(format!(
            "container id={container_id} belongs to the scratchpad; remove it from the scratchpad and retry"
        ));
    }

    // Sway describes every view with a shell and, depending on it, an app ID or X11 window.
    let is_window =
        container.shell.is_some() || container.app_id.is_some() || container.window.is_some();
    if !is_window {
        return capture_group(container, layout_depth, layout_node_count);
    }
    if !container.nodes.is_empty() {
        return Err(format!(
            "window id={container_id} unexpectedly holds child containers"
        ));
    }

    count_layout_node(layout_depth, layout_node_count)?;
    let selector = match (app_id(container), class(container)) {
        (Some(app_id), _) => Some(Selector::AppId(app_id.to_owned())),
        (None, Some(class)) => Some(Selector::Class(class.to_owned())),
        (None, None) => None,
    };

    Ok(CapturedNode::Window {
        container_id,
        selector,
    })
}

/// Counts one emitted layout node against the limits `check` and `load` enforce, so a
/// filled-in draft can always be loaded. Collapsed wrappers are not layout nodes.
fn count_layout_node(layout_depth: usize, layout_node_count: &mut usize) -> Result<(), String> {
    *layout_node_count += 1;
    if layout_depth > MAX_LAYOUT_DEPTH {
        return Err(format!(
            "the arrangement nests deeper than the layout format's {MAX_LAYOUT_DEPTH} levels"
        ));
    }
    if *layout_node_count > MAX_LAYOUT_NODES {
        return Err(format!(
            "the arrangement has more than the layout format's {MAX_LAYOUT_NODES} windows and groups"
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
    Group {
        group: GroupLayout,
        #[serde(skip_serializing_if = "Option::is_none")]
        weight: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        active: Option<usize>,
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
    indent_columns: usize,
}

impl Serialize for DraftCommand {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let flow = serde_saphyr::FlowSeq(&self.arguments);
        let flow_yaml = serde_saphyr::to_string_with_options(&flow, draft_options())
            .map_err(serde::ser::Error::custom)?;
        let sequence = flow_yaml.trim_end_matches('\n');
        let line_length = self.indent_columns + "command: ".len() + sequence.chars().count();

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
    infer_window_command: &mut dyn FnMut(i64) -> Option<Vec<String>>,
) -> Result<RenderedDraft, String> {
    let mut draft_window_ids = HashMap::new();
    let mut commands = CommandCounts {
        total: 0,
        inferred: 0,
        inference_requested: infer_commands,
    };

    let layout = build_draft_node(
        &captured.root,
        None,
        2,
        &mut draft_window_ids,
        infer_window_command,
        &mut commands,
    );
    let focus = draft_window_ids.remove(&captured.focused_container_id).ok_or_else(|| {
        format!(
            "focus is on container id={} rather than a window, for example after `focus parent`; focus a window and retry",
            captured.focused_container_id
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
    // Version is the first field; annotate only its line, leaving all serialized data intact.
    let yaml = yaml.replacen(
        "version: 1\n",
        "version: 1  # ← Required; do not remove\n",
        1,
    );
    let inference_header = if infer_commands { INFERENCE_HEADER } else { "" };

    Ok(RenderedDraft {
        yaml: format!("{DRAFT_HEADER}{inference_header}{yaml}"),
        commands,
    })
}

/// `weight` comes only from an enclosing split; the root and children of tabbed/stacked
/// groups omit it. `draft_window_ids` maps each captured window's container ID to its draft ID.
fn build_draft_node(
    captured: &CapturedNode,
    weight: Option<f64>,
    indent_columns: usize,
    draft_window_ids: &mut HashMap<i64, String>,
    infer_window_command: &mut dyn FnMut(i64) -> Option<Vec<String>>,
    commands: &mut CommandCounts,
) -> DraftNode {
    match captured {
        CapturedNode::Window {
            container_id,
            selector,
        } => {
            let draft_window_id = format!("window{}", draft_window_ids.len() + 1);
            draft_window_ids.insert(*container_id, draft_window_id.clone());

            let command = infer_window_command(*container_id);
            commands.total += 1;
            if command.is_some() {
                commands.inferred += 1;
            }

            DraftNode::Window {
                id: draft_window_id,
                weight,
                command: command.map(|arguments| DraftCommand {
                    arguments,
                    indent_columns,
                }),
                selector: selector.clone(),
            }
        }
        CapturedNode::Group {
            layout,
            active,
            split_extents_pixels,
            children,
        } => {
            let child_weights = split_extents_pixels.as_deref().map(percentage_weights);
            let mut draft_children = Vec::new();
            for (child_index, child) in children.iter().enumerate() {
                let child_weight = child_weights.as_ref().map(|weights| weights[child_index]);

                // Each child adds a two-space sequence indent and a two-space dash.
                let draft_child = build_draft_node(
                    child,
                    child_weight,
                    indent_columns + 4,
                    draft_window_ids,
                    infer_window_command,
                    commands,
                );
                draft_children.push(draft_child);
            }

            DraftNode::Group {
                group: *layout,
                weight,
                active: active.filter(|child_index| *child_index > 1),
                children: draft_children,
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
