//! Raw Sway IPC through `swaymsg`, decoded independently of the production
//! `swayipc` client so the harness judges sway-stage against its own view of Sway.
//!
//! The types model only fields the harness reads, and unmodeled fields are ignored.
//! Identities, structure, focus, and geometry are required. `Option` fields accept an
//! absent or null value; Sway reports view properties only for views, for example.

use std::env;
use std::fs;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::process;

/// Bounds each query process and each event fence.
pub const IPC_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_EVENT_BUFFER_BYTES: usize = 1024 * 1024;
const READ_CHUNK_BYTES: usize = 64 * 1024;
const SUBSCRIBED_EVENTS: &str = r#"["workspace","window","tick"]"#;

/// `swaymsg` bound to one explicit compositor socket. It provides read-only queries, tick
/// fences, the event subscription, and only the lifecycle's three narrow mutations.
pub struct Swaymsg {
    executable: PathBuf,
    socket: PathBuf,
}

impl Swaymsg {
    /// Uses `SWAYSOCK` only. Never fall back to `I3SOCK` or socket discovery, which could
    /// reach a compositor other than the disposable session.
    pub fn from_environment() -> Result<Self, String> {
        let socket = env::var_os("SWAYSOCK")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .filter(|path| {
                fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
            })
            .ok_or("SWAYSOCK must identify an existing Unix socket")?;
        let executable = process::find_executable("swaymsg")?;
        Ok(Self { executable, socket })
    }

    pub fn get_tree(&self) -> Result<TreeNode, String> {
        self.query("get_tree", None)
    }

    /// Pin sway-stage children to the socket used for our independent observations.
    pub fn select_socket(&self, command: &mut Command) {
        command.env("SWAYSOCK", &self.socket);
    }

    /// Read-only diagnostics remain possible after SIGINT, without another event fence.
    pub fn remaining_tree(&self) -> Result<TreeNode, String> {
        let mut command = self.invocation();
        command.args(["-t", "get_tree"]);
        let output = process::run_diagnostic(command, IPC_TIMEOUT)?;
        serde_json::from_str(&output)
            .map_err(|error| format!("remaining-window tree: invalid reply: {error}"))
    }

    pub fn switch_disposable(&self, number: &str) -> Result<(), String> {
        self.command(&disposable_command(number)?)
    }

    pub fn restore_workspace(&self, name: &str) -> Result<(), String> {
        self.command(&restoration_command(name)?)
    }

    /// The caller must have just revalidated this window's ownership and hierarchy.
    pub fn close_owned_window(&self, container_id: i64) -> Result<(), String> {
        self.command(&close_command(container_id)?)
    }

    fn command(&self, text: &str) -> Result<(), String> {
        process::check_interruption()?;
        let mut command = self.invocation();
        command.args(["-t", "command", text]);
        let output = process::run(command, "Sway command", IPC_TIMEOUT)?;
        decode_command_reply(&output).map_err(|error| format!("Sway command {text:?}: {error}"))
    }

    pub fn get_workspaces(&self) -> Result<Vec<Workspace>, String> {
        self.query("get_workspaces", None)
    }

    pub fn seat_names(&self) -> Result<Vec<String>, String> {
        #[derive(Deserialize)]
        struct Seat {
            name: String,
        }

        let seats: Vec<Seat> = self.query("get_seats", None)?;
        Ok(seats.into_iter().map(|seat| seat.name).collect())
    }

    /// Sway queues the tick on every tick subscriber's connection before replying.
    pub fn send_tick(&self, payload: &str) -> Result<(), String> {
        #[derive(Deserialize)]
        struct TickReply {
            success: bool,
        }

        let reply: TickReply = self.query("send_tick", Some(payload))?;
        if !reply.success {
            return Err("Sway rejected the event fence tick".into());
        }
        Ok(())
    }

    /// Starts `swaymsg -m`, which prints each workspace, window, and tick event as one
    /// flushed line of JSON until it is stopped.
    pub fn subscribe(&self) -> Result<EventSubscription, String> {
        let mut command = self.invocation();
        command.args(["-m", "-t", "subscribe", SUBSCRIBED_EVENTS]);
        let (helper, output) = process::spawn_with_output(command, "Sway event subscription")?;
        Ok(EventSubscription {
            helper: Some(helper),
            output,
            pending: Vec::new(),
        })
    }

