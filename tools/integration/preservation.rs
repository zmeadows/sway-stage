//! Stable foreign-window placement, independent of production state and ownership.
//! Comparing snapshots cannot prove absence of transient changes. Callers must also
//! validate fenced event history, allowing only the activity their current phase expects.

use std::collections::BTreeMap;

use crate::ipc::{Floating, Layout, NodeType, Rect, ScratchpadState, TreeNode};
use crate::layout::{Signature, is_window, signature};
use crate::observation::{Failure, Snapshot};

/// An ancestor's role in a window's placement. Ordering matters inside workspaces, but
/// output/workspace sibling positions may change as disposable workspaces come and go.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Ancestor {
    id: i64,
    node_type: NodeType,
    name: Option<String>,
    layout: Layout,
    rect: Rect,
    floating: Option<Floating>,
    fullscreen_mode: Option<u8>,
    scratchpad_state: Option<ScratchpadState>,
    tiled_ids: Vec<i64>,
    floating_ids: Vec<i64>,
}

#[derive(Debug, PartialEq, Eq)]
struct PreservedWindow {
    ancestry: Vec<Ancestor>,
    window: Signature,
}

/// Only observation evidence: this never authorizes closing any window in the baseline.
#[derive(Debug)]
pub struct ForeignWindows {
    target_workspace_id: i64,
    target_output_ancestry: Vec<(i64, Option<String>)>,
    windows: BTreeMap<i64, PreservedWindow>,
}

impl ForeignWindows {
    pub fn record(observed: &Snapshot, target_workspace_id: i64) -> Result<Self, Failure> {
        observed.validate()?;
        let target_output_ancestry = output_ancestry(&observed.tree, target_workspace_id)?;
        let mut windows = BTreeMap::new();
        record_windows(
            &observed.tree,
            target_workspace_id,
            false,
            &mut Vec::new(),
            &mut windows,
        )?;
        Ok(Self {
            target_workspace_id,
            target_output_ancestry,
            windows,
        })
    }

    /// Verify every recorded foreign window, including those in the scratchpad. New
    /// windows remain the responsibility of the existing strict global event checks.
    pub fn verify(&self, observed: &Snapshot) -> Result<(), Failure> {
        let current = Self::record(observed, self.target_workspace_id)?;
        if current.target_output_ancestry != self.target_output_ancestry {
            return Err(Failure::Uncertain(format!(
                "target workspace id={} changed output ancestry",
                self.target_workspace_id
            )));
        }
        for (window_id, baseline) in &self.windows {
            let Some(current) = current.windows.get(window_id) else {
                return Err(Failure::Uncertain(format!(
                    "foreign window id={window_id} disappeared or entered the target workspace"
                )));
            };
            if current.ancestry != baseline.ancestry {
                return Err(Failure::Uncertain(format!(
                    "foreign window id={window_id} ancestry/parent/order/ancestor state changed: \
                     expected {:?}, observed {:?}",
                    baseline.ancestry, current.ancestry
                )));
            }
            if current.window != baseline.window {
                return Err(Failure::Uncertain(format!(
                    "foreign window id={window_id} identity/geometry/state changed: \
                     expected {:?}, observed {:?}",
                    baseline.window, current.window
                )));
            }
        }
        Ok(())
    }
}

fn record_windows(
    node: &TreeNode,
    target_workspace_id: i64,
    inside_target: bool,
    ancestry: &mut Vec<Ancestor>,
    windows: &mut BTreeMap<i64, PreservedWindow>,
) -> Result<(), Failure> {
    let inside_target = inside_target || node.id == target_workspace_id;
    if is_window(node) && !inside_target {
        let has_workspace = ancestry
            .iter()
            .any(|ancestor| ancestor.node_type == NodeType::Workspace);
        let has_output = ancestry
            .iter()
            .any(|ancestor| ancestor.node_type == NodeType::Output);
        if !has_workspace || !has_output {
            return Err(Failure::Uncertain(format!(
                "foreign window id={} has no workspace/output ancestry",
                node.id
            )));
        }
        if node.fullscreen_mode.is_none()
            || node.floating.is_none()
            || node.scratchpad_state.is_none()
        {
            return Err(Failure::Uncertain(format!(
                "foreign window id={} lacks floating/fullscreen/scratchpad evidence",
                node.id
            )));
        }
        let mut window = signature(node);
        // Focus legitimately changes while entering/leaving the fixture workspace. Every
        // scenario still checks focus and events separately, including away-and-back.
        window.ignore_focus();
        windows.insert(
            node.id,
            PreservedWindow {
                ancestry: ancestry.clone(),
                window,
            },
        );
    }

    let below_workspace = ancestry
        .iter()
        .any(|ancestor| ancestor.node_type == NodeType::Workspace)
        || node.node_type == NodeType::Workspace;
    if node.node_type != NodeType::Root {
        ancestry.push(Ancestor {
            id: node.id,
            node_type: node.node_type,
            name: if matches!(node.node_type, NodeType::Workspace | NodeType::Output) {
                node.name.clone()
            } else {
                None
            },
            layout: node.layout,
            rect: node.rect,
            floating: node.floating,
            fullscreen_mode: node.fullscreen_mode,
            scratchpad_state: node.scratchpad_state,
            tiled_ids: if below_workspace {
                node.nodes.iter().map(|child| child.id).collect()
            } else {
                Vec::new()
            },
            floating_ids: if below_workspace {
                node.floating_nodes.iter().map(|child| child.id).collect()
            } else {
                Vec::new()
            },
        });
    }
    for child in node.nodes.iter().chain(&node.floating_nodes) {
        record_windows(child, target_workspace_id, inside_target, ancestry, windows)?;
    }
    if node.node_type != NodeType::Root {
        ancestry.pop();
    }
    Ok(())
}

/// Frozen containing output identities; never choose an output or mutate one.
pub fn output_ancestry(
    tree: &TreeNode,
    workspace_id: i64,
) -> Result<Vec<(i64, Option<String>)>, Failure> {
    fn find(node: &TreeNode, id: i64, outputs: &mut Vec<(i64, Option<String>)>) -> bool {
        if node.node_type == NodeType::Output {
            outputs.push((node.id, node.name.clone()));
        }
        if node.id == id {
            return node.node_type == NodeType::Workspace;
        }
        for child in node.nodes.iter().chain(&node.floating_nodes) {
            if find(child, id, outputs) {
                return true;
            }
        }
        if node.node_type == NodeType::Output {
            outputs.pop();
        }
        false
    }
    let mut outputs = Vec::new();
    if !find(tree, workspace_id, &mut outputs) || outputs.is_empty() {
        return Err(Failure::Uncertain(format!(
            "workspace id={workspace_id} missing output ancestry"
        )));
    }
    Ok(outputs)
}

#[cfg(test)]
#[path = "preservation_tests.rs"]
mod tests;
