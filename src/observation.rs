//! Bounded, ordered Sway observation and read-only tree inspection. No desktop mutations:
//! nothing here sends commands, changes focus, or decides window ownership.

use std::collections::HashSet;
use std::env;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use swayipc::{
    Connection, Event, EventType, Floating, Node, NodeLayout, NodeType, ScratchpadState,
    WorkspaceChange,
};

// Overflow is fatal rather than silently dropping events or allowing unbounded retention.
const EVENT_QUEUE_CAPACITY: usize = 256;
static NEXT_FENCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Sway's own IPC socket. Never use `Connection::new()`: it prefers `I3SOCK` and can spawn
/// socket-discovery processes, so it could reach another compositor or launch a process.
pub fn sway_socket() -> Result<PathBuf, String> {
    env::var_os("SWAYSOCK")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "connect: SWAYSOCK is unset or empty; run inside Sway".to_owned())
}

/// The observer needs a fresh connection for its subscription and for every snapshot.
pub fn connect(socket: &Path) -> Result<UnixStream, String> {
    UnixStream::connect(socket).map_err(|error| format!("connect to SWAYSOCK: {error}"))
}

enum Message {
    Subscribed,
    Event(Event),
}

pub struct Snapshot {
    pub target_workspace_id: i64,
    pub tree: Node,
    /// Ordered history through the tick sent after the queries; not an atomic tree/event view.
    pub events: Vec<Event>,
}

pub struct Observer {
    /// A clone of the reader thread's event socket, kept only to shut it down.
    event_socket: UnixStream,
    reader: Option<JoinHandle<()>>,
    messages: mpsc::Receiver<Message>,
    /// A terminal failure, set by the reader thread or the observer; never cleared.
    failure: Arc<Mutex<Option<String>>>,
    /// The focused workspace frozen by the first snapshot.
    target_workspace_id: Option<i64>,
    /// Containing output's container ID, frozen with the target workspace.
    target_output_id: Option<i64>,
}

impl Observer {
    /// Returns only after Sway acknowledges all subscriptions. Connections are supplied
    /// explicitly so neither socket discovery nor a live compositor is needed in tests.
    pub fn subscribe(socket: UnixStream, deadline: Instant) -> Result<Self, String> {
        let shutdown_socket = socket
            .try_clone()
            .map_err(|error| format!("clone event socket: {error}"))?;

        // Never time out/retry read_exact: swayipc discards partial-frame progress.
        socket
            .set_read_timeout(None)
            .and_then(|()| socket.set_write_timeout(None))
            .map_err(|error| format!("configure event socket: {error}"))?;

        let (sender, messages) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let failure = Arc::new(Mutex::new(None));
        let reader_failure = Arc::clone(&failure);
        let reader = thread::Builder::new()
            .name("sway-events".into())
            .spawn(move || {
                let read_events = || -> Result<(), String> {
                    let mut event_stream = Connection::from(socket)
                        .subscribe([EventType::Workspace, EventType::Window, EventType::Tick])
                        .map_err(|error| format!("subscribe to workspace/window/tick: {error}"))?;
                    sender
                        .try_send(Message::Subscribed)
                        .map_err(|_| "event history unavailable".to_owned())?;

                    loop {
                        let event = event_stream
                            .next()
                            .ok_or("event stream ended")?
                            .map_err(|error| format!("read Sway event: {error}"))?;
                        sender.try_send(Message::Event(event)).map_err(|_| {
                            "event history unavailable: queue overflow or receiver closed"
                                .to_owned()
                        })?;
                    }
                };

                if let Err(error) = read_events()
                    && let Ok(mut failure) = reader_failure.lock()
                    && failure.is_none()
                {
                    *failure = Some(error);
                }
            })
            .map_err(|error| format!("start event reader: {error}"))?;

        let mut observer = Self {
            event_socket: shutdown_socket,
            reader: Some(reader),
            messages,
            failure,
            target_workspace_id: None,
            target_output_id: None,
        };

        match observer.receive(deadline)? {
            Some(Message::Subscribed) => Ok(observer),
            _ => {
                Err("subscribe: deadline expired or unexpected event before acknowledgement".into())
            }
        }
    }

