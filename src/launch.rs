//! Sequential launch and guarded, recursive tiled construction.

use crate::config::{Config, GroupLayout, LayoutNode, Matcher, NodeKind};
use crate::diagnostics::{Diagnostics, SizingWarning};
use crate::observation::{
    self, Observer, Snapshot, app_id, class, locate_windows, proportions, sibling_dimensions,
};
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use swayipc::{
    CommandType, Event, Floating, Node, NodeLayout, NodeType, ScratchpadState, WindowChange,
};

pub(crate) const IPC_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_NEW_WINDOW_HISTORY: usize = 256;
const MAX_COMMAND_REPLY_BYTES: usize = 1024 * 1024;
// Shares are fractions of the sibling sum; each bound includes floating-point rounding at
// its exact boundary. Sizing keeps adjusting until every child is within the target, but
// only a residual beyond the tolerance (gaps, borders, minimum sizes) is a warning.
const SIZING_TARGET: f64 = 0.01 + f64::EPSILON;
const SIZING_TOLERANCE: f64 = 0.05 + f64::EPSILON;
const MAX_SIZING_PASSES: usize = 3;
// Less than a tenth of a percentage point of improvement is not worth another pass.
const MIN_SIZING_PROGRESS: f64 = 0.001;
// Focus that moves between this run's containers inside the target workspace, as with
// focus_follows_mouse, is restored at most this many times per focus request or launch.
// Continued movement then stops the run instead of fighting the user's input.
const MAX_FOCUS_RESTORATIONS: usize = 3;

pub fn load(
    config: &Config,
    expected_workspace_id: Option<i64>,
    diagnostics: &mut Diagnostics,
) -> Result<(), String> {
    let mut connect = || {
        let socket = observation::sway_socket()?;
        observation::connect(&socket)
    };
    let sizing_warnings = load_with(config, expected_workspace_id, &mut connect, diagnostics)?;
    diagnostics.report_sizing_warnings(&sizing_warnings);

    Ok(())
}

// Concrete socket injection keeps all fixture traffic disconnected from socket discovery.
fn load_with(
    config: &Config,
    expected_workspace_id: Option<i64>,
    connect: &mut dyn FnMut() -> Result<UnixStream, String>,
    diagnostics: &mut Diagnostics,
) -> Result<Vec<SizingWarning>, String> {
    diagnostics.stage("subscribe");
    let mut observer = Observer::subscribe(connect()?, Instant::now() + IPC_TIMEOUT)?;
    let initial = observer.snapshot_for_workspace(
        connect()?,
        Instant::now() + IPC_TIMEOUT,
        expected_workspace_id,
    )?;
    let target_workspace_id = initial.target_workspace_id;

    diagnostics.stage(format!("prelaunch workspace={target_workspace_id}"));
    require_empty(&initial, diagnostics)?;
    let baseline_foreign_window_ids = locate_windows(&initial.tree)
        .iter()
        .filter(|window| {
            window.workspace_id.is_some() && window.workspace_id != Some(target_workspace_id)
        })
        .map(|window| window.node.id)
        .collect();
    let mut preexisting_container_ids = initial.tree.iter().map(|node| node.id).collect();
    record_prelaunch_events(&initial, &mut preexisting_container_ids)?;

    let workspace = initial
        .tree
        .find_as_ref(|node| node.id == target_workspace_id)
        .ok_or("target missing")?;
    let empty_workspace = ExpectedHierarchy::Group {
        id: Some(target_workspace_id),
        layout: workspace.layout,
        children: Vec::new(),
    };
    let mut session = Construction {
        observer,
        connect,
        diagnostics,
        target_workspace_id,
        preexisting_container_ids,
        baseline_foreign_window_ids,
        accepted_windows: BTreeMap::new(),
        expected_hierarchy: empty_workspace,
        window_timeout_ms: config.window_timeout_ms,
        default_cwd: config.cwd.as_ref(),
    };

    let root_path = session.build_subtree(&config.layout.value, None)?;

    session
        .diagnostics
        .record("construction complete; applying weights from outer splits inward");
    let mut sizing_warnings = Vec::new();
    session.apply_sizing(&config.layout.value, &root_path, &mut sizing_warnings)?;

    session
        .diagnostics
        .stage("initialize tabbed/stacked selections");
    session.initialize_group_selection(&config.layout.value, &root_path)?;

    session.apply_final_focus(&config.layout.value, config.focus.as_deref(), &root_path)?;

    Ok(sizing_warnings)
}

// The workspace is the outer Group. Structural IDs are bound from the first verified
// tree after creating a group, never predicted or retained across a replaced wrapper.
#[derive(Clone, Debug)]
enum ExpectedHierarchy {
    /// A window's container ID.
    Window(i64),
    Group {
        /// `None` until the first verified tree after creating this group supplies its ID.
        id: Option<i64>,
        layout: NodeLayout,
        children: Vec<ExpectedHierarchy>,
    },
}

impl ExpectedHierarchy {
    fn subtree_mut(&mut self, path: &[usize]) -> &mut Self {
        let Some((child_index, rest)) = path.split_first() else {
            return self;
        };
        let Self::Group { children, .. } = self else {
            unreachable!("group path");
        };

        children[*child_index].subtree_mut(rest)
    }

    /// Requires `observed` to match this expectation exactly, binding unknown group IDs.
    fn verify(&mut self, observed: &Node) -> Result<(), String> {
        if !observed.floating_nodes.is_empty()
            || matches!(observed.floating, Some(Floating::AutoOn | Floating::UserOn))
            || matches!(
                observed.scratchpad_state,
                Some(ScratchpadState::Fresh | ScratchpadState::Changed)
            )
        {
            return Err(format!("invalid hierarchy state id={}", observed.id));
        }

        let matches_expected_node = match &*self {
            Self::Window(expected_window_id) => {
                observed.id == *expected_window_id && observed.nodes.is_empty()
            }
            Self::Group {
                id: expected_group_id,
                layout: expected_layout,
                children: expected_children,
            } => {
                expected_group_id.is_none_or(|group_id| group_id == observed.id)
                    && observed.layout == *expected_layout
                    && observed.nodes.len() == expected_children.len()
                    && (observed.node_type == NodeType::Workspace
                        || (observed.node_type == NodeType::Con
                            && observed.fullscreen_mode == Some(0)
                            && observed.app_id.is_none()
                            && observed.window.is_none()
                            && observed.shell.is_none()))
            }
        };
        if !matches_expected_node {
            return Err(format!(
                "unexpected hierarchy at id={} layout={:?} children={:?}",
                observed.id,
                observed.layout,
                observed
                    .nodes
                    .iter()
                    .map(|child| child.id)
                    .collect::<Vec<_>>()
            ));
        }

        if let Self::Group { id, children, .. } = self {
            *id = Some(observed.id);
            for (expected_child, observed_child) in children.iter_mut().zip(&observed.nodes) {
                expected_child.verify(observed_child)?;
            }
        }

        Ok(())
    }

    fn id(&self) -> i64 {
        match self {
            Self::Window(id) | Self::Group { id: Some(id), .. } => *id,
            Self::Group { id: None, .. } => unreachable!("verified structural identity"),
        }
    }
}