    /// The explicit executable and socket, with raw JSON output.
    fn invocation(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command.arg("-s").arg(&self.socket).arg("-r");
        command
    }

    fn query<T: DeserializeOwned>(
        &self,
        message_type: &str,
        payload: Option<&str>,
    ) -> Result<T, String> {
        let mut command = self.invocation();
        command.args(["-t", message_type]);
        if let Some(payload) = payload {
            command.arg(payload);
        }

        let operation = format!("Sway {message_type}");
        let output = process::run(command, &operation, IPC_TIMEOUT)?;
        serde_json::from_str(&output)
            .map_err(|error| format!("{operation}: invalid reply: {error}"))
    }
}

/// The persistent `swaymsg -m` helper and its output.
pub struct EventSubscription {
    /// Our own IPC helper, never an application. `None` once stopped.
    helper: Option<Child>,
    /// A blocking socket; each read is bounded by a read timeout instead.
    output: UnixStream,
    /// Received bytes after the last complete line.
    pending: Vec<u8>,
}

impl EventSubscription {
    /// Returns the next event, waiting no later than `deadline`. An expired wait loses no
    /// data: a timed-out read consumes nothing, and a partial line stays pending.
    pub fn next_event(&mut self, deadline: Instant) -> Result<Event, String> {
        let mut chunk = [0; READ_CHUNK_BYTES];
        loop {
            process::check_interruption()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("event observation deadline expired".into());
            }
            if let Some(newline_index) = self.pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=newline_index).collect();
                return decode_event(&line[..newline_index]);
            }

            self.output
                .set_read_timeout(Some(remaining.min(Duration::from_millis(50))))
                .map_err(|error| format!("configure event socket: {error}"))?;
            match self.output.read(&mut chunk) {
                Ok(0) => return Err("Sway event subscription disconnected".into()),
                Ok(count) => {
                    self.pending.extend_from_slice(&chunk[..count]);
                    if self.pending.len() > MAX_EVENT_BUFFER_BYTES {
                        return Err("event buffer exceeded 1 MiB".into());
                    }
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    // Short reads allow SIGINT polling without losing partial lines.
                    continue;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(format!("read Sway events: {error}")),
            }
        }
    }

    /// Stops the helper and reports a failure to reap it.
    pub fn close(mut self) -> Result<(), String> {
        self.stop()
    }

    fn stop(&mut self) -> Result<(), String> {
        match self.helper.take() {
            Some(mut helper) => process::kill_and_reap(&mut helper, "Sway event subscription"),
            None => Ok(()),
        }
    }
}

/// Positive decimal workspace numbers, with neither leading zeros nor command syntax.
pub fn workspace_number(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 9
        || !matches!(bytes[0], b'1'..=b'9')
        || !bytes.iter().all(u8::is_ascii_digit)
    {
        return Err("use an explicit positive workspace number (1–999999999)".into());
    }
    Ok(text.to_owned())
}

/// Validate before any switch, and again where the restoration command is constructed.
pub fn validate_restoration_name(name: &str) -> Result<(), String> {
    let mut bytes = name.bytes();
    if !bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || b" _.:+-".contains(&byte))
    {
        return Err("starting workspace name cannot be safely represented by this harness".into());
    }
    Ok(())
}

fn disposable_command(number: &str) -> Result<String, String> {
    let number = workspace_number(number)?;
    Ok(format!("workspace --no-auto-back-and-forth {number}"))
}

fn restoration_command(name: &str) -> Result<String, String> {
    validate_restoration_name(name)?;
    Ok(format!("workspace --no-auto-back-and-forth \"{name}\""))
}

fn close_command(container_id: i64) -> Result<String, String> {
    if container_id <= 0 {
        return Err("owned window must have a positive container ID".into());
    }
    Ok(format!("[con_id={container_id}] kill"))
}