    fn check_health(&self) -> Result<(), String> {
        let failure = self
            .failure
            .lock()
            .map_err(|_| "event reader status poisoned")?;
        if let Some(error) = failure.as_ref() {
            return Err(error.clone());
        }

        if self
            .reader
            .as_ref()
            .is_some_and(|reader| reader.is_finished())
        {
            return Err("event reader stopped; history cannot be trusted".into());
        }

        Ok(())
    }

    fn fail<T>(&mut self, error: String) -> Result<T, String> {
        if let Ok(mut failure) = self.failure.lock() {
            *failure = Some(error.clone());
        }

        let _ = self.event_socket.shutdown(Shutdown::Both);
        Err(error)
    }

    fn receive(&mut self, deadline: Instant) -> Result<Option<Message>, String> {
        self.check_health()?;
        let remaining_time = deadline.saturating_duration_since(Instant::now());
        if remaining_time.is_zero() {
            return Ok(None);
        }

        let received_message = self.messages.recv_timeout(remaining_time);
        self.check_health()?;

        match received_message {
            Ok(message) => Ok(Some(message)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.fail("event reader disconnected; history cannot be trusted".into())
            }
        }
    }

    /// Idle expiry is not an IPC error and does not interrupt the reader's frame.
    /// Periodically query fresh trees too: not every identity/property update emits an event.
    pub fn next_until(&mut self, deadline: Instant) -> Result<Option<Event>, String> {
        if self.target_workspace_id.is_none() {
            return self.fail("capture an initial snapshot before consuming events".into());
        }

        match self.receive(deadline)? {
            Some(Message::Event(event)) => {
                if let Event::Workspace(workspace) = &event {
                    if workspace.change == WorkspaceChange::Reload {
                        return self.fail("workspace reload: revalidation required".into());
                    }

                    let Some(current_workspace) = workspace.current.as_ref() else {
                        return self.fail("workspace event has no current workspace".into());
                    };
                    if current_workspace.node_type != NodeType::Workspace
                        || workspace
                            .old
                            .as_ref()
                            .is_some_and(|old| old.node_type != NodeType::Workspace)
                    {
                        return self.fail("workspace event has an invalid workspace node".into());
                    }

                    if workspace.change == WorkspaceChange::Focus
                        && let Some(target_workspace_id) = self.target_workspace_id
                        && (current_workspace.id != target_workspace_id
                            || workspace
                                .old
                                .as_ref()
                                .is_some_and(|old| old.id != target_workspace_id))
                    {
                        return self.fail(format!(
                            "workspace focus changed from target id={target_workspace_id}"
                        ));
                    }

                    if workspace.change == WorkspaceChange::Empty
                        && Some(current_workspace.id) == self.target_workspace_id
                    {
                        return self.fail("target workspace was destroyed".into());
                    }

                    if workspace.change == WorkspaceChange::Move
                        && Some(current_workspace.id) == self.target_workspace_id
                    {
                        return self.fail(format!(
                            "target workspace id={} moved to another output",
                            current_workspace.id
                        ));
                    }
                }

                if let Event::Window(window) = &event
                    && !matches!(
                        window.container.node_type,
                        NodeType::Con | NodeType::FloatingCon
                    )
                {
                    return self.fail("window event has an invalid container node".into());
                }
                Ok(Some(event))
            }
            Some(Message::Subscribed) => self.fail("duplicate subscription acknowledgement".into()),
            None => Ok(None),
        }
    }

    /// Each snapshot uses a separate, disposable query connection. The temporary query
    /// thread makes even a stalled/fragmented reply subject to the same monotonic deadline.
    pub fn snapshot(&mut self, socket: UnixStream, deadline: Instant) -> Result<Snapshot, String> {
        self.snapshot_for_workspace(socket, deadline, None)
    }