struct Construction<'a> {
    observer: Observer,
    connect: &'a mut dyn FnMut() -> Result<UnixStream, String>,
    diagnostics: &'a mut Diagnostics,
    /// Container ID of the frozen target workspace.
    target_workspace_id: i64,
    /// Every container ID observed before the next launch, except accepted windows.
    /// A launch can accept only a window whose ID is absent from this set.
    preexisting_container_ids: HashSet<i64>,
    /// Windows proven foreign by the initial empty-target baseline. Later launches must
    /// not expand this set; contradictory structural events revoke close eligibility.
    baseline_foreign_window_ids: HashSet<i64>,
    /// This run's accepted windows by container ID, with the selector each must keep.
    accepted_windows: BTreeMap<i64, &'a Matcher>,
    expected_hierarchy: ExpectedHierarchy,
    window_timeout_ms: u64,
    default_cwd: Option<&'a PathBuf>,
}

impl<'a> Construction<'a> {
    fn verify(&mut self, snapshot: &Snapshot) -> Result<(), String> {
        validate_accepted_windows(
            snapshot,
            &self.accepted_windows,
            self.target_workspace_id,
            self.diagnostics,
        )?;

        let workspace = snapshot
            .tree
            .find_as_ref(|node| node.id == self.target_workspace_id)
            .ok_or("target missing")?;
        self.diagnostics.record(&format!(
            "expected hierarchy {:?}; observed {}",
            self.expected_hierarchy,
            describe_hierarchy(workspace)
        ));
        self.expected_hierarchy.verify(workspace)
    }

    // No mutation/launch is allowed through a fence with newer window activity.
    fn settled_snapshot(&mut self) -> Result<Snapshot, String> {
        let deadline = Instant::now() + IPC_TIMEOUT;
        loop {
            let snapshot = self.observer.snapshot((self.connect)()?, deadline)?;
            for event in &snapshot.events {
                let Event::Window(window) = event else {
                    continue;
                };
                let known_foreign_close = window.change == WindowChange::Close
                    && self
                        .baseline_foreign_window_ids
                        .contains(&window.container.id)
                    && window.container.nodes.is_empty()
                    && window.container.floating_nodes.is_empty();
                if !self.accepted_windows.contains_key(&window.container.id)
                    && !known_foreign_close
                    && matches!(
                        window.change,
                        WindowChange::New
                            | WindowChange::Close
                            | WindowChange::Move
                            | WindowChange::Floating
                            | WindowChange::FullscreenMode
                    )
                {
                    return Err(format!(
                        "window activity id={} during prelaunch/command fence; ownership is uncertain",
                        window.container.id
                    ));
                }
            }

            self.verify(&snapshot)?;

            self.preexisting_container_ids.extend(
                snapshot
                    .tree
                    .iter()
                    .map(|node| node.id)
                    .filter(|id| !self.accepted_windows.contains_key(id)),
            );
            for event in &snapshot.events {
                if let Event::Window(window) = event
                    && !self.accepted_windows.contains_key(&window.container.id)
                {
                    self.preexisting_container_ids.insert(window.container.id);
                }
            }

            // Harmless events (focus, title, ...) may still be newer than this tree.
            let activity: Vec<_> = snapshot
                .events
                .iter()
                .filter_map(|event| match event {
                    Event::Window(window) => {
                        Some(format!("{:?} id={}", window.change, window.container.id))
                    }
                    _ => None,
                })
                .collect();
            if activity.is_empty() {
                return Ok(snapshot);
            }
            self.diagnostics.record(&format!(
                "window activity during fence: {}; observing again",
                activity.join(", ")
            ));
        }
    }

    /// Runs `commands` as one request between settled, fully verified snapshots and requires
    /// the result to match `expected_after`. A command whose effect depends on focus must
    /// establish that focus earlier in the same request; see `with_workspace_focus`.
    fn guarded_command(
        &mut self,
        commands: &[String],
        expected_after: ExpectedHierarchy,
    ) -> Result<Snapshot, String> {
        self.diagnostics
            .stage(format!("command {}", commands.join("; ")));
        self.settled_snapshot()?;
        run_checked_command(self.connect, self.diagnostics, commands)?;

        self.expected_hierarchy = expected_after;
        let after_command = self.settled_snapshot()?;
        self.diagnostics.record(&format!(
            "verified structural identities {:?}",
            self.expected_hierarchy
        ));

        Ok(after_command)
    }

    /// Focuses an owned container and returns a settled snapshot that shows it focused.
    fn focus_container(&mut self, path: &[usize]) -> Result<Snapshot, String> {
        let container_id = self.expected_hierarchy.subtree_mut(path).id();
        let mut after_command = self.guarded_command(
            &[format!("[con_id={container_id}] focus")],
            self.expected_hierarchy.clone(),
        )?;

        let mut focus_restorations = 0;
        while !is_focused(&after_command.tree, container_id) {
            restore_focus(
                self.connect,
                self.diagnostics,
                container_id,
                &mut focus_restorations,
            )?;
            after_command = self.settled_snapshot()?;
        }

        Ok(after_command)
    }

    /// Prefixes a criteria-free command so that it acts on the frozen workspace itself.
    /// Criteria cannot select a workspace, and `layout` or `split` given a child container
    /// acts on that child or wraps the workspace's children instead, so focus the workspace
    /// through its verified direct child. Sway handles no input between one request's
    /// commands, so pointer focus cannot redirect the final command as it could between
    /// separate requests. The request is still not atomic: Sway runs the final command even
    /// after the criteria commands fail. If the child closes or moves just before Sway runs
    /// the request, that command acts on whatever is focused then, possibly outside the
    /// target. Checking every reply and the resulting hierarchy only detects this afterward.
    fn with_workspace_focus(&mut self, criteria_free_command: &str) -> Vec<String> {
        let direct_child_id = self.expected_hierarchy.subtree_mut(&[0]).id();
        vec![
            format!("[con_id={direct_child_id}] focus"),
            format!("[con_id={direct_child_id}] focus parent"),
            criteria_free_command.to_owned(),
        ]
    }

    fn initialize_group_selection(
        &mut self,
        layout_node: &LayoutNode,
        path: &[usize],
    ) -> Result<(), String> {
        let NodeKind::Group {
            layout, children, ..
        } = &layout_node.kind
        else {
            return Ok(());
        };

        for (child_index, child) in children.iter().enumerate() {
            let mut child_path = path.to_vec();
            child_path.push(child_index);
            self.initialize_group_selection(&child.value, &child_path)?;
        }

        // Focusing a descendant also selects its ancestors. Restore this group after
        // visiting every child, following nested active selections so they stay intact.
        // Inactive groups keep their selections when focus leaves their subtree.
        if matches!(layout, GroupLayout::Tabbed | GroupLayout::Stacked) {
            let (_, selected_leaf_path) = find_focus_leaf(layout_node, None, path)
                .ok_or("validated group has no selected leaf")?;
            let snapshot = self.focus_container(&selected_leaf_path)?;
            let group_id = self.expected_hierarchy.subtree_mut(path).id();
            let observed_group = snapshot
                .tree
                .find_as_ref(|node| node.id == group_id)
                .ok_or("selection group missing")?;
            verify_group_selections(layout_node, observed_group, None)?;
        }

        Ok(())
    }

