//! Sequential launch and guarded, recursive tiled construction.

use crate::config::{Config, LayoutNode, Matcher, NodeKind, Orientation};
use crate::diagnostics::Diagnostics;
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
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use swayipc::{
    CommandType, Event, Floating, Node, NodeLayout, NodeType, ScratchpadState, WindowChange,
};

const IPC_TIMEOUT: Duration = Duration::from_secs(2);
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

pub fn load(config: &Config, diagnostics: &mut Diagnostics) -> Result<(), String> {
    let socket = observation::sway_socket()?;
    let mut connect = || observation::connect(&socket);
    let sizing_warnings = load_with(config, &mut connect, diagnostics)?;
    diagnostics.report_sizing_warnings(&sizing_warnings);
    Ok(())
}

// Concrete socket injection keeps all fixture traffic disconnected from socket discovery.
fn load_with(
    config: &Config,
    connect: &mut dyn FnMut() -> Result<UnixStream, String>,
    diagnostics: &mut Diagnostics,
) -> Result<Vec<String>, String> {
    diagnostics.stage("subscribe");
    let mut observer = Observer::subscribe(connect()?, Instant::now() + IPC_TIMEOUT)?;
    let initial = observer.snapshot(connect()?, Instant::now() + IPC_TIMEOUT)?;
    let target = initial.target;

    diagnostics.stage(format!("prelaunch workspace={target}"));
    require_empty(&initial, diagnostics)?;
    let mut preexisting_ids = initial.tree.iter().map(|node| node.id).collect();
    record_prelaunch_events(&initial, &mut preexisting_ids)?;

    let workspace = initial
        .tree
        .find_as_ref(|node| node.id == target)
        .ok_or("target missing")?;
    let empty_workspace = Hierarchy::Split {
        id: Some(target),
        layout: workspace.layout,
        children: Vec::new(),
    };
    let mut session = Construction {
        observer,
        connect,
        diagnostics,
        target,
        preexisting_ids,
        accepted: BTreeMap::new(),
        expected_hierarchy: empty_workspace,
        window_timeout_ms: config.window_timeout_ms,
    };
    let root_path = session.build(&config.layout.value, None)?;

    session
        .diagnostics
        .record("construction complete; applying weights from outer splits inward");
    let mut sizing_warnings = Vec::new();
    session.apply_sizing(&config.layout.value, &root_path, &mut sizing_warnings)?;

    let (focus_leaf, focus_path) =
        find_focus_leaf(&config.layout.value, config.focus.as_deref(), &root_path)
            .ok_or("validated final focus window missing from layout")?;
    let focus_id = session.expected_hierarchy.at_mut(&focus_path).id();
    session
        .diagnostics
        .stage(format!("final focus leaf={focus_leaf:?} id={focus_id}"));
    session.focus(&focus_path)?;
    session.diagnostics.record(&format!(
        "final focus verified leaf={focus_leaf:?} id={focus_id} workspace={target}"
    ));
    Ok(sizing_warnings)
}

// The workspace is the outer Split. Structural IDs are bound from the first verified
// tree after a split, never predicted or retained across an explicitly replaced wrapper.
#[derive(Clone, Debug)]
enum Hierarchy {
    /// A window's container ID.
    Window(i64),
    Split {
        /// `None` until the first verified tree after creating this split supplies its ID.
        id: Option<i64>,
        layout: NodeLayout,
        children: Vec<Hierarchy>,
    },
}

impl Hierarchy {
    fn at_mut(&mut self, path: &[usize]) -> &mut Self {
        let Some((child_index, rest)) = path.split_first() else {
            return self;
        };
        let Self::Split { children, .. } = self else {
            unreachable!("split path");
        };
        children[*child_index].at_mut(rest)
    }

    /// Requires `observed` to match this expectation exactly, binding unknown split IDs.
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