    /// Constrains initial target selection to a saved pre-menu workspace, when supplied.
    /// The comparison and event fence belong to the same observation used by construction.
    pub fn snapshot_for_workspace(
        &mut self,
        socket: UnixStream,
        deadline: Instant,
        expected_workspace_id: Option<i64>,
    ) -> Result<Snapshot, String> {
        self.check_health()?;

        let shutdown_socket = socket
            .try_clone()
            .map_err(|error| format!("clone query socket: {error}"))?;
        let fence_token = format!(
            "sway-stage:{}:{}",
            std::process::id(),
            NEXT_FENCE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let query_fence_token = fence_token.clone();

        let (sender, receiver) = mpsc::channel();
        let query_thread = thread::Builder::new()
            .name("sway-snapshot".into())
            .spawn(move || {
                let query = || -> Result<_, String> {
                    let mut connection = Connection::from(socket);
                    let workspaces = connection
                        .get_workspaces()
                        .map_err(|error| format!("get workspaces: {error}"))?;
                    let tree = connection
                        .get_tree()
                        .map_err(|error| format!("get tree: {error}"))?;

                    // Sway queues this tick behind earlier events on the event socket.
                    // Unlike an empty channel, its receipt proves the reader caught up.
                    if !connection
                        .send_tick(query_fence_token)
                        .map_err(|error| format!("send observation fence: {error}"))?
                    {
                        return Err("Sway rejected observation fence".into());
                    }
                    Ok((workspaces, tree))
                };
                let _ = sender.send(query());
            })
            .map_err(|error| format!("start snapshot reader: {error}"))?;

        let query_result =
            receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        // Also cancels a read_exact partway through a header/payload. Never reuse it.
        let _ = shutdown_socket.shutdown(Shutdown::Both);
        if query_thread.join().is_err() {
            return self.fail("snapshot reader panicked".into());
        }

        let (workspaces, tree) = match query_result {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(error)) => return self.fail(error),
            Err(error) => return self.fail(format!("snapshot deadline/channel failure: {error}")),
        };

        let mut focused_workspaces = workspaces.iter().filter(|workspace| workspace.focused);
        let target_workspace_id = match (focused_workspaces.next(), focused_workspaces.next()) {
            (Some(workspace), None) => workspace.id,
            _ => return self.fail("snapshot: expected exactly one focused workspace".into()),
        };

        let mut workspace_ids = HashSet::new();
        if workspaces
            .iter()
            .any(|workspace| !workspace_ids.insert(workspace.id))
        {
            return self.fail("snapshot: duplicate workspace identities".into());
        }

        if let Some(original_workspace_id) = self.target_workspace_id
            && original_workspace_id != target_workspace_id
        {
            return self.fail(format!(
                "snapshot: focus left target id={original_workspace_id}"
            ));
        }

        if let Some(expected_workspace_id) = expected_workspace_id
            && expected_workspace_id != target_workspace_id
        {
            return self.fail(format!(
                "snapshot: focused workspace id={target_workspace_id} differs from pre-menu workspace id={expected_workspace_id}; keep the original workspace focused"
            ));
        }
        self.target_workspace_id = Some(target_workspace_id);

        let mut container_ids = HashSet::new();
        if tree.node_type != NodeType::Root
            || tree.iter().any(|node| !container_ids.insert(node.id))
        {
            return self.fail("snapshot: invalid root or duplicate container identities".into());
        }

        let Some(workspace) = tree.find_as_ref(|node| node.id == target_workspace_id) else {
            return self.fail("snapshot: target workspace missing from tree".into());
        };
        if workspace.node_type != NodeType::Workspace
            || workspace.iter().filter(|node| node.focused).count() != 1
            || tree.iter().filter(|node| node.focused).count() != 1
        {
            return self.fail("snapshot: workspace/tree focus is inconsistent".into());
        }

        let Some(output) = tree.nodes.iter().find(|container| {
            container.node_type == NodeType::Output
                && container
                    .find_as_ref(|node| node.id == target_workspace_id)
                    .is_some()
        }) else {
            return self.fail("snapshot: target workspace has no containing output".into());
        };
        if let Some(original_output_id) = self.target_output_id
            && output.id != original_output_id
        {
            return self.fail(format!(
                "snapshot: target workspace id={target_workspace_id} changed output from id={original_output_id} to id={}",
                output.id
            ));
        }
        self.target_output_id = Some(output.id);

        let mut events = Vec::new();
        loop {
            match self.next_until(deadline)? {
                Some(Event::Tick(tick)) if !tick.first && tick.payload == fence_token => break,
                Some(Event::Tick(_)) => {}
                Some(event) => {
                    if events.len() == EVENT_QUEUE_CAPACITY {
                        return self.fail("snapshot event history limit exceeded".into());
                    }
                    events.push(event);
                }
                None => return self.fail("snapshot: event fence deadline expired".into()),
            }
        }

        Ok(Snapshot {
            target_workspace_id,
            tree,
            events,
        })
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        // The worker never waits on a channel send. shutdown wakes idle and partial
        // socket reads (and a stalled subscription), so joining does not wait for Sway.
        let _ = self.event_socket.shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// A window-like container with the ancestry that determines its placement. Location
/// alone never establishes ownership.
pub struct LocatedWindow<'a> {
    pub node: &'a Node,
    pub workspace_id: Option<i64>,
    pub floating: bool,
    pub fullscreen: bool,
    pub scratchpad: bool,
}

/// Every `con`/`floating_con` that has view properties or no children, across the whole
/// tree in pre-order with tiled children before floating ones. Structural split wrappers
/// are excluded; an empty container counts, since it may still become a window.
pub fn locate_windows(tree: &Node) -> Vec<LocatedWindow<'_>> {
    let mut windows = Vec::new();
    collect_windows(tree, None, false, false, false, &mut windows);
    windows
}

