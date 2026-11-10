//! Ordered workspace focus history and validated, read-only Sway snapshots.
//!
//! One persistent subscription retains every workspace, window, and tick event across
//! harness operations, so a switch away and back between snapshots is still detected. A
//! fence sends a unique tick after the queries; reading through it proves that events
//! emitted earlier have arrived. Fences narrow races, but a snapshot and a later command
//! can never be atomic.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::time::Instant;

use crate::ipc::{
    Event, EventSubscription, Floating, IPC_TIMEOUT, NodeType, ScratchpadState, Swaymsg, TreeNode,
    WindowEvent, Workspace, WorkspaceChange,
};

/// Bounds the events one fence reads, including ticks and workspace events.
const MAX_FENCE_EVENTS: usize = 4096;
/// Sway's hidden scratchpad workspace, which `get_workspaces` omits.
const SCRATCHPAD_WORKSPACE: &str = "__i3_scratch";

/// A workspace's container ID and name; the harness keeps control only while both persist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceIdentity {
    pub id: i64,
    pub name: String,
}

/// The one workspace switch a fence may accept: focus moving from the workspace with
/// `from_id` to the workspace named `destination_name`.
pub struct ExpectedSwitch {
    pub from_id: i64,
    pub destination_name: String,
}

/// The event subscription and the focus history derived from it.
pub struct Events {
    subscription: EventSubscription,
    /// The workspace that event history has established as focused: recorded by preflight,
    /// then changed only by an expected switch. Until it is set, any focus change fails.
    current_workspace: Option<WorkspaceIdentity>,
    /// Cleared by any fence or snapshot consistency failure, and never restored. Unlike an
    /// ordinary failure, untrusted history forbids further fences and workspace restoration.
    focus_history_trusted: bool,
}

impl Events {
    /// Returns once Sway's initial tick proves the subscription is registered.
    pub fn subscribe(swaymsg: &Swaymsg) -> Result<Self, String> {
        let mut subscription = swaymsg.subscribe()?;
        let readiness = subscription
            .next_event(Instant::now() + IPC_TIMEOUT)
            .and_then(|event| match event {
                Event::Tick(tick) if tick.first => Ok(()),
                _ => Err("missing initial subscription tick".into()),
            });
        if let Err(mut error) = readiness {
            if let Err(stop_error) = subscription.close() {
                error.push_str(&format!("; {stop_error}"));
            }
            return Err(error);
        }

        Ok(Self {
            subscription,
            current_workspace: None,
            focus_history_trusted: true,
        })
    }

    pub fn current_workspace(&self) -> Option<&WorkspaceIdentity> {
        self.current_workspace.as_ref()
    }

    /// Records the focused workspace that preflight observed.
    pub fn set_current_workspace(&mut self, workspace: &Workspace) {
        self.current_workspace = Some(WorkspaceIdentity {
            id: workspace.id,
            name: workspace.name.clone(),
        });
    }

    pub fn focus_history_trusted(&self) -> bool {
        self.focus_history_trusted
    }

    /// Marks focus history untrusted and returns `message` as the error, for failures that
    /// leave user control uncertain rather than merely failing a check.
    pub fn distrust_focus_history(&mut self, message: impl Into<String>) -> String {
        self.focus_history_trusted = false;
        message.into()
    }

    /// Reads events through a new tick fence, returning the window events before it. Every
    /// failure here, including a failed tick request, makes focus history untrusted.
    pub fn fence(
        &mut self,
        swaymsg: &Swaymsg,
        expected_switch: Option<&ExpectedSwitch>,
    ) -> Result<Vec<WindowEvent>, String> {
        if !self.focus_history_trusted {
            return Err("event history is no longer trusted; refusing another fence".into());
        }

        let result = self.read_through_fence(swaymsg, expected_switch);
        if result.is_err() {
            self.focus_history_trusted = false;
        }
        result
    }

