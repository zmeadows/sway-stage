//! Sequential launch and guarded, recursive tiled construction.

use crate::config::{Config, Matcher, NodeKind, Orientation};
use crate::diagnostics::Diagnostics;
use crate::observation::{Observer, Snapshot};
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
const OBSERVED_LIMIT: usize = 256;
// Include floating-point rounding at the exact five-percentage-point boundary.
const SIZE_TOLERANCE: f64 = 0.05 + f64::EPSILON;
const SIZE_PASSES: usize = 3;
// Less than a tenth of a percentage point of improvement is not worth another pass.
const SIZE_PROGRESS: f64 = 0.001;

pub fn load(config: &Config, diagnostics: &mut Diagnostics) -> Result<(), String> {
    let socket = env::var_os("SWAYSOCK")
        .filter(|path| !path.is_empty())
        .ok_or("connect: SWAYSOCK is unset or empty; run inside Sway")?;
    let mut connect =
        || UnixStream::connect(&socket).map_err(|error| format!("connect to SWAYSOCK: {error}"));
    let warnings = load_with(config, &mut connect, diagnostics)?;
    diagnostics.sizing_warnings(&warnings);
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
    let mut existing = initial.tree.iter().map(|node| node.id).collect();
    prelaunch_history(&initial, &mut existing)?;
    let workspace = initial
        .tree
        .find_as_ref(|n| n.id == target)
        .ok_or("target missing")?;
    let expected = Hierarchy::Split(Some(target), workspace.layout, Vec::new());
    let mut session = Construction {
        observer,
        connect,
        diagnostics,
        target,
        existing,
        accepted: BTreeMap::new(),
        expected,
        timeout_ms: config.window_timeout_ms,
    };
    let root = session.build(&config.layout.value, None)?;
    session
        .diagnostics
        .record("construction complete; applying weights from outer splits inward");
    let mut warnings = Vec::new();
    session.size(&config.layout.value, &root, &mut warnings)?;
    let (logical, path) = focus_path(&config.layout.value, config.focus.as_deref(), &root)
        .ok_or("validated final focus window missing from layout")?;
    let id = session.expected.at_mut(&path).id();
    session
        .diagnostics
        .stage(format!("final focus leaf={logical:?} id={id}"));
    session.focus(&path)?;
    session.diagnostics.record(&format!(
        "final focus verified leaf={logical:?} id={id} workspace={target}"
    ));
    Ok(warnings)
}

// The workspace is the outer Split. Structural IDs are bound from the first verified
// tree after a split, never predicted or retained across an explicitly replaced wrapper.
#[derive(Clone, Debug)]
enum Hierarchy {
    Window(i64),
    Split(Option<i64>, NodeLayout, Vec<Hierarchy>),
}

impl Hierarchy {
    fn at_mut(&mut self, path: &[usize]) -> &mut Self {
        let Some((index, rest)) = path.split_first() else {
            return self;
        };
        let Self::Split(_, _, children) = self else {
            unreachable!("split path");
        };
        children[*index].at_mut(rest)
    }

    fn verify(&mut self, node: &Node) -> Result<(), String> {
        if !node.floating_nodes.is_empty()
            || matches!(node.floating, Some(Floating::AutoOn | Floating::UserOn))
            || matches!(
                node.scratchpad_state,
                Some(ScratchpadState::Fresh | ScratchpadState::Changed)
            )
        {
            return Err(format!("invalid hierarchy state id={}", node.id));
        }
        match self {
            Self::Window(id) if node.id == *id && node.nodes.is_empty() => Ok(()),
            Self::Split(id, layout, children)
                if id.is_none_or(|id| id == node.id)
                    && node.layout == *layout
                    && node.nodes.len() == children.len()
                    && (node.node_type == NodeType::Workspace
                        || (node.node_type == NodeType::Con
                            && node.fullscreen_mode == Some(0)
                            && node.app_id.is_none()
                            && node.window.is_none()
                            && node.shell.is_none())) =>
            {
                *id = Some(node.id);
                for (expected, actual) in children.iter_mut().zip(&node.nodes) {
                    expected.verify(actual)?;
                }
                Ok(())
            }
            _ => Err(format!(
                "unexpected hierarchy at id={} layout={:?} children={:?}",
                node.id,
                node.layout,
                node.nodes.iter().map(|n| n.id).collect::<Vec<_>>()
            )),
        }
    }