/// The flags carry placement inherited from `node`'s ancestors.
fn collect_windows<'a>(
    node: &'a Node,
    workspace_id: Option<i64>,
    floating: bool,
    fullscreen: bool,
    scratchpad: bool,
    windows: &mut Vec<LocatedWindow<'a>>,
) {
    let workspace_id = if node.node_type == NodeType::Workspace {
        Some(node.id)
    } else {
        workspace_id
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
        windows.push(LocatedWindow {
            node,
            workspace_id,
            floating,
            fullscreen,
            scratchpad,
        });
    }

    for child in &node.nodes {
        collect_windows(
            child,
            workspace_id,
            floating,
            fullscreen,
            scratchpad,
            windows,
        );
    }
    for child in &node.floating_nodes {
        collect_windows(child, workspace_id, true, fullscreen, scratchpad, windows);
    }
}

/// A native Wayland app ID. Empty values cannot identify a window.
pub fn app_id(node: &Node) -> Option<&str> {
    node.app_id.as_deref().filter(|value| !value.is_empty())
}

/// An Xwayland `window_properties.class`. Empty values cannot identify a window.
pub fn class(node: &Node) -> Option<&str> {
    node.window_properties
        .as_ref()
        .and_then(|properties| properties.class.as_deref())
        .filter(|value| !value.is_empty())
}

/// Child extents along a split's own axis: widths for `splith`, heights for `splitv`.
/// Container rects include decorations; content geometry and Sway's parent-area `percent`
/// are never used, so space between siblings is excluded from their sum.
pub fn sibling_dimensions(group: &Node) -> Result<Vec<f64>, String> {
    let horizontal = match group.layout {
        NodeLayout::SplitH => true,
        NodeLayout::SplitV => false,
        layout => {
            return Err(format!(
                "group id={} has no split axis: {layout:?}",
                group.id
            ));
        }
    };

    let dimensions: Vec<_> = group
        .nodes
        .iter()
        .map(|child| {
            f64::from(if horizontal {
                child.rect.width
            } else {
                child.rect.height
            })
        })
        .collect();
    if dimensions.len() < 2 || dimensions.iter().any(|dimension| *dimension <= 0.0) {
        return Err(format!(
            "nonpositive or missing sibling dimensions: {dimensions:?}"
        ));
    }

    Ok(dimensions)
}

/// Each value's share of their sum.
pub fn proportions(values: &[f64]) -> Vec<f64> {
    let total: f64 = values.iter().sum();
    values.iter().map(|value| value / total).collect()
}

#[cfg(test)]
#[path = "observation_fixtures.rs"]
pub(crate) mod fixtures;

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;