    fn read_through_fence(
        &mut self,
        swaymsg: &Swaymsg,
        expected_switch: Option<&ExpectedSwitch>,
    ) -> Result<Vec<WindowEvent>, String> {
        // The tick request counts against the same deadline as the events before it.
        let deadline = Instant::now() + IPC_TIMEOUT;
        let token = fence_token()?;
        swaymsg.send_tick(&token)?;

        let mut window_events = Vec::new();
        let mut switched = false;
        for _ in 0..MAX_FENCE_EVENTS {
            let workspace_event = match self.subscription.next_event(deadline)? {
                Event::Tick(tick) => {
                    // The initial tick and other clients' ticks cannot complete this fence.
                    if tick.payload == token && !tick.first {
                        if expected_switch.is_some() && !switched {
                            return Err("workspace switch produced no expected focus event".into());
                        }
                        return Ok(window_events);
                    }
                    continue;
                }
                Event::Window(window_event) => {
                    window_events.push(window_event);
                    continue;
                }
                Event::Workspace(workspace_event) => workspace_event,
            };

            let current_id = workspace_event
                .current
                .as_ref()
                .map(|workspace| workspace.id);
            let old_id = workspace_event.old.as_ref().map(|workspace| workspace.id);
            match workspace_event.change {
                WorkspaceChange::Focus => {
                    if let Some(expected) = expected_switch
                        && !switched
                    {
                        let destination = match (&workspace_event.old, &workspace_event.current) {
                            (Some(old), Some(current))
                                if old.id == expected.from_id
                                    && current.name.as_deref()
                                        == Some(expected.destination_name.as_str()) =>
                            {
                                WorkspaceIdentity {
                                    id: current.id,
                                    name: expected.destination_name.clone(),
                                }
                            }
                            _ => {
                                return Err(format!(
                                    "unexpected workspace switch: old={:?} current={:?}",
                                    workspace_event.old, workspace_event.current
                                ));
                            }
                        };
                        self.current_workspace = Some(destination);
                        switched = true;
                    } else if let Some(current_workspace) = &self.current_workspace {
                        // Refocusing the same workspace is harmless. Any other focus change
                        // means the user has taken control, even if focus has returned by
                        // the next snapshot.
                        if current_id != Some(current_workspace.id)
                            || old_id.is_some_and(|id| id != current_workspace.id)
                        {
                            return Err(
                                "workspace focus changed; user control takes precedence".into()
                            );
                        }
                    } else {
                        return Err("workspace focus changed during initial preflight".into());
                    }
                }
                WorkspaceChange::Reload | WorkspaceChange::Rename | WorkspaceChange::Move => {
                    return Err(format!(
                        "workspace {:?} during harness execution",
                        workspace_event.change
                    ));
                }
                WorkspaceChange::Empty => {
                    if let Some(current_workspace) = &self.current_workspace
                        && current_id == Some(current_workspace.id)
                    {
                        return Err("focused workspace was destroyed".into());
                    }
                }
                WorkspaceChange::Init | WorkspaceChange::Urgent => {}
            }
        }
        Err(format!("event history exceeded {MAX_FENCE_EVENTS} events"))
    }

    /// Stops the subscription helper and reports a failure to reap it.
    pub fn close(self) -> Result<(), String> {
        self.subscription.close()
    }
}

/// A failed verification, classified by what it leaves in doubt. Either kind ends the run
/// without further cleanup; only `report` turns one into a message.
#[derive(Debug)]
pub enum Failure {
    /// An expectation failed, such as sizing, while window ownership and user control
    /// remain established. Workspace restoration may still be attempted.
    Check(String),
    /// Window ownership, placement, identity, or user control is uncertain.
    Uncertain(String),
}

impl Failure {
    /// Returns the message to report. An uncertain failure also distrusts focus history,
    /// which refuses every later fence and so forbids cleanup and workspace restoration.
    pub fn report(self, events: &mut Events) -> String {
        self.report_history(&mut events.focus_history_trusted)
    }

    fn report_history(self, focus_history_trusted: &mut bool) -> String {
        match self {
            Failure::Check(message) => message,
            Failure::Uncertain(message) => {
                *focus_history_trusted = false;
                message
            }
        }
    }
}

// Strings come only from ordinary filesystem/process checks or observations that already
// invalidate their own history. Verification retains Failure until the central report.
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Check(message)
    }
}

/// Sway state from `get_workspaces`, then `get_tree`, then a fence covering every event
/// emitted before both replies. The tree may still predate some of those events.
pub struct Snapshot {
    pub tree: TreeNode,
    pub workspaces: Vec<Workspace>,
    /// The uniquely focused workspace, validated to be present in `tree`.
    pub focused_workspace_id: i64,
    pub window_events: Vec<WindowEvent>,
}

impl Snapshot {
    pub fn focused_workspace(&self) -> &TreeNode {
        walk(&self.tree)
            .into_iter()
            .map(|placed| placed.node)
            .find(|node| node.id == self.focused_workspace_id)
            .expect("snapshot validation found the focused workspace in this tree")
    }
}

/// Query failures are ordinary failures; any failure from the fence onward also makes
/// focus history untrusted.
pub fn snapshot(
    swaymsg: &Swaymsg,
    events: &mut Events,
    expected_switch: Option<&ExpectedSwitch>,
) -> Result<Snapshot, String> {
    let workspaces = swaymsg.get_workspaces()?;
    let tree = swaymsg.get_tree()?;
    let window_events = events.fence(swaymsg, expected_switch)?;

    let focused_workspace_id = validate_snapshot(&tree, &workspaces, events.current_workspace())
        .map_err(|error| events.distrust_focus_history(error))?;
    Ok(Snapshot {
        tree,
        workspaces,
        focused_workspace_id,
        window_events,
    })
}