    fn id(&self) -> i64 {
        match self {
            Self::Window(id) | Self::Split(Some(id), _, _) => *id,
            Self::Split(None, _, _) => unreachable!("verified structural identity"),
        }
    }
}

struct Construction<'a> {
    observer: Observer,
    connect: &'a mut dyn FnMut() -> Result<UnixStream, String>,
    diagnostics: &'a mut Diagnostics,
    target: i64,
    existing: HashSet<i64>,
    accepted: BTreeMap<i64, &'a Matcher>,
    expected: Hierarchy,
    timeout_ms: u64,
}

impl<'a> Construction<'a> {
    fn verify(&mut self, snapshot: &Snapshot) -> Result<(), String> {
        validate_accepted(snapshot, &self.accepted, self.target, self.diagnostics)?;
        let workspace = snapshot
            .tree
            .find_as_ref(|n| n.id == self.target)
            .ok_or("target missing")?;
        self.diagnostics.record(&format!(
            "expected hierarchy {:?}; observed {}",
            self.expected,
            describe(workspace)
        ));
        self.expected.verify(workspace)
    }

    // No mutation/launch is allowed through a fence with newer window activity.
    fn settled(&mut self) -> Result<Snapshot, String> {
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
            self.existing.extend(
                snapshot
                    .tree
                    .iter()
                    .map(|n| n.id)
                    .filter(|id| !self.accepted.contains_key(id)),
            );
            for event in &snapshot.events {
                if let Event::Window(window) = event
                    && !self.accepted.contains_key(&window.container.id)
                {
                    self.existing.insert(window.container.id);
                }
            }
            if !snapshot
                .events
                .iter()
                .any(|e| matches!(e, Event::Window(_)))
            {
                return Ok(snapshot);
            }
        }
    }

    fn command(&mut self, command: String, expected: Hierarchy) -> Result<Snapshot, String> {
        self.diagnostics.stage(format!("command {command}"));
        let before = self.settled()?;
        if matches!(command.as_str(), "splith" | "splitv")
            && !before
                .tree
                .find_as_ref(|n| n.id == self.target)
                .is_some_and(|n| n.focused)
        {
            return Err("workspace wrapping requires explicit target parent focus".into());
        }
        let replies = run_command((self.connect)()?, command, Instant::now() + IPC_TIMEOUT)?;
        self.diagnostics
            .record(&format!("command replies: {replies:?}"));
        if replies.is_empty() || replies.iter().any(Result::is_err) {
            return Err(format!("Sway command failed: {replies:?}"));
        }
        self.expected = expected;
        let snapshot = self.settled()?;
        self.diagnostics.record(&format!(
            "verified structural identities {:?}",
            self.expected
        ));
        Ok(snapshot)
    }

    fn focus(&mut self, path: &[usize]) -> Result<(), String> {
        let id = self.expected.at_mut(path).id();
        let snapshot = self.command(format!("[con_id={id}] focus"), self.expected.clone())?;
        if !snapshot
            .tree
            .find_as_ref(|n| n.id == id)
            .is_some_and(|n| n.focused)
        {
            return Err(format!("explicit focus id={id} was not observed"));
        }
        Ok(())
    }

    fn size(
        &mut self,
        node: &crate::config::Node,
        path: &[usize],
        warnings: &mut Vec<String>,
    ) -> Result<(), String> {
        let NodeKind::Split { split, children } = &node.kind else {
            return Ok(());
        };
        let id = self.expected.at_mut(path).id();
        let axis = match split {
            Orientation::Horizontal => "width",
            Orientation::Vertical => "height",
        };
        let weights: Vec<_> = children.iter().map(|child| child.value.weight).collect();
        let targets = proportions(&weights);
        self.diagnostics
            .stage(format!("sizing group={id} path={path:?} axis={axis}"));
        self.diagnostics.record(&format!(
            "weights={weights:?}; targets={}; tolerance=5 percentage points",
            shares(&targets)
        ));
        let mut snapshot = self.settled()?;
        let mut limitation = None;
        for pass in 0..SIZE_PASSES {
            let geometry = sibling_geometry(&snapshot.tree, id, split);
            let (dimensions, observed) = match geometry {
                Ok(geometry) => geometry,
                Err(reason) => {
                    limitation = Some(reason);
                    break;
                }
            };
            let before_error = sizing_error(&targets, &observed);
            self.diagnostics.record(&format!(
                "pass={} before dimensions={dimensions:?}; observed={}",
                pass + 1,
                shares(&observed)
            ));
            if before_error <= SIZE_TOLERANCE {
                break;
            }

            // resize set changes every other sibling as well. Recalculate each next
            // target from the resulting tree, never from a stale percent property.
            for index in 0..children.len() {
                let (dimensions, observed) = match sibling_geometry(&snapshot.tree, id, split) {
                    Ok(geometry) => geometry,
                    Err(reason) => {
                        limitation = Some(reason);
                        break;
                    }
                };
                if sizing_error(&targets, &observed) <= SIZE_TOLERANCE {
                    break;
                }
                if (targets[index] - observed[index]).abs() <= SIZE_TOLERANCE {
                    continue;
                }
                let pixels = (targets[index] * dimensions.iter().sum::<f64>()).round();
                // Zero means "leave unchanged" to resize set. Do not invent a
                // minimum size or override Sway/application constraints.
                if pixels < 1.0 || pixels == dimensions[index] {
                    continue;
                }
                let mut child_path = path.to_vec();
                child_path.push(index);
                let child_id = self.expected.at_mut(&child_path).id();
                snapshot = self.command(
                    format!("[con_id={child_id}] resize set {axis} {pixels:.0} px"),
                    self.expected.clone(),
                )?;
                match sibling_geometry(&snapshot.tree, id, split) {
                    Ok((_, observed)) => self.diagnostics.record(&format!(
                        "pass={} after child={child_id}; observed={}",
                        pass + 1,
                        shares(&observed)
                    )),
                    Err(reason) => {
                        limitation = Some(reason);
                        break;
                    }
                }
            }
            if limitation.is_some() {
                break;
            }
            let (_, observed) = sibling_geometry(&snapshot.tree, id, split)?;
            let after_error = sizing_error(&targets, &observed);
            if after_error <= SIZE_TOLERANCE {
                break;
            }
            // Includes unchanged geometry at minimum sizes and cycles that return
            // to the same error. Never repeat a pass without meaningful progress.
            if before_error - after_error < SIZE_PROGRESS {
                self.diagnostics
                    .record("stopping sizing: no meaningful progress");
                break;
            }
        }
        let residual = if let Some(reason) = limitation {
            Some(format!("observed=unavailable ({reason})"))
        } else {
            let (_, observed) = sibling_geometry(&snapshot.tree, id, split)?;
            (sizing_error(&targets, &observed) > SIZE_TOLERANCE)
                .then(|| format!("observed={}", shares(&observed)))
        };
        if let Some(residual) = residual {
            let warning = format!(
                "sizing group={id} path={path:?} axis={axis}: targets={}; {residual}; bounded adjustment stopped (tolerance 5 percentage points)",
                shares(&targets)
            );
            self.diagnostics.record(&format!("warning: {warning}"));
            warnings.push(warning);
        }

        for (index, child) in children.iter().enumerate() {
            let mut child_path = path.to_vec();
            child_path.push(index);
            self.size(&child.value, &child_path, warnings)?;
        }
        Ok(())
    }

    fn build(
        &mut self,
        node: &'a crate::config::Node,
        after: Option<Vec<usize>>,
    ) -> Result<Vec<usize>, String> {
        let NodeKind::Split { split, children } = &node.kind else {
            return self.launch(node, after);
        };
        let first = self.build(&children[0].value, after)?;
        let layout = match split {
            Orientation::Horizontal => NodeLayout::SplitH,
            Orientation::Vertical => NodeLayout::SplitV,
        };
        let split_command = match split {
            Orientation::Horizontal => "splith",
            Orientation::Vertical => "splitv",
        };
        let mut expected = self.expected.clone();
        let group;
        let command;
        if first.is_empty() {
            // The first completed child occupies the workspace. workspace_split wraps
            // its populated children, preserving even equal-axis meaningful groups.
            // Select its direct owned child and explicitly focus its known parent.
            let direct = vec![0];
            self.focus(&direct)?;
            let id = self.expected.at_mut(&direct).id();
            let snapshot =
                self.command(format!("[con_id={id}] focus parent"), self.expected.clone())?;
            if !snapshot
                .tree
                .find_as_ref(|n| n.id == self.target)
                .is_some_and(|n| n.focused)
            {
                return Err("expected explicit workspace parent focus before wrapping".into());
            }
            let Hierarchy::Split(_, old_layout, old_children) = expected else {
                unreachable!()
            };
            expected = Hierarchy::Split(
                Some(self.target),
                layout,
                vec![Hierarchy::Split(None, old_layout, old_children)],
            );
            command = split_command.to_owned();
            group = Vec::new();
        } else {
            let id = self.expected.at_mut(&first).id();
            let parent = &first[..first.len() - 1];
            let Hierarchy::Split(_, parent_layout, siblings) = expected.at_mut(parent) else {
                unreachable!()
            };
            let singleton = siblings.len() == 1;
            let ordinary_parent = matches!(parent_layout, NodeLayout::SplitH | NodeLayout::SplitV);
            if singleton {
                *parent_layout = layout;
                group = parent.to_vec();
            } else {
                let child = expected.at_mut(&first).clone();
                *expected.at_mut(&first) = Hierarchy::Split(None, layout, vec![child]);
                group = first;
            }
            command = if singleton && !ordinary_parent {
                let layout_command = match split {
                    Orientation::Horizontal => "layout splith",
                    Orientation::Vertical => "layout splitv",
                };
                format!("[con_id={id}] {layout_command}")
            } else {
                format!("[con_id={id}] {split_command}")
            };
        }
        self.diagnostics.record(&format!(
            "expected split/reparent transition {:?} -> {expected:?}",
            self.expected
        ));
        self.command(command, expected)?;
        for (index, child) in children.iter().enumerate().skip(1) {
            let mut previous = group.clone();
            previous.push(index - 1);
            self.build(&child.value, Some(previous))?;
        }
        Ok(group)
    }

    fn launch(
        &mut self,
        node: &'a crate::config::Node,
        after: Option<Vec<usize>>,
    ) -> Result<Vec<usize>, String> {
        let NodeKind::Window {
            id,
            command,
            matcher,
            cwd,
            env,
        } = &node.kind
        else {
            unreachable!()
        };
        if let Some(path) = &after {
            self.focus(path)?;
        }
        self.diagnostics
            .stage(format!("prelaunch leaf={id:?} workspace={}", self.target));
        let before = self.settled()?;
        if let Some(path) = &after {
            let previous = self.expected.at_mut(path).id();
            if !before
                .tree
                .find_as_ref(|n| n.id == previous)
                .is_some_and(|n| n.focused)
            {
                return Err(format!(
                    "insertion focus left explicitly selected container id={previous} before launch"
                ));
            }
        }
        let mut path = after.unwrap_or_else(|| vec![0]);
        if !self.accepted.is_empty() {
            *path.last_mut().expect("sibling path") += 1;
        }
        let mut windows = Windows {
            target: self.target,
            existing: self.existing.clone(),
            matcher,
            accepted: &self.accepted,
            observed: BTreeMap::new(),
            matched: None,
            invalid: BTreeMap::new(),
        };
        let started = Instant::now();
        let deadline = started
            .checked_add(Duration::from_millis(self.timeout_ms))
            .ok_or("window_timeout_ms exceeds the monotonic clock range; use a smaller timeout")?;
        self.diagnostics
            .stage(format!("launch leaf={id:?} workspace={}", self.target));
        self.diagnostics.record(&format!(
            "expected {matcher:?}; launching once (argv/env omitted)"
        ));
        let mut child = spawn(command, cwd.as_ref(), env)?;
        self.diagnostics
            .record(&format!("spawned pid={}", child.id()));
        let mut exited = false;
        let mut accepted = None;
        let mut expected = self.expected.clone();
        loop {
            self.diagnostics.stage(format!(
                "{} leaf={id:?}",
                if accepted.is_some() {
                    "final verification"
                } else {
                    "wait"
                }
            ));
            if Instant::now() >= deadline {
                return Err(format!(
                    "window timeout: {}",
                    windows.wait_diagnostics(started)
                ));
            }
            let snapshot = self
                .observer
                .snapshot(
                    (self.connect)()?,
                    deadline.min(Instant::now() + IPC_TIMEOUT),
                )
                .map_err(|error| format!("{error}; {}", windows.wait_diagnostics(started)))?;
            let was_accepted = accepted.is_some();
            let candidate = windows.reconcile(&snapshot, self.diagnostics)?;
            if let Some(previous) = accepted {
                if candidate != Some(previous) {
                    return Err(format!(
                        "accepted window id={previous} disappeared or failed revalidation"
                    ));
                }
            } else if let Some(candidate) = candidate {
                let parent = &path[..path.len() - 1];
                let Hierarchy::Split(_, _, children) = expected.at_mut(parent) else {
                    unreachable!()
                };
                children.insert(
                    *path.last().expect("leaf path"),
                    Hierarchy::Window(candidate),
                );
                accepted = Some(candidate);
                self.diagnostics.record(&format!(
                    "provisionally accepted leaf={id:?} id={candidate}; rechecking fresh state"
                ));
            }
            if let Some(candidate) = accepted {
                let workspace = snapshot
                    .tree
                    .find_as_ref(|n| n.id == self.target)
                    .ok_or("target missing")?;
                self.diagnostics.record(&format!(
                    "expected hierarchy {expected:?}; observed {}",
                    describe(workspace)
                ));
                expected.verify(workspace)?;
                if !snapshot
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::Window(_)))
                {
                    // Require two fresh trees, as in the original single-window path.
                    if was_accepted {
                        self.diagnostics.record(&format!(
                            "accepted leaf={id:?} id={candidate}; hierarchy/identity verified"
                        ));
                        reap_if_exited(&mut child, &mut exited, self.diagnostics)?;
                        self.accepted.insert(candidate, matcher);
                        self.expected = expected;
                        return Ok(path);
                    }
                }
                if !was_accepted {
                    continue;
                }
            }
            reap_if_exited(&mut child, &mut exited, self.diagnostics)?;
            let poll = deadline.min(Instant::now() + POLL_INTERVAL);
            while let Some(event) = self.observer.next_until(poll)? {
                windows.event(&event, self.diagnostics)?;
            }
        }
    }
}