fn decode_command_reply(output: &str) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Reply {
        success: bool,
    }

    let replies: Vec<Reply> = serde_json::from_str(output)
        .map_err(|_| "invalid command reply; expected exactly one explicit success".to_owned())?;
    if !matches!(replies.as_slice(), [Reply { success: true }]) {
        return Err("expected exactly one successful command reply".into());
    }
    Ok(())
}

impl Drop for EventSubscription {
    /// An early return or panic must not leave the helper running.
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Raw monitor output omits the event type, but the payloads have distinct keys.
fn decode_event(line: &[u8]) -> Result<Event, String> {
    let value: serde_json::Value =
        serde_json::from_slice(line).map_err(|error| format!("invalid event JSON: {error}"))?;
    let Some(object) = value.as_object() else {
        return Err("Sway event is not a JSON object".into());
    };
    let is_tick = object.contains_key("payload");
    let is_window = object.contains_key("container");

    let event = if is_tick {
        serde_json::from_value(value).map(Event::Tick)
    } else if is_window {
        serde_json::from_value(value).map(Event::Window)
    } else {
        serde_json::from_value(value).map(Event::Workspace)
    };
    event.map_err(|error| format!("invalid Sway event: {error}"))
}

/// A `get_tree` node. Configured layout nodes are a separate concept and never this type.
#[derive(Debug, Deserialize)]
pub struct TreeNode {
    pub id: i64,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    pub name: Option<String>,
    pub layout: Layout,
    pub focused: bool,
    pub rect: Rect,
    pub app_id: Option<String>,
    pub pid: Option<i32>,
    /// An Xwayland window's X11 ID.
    pub window: Option<i64>,
    pub shell: Option<Shell>,
    pub window_properties: Option<WindowProperties>,
    pub floating: Option<Floating>,
    pub fullscreen_mode: Option<u8>,
    pub scratchpad_state: Option<ScratchpadState>,
    pub nodes: Vec<TreeNode>,
    pub floating_nodes: Vec<TreeNode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    Root,
    Output,
    Workspace,
    Con,
    FloatingCon,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    None,
    SplitH,
    SplitV,
    Stacked,
    Tabbed,
    Output,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shell {
    XdgShell,
    Xwayland,
}

#[derive(Debug, Deserialize)]
pub struct WindowProperties {
    /// The Xwayland `WM_CLASS` class.
    pub class: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Floating {
    AutoOn,
    AutoOff,
    UserOn,
    UserOff,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScratchpadState {
    None,
    Fresh,
    Changed,
}

/// A `get_workspaces` entry.
#[derive(Debug, Deserialize)]
pub struct Workspace {
    pub id: i64,
    /// The leading workspace number, or -1 when the name does not start with one.
    pub num: i32,
    pub name: String,
    pub focused: bool,
}

#[derive(Debug)]
pub enum Event {
    Tick(TickEvent),
    Window(WindowEvent),
    Workspace(WorkspaceEvent),
}

#[derive(Debug, Deserialize)]
pub struct TickEvent {
    /// Set only on the tick Sway sends when the subscription starts.
    pub first: bool,
    pub payload: String,
}

#[derive(Debug, Deserialize)]
pub struct WindowEvent {
    pub change: WindowChange,
    pub container: EventContainer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowChange {
    New,
    Close,
    Focus,
    Title,
    FullscreenMode,
    Move,
    Floating,
    Urgent,
    Mark,
}

/// The identity fields of a window event's container.
#[derive(Debug, Deserialize)]
pub struct EventContainer {
    pub id: i64,
    pub app_id: Option<String>,
    pub window_properties: Option<WindowProperties>,
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceEvent {
    pub change: WorkspaceChange,
    pub current: Option<EventWorkspace>,
    /// The previously focused workspace, reported only for focus changes.
    pub old: Option<EventWorkspace>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceChange {
    Init,
    Empty,
    Focus,
    Move,
    Rename,
    Urgent,
    Reload,
}

/// The identity fields of a workspace in a workspace event.
#[derive(Debug, Deserialize)]
pub struct EventWorkspace {
    pub id: i64,
    pub name: Option<String>,
}

#[cfg(test)]
#[path = "ipc_tests.rs"]
mod tests;