/// Requires unique identities, one consistent focus that matches the established current
/// workspace, and agreement between the workspace query and the tree. Returns the focused
/// workspace's ID.
fn validate_snapshot(
    tree: &TreeNode,
    workspaces: &[Workspace],
    current_workspace: Option<&WorkspaceIdentity>,
) -> Result<i64, String> {
    let placed_nodes = walk(tree);
    let mut container_ids = HashSet::new();
    let container_ids_unique = placed_nodes
        .iter()
        .all(|placed| container_ids.insert(placed.node.id));
    if tree.node_type != NodeType::Root || !container_ids_unique {
        return Err("invalid tree or duplicate container identities".into());
    }

    let mut workspace_ids = HashSet::new();
    let mut workspace_names = HashSet::new();
    for workspace in workspaces {
        if !workspace_ids.insert(workspace.id) || !workspace_names.insert(workspace.name.as_str()) {
            return Err("ambiguous workspace identities or names".into());
        }
    }

    let focused_workspaces: Vec<&Workspace> = workspaces
        .iter()
        .filter(|workspace| workspace.focused)
        .collect();
    let [focused_workspace] = focused_workspaces.as_slice() else {
        return Err("expected exactly one focused workspace".into());
    };
    if let Some(current_workspace) = current_workspace
        && (focused_workspace.id != current_workspace.id
            || focused_workspace.name != current_workspace.name)
    {
        return Err("workspace focus or identity changed".into());
    }

    let mut focused_nodes = placed_nodes.iter().filter(|placed| placed.node.focused);
    let focused_node_workspace_id = match (focused_nodes.next(), focused_nodes.next()) {
        (Some(placed), None) => placed.workspace.map(|workspace| workspace.id),
        _ => None,
    };
    if focused_node_workspace_id != Some(focused_workspace.id) {
        return Err("tree and workspace focus disagree".into());
    }

    let tree_workspaces: HashMap<i64, &TreeNode> = placed_nodes
        .iter()
        .map(|placed| placed.node)
        .filter(|node| {
            node.node_type == NodeType::Workspace
                && node.name.as_deref() != Some(SCRATCHPAD_WORKSPACE)
        })
        .map(|node| (node.id, node))
        .collect();
    let tree_workspace_ids: HashSet<i64> = tree_workspaces.keys().copied().collect();
    if tree_workspace_ids != workspace_ids {
        return Err("workspace query and raw tree identities disagree".into());
    }
    for workspace in workspaces {
        let tree_name = tree_workspaces
            .get(&workspace.id)
            .and_then(|node| node.name.as_deref());
        if tree_name != Some(workspace.name.as_str()) {
            return Err("workspace query and raw tree names disagree".into());
        }
    }

    Ok(focused_workspace.id)
}

/// A tree node with the placement it inherits from its ancestors.
pub struct PlacedNode<'a> {
    pub node: &'a TreeNode,
    /// The enclosing workspace, which for a workspace is itself; `None` above workspaces.
    pub workspace: Option<&'a TreeNode>,
    /// The node or an ancestor is floating, fullscreen, or in the scratchpad.
    pub unsafe_state: bool,
}

/// Every node in pre-order, with tiled children before floating ones.
pub fn walk(tree: &TreeNode) -> Vec<PlacedNode<'_>> {
    let mut placed_nodes = Vec::new();
    walk_into(tree, None, false, &mut placed_nodes);
    placed_nodes
}

fn walk_into<'a>(
    node: &'a TreeNode,
    parent_workspace: Option<&'a TreeNode>,
    parent_unsafe_state: bool,
    placed_nodes: &mut Vec<PlacedNode<'a>>,
) {
    let mut unsafe_state = parent_unsafe_state;
    let workspace = if node.node_type == NodeType::Workspace {
        unsafe_state |= node.name.as_deref() == Some(SCRATCHPAD_WORKSPACE);
        Some(node)
    } else {
        // Sway reports fullscreen_mode 1 on every workspace, so only other nodes carry
        // fullscreen state.
        unsafe_state |= node.fullscreen_mode.is_some_and(|mode| mode != 0);
        parent_workspace
    };
    unsafe_state |= node.node_type == NodeType::FloatingCon
        || matches!(node.floating, Some(Floating::AutoOn | Floating::UserOn))
        || matches!(
            node.scratchpad_state,
            Some(ScratchpadState::Fresh | ScratchpadState::Changed)
        );

    placed_nodes.push(PlacedNode {
        node,
        workspace,
        unsafe_state,
    });
    for child in &node.nodes {
        walk_into(child, workspace, unsafe_state, placed_nodes);
    }
    for child in &node.floating_nodes {
        walk_into(child, workspace, true, placed_nodes);
    }
}

/// A random tick payload that no other client's tick can plausibly repeat.
fn fence_token() -> Result<String, String> {
    let mut random_bytes = [0; 16];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut random_bytes))
        .map_err(|error| format!("read /dev/urandom for an event fence token: {error}"))?;
    let hex: String = random_bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(format!("sway-stage-integration:{hex}"))
}

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;