        match self {
            Self::Window(id) if observed.id == *id && observed.nodes.is_empty() => Ok(()),
            Self::Split {
                id,
                layout,
                children,
            } if id.is_none_or(|id| id == observed.id)
                && observed.layout == *layout
                && observed.nodes.len() == children.len()
                && (observed.node_type == NodeType::Workspace
                    || (observed.node_type == NodeType::Con
                        && observed.fullscreen_mode == Some(0)
                        && observed.app_id.is_none()
                        && observed.window.is_none()
                        && observed.shell.is_none())) =>
            {
                *id = Some(observed.id);
                for (expected_child, observed_child) in children.iter_mut().zip(&observed.nodes) {
                    expected_child.verify(observed_child)?;
                }
                Ok(())
            }
            _ => Err(format!(
                "unexpected hierarchy at id={} layout={:?} children={:?}",
                observed.id,
                observed.layout,
                observed
                    .nodes
                    .iter()
                    .map(|child| child.id)
                    .collect::<Vec<_>>()
            )),
        }
    }

    fn id(&self) -> i64 {
        match self {
            Self::Window(id) | Self::Split { id: Some(id), .. } => *id,
            Self::Split { id: None, .. } => unreachable!("verified structural identity"),
        }
    }
}

struct Construction<'a> {
    observer: Observer,
    connect: &'a mut dyn FnMut() -> Result<UnixStream, String>,
    diagnostics: &'a mut Diagnostics,
    /// Container ID of the frozen target workspace.
    target: i64,
    /// Every container ID observed before the next launch, except accepted windows.
    /// A launch can accept only a window whose ID is absent from this set.
    preexisting_ids: HashSet<i64>,
    /// This run's accepted windows by container ID, with the selector each must keep.
    accepted: BTreeMap<i64, &'a Matcher>,
    expected_hierarchy: Hierarchy,
    window_timeout_ms: u64,
}

impl<'a> Construction<'a> {
    fn verify(&mut self, snapshot: &Snapshot) -> Result<(), String> {
        validate_accepted_windows(snapshot, &self.accepted, self.target, self.diagnostics)?;

        let workspace = snapshot
            .tree
            .find_as_ref(|node| node.id == self.target)
            .ok_or("target missing")?;
        self.diagnostics.record(&format!(
            "expected hierarchy {:?}; observed {}",
            self.expected_hierarchy,
            describe(workspace)
        ));
        self.expected_hierarchy.verify(workspace)
    }

