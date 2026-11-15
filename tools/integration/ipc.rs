//! Raw Sway IPC through `swaymsg`, decoded independently of the production
//! `swayipc` client so the harness judges sway-stage against its own view of Sway.
//!
//! The types model only fields the harness reads, and unmodeled fields are ignored.
//! Identities, structure, focus, and geometry are required. `Option` fields accept an
//! absent or null value; Sway reports view properties only for views, for example.

use std::fs;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
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
/// fences, the event subscription, and only the lifecycle's narrow mutations.
pub struct Swaymsg {
    executable: PathBuf,
    socket: PathBuf,
}

impl Swaymsg {
    /// The headless session's private socket, named by the caller: the launcher creates it,
    /// and the runner checks that its `SWAYSOCK` names one. Never consults `I3SOCK` or
    /// socket discovery, so a missing private socket cannot fall back to another session.
    pub fn at_socket(socket: PathBuf) -> Result<Self, String> {
        if !fs::metadata(&socket).is_ok_and(|metadata| metadata.file_type().is_socket()) {
            return Err(format!("no Sway IPC socket at {}", socket.display()));
        }
        let executable = process::find_executable("swaymsg")?;
        Ok(Self { executable, socket })
    }

    /// The `swaymsg` executable behind every query, for a script that must use the same one.
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// The private socket this client always names explicitly.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn get_tree(&self) -> Result<TreeNode, String> {
        self.query("get_tree", None)
    }

    /// The unparsed reply, for a caller that keeps it as a diagnostic.
    pub fn get_tree_json(&self) -> Result<String, String> {
        self.raw_query("get_tree", None)
    }

    /// Pin sway-stage children to the socket used for our independent observations.
    pub fn select_socket(&self, command: &mut Command) {
        command.env("SWAYSOCK", &self.socket);
    }

    /// The caller observes the result with an expected-switch fence.
    pub fn switch_workspace(&self, name: &str) -> Result<(), String> {
        self.command(&switch_command(name)?)
    }

    /// Lifecycle has just fenced and independently verified the empty workspace's ID.
    pub fn rename_workspace(&self, old_name: &str, new_name: &str) -> Result<(), String> {
        self.command(&rename_command(old_name, new_name)?)
    }

    /// The caller must have just revalidated this window's ownership and hierarchy.
    pub fn close_owned_window(&self, container_id: i64) -> Result<(), String> {
        self.command(&close_command(container_id)?)
    }

    /// Focuses exactly one window. The caller must have just verified that this run opened
    /// it and that it is tiled on the focused workspace, so focus stays on that workspace;
    /// the caller observes the result with a fence.
    pub fn focus_owned_window(&self, container_id: i64) -> Result<(), String> {
        self.command(&focus_command(container_id)?)
    }

    /// Makes exactly one window floating. The caller must have just verified that this run
    /// opened it, and observes the result with a fence.
    pub fn float_owned_window(&self, container_id: i64) -> Result<(), String> {
        self.command(&float_command(container_id)?)
    }

    /// Starts `/bin/sh SCRIPT` through Sway's own `exec`, as a keybinding would, so Sway
    /// records a launch context for the focused workspace. The process inherits the
    /// compositor's environment, never the harness's, and is not our child: it is observed
    /// through files it writes and never signalled.
    pub fn exec_script(&self, script: &Path) -> Result<(), String> {
        self.command(&exec_command(script)?)
    }

    /// Adds a Sway `assign` rule that places every new window with exactly `app_id` on the
    /// workspace named `workspace_name`, creating it if needed. Sway keeps the rule until
    /// the private compositor stops, so the caller names an app ID that only its own
    /// window has.
    pub fn assign_app_id_to_workspace(
        &self,
        app_id: &str,
        workspace_name: &str,
    ) -> Result<(), String> {
        self.command(&assign_command(app_id, workspace_name)?)
    }

    /// Only after a fresh empty-target, focus, identity, and output check by the caller.
    pub fn set_empty_workspace_layout(&self, layout: InitialWorkspaceLayout) -> Result<(), String> {
        self.command(layout.command())
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

    pub fn get_outputs(&self) -> Result<Vec<Output>, String> {
        self.query("get_outputs", None)
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
        let output = self.raw_query(message_type, payload)?;
        serde_json::from_str(&output)
            .map_err(|error| format!("Sway {message_type}: invalid reply: {error}"))
    }

    fn raw_query(&self, message_type: &str, payload: Option<&str>) -> Result<String, String> {
        let mut command = self.invocation();
        command.args(["-t", message_type]);
        if let Some(payload) = payload {
            command.arg(payload);
        }

        process::run(command, &format!("Sway {message_type}"), IPC_TIMEOUT)
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

/// Only names that can be quoted into a Sway command without escaping. Checked wherever a
/// command naming a workspace is constructed.
pub fn validate_workspace_name(name: &str) -> Result<(), String> {
    let mut bytes = name.bytes();
    if !bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || b" _.:+-".contains(&byte))
    {
        return Err(format!(
            "workspace name {name:?} cannot be safely represented in a Sway command"
        ));
    }
    Ok(())
}

/// Also the text an interference script sends, so it is validated the same way.
pub fn switch_command(name: &str) -> Result<String, String> {
    validate_workspace_name(name)?;
    Ok(format!("workspace --no-auto-back-and-forth \"{name}\""))
}

/// Also the text an interference script sends, so it is validated the same way.
pub fn rename_command(old_name: &str, new_name: &str) -> Result<String, String> {
    validate_workspace_name(old_name)?;
    validate_workspace_name(new_name)?;
    Ok(format!("rename workspace \"{old_name}\" to \"{new_name}\""))
}

fn close_command(container_id: i64) -> Result<String, String> {
    if container_id <= 0 {
        return Err("owned window must have a positive container ID".into());
    }
    Ok(format!("[con_id={container_id}] kill"))
}

fn focus_command(container_id: i64) -> Result<String, String> {
    if container_id <= 0 {
        return Err("owned window must have a positive container ID".into());
    }
    Ok(format!("[con_id={container_id}] focus"))
}

fn float_command(container_id: i64) -> Result<String, String> {
    if container_id <= 0 {
        return Err("owned window must have a positive container ID".into());
    }
    Ok(format!("[con_id={container_id}] floating enable"))
}

/// Sway's command parser and the `sh -c` that `exec` runs would both interpret quotes,
/// whitespace, `;`, `,`, and `$`, so only an absolute path made of plainly safe bytes is
/// sent. Everything else the script needs is already written inside it. Also the text an
/// interference script sends.
pub fn exec_command(script: &Path) -> Result<String, String> {
    let text = script
        .to_str()
        .filter(|text| text.starts_with('/'))
        .filter(|text| {
            text.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
        })
        .ok_or_else(|| {
            format!(
                "script path {} cannot be safely represented in a Sway exec command; \
                 set TMPDIR to a plain absolute directory",
                script.display()
            )
        })?;
    Ok(format!("exec /bin/sh {text}"))
}

/// Criteria values are regular expressions inside a quoted argument, so only app IDs of
/// lowercase ASCII letters, digits, and `-` are sent, anchored to match exactly.
fn assign_command(app_id: &str, workspace_name: &str) -> Result<String, String> {
    validate_workspace_name(workspace_name)?;
    let is_plain_app_id = !app_id.is_empty()
        && app_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if !is_plain_app_id {
        return Err(format!(
            "app ID {app_id:?} cannot be safely represented in a Sway assign rule"
        ));
    }
    Ok(format!(
        "assign [app_id=\"^{app_id}$\"] workspace \"{workspace_name}\""
    ))
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
    pub num: Option<i32>,
    pub focused: bool,
    /// Direct children in seat focus-history order, distinct from global focus.
    pub focus: Vec<i64>,
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

/// A layout set on an empty workspace: the initial conditions exercised before ordinary
/// construction, and each layout of the session round-trip's workspaces,
/// whose windows Sway then arranges itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitialWorkspaceLayout {
    SplitH,
    SplitV,
    Tabbed,
    Stacked,
}

impl InitialWorkspaceLayout {
    pub fn command(self) -> &'static str {
        match self {
            Self::SplitH => "layout splith",
            Self::SplitV => "layout splitv",
            Self::Tabbed => "layout tabbed",
            Self::Stacked => "layout stacking",
        }
    }

    pub fn observed_layout(self) -> Layout {
        match self {
            Self::SplitH => Layout::SplitH,
            Self::SplitV => Layout::SplitV,
            Self::Tabbed => Layout::Tabbed,
            Self::Stacked => Layout::Stacked,
        }
    }
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

/// A `get_outputs` entry.
#[derive(Debug, Deserialize)]
pub struct Output {
    pub name: String,
    pub active: bool,
    /// The output's logical area in the global layout.
    pub rect: Rect,
    /// The visible workspace's name; null for a disabled output.
    pub current_workspace: Option<String>,
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
    #[serde(rename = "type")]
    pub node_type: Option<NodeType>,
    pub num: Option<i32>,
    pub id: i64,
    pub name: Option<String>,
}

#[cfg(test)]
#[path = "ipc_tests.rs"]
mod tests;