    fn apply_final_focus(
        &mut self,
        layout_node: &LayoutNode,
        configured_focus: Option<&str>,
        root_path: &[usize],
    ) -> Result<(), String> {
        let (focus_leaf, focus_path) = find_focus_leaf(layout_node, configured_focus, root_path)
            .ok_or("validated final focus window missing from layout")?;
        let focus_id = self.expected_hierarchy.subtree_mut(&focus_path).id();
        self.diagnostics
            .stage(format!("final focus leaf={focus_leaf:?} id={focus_id}"));
        let snapshot = self.focus_container(&focus_path)?;

        // The same guarded snapshot proves ownership, hierarchy, global focus, and
        // inactive selections. Explicit focus overrides active only on its ancestry.
        let root_id = self.expected_hierarchy.subtree_mut(root_path).id();
        let observed_root = snapshot
            .tree
            .find_as_ref(|node| node.id == root_id)
            .ok_or("layout root missing during selection verification")?;
        let explicit_focus_path = configured_focus.map(|_| &focus_path[root_path.len()..]);
        verify_group_selections(layout_node, observed_root, explicit_focus_path)?;
        self.diagnostics.record(&format!(
            "final focus verified leaf={focus_leaf:?} id={focus_id} workspace={}; group selections verified",
            self.target_workspace_id
        ));

        Ok(())
    }

    /// Applies split weights and recurses through every group, including tabs/stacks.
    /// Residual deviations become warnings; IPC, ownership and structural failures remain fatal.
    fn apply_sizing(
        &mut self,
        layout_node: &LayoutNode,
        path: &[usize],
        warnings: &mut Vec<SizingWarning>,
    ) -> Result<(), String> {
        let NodeKind::Group {
            layout, children, ..
        } = &layout_node.kind
        else {
            return Ok(());
        };

        match layout {
            GroupLayout::SplitH => self.apply_split_sizing(children, path, "width", warnings)?,
            GroupLayout::SplitV => self.apply_split_sizing(children, path, "height", warnings)?,
            GroupLayout::Tabbed | GroupLayout::Stacked => {}
        }

        for (child_index, child) in children.iter().enumerate() {
            let mut child_path = path.to_vec();
            child_path.push(child_index);
            self.apply_sizing(&child.value, &child_path, warnings)?;
        }

        Ok(())
    }

    fn apply_split_sizing(
        &mut self,
        children: &[serde_saphyr::Spanned<LayoutNode>],
        path: &[usize],
        axis: &str,
        warnings: &mut Vec<SizingWarning>,
    ) -> Result<(), String> {
        let group_id = self.expected_hierarchy.subtree_mut(path).id();
        let weights: Vec<_> = children.iter().map(|child| child.value.weight()).collect();
        let target_shares = proportions(&weights);

        self.diagnostics
            .stage(format!("sizing group={group_id} path={path:?} axis={axis}"));
        self.diagnostics.record(&format!(
            "weights={weights:?}; targets={}; aim within 1 percentage point, warn beyond 5",
            format_shares(&target_shares)
        ));

        let mut snapshot = self.settled_snapshot()?;
        let mut unavailable_geometry = None;
        for sizing_pass_index in 0..MAX_SIZING_PASSES {
            let geometry = sibling_geometry(&snapshot.tree, group_id);
            let (sibling_extents_pixels, observed_shares) = match geometry {
                Ok(geometry) => geometry,
                Err(reason) => {
                    unavailable_geometry = Some(reason);
                    break;
                }
            };

            let error_before_pass = max_share_error(&target_shares, &observed_shares);
            self.diagnostics.record(&format!(
                "pass={} before dimensions={sibling_extents_pixels:?}; observed={}",
                sizing_pass_index + 1,
                format_shares(&observed_shares)
            ));
            if error_before_pass <= SIZING_TARGET {
                break;
            }

            // resize set changes every other sibling as well. Recalculate each next
            // target from the resulting tree, never from a stale percent property.
            for child_index in 0..children.len() {
                let geometry = sibling_geometry(&snapshot.tree, group_id);
                let (sibling_extents_pixels, observed_shares) = match geometry {
                    Ok(geometry) => geometry,
                    Err(reason) => {
                        unavailable_geometry = Some(reason);
                        break;
                    }
                };
                if max_share_error(&target_shares, &observed_shares) <= SIZING_TARGET {
                    break;
                }

                let child_error = (target_shares[child_index] - observed_shares[child_index]).abs();
                if child_error <= SIZING_TARGET {
                    continue;
                }

                let sibling_total_pixels: f64 = sibling_extents_pixels.iter().sum();
                let target_extent_pixels =
                    (target_shares[child_index] * sibling_total_pixels).round();
                // Zero means "leave unchanged" to resize set. Do not invent a
                // minimum size or override Sway/application constraints.
                if target_extent_pixels < 1.0
                    || target_extent_pixels == sibling_extents_pixels[child_index]
                {
                    continue;
                }

                let mut child_path = path.to_vec();
                child_path.push(child_index);
                let child_id = self.expected_hierarchy.subtree_mut(&child_path).id();
                snapshot = self.guarded_command(
                    &[format!(
                        "[con_id={child_id}] resize set {axis} {target_extent_pixels:.0} px"
                    )],
                    self.expected_hierarchy.clone(),
                )?;

                match sibling_geometry(&snapshot.tree, group_id) {
                    Ok((_, observed_shares)) => self.diagnostics.record(&format!(
                        "pass={} after child={child_id}; observed={}",
                        sizing_pass_index + 1,
                        format_shares(&observed_shares)
                    )),
                    Err(reason) => {
                        unavailable_geometry = Some(reason);
                        break;
                    }
                }
            }

            if unavailable_geometry.is_some() {
                break;
            }

            let (_, observed_shares) = sibling_geometry(&snapshot.tree, group_id)?;
            let error_after_pass = max_share_error(&target_shares, &observed_shares);
            if error_after_pass <= SIZING_TARGET {
                break;
            }

            // Includes unchanged geometry at minimum sizes and cycles that return
            // to the same error. Never repeat a pass without meaningful progress.
            if error_before_pass - error_after_pass < MIN_SIZING_PROGRESS {
                self.diagnostics
                    .record("stopping sizing: no meaningful progress");
                break;
            }
        }

        let geometry_unverified = unavailable_geometry.is_some();
        let residual = if let Some(reason) = unavailable_geometry {
            Some(format!("observed=unavailable ({reason})"))
        } else {
            let (_, observed_shares) = sibling_geometry(&snapshot.tree, group_id)?;
            (max_share_error(&target_shares, &observed_shares) > SIZING_TOLERANCE)
                .then(|| format!("observed={}", format_shares(&observed_shares)))
        };
        if let Some(residual) = residual {
            let warning = format!(
                "sizing group={group_id} path={path:?} axis={axis}: targets={}; {residual}; bounded adjustment stopped (tolerance 5 percentage points)",
                format_shares(&target_shares)
            );
            self.diagnostics.record(&format!("warning: {warning}"));
            warnings.push(SizingWarning {
                detail: warning,
                geometry_unverified,
            });
        }

        Ok(())
    }