    // No mutation/launch is allowed through a fence with newer window activity.
    fn settled_snapshot(&mut self) -> Result<Snapshot, String> {
        let deadline = Instant::now() + IPC_TIMEOUT;
        loop {
            let snapshot = self.observer.snapshot((self.connect)()?, deadline)?;
            for event in &snapshot.events {
                if let Event::Window(window) = event
                    && !self.accepted.contains_key(&window.container.id)
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

            self.preexisting_ids.extend(
                snapshot
                    .tree
                    .iter()
                    .map(|node| node.id)
                    .filter(|id| !self.accepted.contains_key(id)),
            );
            for event in &snapshot.events {
                if let Event::Window(window) = event
                    && !self.accepted.contains_key(&window.container.id)
                {
                    self.preexisting_ids.insert(window.container.id);
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

    /// Runs one command between settled, fully verified snapshots and requires the result
    /// to match `expected_after`. `require_focused` names the container a command's effect
    /// depends on being focused, such as a criteria-free split. Commands with exact con_id
    /// criteria pass `None`.
    fn guarded_command(
        &mut self,
        command: String,
        require_focused: Option<i64>,
        expected_after: Hierarchy,
    ) -> Result<Snapshot, String> {
        self.diagnostics.stage(format!("command {command}"));
        let before = self.settled_snapshot()?;
        if let Some(id) = require_focused
            && !is_focused(&before.tree, id)
        {
            return Err(format!(
                "command {command:?} requires focused container id={id}; observed focus is elsewhere, refusing to run it"
            ));
        }

        let replies = run_command((self.connect)()?, command, Instant::now() + IPC_TIMEOUT)?;
        self.diagnostics
            .record(&format!("command replies: {replies:?}"));
        if replies.is_empty() || replies.iter().any(Result::is_err) {
            return Err(format!("Sway command failed: {replies:?}"));
        }

        self.expected_hierarchy = expected_after;
        let after = self.settled_snapshot()?;
        self.diagnostics.record(&format!(
            "verified structural identities {:?}",
            self.expected_hierarchy
        ));
        Ok(after)
    }

    fn focus(&mut self, path: &[usize]) -> Result<(), String> {
        let id = self.expected_hierarchy.at_mut(path).id();
        let after = self.guarded_command(
            format!("[con_id={id}] focus"),
            None,
            self.expected_hierarchy.clone(),
        )?;
        if !is_focused(&after.tree, id) {
            return Err(format!("explicit focus id={id} was not observed"));
        }
        Ok(())
    }

    /// Applies one split's weights, then recurses into nested splits. Residual deviations
    /// become warnings; IPC, ownership and structural failures remain fatal.
    fn apply_sizing(
        &mut self,
        layout_node: &LayoutNode,
        path: &[usize],
        warnings: &mut Vec<String>,
    ) -> Result<(), String> {
        let NodeKind::Split {
            orientation,
            children,
        } = &layout_node.kind
        else {
            return Ok(());
        };
        let group_id = self.expected_hierarchy.at_mut(path).id();
        let axis = match orientation {
            Orientation::Horizontal => "width",
            Orientation::Vertical => "height",
        };
        let weights: Vec<_> = children.iter().map(|child| child.value.weight).collect();
        let target_shares = proportions(&weights);

        self.diagnostics
            .stage(format!("sizing group={group_id} path={path:?} axis={axis}"));
        self.diagnostics.record(&format!(
            "weights={weights:?}; targets={}; aim within 1 percentage point, warn beyond 5",
            format_shares(&target_shares)
        ));

        let mut snapshot = self.settled_snapshot()?;
        let mut unavailable_geometry = None;
        for pass in 0..MAX_SIZING_PASSES {
            let geometry = sibling_geometry(&snapshot.tree, group_id);
            let (dimensions, observed_shares) = match geometry {
                Ok(geometry) => geometry,
                Err(reason) => {
                    unavailable_geometry = Some(reason);
                    break;
                }
            };
            let error_before_pass = max_share_error(&target_shares, &observed_shares);
            self.diagnostics.record(&format!(
                "pass={} before dimensions={dimensions:?}; observed={}",
                pass + 1,
                format_shares(&observed_shares)
            ));
            if error_before_pass <= SIZING_TARGET {
                break;
            }

            // resize set changes every other sibling as well. Recalculate each next
            // target from the resulting tree, never from a stale percent property.
            for child_index in 0..children.len() {
                let geometry = sibling_geometry(&snapshot.tree, group_id);
                let (dimensions, observed_shares) = match geometry {
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

                let target_pixels =
                    (target_shares[child_index] * dimensions.iter().sum::<f64>()).round();
                // Zero means "leave unchanged" to resize set. Do not invent a
                // minimum size or override Sway/application constraints.
                if target_pixels < 1.0 || target_pixels == dimensions[child_index] {
                    continue;
                }

                let mut child_path = path.to_vec();
                child_path.push(child_index);
                let child_id = self.expected_hierarchy.at_mut(&child_path).id();
                snapshot = self.guarded_command(
                    format!("[con_id={child_id}] resize set {axis} {target_pixels:.0} px"),
                    None,
                    self.expected_hierarchy.clone(),
                )?;
                match sibling_geometry(&snapshot.tree, group_id) {
                    Ok((_, observed_shares)) => self.diagnostics.record(&format!(
                        "pass={} after child={child_id}; observed={}",
                        pass + 1,
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
            warnings.push(warning);
        }

        for (child_index, child) in children.iter().enumerate() {
            let mut child_path = path.to_vec();
            child_path.push(child_index);
            self.apply_sizing(&child.value, &child_path, warnings)?;
        }
        Ok(())
    }

    /// Builds a layout subtree after the sibling at `insert_after`, or as the workspace's
    /// first content, and returns the subtree's path in the expected hierarchy.
    fn build(
        &mut self,
        layout_node: &'a LayoutNode,
        insert_after: Option<Vec<usize>>,
    ) -> Result<Vec<usize>, String> {
        let NodeKind::Split {
            orientation,
            children,
        } = &layout_node.kind
        else {
            return self.launch_window(layout_node, insert_after);
        };
        let first_path = self.build(&children[0].value, insert_after)?;

        let split_layout = match orientation {
            Orientation::Horizontal => NodeLayout::SplitH,
            Orientation::Vertical => NodeLayout::SplitV,
        };
        let split_command = match orientation {
            Orientation::Horizontal => "splith",
            Orientation::Vertical => "splitv",
        };
        let mut expected_after = self.expected_hierarchy.clone();
        let group_path;
        let command;
        let require_focused;
        if first_path.is_empty() {
            // The first completed child occupies the workspace. workspace_split wraps
            // its populated children, preserving even equal-axis meaningful groups.
            // Select its direct owned child and explicitly focus its known parent. The
            // split has no criteria and acts on the focused node, so it requires the
            // workspace to be focused on its own settled fence.
            let direct_child_path = vec![0];
            self.focus(&direct_child_path)?;
            let direct_child_id = self.expected_hierarchy.at_mut(&direct_child_path).id();
            self.guarded_command(
                format!("[con_id={direct_child_id}] focus parent"),
                None,
                self.expected_hierarchy.clone(),
            )?;

            let Hierarchy::Split {
                layout: old_layout,
                children: old_children,
                ..
            } = expected_after
            else {
                unreachable!()
            };
            let wrapped_children = Hierarchy::Split {
                id: None,
                layout: old_layout,
                children: old_children,
            };
            expected_after = Hierarchy::Split {
                id: Some(self.target),
                layout: split_layout,
                children: vec![wrapped_children],
            };
            command = split_command.to_owned();
            require_focused = Some(self.target);
            group_path = Vec::new();
        } else {
            let first_id = self.expected_hierarchy.at_mut(&first_path).id();
            let parent_path = &first_path[..first_path.len() - 1];
            let Hierarchy::Split {
                layout: parent_layout,
                children: siblings,
                ..
            } = expected_after.at_mut(parent_path)
            else {
                unreachable!()
            };
            let singleton = siblings.len() == 1;
            let ordinary_parent = matches!(parent_layout, NodeLayout::SplitH | NodeLayout::SplitV);

            if singleton {
                *parent_layout = split_layout;
                group_path = parent_path.to_vec();
            } else {
                let first = expected_after.at_mut(&first_path).clone();
                *expected_after.at_mut(&first_path) = Hierarchy::Split {
                    id: None,
                    layout: split_layout,
                    children: vec![first],
                };
                group_path = first_path;
            }

            command = if singleton && !ordinary_parent {
                let layout_command = match orientation {
                    Orientation::Horizontal => "layout splith",
                    Orientation::Vertical => "layout splitv",
                };
                format!("[con_id={first_id}] {layout_command}")
            } else {
                format!("[con_id={first_id}] {split_command}")
            };
            // con_id criteria select the container regardless of focus.
            require_focused = None;
        }

        self.diagnostics.record(&format!(
            "expected split/reparent transition {:?} -> {expected_after:?}",
            self.expected_hierarchy
        ));
        self.guarded_command(command, require_focused, expected_after)?;

        for (child_index, child) in children.iter().enumerate().skip(1) {
            let mut previous_sibling_path = group_path.clone();
            previous_sibling_path.push(child_index - 1);
            self.build(&child.value, Some(previous_sibling_path))?;
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
            id,
            command,
            matcher,
            cwd,
            env,
        } = &layout_node.kind
        else {
            unreachable!()
        };

        if let Some(previous_path) = &insert_after {
            self.focus(previous_path)?;
        }
        self.diagnostics
            .stage(format!("prelaunch leaf={id:?} workspace={}", self.target));
        let prelaunch = self.settled_snapshot()?;
        if let Some(previous_path) = &insert_after {
            let previous_id = self.expected_hierarchy.at_mut(previous_path).id();
            if !is_focused(&prelaunch.tree, previous_id) {
                return Err(format!(
                    "insertion focus left explicitly selected container id={previous_id} before launch"
                ));
            }
        }

        let mut window_path = insert_after.unwrap_or_else(|| vec![0]);
        if !self.accepted.is_empty() {
            *window_path.last_mut().expect("sibling path") += 1;
        }
        let mut new_windows = NewWindowTracker {
            target: self.target,
            preexisting_ids: self.preexisting_ids.clone(),
            matcher,
            accepted: &self.accepted,
            observed: BTreeMap::new(),
            matched: None,
            disqualified: BTreeMap::new(),
        };
        let started = Instant::now();
        let deadline = started
            .checked_add(Duration::from_millis(self.window_timeout_ms))
            .ok_or("window_timeout_ms exceeds the monotonic clock range; use a smaller timeout")?;

        self.diagnostics
            .stage(format!("launch leaf={id:?} workspace={}", self.target));
        self.diagnostics.record(&format!(
            "expected {matcher:?}; launching once (argv/env omitted)"
        ));
        let mut process = spawn(command, cwd.as_ref(), env)?;
        // From here on, a failure may leave this application and a partial layout behind.
        self.diagnostics.spawned(process.id());

        let mut process_exited = false;
        let mut provisional = None;
        let mut expected_after = self.expected_hierarchy.clone();
        loop {
            let stage = if provisional.is_some() {
                "final verification"
            } else {
                "wait"
            };
            self.diagnostics.stage(format!("{stage} leaf={id:?}"));
            if Instant::now() >= deadline {
                return Err(format!(
                    "window timeout: {}",
                    new_windows.wait_diagnostics(started)
                ));
            }
            let snapshot = self
                .observer
                .snapshot(
                    (self.connect)()?,
                    deadline.min(Instant::now() + IPC_TIMEOUT),
                )
                .map_err(|error| format!("{error}; {}", new_windows.wait_diagnostics(started)))?;

            let already_provisional = provisional.is_some();
            let candidate = new_windows.reconcile(&snapshot, self.diagnostics)?;
            if let Some(provisional_id) = provisional {
                if candidate != Some(provisional_id) {
                    return Err(format!(
                        "accepted window id={provisional_id} disappeared or failed revalidation"
                    ));
                }
            } else if let Some(candidate_id) = candidate {
                let parent_path = &window_path[..window_path.len() - 1];
                let Hierarchy::Split { children, .. } = expected_after.at_mut(parent_path) else {
                    unreachable!()
                };
                children.insert(
                    *window_path.last().expect("leaf path"),
                    Hierarchy::Window(candidate_id),
                );
                provisional = Some(candidate_id);
                self.diagnostics.record(&format!(
                    "provisionally accepted leaf={id:?} id={candidate_id}; rechecking fresh state"
                ));
            }

            if let Some(provisional_id) = provisional {
                let workspace = snapshot
                    .tree
                    .find_as_ref(|node| node.id == self.target)
                    .ok_or("target missing")?;
                self.diagnostics.record(&format!(
                    "expected hierarchy {expected_after:?}; observed {}",
                    describe(workspace)
                ));
                expected_after.verify(workspace)?;

                // Accept only from a second fresh tree with no newer window activity.
                let window_activity = snapshot
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::Window(_)));
                if !window_activity && already_provisional {
                    self.diagnostics.record(&format!(
                        "accepted leaf={id:?} id={provisional_id}; hierarchy/identity verified"
                    ));
                    reap_if_exited(&mut process, &mut process_exited, self.diagnostics)?;
                    self.accepted.insert(provisional_id, matcher);
                    self.expected_hierarchy = expected_after;
                    return Ok(window_path);
                }
                // Recheck a newly provisional window at once instead of polling first.
                if !already_provisional {
                    continue;
                }
            }

            reap_if_exited(&mut process, &mut process_exited, self.diagnostics)?;
            let poll_until = deadline.min(Instant::now() + POLL_INTERVAL);
            while let Some(event) = self.observer.next_until(poll_until)? {
                new_windows.observe_event(&event, self.diagnostics)?;
            }
        }
    }
}

fn max_share_error(target_shares: &[f64], observed_shares: &[f64]) -> f64 {
    target_shares
        .iter()
        .zip(observed_shares)
        .map(|(target, observed)| (target - observed).abs())
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

/// The configured focus window, or the first window in depth-first order, with its path.
fn find_focus_leaf<'a>(
    layout_node: &'a LayoutNode,
    configured_focus: Option<&str>,
    path: &[usize],
) -> Option<(&'a str, Vec<usize>)> {
    match &layout_node.kind {
        NodeKind::Window { id, .. } => configured_focus
            .is_none_or(|focus| focus == id)
            .then(|| (id.as_str(), path.to_vec())),
        NodeKind::Split { children, .. } => {
            for (child_index, child) in children.iter().enumerate() {
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

/// A disposable command connection is cancelled on expiry; never retry a command
/// whose reply was lost because Sway may already have executed it.
fn run_command(
    socket: UnixStream,
    command: String,
    deadline: Instant,
) -> Result<Vec<Result<(), String>>, String> {
    let control = socket
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
                let payload_size =
                    u32::from_ne_bytes(header[6..10].try_into().expect("header size")) as usize;
                let reply_type =
                    u32::from_ne_bytes(header[10..14].try_into().expect("header type"));
                if reply_type != 0 || payload_size > MAX_COMMAND_REPLY_BYTES {
                    return Err("command IPC: invalid reply type or reply exceeds 1 MiB".into());
                }

                let mut payload = vec![0; payload_size];
                socket
                    .read_exact(&mut payload)
                    .map_err(|error| format!("command IPC payload: {error}"))?;
                let replies: Vec<Reply> = CommandType::RunCommand
                    .decode((reply_type, payload))
                    .map_err(|error| format!("command IPC decode: {error}"))?;
                Ok(replies
                    .into_iter()
                    .map(|reply| {
                        if !reply.success || reply.error.is_some() {
                            Err(reply
                                .error
                                .unwrap_or_else(|| "Sway reported success=false".into()))
                        } else {
                            Ok(())
                        }
                    })
                    .collect())
            };
            let _ = sender.send(exchange());
        })
        .map_err(|error| format!("start command reader: {error}"))?;

    let result = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let _ = control.shutdown(Shutdown::Both);
    if reader.join().is_err() {
        return Err("command reader panicked".into());
    }
    match result {
        Ok(Ok(replies)) => Ok(replies),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(format!("command deadline/channel failure: {error}")),
    }
}

fn is_focused(tree: &Node, id: i64) -> bool {
    tree.find_as_ref(|node| node.id == id)
        .is_some_and(|node| node.focused)
}

fn describe(node: &Node) -> String {
    if node.nodes.is_empty() {
        return node.id.to_string();
    }
    format!(
        "{}:{:?}[{}]",
        node.id,
        node.layout,
        node.nodes
            .iter()
            .map(describe)
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn validate_accepted_windows(
    snapshot: &Snapshot,
    accepted: &BTreeMap<i64, &Matcher>,
    target: i64,
    diagnostics: &mut Diagnostics,
) -> Result<(), String> {
    for event in &snapshot.events {
        reject_unsafe_window_event(event, accepted, diagnostics)?;
    }

    let windows = locate_windows(&snapshot.tree);
    for (id, matcher) in accepted {
        let located = windows
            .iter()
            .find(|window| window.node.id == *id)
            .ok_or_else(|| format!("accepted window id={id} disappeared"))?;
        if located.workspace != Some(target)
            || located.floating
            || located.fullscreen
            || located.scratchpad
            || located.node.fullscreen_mode != Some(0)
            || !matches_window(matcher, located.node)
        {
            return Err(format!(
                "accepted window id={id} lost placement, identity or tiled state"
            ));
        }
    }
    Ok(())
}

/// The node's identifier of the selector's own kind: an app ID never satisfies a class
/// selector, nor a class an app ID selector.
fn identity<'a>(matcher: &Matcher, node: &'a Node) -> Option<&'a str> {
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
    identity(matcher, node) == Some(selector.as_str())
}

/// Rejects any window move, and any window event showing that an accepted window (or a
/// container holding one) closed, reappeared as new, or changed floating, fullscreen or
/// identity state.
fn reject_unsafe_window_event(
    event: &Event,
    accepted: &BTreeMap<i64, &Matcher>,
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
        .any(|contained| accepted.contains_key(&contained.id));
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

    if let Some(matcher) = accepted.get(&node.id)
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
    process_exited: &mut bool,
    diagnostics: &mut Diagnostics,
) -> Result<(), String> {
    if !*process_exited
        && let Some(status) = process
            .try_wait()
            .map_err(|error| format!("inspect child: {error}"))?
    {
        *process_exited = true;
        diagnostics.record(&format!(
            "launch process exited: {status}; continuing window observation (delegation is allowed)"
        ));
    }
    Ok(())
}

fn require_empty(snapshot: &Snapshot, diagnostics: &mut Diagnostics) -> Result<(), String> {
    let workspace = snapshot
        .tree
        .find_as_ref(|node| node.id == snapshot.target)
        .ok_or("target workspace missing")?;
    diagnostics.record(&format!(
        "selected workspace id={}; initial occupancy: tiled={} floating={}",
        snapshot.target,
        workspace.nodes.len(),
        workspace.floating_nodes.len()
    ));
    if !workspace.nodes.is_empty() || !workspace.floating_nodes.is_empty() {
        return Err("target workspace must be completely empty, including tiled/floating containers; choose an empty disposable workspace".into());
    }
    Ok(())
}

/// Adds window IDs from the initial fence history to `preexisting_ids`. Structural window
/// activity during that history makes the empty-workspace observation uncertain.
fn record_prelaunch_events(
    snapshot: &Snapshot,
    preexisting_ids: &mut HashSet<i64>,
) -> Result<(), String> {
    for event in &snapshot.events {
        if let Event::Window(window) = event {
            preexisting_ids.insert(window.container.id);
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
    target: i64,
    preexisting_ids: HashSet<i64>,
    matcher: &'a Matcher,
    accepted: &'a BTreeMap<i64, &'a Matcher>,
    /// Each new window's latest description, for logs and timeout diagnostics.
    observed: BTreeMap<i64, String>,
    /// The single new window whose identity has matched the selector, if any.
    matched: Option<i64>,
    /// New windows whose observed history rules out acceptance, with the reason.
    disqualified: BTreeMap<i64, &'static str>,
}

impl NewWindowTracker<'_> {
    fn observe_window(
        &mut self,
        node: &Node,
        location: &str,
        diagnostics: &mut Diagnostics,
    ) -> Result<(), String> {
        let first_observation = !self.observed.contains_key(&node.id);
        if first_observation && self.observed.len() >= MAX_NEW_WINDOW_HISTORY {
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
        if self.observed.get(&node.id) != Some(&description) {
            diagnostics.record(&format!(
                "observed new window: {description}; expected {:?}",
                self.matcher
            ));
            self.observed.insert(node.id, description);
        }

        if matches_window(self.matcher, node) {
            if let Some(previous) = self.matched
                && previous != node.id
            {
                return Err(format!(
                    "ambiguous new windows id={previous} and id={} match {:?}",
                    node.id, self.matcher
                ));
            }
            self.matched = Some(node.id);
            if let Some(reason) = self.disqualified.get(&node.id) {
                return Err(format!(
                    "matching window id={} {reason}; refusing repair",
                    node.id,
                ));
            }
        } else if identity(self.matcher, node).is_some() {
            self.disqualified
                .insert(node.id, "changed identity after a nonmatching identity");
            if self.matched == Some(node.id) {
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
        reject_unsafe_window_event(event, self.accepted, diagnostics)?;

        let node = &window.container;
        if self.accepted.contains_key(&node.id) {
            return Ok(());
        }
        if self.preexisting_ids.contains(&node.id) {
            return Ok(());
        }

        if matches!(node.floating, Some(Floating::AutoOn | Floating::UserOn))
            || node.node_type == NodeType::FloatingCon
            || node.fullscreen_mode.is_some_and(|mode| mode != 0)
        {
            self.disqualified.insert(node.id, "was floating/fullscreen");
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
        validate_accepted_windows(snapshot, self.accepted, self.target, diagnostics)?;
        for event in &snapshot.events {
            self.observe_event(event, diagnostics)?;
        }

        let windows = locate_windows(&snapshot.tree);
        let mut candidate = None;
        let mut unresolved = false;
        for located in &windows {
            let node = located.node;
            if self.accepted.contains_key(&node.id) {
                continue;
            }
            if self.preexisting_ids.contains(&node.id) {
                if located.workspace == Some(self.target) {
                    return Err(format!(
                        "preexisting window id={} entered target workspace",
                        node.id
                    ));
                }
                continue;
            }

            let location = format!(
                "workspace={:?} scratchpad={} floating={}",
                located.workspace, located.scratchpad, located.floating
            );
            if located.floating || located.fullscreen {
                self.disqualified.insert(node.id, "was floating/fullscreen");
            }
            self.observe_window(node, &location, diagnostics)?;
            if located.workspace == Some(self.target)
                && (located.floating || located.fullscreen || located.scratchpad)
            {
                return Err(format!(
                    "unexpected floating/fullscreen/scratchpad window id={} on target workspace; refusing repair",
                    node.id
                ));
            }

            if matches_window(self.matcher, node) {
                if located.workspace != Some(self.target) || located.scratchpad {
                    return Err(format!(
                        "placement failure: matching window id={} on wrong workspace or scratchpad ({location}); expected workspace={}; check Sway assignment rules and activation context",
                        node.id, self.target
                    ));
                }
                if located.floating || located.fullscreen || node.fullscreen_mode != Some(0) {
                    return Err(format!(
                        "matching window id={} is floating/fullscreen or fullscreen state is unknown",
                        node.id
                    ));
                }
                candidate = Some(node.id);
            } else if located.workspace == Some(self.target) {
                if identity(self.matcher, node).is_some() {
                    return Err(format!(
                        "unexpected target-workspace window id={}: expected {:?}, observed {}; refusing adoption",
                        node.id, self.matcher, self.observed[&node.id]
                    ));
                }
                unresolved = true;
            }
        }

        // An event can be newer than its accompanying tree. Never accept that stale
        // tree; query again. If it closed, the close event above already failed.
        if self
            .observed
            .keys()
            .any(|id| !windows.iter().any(|window| window.node.id == *id))
        {
            unresolved = true;
        }

        if let Some(matched) = self.matched {
            let in_event_history = snapshot.events.iter().any(
                |event| matches!(event, Event::Window(window) if window.container.id == matched),
            );
            if let Some(located) = windows.iter().find(|window| window.node.id == matched) {
                if !matches_window(self.matcher, located.node) {
                    return Err(format!("matched window id={matched} changed identity"));
                }
            } else if !in_event_history {
                return Err(format!("matched window id={matched} disappeared"));
            }
        }
        Ok(if unresolved { None } else { candidate })
    }

    fn wait_diagnostics(&self, started: Instant) -> String {
        let observed = if self.observed.is_empty() {
            "no new windows observed".into()
        } else {
            format!(
                "new windows observed (unmatched or unresolved): {}",
                self.observed
                    .values()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        };
        format!(
            "after {}ms waiting for {:?}: {observed}; check command, selector and placement rules",
            started.elapsed().as_millis(),
            self.matcher
        )
    }
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
