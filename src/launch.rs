//! Conservative single-window launch; no Sway layout or focus commands.

use crate::config::{Config, Matcher, NodeKind};
use crate::diagnostics::Diagnostics;
use crate::observation::{Observer, Snapshot};
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use swayipc::{Event, Floating, Node, NodeType, ScratchpadState, WindowChange};

const IPC_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const OBSERVED_LIMIT: usize = 256;

pub fn load(config: &Config, diagnostics: &mut Diagnostics) -> Result<(), String> {
    single_window(config)?;
    let socket = env::var_os("SWAYSOCK")
        .filter(|path| !path.is_empty())
        .ok_or("connect: SWAYSOCK is unset or empty; run inside Sway")?;
    let mut connect =
        || UnixStream::connect(&socket).map_err(|error| format!("connect to SWAYSOCK: {error}"));
    load_with(config, &mut connect, diagnostics)
}

fn single_window(config: &Config) -> Result<(), String> {
    if !matches!(config.layout.value.kind, NodeKind::Window { .. }) {
        return Err("nested layout execution is not yet implemented; load requires a single window at the root (use check to validate general layouts)".into());
    }
    Ok(())
}

// Supplying concrete sockets is the same isolated test seam used by Observer. Process
// spawning and the entire observation/acceptance sequence still run normally in fixtures.
fn load_with(
    config: &Config,
    connect: &mut dyn FnMut() -> Result<UnixStream, String>,
    diagnostics: &mut Diagnostics,
) -> Result<(), String> {
    single_window(config)?;
    let NodeKind::Window {
        id,
        command,
        matcher,
        cwd,
        env,
    } = &config.layout.value.kind
    else {
        unreachable!("single_window validated the root");
    };
    diagnostics.stage(format!("subscribe leaf={id:?}"));
    let mut observer = Observer::subscribe(connect()?, Instant::now() + IPC_TIMEOUT)?;
    let initial = observer.snapshot(connect()?, Instant::now() + IPC_TIMEOUT)?;
    let target = initial.target;
    diagnostics.stage(format!("prelaunch leaf={id:?} workspace={target}"));
    require_empty(&initial, diagnostics)?;
    let mut existing: HashSet<i64> = initial.tree.iter().map(|node| node.id).collect();
    prelaunch_history(&initial, &mut existing)?;

    // A second fenced tree reconciles events newer than the first tree. Keep the union
    // of IDs: a preexisting window which closes cannot later become ours.
    let before = observer.snapshot(connect()?, Instant::now() + IPC_TIMEOUT)?;
    require_empty(&before, diagnostics)?;
    existing.extend(before.tree.iter().map(|node| node.id));
    prelaunch_history(&before, &mut existing)?;
    let mut windows = Windows {
        target,
        existing,
        matcher,
        observed: BTreeMap::new(),
        matched: None,
        invalid: BTreeMap::new(),
    };

    let duration = Duration::from_millis(config.window_timeout_ms);
    let started = Instant::now();
    let deadline = started
        .checked_add(duration)
        .ok_or("window_timeout_ms exceeds the monotonic clock range; use a smaller timeout")?;
    diagnostics.stage(format!("launch leaf={id:?} workspace={target}"));
    diagnostics.record(&format!(
        "expected {matcher:?}; launching once (argv/env omitted)"
    ));
    let mut child = spawn(command, cwd.as_ref(), env)?;
    diagnostics.record(&format!("spawned pid={}", child.id()));
    let mut exited = false;
    let mut accepted = None;
    loop {
        let phase = if accepted.is_some() {
            "final verification"
        } else {
            "wait"
        };
        diagnostics.stage(format!("{phase} leaf={id:?} workspace={target}"));
        if Instant::now() >= deadline {
            return Err(format!(
                "window timeout: {}",
                windows.wait_diagnostics(started)
            ));
        }
        let snapshot = observer
            .snapshot(connect()?, deadline.min(Instant::now() + IPC_TIMEOUT))
            .map_err(|error| format!("{error}; {}", windows.wait_diagnostics(started)))?;
        let candidate = windows.reconcile(&snapshot, diagnostics)?;
        if let Some(accepted) = accepted {
            if candidate != Some(accepted) {
                return Err(format!(
                    "accepted window id={accepted} or surrounding window state failed revalidation"
                ));
            }
            // Any event in this batch may postdate its tree. Only finish at a fence
            // without window activity, following a fresh verification of the same ID.
            if !snapshot
                .events
                .iter()
                .any(|event| matches!(event, Event::Window(_)))
            {
                diagnostics.record(&format!(
                    "accepted id={accepted}; final hierarchy/identity verified after {}ms",
                    started.elapsed().as_millis()
                ));
                reap_if_exited(&mut child, &mut exited, diagnostics)?;
                return Ok(());
            }
        } else if let Some(candidate) = candidate {
            accepted = Some(candidate);
            diagnostics.record(&format!(
                "provisionally accepted id={candidate}; rechecking fresh state"
            ));
            continue;
        }
        reap_if_exited(&mut child, &mut exited, diagnostics)?;
        let poll = deadline.min(Instant::now() + POLL_INTERVAL);
        while let Some(event) = observer.next_until(poll)? {
            windows.event(&event, diagnostics)?;
        }
    }
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
        let node = &window.container;
        // Window IPC events omit workspace ancestry. A move away and back cannot be
        // disproved with a later tree, even for foreign windows. Abort conservatively.
        if window.change == WindowChange::Move {
            return Err(format!(
                "window move id={}; event lacks workspace ancestry, target safety is uncertain",
                node.id
            ));
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
        for event in &snapshot.events {
            self.event(event, diagnostics)?;
        }
        let mut windows = Vec::new();
        collect(&snapshot.tree, None, false, false, false, &mut windows);
        let mut candidate = None;
        let mut unresolved = false;
        for located in &windows {
            let node = located.node;
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
        // A single leaf must be the direct, sole tiled child. Empty wrappers or foreign
        // structural containers are not something this milestone can safely interpret.
        if let Some(candidate) = candidate {
            let workspace = snapshot
                .tree
                .find_as_ref(|n| n.id == self.target)
                .ok_or("target missing")?;
            if workspace.nodes.len() != 1
                || workspace.nodes[0].id != candidate
                || !workspace.floating_nodes.is_empty()
            {
                return Err("unexpected single-window hierarchy on target workspace".into());
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