    /// Builds a layout subtree after the sibling at `insert_after`, or as the workspace's
    /// first content, and returns the subtree's path in the expected hierarchy.
    fn build_subtree(
        &mut self,
        layout_node: &'a LayoutNode,
        insert_after: Option<Vec<usize>>,
    ) -> Result<Vec<usize>, String> {
        let NodeKind::Group {
            layout, children, ..
        } = &layout_node.kind
        else {
            return self.launch_window(layout_node, insert_after);
        };

        let (group_layout, layout_command, split_command) = match layout {
            GroupLayout::SplitH => (NodeLayout::SplitH, "splith", "splith"),
            GroupLayout::SplitV => (NodeLayout::SplitV, "splitv", "splitv"),
            GroupLayout::Tabbed => (NodeLayout::Tabbed, "tabbed", "splith"),
            GroupLayout::Stacked => (NodeLayout::Stacked, "stacking", "splith"),
        };
        let split_layout = match layout {
            GroupLayout::SplitV => NodeLayout::SplitV,
            _ => NodeLayout::SplitH,
        };

        let first_path = self.build_subtree(&children[0].value, insert_after)?;

        let mut expected_after = self.expected_hierarchy.clone();
        let group_path;
        let commands;
        if first_path.is_empty() {
            // The first completed child occupies the workspace. workspace_split wraps
            // its populated children, preserving even equal-axis meaningful groups.
            // The split has no criteria and acts on the focused workspace.
            let ExpectedHierarchy::Group {
                layout: old_layout,
                children: old_children,
                ..
            } = expected_after
            else {
                unreachable!()
            };
            let wrapped_children = ExpectedHierarchy::Group {
                id: None,
                layout: old_layout,
                children: old_children,
            };
            expected_after = ExpectedHierarchy::Group {
                id: Some(self.target_workspace_id),
                layout: split_layout,
                children: vec![wrapped_children],
            };
            commands = self.with_workspace_focus(split_command);
            group_path = Vec::new();
        } else {
            let first_id = self.expected_hierarchy.subtree_mut(&first_path).id();
            let parent_path = &first_path[..first_path.len() - 1];
            let ExpectedHierarchy::Group {
                layout: parent_layout,
                children: siblings,
                ..
            } = expected_after.subtree_mut(parent_path)
            else {
                unreachable!()
            };
            let parent_has_one_child = siblings.len() == 1;
            let parent_is_split = matches!(parent_layout, NodeLayout::SplitH | NodeLayout::SplitV);

            if parent_has_one_child {
                // Only the first subtree can occupy its entire parent here, and that
                // parent is the workspace. A leaf-targeted layout command would wrap
                // workspace children instead of changing the workspace's own layout.
                *parent_layout = group_layout;
                group_path = parent_path.to_vec();
                if parent_is_split && matches!(layout, GroupLayout::SplitH | GroupLayout::SplitV) {
                    commands = vec![format!("[con_id={first_id}] {split_command}")];
                } else {
                    commands = self.with_workspace_focus(&format!("layout {layout_command}"));
                }
            } else {
                let first_child_hierarchy = expected_after.subtree_mut(&first_path).clone();
                *expected_after.subtree_mut(&first_path) = ExpectedHierarchy::Group {
                    id: None,
                    layout: split_layout,
                    children: vec![first_child_hierarchy],
                };
                group_path = first_path;
                commands = vec![format!("[con_id={first_id}] {split_command}")];
            }
        }

        self.diagnostics.record(&format!(
            "expected group/reparent transition {:?} -> {expected_after:?}",
            self.expected_hierarchy
        ));
        self.guarded_command(&commands, expected_after)?;

        let ExpectedHierarchy::Group {
            layout: observed_layout,
            ..
        } = self.expected_hierarchy.subtree_mut(&group_path)
        else {
            unreachable!("constructed group");
        };
        if *observed_layout != group_layout {
            // A split command first creates the exact wrapper. layout operates on its
            // child's parent. This parent's siblings number at least two, preventing
            // layout.c's singleton-grandparent flattening. At the root, operate on the
            // workspace itself so layout cannot introduce an extra workspace wrapper.
            let commands = if group_path.is_empty() {
                self.with_workspace_focus(&format!("layout {layout_command}"))
            } else {
                let mut first_child_path = group_path.clone();
                first_child_path.push(0);
                let first_child_id = self.expected_hierarchy.subtree_mut(&first_child_path).id();
                vec![format!("[con_id={first_child_id}] layout {layout_command}")]
            };

            let mut expected_after = self.expected_hierarchy.clone();
            let ExpectedHierarchy::Group { layout, .. } = expected_after.subtree_mut(&group_path)
            else {
                unreachable!("constructed group");
            };
            *layout = group_layout;
            self.guarded_command(&commands, expected_after)?;
        }

        for (child_index, child) in children.iter().enumerate().skip(1) {
            let mut previous_sibling_path = group_path.clone();
            previous_sibling_path.push(child_index - 1);
            self.build_subtree(&child.value, Some(previous_sibling_path))?;
        }

        Ok(group_path)
    }