fn proportions(weights: &[f64]) -> Vec<f64> {
    let total: f64 = weights.iter().sum();
    weights.iter().map(|weight| weight / total).collect()
}

fn sizing_error(targets: &[f64], observed: &[f64]) -> f64 {
    targets
        .iter()
        .zip(observed)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f64::max)
}

fn shares(values: &[f64]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|v| format!("{:.2}%", v * 100.0))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

// Call only after verifying the complete owned hierarchy. Use container rects,
// which include decorations, rather than content geometry or parent-area percent.
fn sibling_geometry(
    tree: &Node,
    id: i64,
    split: &Orientation,
) -> Result<(Vec<f64>, Vec<f64>), String> {
    let group = tree
        .find_as_ref(|node| node.id == id)
        .ok_or("sizing group missing")?;
    let dimensions: Vec<_> = group
        .nodes
        .iter()
        .map(|child| {
            f64::from(match split {
                Orientation::Horizontal => child.rect.width,
                Orientation::Vertical => child.rect.height,
            })
        })
        .collect();
    if dimensions.len() < 2 || dimensions.iter().any(|dimension| *dimension <= 0.0) {
        return Err(format!(
            "nonpositive or missing sibling dimensions: {dimensions:?}"
        ));
    }
    if dimensions.iter().sum::<f64>() > f64::from(i32::MAX) {
        return Err("sibling dimensions exceed Sway's integer resize range".into());
    }
    let observed = proportions(&dimensions);
    Ok((dimensions, observed))
}

