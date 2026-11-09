//! Development-only observation boundary for the future loader. No desktop mutations.

use std::collections::HashSet;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use swayipc::{Connection, Event, EventType, Node, NodeType, WorkspaceChange};

// Overflow is fatal rather than silently dropping events or allowing unbounded retention.
const EVENT_CAPACITY: usize = 256;
static NEXT_FENCE: AtomicU64 = AtomicU64::new(0);

enum Message {
    Subscribed,
    Event(Event),
}

pub struct Snapshot {
    pub target: i64,
    pub tree: Node,
    /// Ordered history through the tick sent after the queries; not an atomic tree/event view.
    pub events: Vec<Event>,
}

pub struct Observer {
    socket: UnixStream,
    reader: Option<JoinHandle<()>>,
    messages: mpsc::Receiver<Message>,
    failure: Arc<Mutex<Option<String>>>,
    target: Option<i64>,
}

impl Observer {
    /// Returns only after Sway acknowledges all subscriptions. Connections are supplied
    /// explicitly so neither socket discovery nor a live compositor is needed in tests.
    pub fn subscribe(socket: UnixStream, deadline: Instant) -> Result<Self, String> {
        let control = socket
            .try_clone()
            .map_err(|error| format!("clone event socket: {error}"))?;
        // Never time out/retry read_exact: swayipc discards partial-frame progress.
        socket
            .set_read_timeout(None)
            .and_then(|()| socket.set_write_timeout(None))
            .map_err(|error| format!("configure event socket: {error}"))?;
        let (sender, messages) = mpsc::sync_channel(EVENT_CAPACITY);
        let failure = Arc::new(Mutex::new(None));
        let reader_failure = Arc::clone(&failure);
        let reader = thread::Builder::new()
            .name("sway-events".into())
            .spawn(move || {
                let read = || -> Result<(), String> {
                    let mut events = Connection::from(socket)
                        .subscribe([EventType::Workspace, EventType::Window, EventType::Tick])
                        .map_err(|error| format!("subscribe to workspace/window/tick: {error}"))?;
                    sender
                        .try_send(Message::Subscribed)
                        .map_err(|_| "event history unavailable".to_owned())?;
                    loop {
                        let event = events
                            .next()
                            .ok_or("event stream ended")?
                            .map_err(|error| format!("read Sway event: {error}"))?;
                        sender.try_send(Message::Event(event)).map_err(|_| {
                            "event history unavailable: queue overflow or receiver closed"
                                .to_owned()
                        })?;
                    }
                };
                if let Err(error) = read()
                    && let Ok(mut failure) = reader_failure.lock()
                    && failure.is_none()
                {
                    *failure = Some(error);
                }
            })
            .map_err(|error| format!("start event reader: {error}"))?;
        let mut observer = Self {
            socket: control,
            reader: Some(reader),
            messages,
            failure,
            target: None,
        };
        match observer.receive(deadline)? {
            Some(Message::Subscribed) => Ok(observer),
            _ => {
                Err("subscribe: deadline expired or unexpected event before acknowledgement".into())
            }
        }
    }

    fn healthy(&self) -> Result<(), String> {
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
        let _ = self.socket.shutdown(Shutdown::Both);
        Err(error)
    }

    fn receive(&mut self, deadline: Instant) -> Result<Option<Message>, String> {
        self.healthy()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        let result = self.messages.recv_timeout(remaining);
        self.healthy()?;
        match result {
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
        if self.target.is_none() {
            return self.fail("capture an initial snapshot before consuming events".into());
        }
        match self.receive(deadline)? {
            Some(Message::Event(event)) => {
                if let Event::Workspace(workspace) = &event {
                    if workspace.change == WorkspaceChange::Reload {
                        return self.fail("workspace reload: revalidation required".into());
                    }
                    let Some(current) = workspace.current.as_ref() else {
                        return self.fail("workspace event has no current workspace".into());
                    };
                    if current.node_type != NodeType::Workspace
                        || workspace
                            .old
                            .as_ref()
                            .is_some_and(|old| old.node_type != NodeType::Workspace)
                    {
                        return self.fail("workspace event has an invalid workspace node".into());
                    }
                    if workspace.change == WorkspaceChange::Focus
                        && let Some(target) = self.target
                        && (current.id != target
                            || workspace.old.as_ref().is_some_and(|old| old.id != target))
                    {
                        return self
                            .fail(format!("workspace focus changed from target id={target}"));
                    }
                    if workspace.change == WorkspaceChange::Empty && Some(current.id) == self.target
                    {
                        return self.fail("target workspace was destroyed".into());
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
        self.healthy()?;
        let control = socket
            .try_clone()
            .map_err(|error| format!("clone query socket: {error}"))?;
        let token = format!(
            "sway-stage:{}:{}",
            std::process::id(),
            NEXT_FENCE.fetch_add(1, Ordering::Relaxed)
        );
        let query_token = token.clone();
        let (sender, result) = mpsc::channel();
        let worker = thread::Builder::new()
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
                        .send_tick(query_token)
                        .map_err(|error| format!("send observation fence: {error}"))?
                    {
                        return Err("Sway rejected observation fence".into());
                    }
                    Ok((workspaces, tree))
                };
                let _ = sender.send(query());
            })
            .map_err(|error| format!("start snapshot reader: {error}"))?;
        let result = result.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        // Also cancels a read_exact partway through a header/payload. Never reuse it.
        let _ = control.shutdown(Shutdown::Both);
        if worker.join().is_err() {
            return self.fail("snapshot reader panicked".into());
        }
        let (workspaces, tree) = match result {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(error)) => return self.fail(error),
            Err(error) => return self.fail(format!("snapshot deadline/channel failure: {error}")),
        };
        let mut focused = workspaces.iter().filter(|workspace| workspace.focused);
        let target = match (focused.next(), focused.next()) {
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
        if let Some(previous) = self.target
            && previous != target
        {
            return self.fail(format!("snapshot: focus left target id={previous}"));
        }
        self.target = Some(target);
        let mut ids = HashSet::new();
        if tree.node_type != NodeType::Root || tree.iter().any(|node| !ids.insert(node.id)) {
            return self.fail("snapshot: invalid root or duplicate container identities".into());
        }
        let Some(workspace) = tree.find_as_ref(|node| node.id == target) else {
            return self.fail("snapshot: target workspace missing from tree".into());
        };
        if workspace.node_type != NodeType::Workspace
            || workspace.iter().filter(|node| node.focused).count() != 1
            || tree.iter().filter(|node| node.focused).count() != 1
        {
            return self.fail("snapshot: workspace/tree focus is inconsistent".into());
        }

        let mut events = Vec::new();
        loop {
            match self.next_until(deadline)? {
                Some(Event::Tick(tick)) if !tick.first && tick.payload == token => break,
                Some(Event::Tick(_)) => {}
                Some(event) => {
                    if events.len() == EVENT_CAPACITY {
                        return self.fail("snapshot event history limit exceeded".into());
                    }
                    events.push(event);
                }
                None => return self.fail("snapshot: event fence deadline expired".into()),
            }
        }
        Ok(Snapshot {
            target,
            tree,
            events,
        })
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        // The worker never waits on a channel send. shutdown wakes idle and partial
        // socket reads (and a stalled subscription), so joining does not wait for Sway.
        let _ = self.socket.shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;