    /// Launches one window leaf after the sibling at `insert_after` and returns its path
    /// once fresh trees confirm the new window in the expected hierarchy.
    fn launch_window(
        &mut self,
        layout_node: &'a LayoutNode,
        insert_after: Option<Vec<usize>>,
    ) -> Result<Vec<usize>, String> {
        let NodeKind::Window {
            id: configured_window_id,
            command,
            matcher,
            cwd,
            env,
        } = &layout_node.kind
        else {
            unreachable!()
        };

        // Sway maps a new window beside whichever container is focused at that moment, so
        // insertion focus must hold from the prelaunch fence until the window appears.
        // One restoration budget covers that fence and the whole wait.
        let insertion_container_id = insert_after
            .as_ref()
            .map(|previous_path| self.expected_hierarchy.subtree_mut(previous_path).id());
        if let Some(previous_path) = &insert_after {
            self.focus_container(previous_path)?;
        }

        let mut insertion_focus_restorations = 0;
        loop {
            self.diagnostics.stage(format!(
                "prelaunch leaf={configured_window_id:?} workspace={}",
                self.target_workspace_id
            ));
            let prelaunch_snapshot = self.settled_snapshot()?;
            if let Some(insertion_id) = insertion_container_id
                && !is_focused(&prelaunch_snapshot.tree, insertion_id)
            {
                restore_focus(
                    self.connect,
                    self.diagnostics,
                    insertion_id,
                    &mut insertion_focus_restorations,
                )?;
            } else {
                break;
            }
        }

        let mut window_path = insert_after.unwrap_or_else(|| vec![0]);
        if !self.accepted_windows.is_empty() {
            *window_path.last_mut().expect("sibling path") += 1;
        }

        let mut new_windows = NewWindowTracker {
            target_workspace_id: self.target_workspace_id,
            preexisting_container_ids: self.preexisting_container_ids.clone(),
            baseline_foreign_window_ids: &mut self.baseline_foreign_window_ids,
            matcher,
            accepted_windows: &self.accepted_windows,
            observed_windows: BTreeMap::new(),
            matched_window_id: None,
            disqualified_windows: BTreeMap::new(),
        };
        let launch_started_at = Instant::now();
        let window_deadline = launch_started_at
            .checked_add(Duration::from_millis(self.window_timeout_ms))
            .ok_or("window_timeout_ms exceeds the monotonic clock range; use a smaller timeout")?;

        self.diagnostics.stage(format!(
            "launch leaf={configured_window_id:?} workspace={}",
            self.target_workspace_id
        ));
        self.diagnostics.record(&format!(
            "expected {matcher:?}; launching once (argv/env omitted)"
        ));
        let effective_cwd = cwd.as_ref().or(self.default_cwd);
        let mut process = spawn(command, effective_cwd, env)?;
        // From here on, a failure may leave this application and a partial layout behind.
        self.diagnostics.spawned(process.id());

        let mut process_exit_status = None;
        let mut provisional_window_id = None;
        let mut expected_after = self.expected_hierarchy.clone();
        loop {
            let stage = if provisional_window_id.is_some() {
                "final verification"
            } else {
                "wait"
            };
            self.diagnostics
                .stage(format!("{stage} leaf={configured_window_id:?}"));
            if Instant::now() >= window_deadline {
                return Err(self.diagnostics.window_timeout(
                    &new_windows.wait_diagnostics(launch_started_at),
                    process_exit_status,
                ));
            }

            let snapshot = self
                .observer
                .snapshot(
                    (self.connect)()?,
                    window_deadline.min(Instant::now() + IPC_TIMEOUT),
                )
                .map_err(|error| {
                    format!(
                        "{error}; {}",
                        new_windows.wait_diagnostics(launch_started_at)
                    )
                })?;

            let was_previously_provisional = provisional_window_id.is_some();
            let candidate_window_id = new_windows.reconcile(&snapshot, self.diagnostics)?;
            if let Some(provisional_window_id) = provisional_window_id {
                if candidate_window_id != Some(provisional_window_id) {
                    return Err(format!(
                        "accepted window id={provisional_window_id} disappeared or failed revalidation"
                    ));
                }
            } else if let Some(candidate_window_id) = candidate_window_id {
                let parent_path = &window_path[..window_path.len() - 1];
                let ExpectedHierarchy::Group { children, .. } =
                    expected_after.subtree_mut(parent_path)
                else {
                    unreachable!()
                };
                children.insert(
                    *window_path.last().expect("leaf path"),
                    ExpectedHierarchy::Window(candidate_window_id),
                );
                provisional_window_id = Some(candidate_window_id);
                self.diagnostics.record(&format!(
                    "provisionally accepted leaf={configured_window_id:?} id={candidate_window_id}; rechecking fresh state"
                ));
            }

            if let Some(provisional_window_id) = provisional_window_id {
                let workspace = snapshot
                    .tree
                    .find_as_ref(|node| node.id == self.target_workspace_id)
                    .ok_or("target missing")?;
                self.diagnostics.record(&format!(
                    "expected hierarchy {expected_after:?}; observed {}",
                    describe_hierarchy(workspace)
                ));
                expected_after.verify(workspace)?;

                // Accept only from a second fresh tree with no newer window activity.
                let window_activity = snapshot
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::Window(_)));
                if !window_activity && was_previously_provisional {
                    self.diagnostics.record(&format!(
                        "accepted leaf={configured_window_id:?} id={provisional_window_id}; hierarchy/identity verified"
                    ));
                    reap_if_exited(&mut process, &mut process_exit_status, self.diagnostics)?;
                    self.accepted_windows.insert(provisional_window_id, matcher);
                    self.expected_hierarchy = expected_after;
                    return Ok(window_path);
                }

                // Recheck a newly provisional window at once instead of polling first.
                if !was_previously_provisional {
                    continue;
                }
            }

            if provisional_window_id.is_none()
                && let Some(insertion_id) = insertion_container_id
                && !is_focused(&snapshot.tree, insertion_id)
            {
                // An unchanged target hierarchy proves both that the new window has not
                // mapped there yet and that the newly focused node belongs to this run.
                // Any difference is left to reconciliation and hierarchy verification.
                let workspace = snapshot
                    .tree
                    .find_as_ref(|node| node.id == self.target_workspace_id)
                    .ok_or("target missing")?;
                if self.expected_hierarchy.clone().verify(workspace).is_ok() {
                    restore_focus(
                        self.connect,
                        self.diagnostics,
                        insertion_id,
                        &mut insertion_focus_restorations,
                    )?;
                    continue;
                }
            }

            reap_if_exited(&mut process, &mut process_exit_status, self.diagnostics)?;
            let poll_until = window_deadline.min(Instant::now() + POLL_INTERVAL);
            while let Some(event) = self.observer.next_until(poll_until)? {
                new_windows.observe_event(&event, self.diagnostics)?;
                // Observe again at once rather than finishing the poll, so moved
                // insertion focus can be restored before the window maps.
                if insertion_container_id.is_some()
                    && matches!(&event, Event::Window(window) if window.change == WindowChange::Focus)
                {
                    break;
                }
            }
        }
    }
}

fn max_share_error(target_shares: &[f64], observed_shares: &[f64]) -> f64 {
    target_shares
        .iter()
        .zip(observed_shares)
        .map(|(target_share, observed_share)| (target_share - observed_share).abs())
        .fold(0.0, f64::max)
}