fn focus_path<'a>(
    node: &'a crate::config::Node,
    requested: Option<&str>,
    path: &[usize],
) -> Option<(&'a str, Vec<usize>)> {
    match &node.kind {
        NodeKind::Window { id, .. } => requested
            .is_none_or(|wanted| wanted == id)
            .then(|| (id.as_str(), path.to_vec())),
        NodeKind::Split { children, .. } => {
            for (index, child) in children.iter().enumerate() {
                let mut child_path = path.to_vec();
                child_path.push(index);
                if let Some(found) = focus_path(&child.value, requested, &child_path) {
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
        .map_err(|e| format!("clone command socket: {e}"))?;
    let (sender, receiver) = mpsc::channel();
    let worker = thread::Builder::new()
        .name("sway-command".into())
        .spawn(move || {
            // swayipc 4.0's run_command drops the success flag. Retain it using
            // its typed decoder, with a bounded frame reader for this command only.
            let read = || -> Result<Vec<Result<(), String>>, String> {
                #[derive(serde::Deserialize)]
                struct Reply {
                    success: bool,
                    error: Option<String>,
                }
                let mut socket = socket;
                socket
                    .write_all(&CommandType::RunCommand.encode_with(command.as_bytes()))
                    .map_err(|e| format!("command IPC write: {e}"))?;
                let mut header = [0; 14];
                socket
                    .read_exact(&mut header)
                    .map_err(|e| format!("command IPC header: {e}"))?;
                if &header[..6] != b"i3-ipc" {
                    return Err("command IPC: invalid reply magic".into());
                }
                let size =
                    u32::from_ne_bytes(header[6..10].try_into().expect("header size")) as usize;
                let kind = u32::from_ne_bytes(header[10..14].try_into().expect("header type"));
                if kind != 0 || size > 1024 * 1024 {
                    return Err("command IPC: invalid reply type or reply exceeds 1 MiB".into());
                }
                let mut payload = vec![0; size];
                socket
                    .read_exact(&mut payload)
                    .map_err(|e| format!("command IPC payload: {e}"))?;
                let replies: Vec<Reply> = CommandType::RunCommand
                    .decode((kind, payload))
                    .map_err(|e| format!("command IPC decode: {e}"))?;
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
            let result = read();
            let _ = sender.send(result);
        })
        .map_err(|e| format!("start command reader: {e}"))?;
    let result = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let _ = control.shutdown(Shutdown::Both);
    if worker.join().is_err() {
        return Err("command reader panicked".into());
    }
    match result {
        Ok(Ok(replies)) => Ok(replies),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(format!("command deadline/channel failure: {error}")),
    }
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

fn validate_accepted(
    snapshot: &Snapshot,
    accepted: &BTreeMap<i64, &Matcher>,
    target: i64,
    diagnostics: &mut Diagnostics,
) -> Result<(), String> {
    for event in &snapshot.events {
        accepted_event(event, accepted, diagnostics)?;
    }
    let mut windows = Vec::new();
    collect(&snapshot.tree, None, false, false, false, &mut windows);
    for (id, matcher) in accepted {
        let located = windows
            .iter()
            .find(|w| w.node.id == *id)
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

fn matches_window(matcher: &Matcher, node: &Node) -> bool {
    match matcher {
        Matcher::AppId(value) => node.app_id.as_ref() == Some(value),
        Matcher::Class(value) => {
            node.window_properties
                .as_ref()
                .and_then(|p| p.class.as_ref())
                == Some(value)
        }
    }
}

fn accepted_event(
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
    if node.iter().any(|n| accepted.contains_key(&n.id))
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
    child: &mut Child,
    exited: &mut bool,
    diagnostics: &mut Diagnostics,
) -> Result<(), String> {
    if !*exited
        && let Some(status) = child
            .try_wait()
            .map_err(|error| format!("inspect child: {error}"))?
    {
        *exited = true;
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

fn prelaunch_history(snapshot: &Snapshot, existing: &mut HashSet<i64>) -> Result<(), String> {
    for event in &snapshot.events {
        if let Event::Window(window) = event {
            existing.insert(window.container.id);
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

struct Located<'a> {
    node: &'a Node,
    workspace: Option<i64>,
    floating: bool,
    fullscreen: bool,
    scratchpad: bool,
}

fn collect<'a>(
    node: &'a Node,
    workspace: Option<i64>,
    floating: bool,
    fullscreen: bool,
    scratchpad: bool,
    windows: &mut Vec<Located<'a>>,
) {
    let workspace = if node.node_type == NodeType::Workspace {
        Some(node.id)
    } else {
        workspace
    };
    let floating = floating
        || node.node_type == NodeType::FloatingCon
        || matches!(node.floating, Some(Floating::AutoOn | Floating::UserOn));
    // Sway sets fullscreen_mode=1 on every workspace for i3 compatibility.
    // Only container values describe fullscreen state that applies to descendants.
    let fullscreen = fullscreen
        || (matches!(node.node_type, NodeType::Con | NodeType::FloatingCon)
            && node.fullscreen_mode.is_some_and(|mode| mode != 0));
    let scratchpad = scratchpad
        || matches!(
            node.scratchpad_state,
            Some(ScratchpadState::Fresh | ScratchpadState::Changed)
        )
        || (node.node_type == NodeType::Workspace && node.name.as_deref() == Some("__i3_scratch"));
    if matches!(node.node_type, NodeType::Con | NodeType::FloatingCon)
        && (node.nodes.is_empty() && node.floating_nodes.is_empty()
            || node.app_id.is_some()
            || node.window.is_some()
            || node.shell.is_some())
    {
        windows.push(Located {
            node,
            workspace,
            floating,
            fullscreen,
            scratchpad,
        });
    }
    for child in &node.nodes {
        collect(child, workspace, floating, fullscreen, scratchpad, windows);
    }
    for child in &node.floating_nodes {
        collect(child, workspace, true, fullscreen, scratchpad, windows);
    }
}

struct Windows<'a> {
    target: i64,
    existing: HashSet<i64>,
    matcher: &'a Matcher,
    accepted: &'a BTreeMap<i64, &'a Matcher>,
    observed: BTreeMap<i64, String>,
    matched: Option<i64>,
    invalid: BTreeMap<i64, &'static str>,
}

impl Windows<'_> {
    fn identity<'a>(&self, node: &'a Node) -> Option<&'a str> {
        match self.matcher {
            Matcher::AppId(_) => node.app_id.as_deref(),
            Matcher::Class(_) => node
                .window_properties
                .as_ref()
                .and_then(|p| p.class.as_deref()),
        }
        .filter(|value| !value.is_empty())
    }

    fn matches(&self, node: &Node) -> bool {
        let expected = match self.matcher {
            Matcher::AppId(value) | Matcher::Class(value) => value,
        };
        self.identity(node) == Some(expected.as_str())
    }

    fn remember(
        &mut self,
        node: &Node,
        location: &str,
        diagnostics: &mut Diagnostics,
    ) -> Result<(), String> {
        if self.observed.len() >= OBSERVED_LIMIT && !self.observed.contains_key(&node.id) {
            return Err("new-window history limit exceeded; observation is uncertain".into());
        }
        let description = format!(
            "id={} app_id={:?} class={:?} {location} floating={:?} fullscreen={:?}",
            node.id,
            node.app_id,
            node.window_properties
                .as_ref()
                .and_then(|p| p.class.as_ref()),
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
        if self.matches(node) {
            if let Some(previous) = self.matched
                && previous != node.id
            {
                return Err(format!(
                    "ambiguous new windows id={previous} and id={} match {:?}",
                    node.id, self.matcher
                ));
            }
            self.matched = Some(node.id);
            if let Some(reason) = self.invalid.get(&node.id) {
                return Err(format!(
                    "matching window id={} {reason}; refusing repair",
                    node.id,
                ));
            }
        } else if self.identity(node).is_some() {
            self.invalid
                .insert(node.id, "changed identity after a nonmatching identity");
            if self.matched == Some(node.id) {
                return Err(format!("matched window id={} changed identity", node.id));
            }
        }
        Ok(())
    }

    fn event(&mut self, event: &Event, diagnostics: &mut Diagnostics) -> Result<(), String> {
        let Event::Window(window) = event else {
            return Ok(());
        };
        accepted_event(event, self.accepted, diagnostics)?;
        let node = &window.container;
        if self.accepted.contains_key(&node.id) {
            return Ok(());
        }
        if self.existing.contains(&node.id) {
            return Ok(());
        }
        if matches!(node.floating, Some(Floating::AutoOn | Floating::UserOn))
            || node.node_type == NodeType::FloatingCon
            || node.fullscreen_mode.is_some_and(|mode| mode != 0)
        {
            self.invalid.insert(node.id, "was floating/fullscreen");
        }
        self.remember(
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

    fn reconcile(
        &mut self,
        snapshot: &Snapshot,
        diagnostics: &mut Diagnostics,
    ) -> Result<Option<i64>, String> {
        validate_accepted(snapshot, self.accepted, self.target, diagnostics)?;
        for event in &snapshot.events {
            self.event(event, diagnostics)?;
        }
        let mut windows = Vec::new();
        collect(&snapshot.tree, None, false, false, false, &mut windows);
        let mut candidate = None;
        let mut unresolved = false;
        for located in &windows {
            let node = located.node;
            if self.accepted.contains_key(&node.id) {
                continue;
            }
            if self.existing.contains(&node.id) {
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
                self.invalid.insert(node.id, "was floating/fullscreen");
            }
            self.remember(node, &location, diagnostics)?;
            if located.workspace == Some(self.target)
                && (located.floating || located.fullscreen || located.scratchpad)
            {
                return Err(format!(
                    "unexpected floating/fullscreen/scratchpad window id={} on target workspace; refusing repair",
                    node.id
                ));
            }
            if self.matches(node) {
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
                if self.identity(node).is_some() {
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
            .any(|id| !windows.iter().any(|w| w.node.id == *id))
        {
            unresolved = true;
        }
        if let Some(matched) = self.matched {
            if let Some(located) = windows.iter().find(|w| w.node.id == matched) {
                if !self.matches(located.node) {
                    return Err(format!("matched window id={matched} changed identity"));
                }
            } else if !snapshot
                .events
                .iter()
                .any(|event| matches!(event, Event::Window(w) if w.container.id == matched))
            {
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