fn format_shares(shares: &[f64]) -> String {
    format!(
        "[{}]",
        shares
            .iter()
            .map(|share| format!("{:.2}%", share * 100.0))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The group's sibling dimensions along its split axis, and their shares of the sum.
/// Call only after verifying the complete owned hierarchy. Verification also fixes the
/// group's observed layout, and therefore its measured axis, to the configured split.
fn sibling_geometry(tree: &Node, group_id: i64) -> Result<(Vec<f64>, Vec<f64>), String> {
    let group = tree
        .find_as_ref(|node| node.id == group_id)
        .ok_or("sizing group missing")?;
    let dimensions = sibling_dimensions(group)?;
    if dimensions.iter().sum::<f64>() > f64::from(i32::MAX) {
        return Err("sibling dimensions exceed Sway's integer resize range".into());
    }
    let shares = proportions(&dimensions);
    Ok((dimensions, shares))
}

/// The explicit focus window, or the leaf reached through default child selections.
fn find_focus_leaf<'a>(
    layout_node: &'a LayoutNode,
    configured_focus: Option<&str>,
    path: &[usize],
) -> Option<(&'a str, Vec<usize>)> {
    match &layout_node.kind {
        NodeKind::Window { id, .. } => configured_focus
            .is_none_or(|focus| focus == id)
            .then(|| (id.as_str(), path.to_vec())),
        NodeKind::Group { children, .. } => {
            for (child_index, child) in children.iter().enumerate() {
                if configured_focus.is_none()
                    && child_index != layout_node.default_focus_child_index()
                {
                    continue;
                }
                let mut child_path = path.to_vec();
                child_path.push(child_index);
                if let Some(found) = find_focus_leaf(&child.value, configured_focus, &child_path) {
                    return Some(found);
                }
            }
            None
        }
    }
}

/// Call only with a hierarchy already verified against the complete owned layout.
/// Sway's focus history lists direct children, even in inactive tab/stack containers.
fn verify_group_selections(
    layout_node: &LayoutNode,
    observed: &Node,
    explicit_focus_path: Option<&[usize]>,
) -> Result<(), String> {
    let NodeKind::Group {
        layout, children, ..
    } = &layout_node.kind
    else {
        return Ok(());
    };
    let focus_child = explicit_focus_path.and_then(|path| path.split_first());
    if matches!(layout, GroupLayout::Tabbed | GroupLayout::Stacked) {
        let selected_child_index = focus_child
            .map(|(child_index, _)| *child_index)
            .unwrap_or_else(|| layout_node.default_focus_child_index());
        let selected_child_id = observed.nodes[selected_child_index].id;
        if observed.focus.first() != Some(&selected_child_id) {
            return Err(format!(
                "group selection id={}: expected child {} id={selected_child_id}; observed child-focus history={:?}",
                observed.id,
                selected_child_index + 1,
                observed.focus
            ));
        }
    }

    for (child_index, (child, observed_child)) in children.iter().zip(&observed.nodes).enumerate() {
        let child_focus_path = focus_child.and_then(|(focus_child_index, rest)| {
            (*focus_child_index == child_index).then_some(rest)
        });
        verify_group_selections(&child.value, observed_child, child_focus_path)?;
    }

    Ok(())
}

/// A disposable command connection is cancelled on expiry; never retry a command
/// whose reply was lost because Sway may already have executed it.
pub(crate) fn run_command(
    socket: UnixStream,
    command: String,
    deadline: Instant,
) -> Result<Vec<Result<(), String>>, String> {
    let shutdown_socket = socket
        .try_clone()
        .map_err(|error| format!("clone command socket: {error}"))?;

    let (sender, receiver) = mpsc::channel();
    let reader = thread::Builder::new()
        .name("sway-command".into())
        .spawn(move || {
            // swayipc 4.0's run_command drops the success flag. Retain it using
            // its typed decoder, with a bounded frame reader for this command only.
            let exchange = || -> Result<Vec<Result<(), String>>, String> {
                #[derive(serde::Deserialize)]
                struct Reply {
                    success: bool,
                    error: Option<String>,
                }

                let mut socket = socket;
                socket
                    .write_all(&CommandType::RunCommand.encode_with(command.as_bytes()))
                    .map_err(|error| format!("command IPC write: {error}"))?;

                let mut header = [0; 14];
                socket
                    .read_exact(&mut header)
                    .map_err(|error| format!("command IPC header: {error}"))?;
                if &header[..6] != b"i3-ipc" {
                    return Err("command IPC: invalid reply magic".into());
                }
                let payload_size_bytes =
                    u32::from_ne_bytes(header[6..10].try_into().expect("header size")) as usize;
                let reply_type =
                    u32::from_ne_bytes(header[10..14].try_into().expect("header type"));
                if reply_type != 0 || payload_size_bytes > MAX_COMMAND_REPLY_BYTES {
                    return Err("command IPC: invalid reply type or reply exceeds 1 MiB".into());
                }

                let mut payload = vec![0; payload_size_bytes];
                socket
                    .read_exact(&mut payload)
                    .map_err(|error| format!("command IPC payload: {error}"))?;
                let replies: Vec<Reply> = CommandType::RunCommand
                    .decode((reply_type, payload))
                    .map_err(|error| format!("command IPC decode: {error}"))?;
                let mut command_results = Vec::with_capacity(replies.len());
                for reply in replies {
                    let command_result = if !reply.success || reply.error.is_some() {
                        Err(reply
                            .error
                            .unwrap_or_else(|| "Sway reported success=false".into()))
                    } else {
                        Ok(())
                    };
                    command_results.push(command_result);
                }

                Ok(command_results)
            };
            let _ = sender.send(exchange());
        })
        .map_err(|error| format!("start command reader: {error}"))?;

    let command_result = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let _ = shutdown_socket.shutdown(Shutdown::Both);
    if reader.join().is_err() {
        return Err("command reader panicked".into());
    }

    match command_result {
        Ok(Ok(replies)) => Ok(replies),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(format!("command deadline/channel failure: {error}")),
    }
}

/// Sends `commands` as one request and requires exactly one success reply per command.
/// Sway replies once for every command it runs and stops early only after an invalid one,
/// so any other reply count means the request was not handled as sent.
fn run_checked_command(
    connect: &mut dyn FnMut() -> Result<UnixStream, String>,
    diagnostics: &mut Diagnostics,
    commands: &[String],
) -> Result<(), String> {
    let request = commands.join("; ");
    let replies = run_command(connect()?, request, Instant::now() + IPC_TIMEOUT)?;
    diagnostics.record(&format!("command replies: {replies:?}"));

    if replies.iter().any(Result::is_err) {
        return Err(format!("Sway command failed: {replies:?}"));
    }
    if replies.len() != commands.len() {
        return Err(format!(
            "Sway command failed: reply count {} differs from command count {}: {replies:?}",
            replies.len(),
            commands.len()
        ));
    }

    Ok(())
}

/// Refocuses `container_id` after focus moved to another node inside the target workspace,
/// as focus_follows_mouse does. Call only right after a snapshot verified the complete,
/// owned target hierarchy, so the newly focused node also belongs to this run; the
/// observer has already failed on any workspace switch. The caller verifies the result
/// with a fresh snapshot. Exhausting the restoration budget is fatal.
fn restore_focus(
    connect: &mut dyn FnMut() -> Result<UnixStream, String>,
    diagnostics: &mut Diagnostics,
    container_id: i64,
    focus_restorations: &mut usize,
) -> Result<(), String> {
    if *focus_restorations == MAX_FOCUS_RESTORATIONS {
        return Err(format!(
            "focus did not stay on container id={container_id} after {MAX_FOCUS_RESTORATIONS} restorations within the target workspace"
        ));
    }
    *focus_restorations += 1;

    let command = format!("[con_id={container_id}] focus");
    diagnostics.stage(format!("restore focus {command}"));
    diagnostics.record(&format!(
        "focus moved within the target workspace; restoring id={container_id} (restoration {} of {MAX_FOCUS_RESTORATIONS})",
        *focus_restorations
    ));
    run_checked_command(connect, diagnostics, &[command])
}

fn is_focused(tree: &Node, container_id: i64) -> bool {
    tree.find_as_ref(|node| node.id == container_id)
        .is_some_and(|node| node.focused)
}

fn describe_hierarchy(node: &Node) -> String {
    if node.nodes.is_empty() {
        return node.id.to_string();
    }

    format!(
        "{}:{:?}[{}]",
        node.id,
        node.layout,
        node.nodes
            .iter()
            .map(describe_hierarchy)
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn validate_accepted_windows(
    snapshot: &Snapshot,
    accepted_windows: &BTreeMap<i64, &Matcher>,
    target_workspace_id: i64,
    diagnostics: &mut Diagnostics,
) -> Result<(), String> {
    for event in &snapshot.events {
        reject_unsafe_window_event(event, accepted_windows, diagnostics)?;
    }

    let windows = locate_windows(&snapshot.tree);
    for (accepted_window_id, matcher) in accepted_windows {
        let located = windows
            .iter()
            .find(|window| window.node.id == *accepted_window_id)
            .ok_or_else(|| format!("accepted window id={accepted_window_id} disappeared"))?;
        if located.workspace_id != Some(target_workspace_id)
            || located.floating
            || located.fullscreen
            || located.scratchpad
            || located.node.fullscreen_mode != Some(0)
            || !matches_window(matcher, located.node)
        {
            return Err(format!(
                "accepted window id={accepted_window_id} lost placement, identity or tiled state"
            ));
        }
    }

    Ok(())
}

/// The node's identifier of the selector's own kind: an app ID never satisfies a class
/// selector, nor a class an app ID selector.
fn selector_identity<'a>(matcher: &Matcher, node: &'a Node) -> Option<&'a str> {
    match matcher {
        Matcher::AppId(_) => app_id(node),
        Matcher::Class(_) => class(node),
    }
}

/// Exact, case-sensitive equality. Validated selectors are never empty.
fn matches_window(matcher: &Matcher, node: &Node) -> bool {
    let selector = match matcher {
        Matcher::AppId(value) | Matcher::Class(value) => value,
    };
    selector_identity(matcher, node) == Some(selector.as_str())
}

/// Rejects any window move, and any window event showing that an accepted window (or a
/// container holding one) closed, reappeared as new, or changed floating, fullscreen or
/// identity state.
fn reject_unsafe_window_event(
    event: &Event,
    accepted_windows: &BTreeMap<i64, &Matcher>,
    diagnostics: &mut Diagnostics,
) -> Result<(), String> {
    let Event::Window(window) = event else {
        return Ok(());
    };
    let node = &window.container;

    // split/container_split reparents without emitting window::move. No ancestry in
    // a move payload can prove it stayed on target, even during our command fence.
    if window.change == WindowChange::Move {
        diagnostics.record(&format!(
            "unexpected move id={}; no safe ancestry history",
            node.id
        ));
        return Err(format!(
            "window move id={}; target safety is uncertain",
            node.id
        ));
    }

    let holds_accepted = node
        .iter()
        .any(|contained| accepted_windows.contains_key(&contained.id));
    if holds_accepted
        && matches!(
            window.change,
            WindowChange::Close
                | WindowChange::Floating
                | WindowChange::FullscreenMode
                | WindowChange::New
        )
    {
        return Err(format!(
            "accepted window/group id={} closed or changed floating/fullscreen/ownership state",
            node.id
        ));
    }

    if let Some(matcher) = accepted_windows.get(&node.id)
        && (window.change == WindowChange::Close
            || matches!(node.floating, Some(Floating::AutoOn | Floating::UserOn))
            || node.fullscreen_mode != Some(0)
            || !matches_window(matcher, node))
    {
        return Err(format!(
            "accepted window id={} closed or changed identity/floating/fullscreen state",
            node.id
        ));
    }

    Ok(())
}

fn spawn(
    argv: &[String],
    cwd: Option<&PathBuf>,
    environment: &BTreeMap<String, String>,
) -> Result<Child, String> {
    let parent_cwd =
        env::current_dir().map_err(|error| format!("read current directory: {error}"))?;
    let effective_cwd = cwd.map_or_else(|| parent_cwd.clone(), |cwd| parent_cwd.join(cwd));
    // Resolve explicit relative executables ourselves: Command's relative-program plus
    // current_dir behavior is platform dependent. Bare names retain child PATH lookup.
    let executable = if argv[0].contains('/') {
        effective_cwd.join(&argv[0])
    } else {
        PathBuf::from(&argv[0])
    };

    Command::new(executable)
        .args(&argv[1..])
        .current_dir(effective_cwd)
        .envs(environment)
        // A terminal interrupt directed at the launcher must not also kill the app.
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!("spawn application: {error}; check executable, child PATH and cwd")
        })
}

fn reap_if_exited(
    process: &mut Child,
    process_exit_status: &mut Option<ExitStatus>,
    diagnostics: &mut Diagnostics,
) -> Result<(), String> {
    if process_exit_status.is_none()
        && let Some(status) = process
            .try_wait()
            .map_err(|error| format!("inspect child: {error}"))?
    {
        *process_exit_status = Some(status);
        diagnostics.record(&format!(
            "launch process exited: {status}; continuing window observation (delegation is allowed)"
        ));
    }

    Ok(())
}

fn require_empty(snapshot: &Snapshot, diagnostics: &mut Diagnostics) -> Result<(), String> {
    let workspace = snapshot
        .tree
        .find_as_ref(|node| node.id == snapshot.target_workspace_id)
        .ok_or("target workspace missing")?;
    diagnostics.record(&format!(
        "selected workspace id={}; initial occupancy: tiled={} floating={}",
        snapshot.target_workspace_id,
        workspace.nodes.len(),
        workspace.floating_nodes.len()
    ));
    if !workspace.nodes.is_empty() || !workspace.floating_nodes.is_empty() {
        return Err("target workspace must be completely empty, including tiled/floating containers; choose an empty disposable workspace".into());
    }

    Ok(())
}

/// Adds window IDs from the initial fence history to `preexisting_container_ids`.
/// Structural window activity makes the empty-workspace observation uncertain.
fn record_prelaunch_events(
    snapshot: &Snapshot,
    preexisting_container_ids: &mut HashSet<i64>,
) -> Result<(), String> {
    for event in &snapshot.events {
        if let Event::Window(window) = event {
            preexisting_container_ids.insert(window.container.id);
            if matches!(
                window.change,
                WindowChange::New
                    | WindowChange::Move
                    | WindowChange::Floating
                    | WindowChange::FullscreenMode
            ) {
                return Err(format!(
                    "window activity id={} during prelaunch fence; empty-workspace history is uncertain",
                    window.container.id
                ));
            }
        }
    }

    Ok(())
}

/// Windows that appeared after one launch's prelaunch snapshot. Decides whether exactly
/// one of them, unclaimed and with the leaf's exact selector, can be accepted for the leaf.
struct NewWindowTracker<'a> {
    target_workspace_id: i64,
    preexisting_container_ids: HashSet<i64>,
    baseline_foreign_window_ids: &'a mut HashSet<i64>,
    matcher: &'a Matcher,
    accepted_windows: &'a BTreeMap<i64, &'a Matcher>,
    /// Each new window's latest description, for logs and timeout diagnostics.
    observed_windows: BTreeMap<i64, String>,
    /// The single new window whose identity has matched the selector, if any.
    matched_window_id: Option<i64>,
    /// New windows whose observed history rules out acceptance, with the reason.
    disqualified_windows: BTreeMap<i64, &'static str>,
}

impl NewWindowTracker<'_> {
    fn observe_window(
        &mut self,
        node: &Node,
        location: &str,
        diagnostics: &mut Diagnostics,
    ) -> Result<(), String> {
        let first_observation = !self.observed_windows.contains_key(&node.id);
        if first_observation && self.observed_windows.len() >= MAX_NEW_WINDOW_HISTORY {
            return Err("new-window history limit exceeded; observation is uncertain".into());
        }

        let description = format!(
            "id={} app_id={:?} class={:?} {location} floating={:?} fullscreen={:?}",
            node.id,
            node.app_id,
            node.window_properties
                .as_ref()
                .and_then(|properties| properties.class.as_ref()),
            node.floating,
            node.fullscreen_mode
        );
        if self.observed_windows.get(&node.id) != Some(&description) {
            diagnostics.record(&format!(
                "observed new window: {description}; expected {:?}",
                self.matcher
            ));
            self.observed_windows.insert(node.id, description);
        }

        if matches_window(self.matcher, node) {
            if let Some(previous) = self.matched_window_id
                && previous != node.id
            {
                return Err(format!(
                    "ambiguous new windows id={previous} and id={} match {:?}",
                    node.id, self.matcher
                ));
            }
            self.matched_window_id = Some(node.id);
            if let Some(reason) = self.disqualified_windows.get(&node.id) {
                return Err(format!(
                    "matching window {reason}: id={}; refusing repair",
                    node.id,
                ));
            }
        } else if selector_identity(self.matcher, node).is_some() {
            self.disqualified_windows
                .insert(node.id, "changed identity after a nonmatching identity");
            if self.matched_window_id == Some(node.id) {
                return Err(format!("matched window id={} changed identity", node.id));
            }
        }

        Ok(())
    }

    fn observe_event(
        &mut self,
        event: &Event,
        diagnostics: &mut Diagnostics,
    ) -> Result<(), String> {
        let Event::Window(window) = event else {
            return Ok(());
        };
        reject_unsafe_window_event(event, self.accepted_windows, diagnostics)?;

        let node = &window.container;
        if matches!(
            window.change,
            WindowChange::New | WindowChange::Floating | WindowChange::FullscreenMode
        ) {
            // These events were already tolerated for preexisting windows during a
            // launch wait, but cannot establish a safe foreign close later.
            for contained in node.iter() {
                self.baseline_foreign_window_ids.remove(&contained.id);
            }
        }

        if self.accepted_windows.contains_key(&node.id) {
            return Ok(());
        }
        if self.preexisting_container_ids.contains(&node.id) {
            return Ok(());
        }

        if matches!(node.floating, Some(Floating::AutoOn | Floating::UserOn))
            || node.node_type == NodeType::FloatingCon
            || node.fullscreen_mode.is_some_and(|mode| mode != 0)
        {
            self.disqualified_windows
                .insert(node.id, "was floating/fullscreen");
        }
        self.observe_window(
            node,
            "workspace=unknown (event; awaiting fresh tree)",
            diagnostics,
        )?;

        if window.change == WindowChange::Close {
            return Err(format!(
                "new/accepted window id={} closed; cannot verify its placement or ownership",
                node.id
            ));
        }

        Ok(())
    }

    /// Returns the matching new target window, or `None` while there is none or while
    /// another new window's identity or tree position is still unresolved.
    fn reconcile(
        &mut self,
        snapshot: &Snapshot,
        diagnostics: &mut Diagnostics,
    ) -> Result<Option<i64>, String> {
        validate_accepted_windows(
            snapshot,
            self.accepted_windows,
            self.target_workspace_id,
            diagnostics,
        )?;
        for event in &snapshot.events {
            self.observe_event(event, diagnostics)?;
        }

        let windows = locate_windows(&snapshot.tree);
        let mut candidate_window_id = None;
        let mut unresolved = false;
        for located in &windows {
            let node = located.node;

            if self.accepted_windows.contains_key(&node.id) {
                continue;
            }
            if self.preexisting_container_ids.contains(&node.id) {
                if located.workspace_id == Some(self.target_workspace_id) {
                    return Err(format!(
                        "preexisting window id={} entered target workspace",
                        node.id
                    ));
                }
                continue;
            }

            let location = format!(
                "workspace={:?} scratchpad={} floating={}",
                located.workspace_id, located.scratchpad, located.floating
            );
            // A matching window already fails for scratchpad placement. Prefer that
            // proven location over the generic floating reason without adding history
            // restrictions to windows whose selector has not matched.
            if located.scratchpad && matches_window(self.matcher, node) {
                self.disqualified_windows
                    .insert(node.id, "was in the scratchpad");
            } else if located.floating || located.fullscreen {
                self.disqualified_windows
                    .insert(node.id, "was floating/fullscreen");
            }
            self.observe_window(node, &location, diagnostics)?;
            if located.workspace_id == Some(self.target_workspace_id)
                && (located.floating || located.fullscreen || located.scratchpad)
            {
                return Err(format!(
                    "unexpected floating/fullscreen/scratchpad window id={} on target workspace; refusing repair",
                    node.id
                ));
            }

            if matches_window(self.matcher, node) {
                if located.workspace_id != Some(self.target_workspace_id) || located.scratchpad {
                    let placement = if located.workspace_id.is_some() {
                        "wrong workspace"
                    } else {
                        "unknown workspace"
                    };
                    return Err(format!(
                        "placement failure: {placement}: matching window id={} ({location}); expected workspace={}; check Sway assignment rules and activation context",
                        node.id, self.target_workspace_id
                    ));
                }
                if located.floating || located.fullscreen || node.fullscreen_mode != Some(0) {
                    return Err(format!(
                        "matching window has unusable tiled state: id={} is floating/fullscreen or fullscreen state is unknown",
                        node.id
                    ));
                }
                candidate_window_id = Some(node.id);
            } else if located.workspace_id == Some(self.target_workspace_id) {
                if selector_identity(self.matcher, node).is_some() {
                    return Err(format!(
                        "unexpected target-workspace window id={}: expected {:?}, observed {}; refusing adoption",
                        node.id, self.matcher, self.observed_windows[&node.id]
                    ));
                }
                unresolved = true;
            }
        }

        // An event can be newer than its accompanying tree. Never accept that stale
        // tree; query again. If it closed, the close event above already failed.
        if self
            .observed_windows
            .keys()
            .any(|id| !windows.iter().any(|window| window.node.id == *id))
        {
            unresolved = true;
        }

        if let Some(matched_window_id) = self.matched_window_id {
            let in_event_history = snapshot.events.iter().any(
                |event| matches!(event, Event::Window(window) if window.container.id == matched_window_id),
            );
            if let Some(located) = windows
                .iter()
                .find(|window| window.node.id == matched_window_id)
            {
                if !matches_window(self.matcher, located.node) {
                    return Err(format!(
                        "matched window id={matched_window_id} changed identity"
                    ));
                }
            } else if !in_event_history {
                return Err(format!("matched window id={matched_window_id} disappeared"));
            }
        }

        Ok(if unresolved {
            None
        } else {
            candidate_window_id
        })
    }

    fn wait_diagnostics(&self, started: Instant) -> String {
        let window_summary = if self.observed_windows.is_empty() {
            "no new windows observed".into()
        } else {
            format!(
                "new windows observed (unmatched or unresolved): {}",
                self.observed_windows
                    .values()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        };
        format!(
            "after {}ms waiting for {:?}: {window_summary}; check command, selector and placement rules",
            started.elapsed().as_millis(),
            self.matcher
        )
    }
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
