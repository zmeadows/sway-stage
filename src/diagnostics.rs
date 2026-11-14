//! Launch diagnostics and bounded, best-effort notifications.
//!
//! `last-run.log` is reserved for layout launches: only `Diagnostics::start` creates or
//! replaces it. Operations that never load a selected layout report through
//! `notify_capture_failure` or `notify_menu_failure`, leaving the previous launch's log in place.

use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Diagnostics {
    /// The operation being performed, independent of whether it was selected in the menu.
    operation: &'static str,
    log_file: Option<File>,
    log_path: Option<PathBuf>,
    started_at: Instant,
    current_stage: String,
    profile_context: Option<ProfileProgress>,
    /// Set only by `spawned`, after `Command::spawn` succeeded for this invocation.
    application_spawned: bool,
    /// Set only when returning a window timeout, for its concise notification.
    timeout_process_status: Option<ExitStatus>,
    sizing_summary: SizingSummary,
}

/// One residual warning per split group, classified where geometry is observed.
#[derive(Debug)]
pub struct SizingWarning {
    pub detail: String,
    pub geometry_unverified: bool,
}

#[derive(Default)]
struct SizingSummary {
    deviated_groups: usize,
    unverified_groups: usize,
}

impl SizingSummary {
    fn notification(&self, profile: bool, log_available: bool) -> Option<String> {
        if self.deviated_groups + self.unverified_groups == 0 {
            return None;
        }

        let loaded = if profile {
            "Profile loaded."
        } else {
            "Layout loaded."
        };
        let mut body = loaded.to_owned();
        if self.deviated_groups > 0 {
            body.push_str(&format!(
                " Groups outside size tolerance: {}.",
                self.deviated_groups
            ));
        }
        if self.unverified_groups > 0 {
            body.push_str(&format!(
                " Groups with unverifiable sizes: {}.",
                self.unverified_groups
            ));
        }
        Some(with_log_reference(body, log_available))
    }
}

// Owned by diagnostics for the entire profile attempt. Only the coordinator updates
// the current entry and records completion, after its final history/focus verification.
struct ProfileProgress {
    total: usize,
    current: Option<ProfileEntry>,
    completed: Vec<(crate::profile::WorkspaceTarget, i64)>,
}

struct ProfileEntry {
    target: crate::profile::WorkspaceTarget,
    layout: String,
    position: usize,
}

impl ProfileProgress {
    fn detail(&self) -> String {
        let completed_targets = self
            .completed
            .iter()
            .map(|(target, id)| format!("({target}, {id})"))
            .collect::<Vec<_>>()
            .join(", ");
        let current = match &self.current {
            Some(entry) => format!("workspace {}, layout {:?}; ", entry.target, entry.layout),
            None => String::new(),
        };
        format!(
            "{current}completed {} workspaces [{completed_targets}]",
            self.completed.len()
        )
    }

    fn notification(&self, preflight: bool) -> String {
        let Some(entry) = &self.current else {
            return format!("Profile preflight stopped (0/{} completed).", self.total);
        };
        // Even validated names can contain sensitive information. Use their YAML position.
        let location = match &entry.target {
            crate::profile::WorkspaceTarget::Number(number) => format!("workspace {number}"),
            crate::profile::WorkspaceTarget::Name(_) => format!("entry {}", entry.position),
        };
        let phase = if preflight {
            "Profile preflight stopped at"
        } else {
            "Profile stopped at"
        };
        format!(
            "{phase} {location} ({}/{} completed).",
            self.completed.len(),
            self.total
        )
    }
}

impl Diagnostics {
    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf) -> Self {
        let (log_file, log_path) = open_log(path).unwrap();
        Self {
            operation: "load",
            log_file: Some(log_file),
            log_path: Some(log_path),
            started_at: Instant::now(),
            current_stage: "test".into(),
            profile_context: None,
            application_spawned: false,
            timeout_process_status: None,
            sizing_summary: SizingSummary::default(),
        }
    }

    /// Replaces the last-run log. Call only for an invocation that loads a selected layout:
    /// `load`, `profile load`, or `menu` after selection and cancelable prompts.
    pub fn start(operation: &'static str) -> Self {
        let mut diagnostics = Self {
            operation,
            log_file: None,
            log_path: None,
            started_at: Instant::now(),
            current_stage: "validation".into(),
            profile_context: None,
            application_spawned: false,
            timeout_process_status: None,
            sizing_summary: SizingSummary::default(),
        };

        match last_run_log_path().and_then(open_log) {
            Ok((log_file, log_path)) => {
                diagnostics.log_file = Some(log_file);
                diagnostics.log_path = Some(log_path);
            }
            Err(error) => stderr(&format!("sway-stage: logging unavailable: {error}")),
        }

        diagnostics
    }

    pub fn profile_started(&mut self, total: usize) {
        self.profile_context = Some(ProfileProgress {
            total,
            current: None,
            completed: Vec::new(),
        });
    }

    pub fn profile_workspace(
        &mut self,
        target: &crate::profile::WorkspaceTarget,
        layout: &str,
        entry_position: usize,
    ) {
        let progress = self.profile_context.as_mut().expect("profile started");
        progress.current = Some(ProfileEntry {
            target: target.clone(),
            layout: layout.to_owned(),
            position: entry_position,
        });
        self.record("profile progress");
    }

    pub fn profile_workspace_completed(&mut self, workspace_id: i64) {
        let progress = self.profile_context.as_mut().expect("profile started");
        let entry = progress.current.as_ref().expect("profile entry selected");
        progress
            .completed
            .push((entry.target.clone(), workspace_id));
        self.record("profile progress");
    }

    pub fn stage(&mut self, stage: impl Into<String>) {
        self.current_stage = stage.into();
        self.record("begin");
    }

    pub fn record(&mut self, message: &str) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        // Escape line breaks/control characters in external text to preserve log records.
        let line = format!(
            "{}.{:03} +{}ms [{}] {}{}\n",
            timestamp.as_secs(),
            timestamp.subsec_millis(),
            self.started_at.elapsed().as_millis(),
            self.current_stage.escape_debug(),
            self.profile_context
                .as_ref()
                .map(|context| format!("{}: ", context.detail().escape_debug()))
                .unwrap_or_default(),
            message.escape_debug(),
        );

        if let Some(log_file) = &mut self.log_file {
            if let Err(error) = log_file.write_all(line.as_bytes()) {
                stderr(&format!(
                    "sway-stage: logging failed: {error}; using stderr"
                ));
                self.log_file = None;
            } else {
                return;
            }
        }

        let _ = io::stderr().lock().write_all(line.as_bytes());
    }

    /// Records a successful `Command::spawn`. A spawn that failed started nothing.
    pub fn spawned(&mut self, pid: u32) {
        self.application_spawned = true;
        self.record(&format!("spawned pid={pid}"));
    }

    /// Retains observed process status without another poll or changing the window deadline.
    pub fn window_timeout(&mut self, detail: &str, process_status: Option<ExitStatus>) -> String {
        self.timeout_process_status = process_status;
        let mut error = format!("window timeout: {detail}");
        if let Some(status) = process_status
            && let Some(context) = launch_exit_context(status)
        {
            error.push_str(&format!(". {context}"));
        }
        error
    }

    /// Logs and notifies a fatal error, returning the primary message for stderr.
    pub fn report_failure(&mut self, error: String) -> String {
        let mut message = format!("{}: {error}", self.current_stage);
        if self.operation == "profile load" {
            message = format!(
                "profile load: {}; {message}",
                self.profile_context
                    .as_ref()
                    .map(ProfileProgress::detail)
                    .unwrap_or_else(|| "completed 0 workspaces []".into())
            );
        }
        // Advice follows launches across the whole session, including earlier profile entries.
        if self.application_spawned {
            if self.operation == "profile load" {
                message.push_str("; application launches may have left partial layouts");
            }
            message
                .push_str("; stopped without cleanup or relaunch; inspect windows before retrying");
        }

        if self.operation == "profile load" && !self.application_spawned {
            message.push_str("; no applications launched");
        }
        self.record(&message);

        if let Some(log_path) = &self.log_path {
            message.push_str(&format!("; log: {}", log_path.display()));
        }
        let notification = self.failure_notification(&error);
        if let Err(error) =
            send_failure_notification(self.operation, &notification, self.failure_urgency(&error))
        {
            self.record(&format!("secondary notification failure: {error}"));
        }

        message
    }

    fn failure_notification(&mut self, error: &str) -> String {
        let mut notification = failure_body(self.operation, error, &self.current_stage, false);
        if let Some(status) = self.timeout_process_status.take()
            && !status.success()
            && let Some(context) = launch_exit_context(status)
        {
            notification.push_str(&format!(" {context}"));
        }
        if self.application_spawned {
            notification.push_str(if self.profile_context.is_some() {
                "\nWindows may remain across profile workspaces. Inspect before retrying."
            } else {
                "\nWindows may remain. Inspect before retrying."
            });
        }
        if let Some(progress) = &self.profile_context {
            notification.push('\n');
            notification
                .push_str(&progress.notification(self.current_stage == "workspace preflight"));
        }
        with_log_reference(notification, self.log_file.is_some())
    }

    fn failure_urgency(&self, error: &str) -> &'static str {
        if self.application_spawned {
            return "critical";
        }

        if self.current_stage.ends_with(": validation") || self.current_stage == "validation" {
            return "normal";
        }

        // Only known refusals at the initial checks are safe. IPC failures and
        // contradictory workspace history remain critical, even before a launch.
        let cause = failure_cause(error);
        let occupied_target = self.current_stage.starts_with("prelaunch workspace=")
            && cause.starts_with("target workspace must be completely empty");
        let profile_refusal = self.current_stage == "workspace preflight"
            && [
                "profile target occupied: ",
                "profile output mismatch: ",
                "profile output unavailable: ",
            ]
            .iter()
            .any(|prefix| cause.starts_with(prefix));

        if occupied_target || profile_refusal {
            "normal"
        } else {
            "critical"
        }
    }

    pub fn report_sizing_warnings(&mut self, warnings: &[SizingWarning]) {
        for warning in warnings {
            stderr(&format!("sway-stage: warning: {}", warning.detail));
            if warning.geometry_unverified {
                self.sizing_summary.unverified_groups += 1;
            } else {
                self.sizing_summary.deviated_groups += 1;
            }
        }

        // Profiles report once, only after the coordinator verifies every workspace.
        if self.profile_context.is_none() {
            self.notify_sizing_warnings();
        }
    }

    pub fn notify_sizing_warnings(&mut self) {
        let summary = std::mem::take(&mut self.sizing_summary);
        let Some(message) =
            summary.notification(self.profile_context.is_some(), self.log_file.is_some())
        else {
            return;
        };
        if let Err(error) = notify("normal", "Sizing warning", &message) {
            self.record(&format!("secondary warning notification failure: {error}"));
        }
    }
}

/// Attempts a normal-urgency notification for any operation's fatal error without touching the
/// last-run log. A notification failure is secondary: it is reported to stderr and
/// returned, and the caller still reports `message` as the primary error.
pub fn notify_failure(operation: &str, message: &str) -> Result<(), String> {
    send_failure_notification(
        operation,
        &failure_body(operation, message, "", false),
        "normal",
    )
}

/// Capture summaries are selected where the arrangement or save failure is known.
/// Detailed workspace names, paths, and recovered commands never enter the notification.
pub fn notify_capture_failure(
    operation: &str,
    error: &crate::capture::Error,
) -> Result<(), String> {
    match error.summary {
        Some(summary) => send_failure_notification(operation, summary, "normal"),
        None => notify_failure(operation, &error.detail),
    }
}

fn capture_failure_body(operation: &str, summary: Option<&'static str>, detail: &str) -> String {
    match summary {
        Some(summary) => summary.to_owned(),
        None => failure_body(operation, detail, "", false),
    }
}

/// Menu preparation never creates a launch log, so these summaries never offer one.
pub fn notify_menu_failure(error: &crate::menu::Error) -> Result<(), String> {
    let operation = match error.kind {
        crate::menu::FailureKind::Capture(_) | crate::menu::FailureKind::DraftSave => "capture",
        crate::menu::FailureKind::ProfileCapture(_) => "profile capture",
        _ => "menu",
    };
    send_failure_notification(operation, &menu_failure_body(error), "normal")
}

fn menu_failure_body(error: &crate::menu::Error) -> String {
    use crate::menu::FailureKind;

    let summary = match &error.kind {
        FailureKind::Snapshot => return failure_body("menu", &error.detail, "", false),
        FailureKind::LayoutDiscovery => {
            "Cannot list saved layouts. Check the layouts directory and filenames."
        }
        FailureKind::ProfileDiscovery => {
            "Cannot list saved profiles. Check the profiles directory and filenames."
        }
        FailureKind::NoActions => {
            "No usable actions found. Run sway-stage check NAME or sway-stage profile check NAME to inspect configurations."
        }
        FailureKind::InvalidProfile => {
            "The selected profile is invalid. Run sway-stage profile check NAME for details."
        }
        FailureKind::NoLayouts => {
            "No saved layouts found. Add a YAML file to your layouts directory."
        }
        FailureKind::NoValidLayouts => {
            "No valid layouts found. Run sway-stage check NAME to inspect a layout."
        }
        FailureKind::InvalidLayout => {
            "The selected layout is invalid. Run sway-stage check NAME for details."
        }
        FailureKind::Capture(summary) => {
            return capture_failure_body("capture", *summary, &error.detail);
        }
        FailureKind::ProfileCapture(summary) => {
            return capture_failure_body("profile capture", *summary, &error.detail);
        }
        FailureKind::FuzzelStart => {
            "Cannot start Fuzzel. Check that Fuzzel 1.11+ is installed and executable."
        }
        FailureKind::FuzzelResponse if error.detail.starts_with("Fuzzel failed: ") => {
            "Fuzzel failed. Check its configuration and that your Wayland session is available."
        }
        FailureKind::FuzzelResponse => {
            "Fuzzel did not return a valid choice. Check its configuration."
        }
        FailureKind::DraftSave => {
            "Could not save the captured layout. Check the layouts directory, permissions, and available storage."
        }
        FailureKind::DirectoryRead => {
            "Cannot list working directories. Check the configured working directory and its permissions."
        }
    };
    summary.to_owned()
}

fn send_failure_notification(operation: &str, message: &str, urgency: &str) -> Result<(), String> {
    notify(
        urgency,
        match operation {
            "load" => "Layout load failed",
            "profile load" => "Profile load failed",
            "capture" => "Workspace capture failed",
            "profile capture" => "Profile capture failed",
            _ => "Menu error",
        },
        message,
    )
    .inspect_err(|error| stderr(&format!("sway-stage: notification unavailable: {error}")))
}

/// Summarize only recognized error prefixes. Never copy arbitrary diagnostic text or
/// user-controlled names into the desktop body; terminal and log output retain them.
fn failure_body(operation: &str, error: &str, stage: &str, log_available: bool) -> String {
    // Validation errors contain paths and arbitrary parser details, so never classify
    // their contents as runtime errors.
    if stage.ends_with(": validation") {
        let summary = if operation == "profile load" {
            "Unable to read or validate the profile and its layouts. Run sway-stage profile check NAME."
        } else {
            "Unable to read or validate the selected layout. Run sway-stage check NAME."
        };
        return with_log_reference(summary.to_owned(), log_available);
    }

    let cause = failure_cause(error);
    let summary = recognized_failure_summary(cause).unwrap_or(match operation {
        "load" => "Unable to finish loading the layout. Inspect workspace state and diagnostics before retrying.",
        "profile load" => "Unable to finish loading the profile. Inspect workspace state and diagnostics before retrying.",
        "capture" => "Unable to capture the workspace.",
        "profile capture" => "Unable to capture the profile.",
        _ => "Unable to complete the menu action.",
    });

    with_log_reference(summary.to_owned(), log_available)
}

/// Remove only internal wrappers, shared by summary and safe-refusal urgency checks.
fn failure_cause(error: &str) -> &str {
    let mut cause = error;
    // Named load wrappers contain validated ASCII configuration names. Remove only
    // known contexts; never search arbitrary error text for something recognizable.
    for prefix in ["profile \"", "layout \""] {
        if let Some(named_error) = cause.strip_prefix(prefix)
            && let Some((name, inner)) = named_error.split_once("\": ")
            && crate::discovery::valid_name(name)
        {
            cause = inner;
        }
    }
    for context in [
        "layout construction: ",
        "workspace selection: ",
        "pre-menu workspace snapshot: ",
        "capture: ",
        "snapshot: ",
    ] {
        if let Some(inner) = cause.strip_prefix(context) {
            cause = inner;
        }
    }

    cause
}

/// These prefixes belong to internal error producers, before any untrusted details.
/// Capture arrangement errors instead carry a safe summary alongside their detail.
fn recognized_failure_summary(cause: &str) -> Option<&'static str> {
    let summary = if cause.starts_with("window timeout: ") {
        "Window timed out. Check launch command, exact app ID/class, and single-instance behavior."
    } else if cause.starts_with("placement failure: wrong workspace: ") {
        "A window for this layout opened on another workspace. Check Sway's window assignment rules."
    } else if cause.starts_with("matching window was in the scratchpad: ") {
        "A window for this layout opened in the scratchpad instead of the target workspace. Check Sway's scratchpad rules."
    } else if cause.starts_with("placement failure: unknown workspace: ") {
        "A matching window's workspace could not be determined. Inspect workspace state before retrying."
    } else if cause.starts_with("ambiguous new windows id=") {
        "Multiple new windows match the expected application. Use a more specific app ID/class or a command that opens one window."
    } else if cause.starts_with("matching window was floating/fullscreen: ")
        || cause.starts_with("matching window has unusable tiled state: ")
    {
        "Loading requires tiled windows. A match is floating/fullscreen or has unknown fullscreen state; check app settings and Sway rules."
    } else if cause.starts_with("unexpected target-workspace window id=")
        || cause.starts_with("unexpected floating/fullscreen/scratchpad window id=")
    {
        "An unexpected window appeared in the target workspace. Check the layout's app ID/class and Sway rules."
    } else if cause.starts_with("preexisting window id=") {
        "A preexisting window entered the target workspace. Keep other windows off the target while loading."
    } else if cause.starts_with("workspace focus changed from target id=")
        || cause.starts_with("focus left target id=")
        || cause.starts_with("focused workspace id=")
    {
        "The target workspace lost focus. Keep it focused while loading."
    } else if cause.starts_with("focus did not stay on container id=") {
        "Focus kept moving between windows on the target workspace. Avoid moving the pointer or focus there until loading finishes."
    } else if cause.starts_with("target workspace id=") {
        "The target workspace changed outputs. Keep it on its original output while loading."
    } else if cause.starts_with("target workspace was destroyed")
        || cause.starts_with("target workspace missing")
    {
        "The target workspace is no longer available. Check workspace state and retry."
    } else if cause.starts_with("target workspace must be completely empty") {
        "The target workspace is occupied. Use a workspace with no windows or empty containers."
    } else if cause.starts_with("profile target occupied: ") {
        "A profile target is occupied. All profile targets must be empty, including floating windows and empty containers."
    } else if cause.starts_with("profile output mismatch: ") {
        "A profile workspace is on a different output than configured. Check the profile's output expectations and Sway workspace placement rules."
    } else if cause.starts_with("profile output unavailable: ") {
        "An output required by the profile is not active. Connect or enable it, or update the profile's output expectation."
    } else if cause.starts_with("profile target changed: ")
        || cause.starts_with("unexpected focused workspace or containing output")
        || cause.starts_with("workspace completion: focused workspace")
        || cause.starts_with("unexpected workspace change ")
        || cause.starts_with("selected workspace identity or output differs")
    {
        "Workspace identity, focus, or output changed during profile loading. Keep workspaces unchanged until loading finishes."
    } else if cause.starts_with("workspace reload: ") {
        "Sway's configuration reloaded during loading. Retry after the reload finishes."
    } else if cause.starts_with("selected workspace number ")
        || cause.starts_with("workspace selection: expected ")
        || cause.starts_with("expected focus event was not observed")
        || cause.starts_with("expected workspace creation event was not observed")
    {
        "Sway's workspace selection could not be verified. Check for workspace changes and retry."
    } else if cause.starts_with("workspace snapshot: ")
        || cause.starts_with("expected exactly one focused workspace")
        || cause.starts_with("workspace/tree focus is inconsistent")
        || cause.starts_with("invalid root or duplicate container identities")
        || cause.starts_with("duplicate workspace identities")
    {
        "Sway reported inconsistent workspace identities or focus. Retry when workspace state is stable."
    } else if cause.starts_with("spawn application: ") {
        "Unable to start an application. Check its executable, command, working directory, and permissions."
    } else if cause.starts_with("Sway command failed: ")
        || cause.starts_with("workspace command rejected: ")
        || cause.starts_with("workspace command: ")
    {
        "Sway rejected a command or returned an unexpected reply. Check Sway's configuration and compatibility."
    } else if cause.starts_with("window activity ") || cause.starts_with("window move id=") {
        "Window activity made loading unsafe. Keep windows unchanged until loading finishes."
    } else if cause.starts_with("new/accepted window") {
        "A newly observed window closed before its placement could be verified. Check application behavior and retry."
    } else if cause.starts_with("matching window changed identity") {
        "A window's app ID/class changed during loading. Use a stable, exact selector."
    } else if cause.starts_with("accepted window") || cause.starts_with("matched window") {
        "A layout window closed or changed identity, placement, or tiled state. Keep layout windows unchanged until loading finishes."
    } else if cause.starts_with("unexpected hierarchy at ")
        || cause.starts_with("invalid hierarchy state ")
    {
        "The workspace hierarchy differs from the expected layout. Check Sway rules and keep windows unchanged while loading."
    } else if [
        "connect: SWAYSOCK ",
        "connect to SWAYSOCK: ",
        "subscribe: ",
        "subscribe to workspace/window/tick: ",
        "get tree: ",
        "get workspaces: ",
        "clone event socket: ",
        "clone query socket: ",
        "clone command socket: ",
        "configure event socket: ",
        "start event reader: ",
        "start snapshot reader: ",
        "start capture reader: ",
        "start command reader: ",
        "read Sway event: ",
        "command IPC",
        "command deadline/channel failure: ",
        "snapshot deadline/channel failure: ",
        "tree deadline/channel failure: ",
        "event reader ",
        "event stream ended",
        "event history ",
        "snapshot event history ",
        "new-window history ",
        "event fence deadline ",
        "send observation fence: ",
        "Sway rejected observation fence",
    ]
    .iter()
    .any(|prefix| cause.starts_with(prefix))
    {
        "Unable to communicate with Sway. Check that Sway is running and its IPC connection is available."
    } else {
        return None;
    };
    Some(summary)
}

fn with_log_reference(summary: String, log_available: bool) -> String {
    if log_available {
        format!("{summary}\n\nSee last-run.log for details.")
    } else {
        summary
    }
}

fn launch_exit_context(status: ExitStatus) -> Option<String> {
    if let Some(code) = status.code() {
        if code == 0 {
            return Some("Launch process exited successfully (delegation is allowed).".into());
        }
        return Some(format!("Launch process exited with code {code}."));
    }
    status
        .signal()
        .map(|signal| format!("Launch process terminated by signal {signal}."))
}

/// Best-effort save confirmation, independent of launch diagnostics.
pub fn notify_draft_saved(unresolved_commands: usize) {
    let _ = notify(
        "low",
        "Layout draft saved",
        &capture_review_guidance(unresolved_commands),
    )
    .inspect_err(|error| stderr(&format!("sway-stage: notification unavailable: {error}")));
}

/// Confirm only a fully published profile, using the bounded notification helper.
pub fn notify_profile_saved(workspace_count: usize, unresolved_commands: usize) {
    let _ = notify(
        "low",
        "Profile saved",
        &format!(
            "Workspace layout drafts saved: {workspace_count}. {}",
            capture_review_guidance(unresolved_commands)
        ),
    )
    .inspect_err(|error| stderr(&format!("sway-stage: notification unavailable: {error}")));
}

fn capture_review_guidance(unresolved_commands: usize) -> String {
    let mut body = String::new();
    if unresolved_commands > 0 {
        body.push_str(&format!(
            "Launch commands needing editing: {unresolved_commands}. "
        ));
    }
    body.push_str("Review launch commands and validate before loading.");
    body
}

fn stderr(message: &str) {
    let _ = writeln!(io::stderr().lock(), "{message}");
}

fn last_run_log_path() -> Result<PathBuf, String> {
    let state_home = if let Some(xdg_state_home) =
        env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty())
    {
        PathBuf::from(xdg_state_home)
    } else {
        let home = env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .ok_or("neither XDG_STATE_HOME nor HOME is usable")?;
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            return Err("HOME must be absolute".into());
        }
        home.join(".local/state")
    };

    if !state_home.is_absolute() {
        return Err("XDG_STATE_HOME must be absolute".into());
    }

    Ok(state_home.join("sway-stage/last-run.log"))
}

fn open_log(path: PathBuf) -> Result<(File, PathBuf), String> {
    let open = || -> std::io::Result<File> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path.parent().expect("log path has a parent"))?;

        // Unlink rather than truncate: do not follow a last-run.log symlink, and do not
        // expose this run through an older log's permissions or hard links. create_new
        // refuses a replacement inserted between unlink and open.
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
    };

    open()
        .map(|file| (file, path.clone()))
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn notify(urgency: &str, summary: &str, message: &str) -> Result<(), String> {
    // Unit tests exercise failure reporting but must never reach the user's desktop.
    // Integration tests run the executable with a fake notify-send on a controlled PATH.
    if cfg!(test) {
        return Ok(());
    }

    let mut child = Command::new("notify-send")
        .args([
            "--app-name=sway-stage",
            &format!("--urgency={urgency}"),
            "--expire-time=20000",
            "--",
            summary,
            message,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("start notify-send: {error}"))?;

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("notify-send: {status}")),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            wait_result => {
                // Only the optional notification helper is terminated, never an application.
                let _ = child.kill();
                let _ = child.wait();
                return Err(match wait_result {
                    Err(error) => format!("wait for notify-send: {error}"),
                    _ => "notify-send exceeded two-second deadline".into(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn without_log(operation: &'static str) -> Diagnostics {
        Diagnostics {
            operation,
            log_file: None,
            log_path: None,
            started_at: Instant::now(),
            current_stage: "runtime".into(),
            profile_context: None,
            application_spawned: false,
            timeout_process_status: None,
            sizing_summary: SizingSummary::default(),
        }
    }

    #[test]
    fn sizing_summaries_distinguish_deviations_and_unverified_geometry() {
        for (deviated_groups, unverified_groups) in [(0, 0), (1, 0), (2, 0), (0, 1), (1, 2)] {
            let summary = SizingSummary {
                deviated_groups,
                unverified_groups,
            };
            for log_available in [false, true] {
                let body = summary.notification(false, log_available);
                if deviated_groups + unverified_groups == 0 {
                    assert!(body.is_none());
                    continue;
                }
                let body = body.unwrap();
                assert!(body.starts_with("Layout loaded."));
                assert_eq!(body.contains("outside size tolerance"), deviated_groups > 0);
                assert_eq!(body.contains("unverifiable"), unverified_groups > 0);
                if deviated_groups > 0 {
                    assert!(body.contains(&format!("outside size tolerance: {deviated_groups}.")));
                }
                if unverified_groups > 0 {
                    assert!(body.contains(&format!("unverifiable sizes: {unverified_groups}.")));
                }
                assert_eq!(body.contains("last-run.log"), log_available);
                assert!(body.len() < 200);
            }
        }
    }

    #[test]
    fn profile_sizing_warnings_accumulate_until_one_final_summary() {
        let mut diagnostics = without_log("profile load");
        diagnostics.profile_started(2);
        for workspace in [3, 4] {
            diagnostics.profile_workspace(
                &crate::profile::WorkspaceTarget::Number(workspace),
                "private",
                1,
            );
            diagnostics.report_sizing_warnings(&[
                SizingWarning {
                    detail: "group=123 /private".into(),
                    geometry_unverified: false,
                },
                SizingWarning {
                    detail: "group=456 /private".into(),
                    geometry_unverified: true,
                },
            ]);
        }
        let body = diagnostics
            .sizing_summary
            .notification(true, false)
            .unwrap();
        assert_eq!(
            body,
            "Profile loaded. Groups outside size tolerance: 2. Groups with unverifiable sizes: 2."
        );
        diagnostics.notify_sizing_warnings();
        assert!(
            diagnostics
                .sizing_summary
                .notification(true, false)
                .is_none()
        );
    }

    #[test]
    fn failure_urgency_distinguishes_safe_refusals_from_uncertain_state() {
        let occupied = "layout \"private\": target workspace must be completely empty";
        let mut diagnostics = without_log("load");
        diagnostics.stage("load private: validation");
        assert_eq!(
            diagnostics.failure_urgency("untrusted parser text"),
            "normal"
        );

        diagnostics.stage("prelaunch workspace=7");
        assert_eq!(diagnostics.failure_urgency(occupied), "normal");
        for cause in [
            "workspace focus changed from target id=7",
            "target workspace id=7 moved to another output",
            "get tree: connection closed",
            "unrecognized: target workspace must be completely empty",
        ] {
            assert_eq!(diagnostics.failure_urgency(cause), "critical", "{cause}");
        }

        diagnostics.operation = "profile load";
        diagnostics.profile_started(2);
        diagnostics.stage("workspace preflight");
        for cause in [
            "profile target occupied: workspace 3",
            "profile output mismatch: workspace 3",
            "profile output unavailable: workspace 3",
        ] {
            let wrapped = format!("profile \"private\": {cause}");
            assert_eq!(diagnostics.failure_urgency(&wrapped), "normal");

            diagnostics.stage("workspace selection");
            assert_eq!(diagnostics.failure_urgency(&wrapped), "critical");
            diagnostics.stage("workspace preflight");
        }
        for cause in [
            "get tree: connection closed",
            "workspace snapshot: inconsistent identities",
            "unexpected workspace change Focus",
            "profile target changed: workspace 3",
            "window activity id=20 during prelaunch fence",
            "unrecognized error",
        ] {
            assert_eq!(diagnostics.failure_urgency(cause), "critical", "{cause}");
        }

        diagnostics.spawned(42);
        assert_eq!(
            diagnostics.failure_urgency("profile target occupied: workspace 3"),
            "critical"
        );
        diagnostics.stage("prelaunch workspace=7");
        assert_eq!(diagnostics.failure_urgency(occupied), "critical");
    }

    #[test]
    fn combined_profile_failures_keep_cause_progress_windows_and_log_concise() {
        use crate::profile::WorkspaceTarget;

        for target in [
            WorkspaceTarget::Number(999_999_999),
            WorkspaceTarget::Name("private name".into()),
        ] {
            for log_available in [false, true] {
                for cause in [
                    "window timeout: private",
                    "placement failure: unknown workspace: private",
                    "matching window has unusable tiled state: private",
                    "ambiguous new windows id=20 private",
                    "spawn application: private",
                ] {
                    let mut diagnostics = without_log("profile load");
                    diagnostics.profile_started(256);
                    diagnostics.profile_workspace(&WorkspaceTarget::Number(1), "private", 1);
                    diagnostics.spawned(42);
                    diagnostics.profile_workspace_completed(101);
                    diagnostics.profile_workspace(&target, "private", 256);
                    if cause.starts_with("window timeout: ") {
                        diagnostics.window_timeout("private", Some(ExitStatus::from_raw(11)));
                    }

                    diagnostics.log_file = log_available.then(|| File::open("/dev/null").unwrap());
                    let body = diagnostics.failure_notification(cause);
                    assert!(body.starts_with(recognized_failure_summary(cause).unwrap()));
                    assert!(body.contains("Windows may remain across profile workspaces."));
                    assert!(body.contains("Inspect before retrying."));
                    assert!(body.contains("1/256 completed"));
                    assert!(body.contains(match target {
                        WorkspaceTarget::Number(_) => "workspace 999999999",
                        WorkspaceTarget::Name(_) => "entry 256",
                    }));
                    assert_eq!(body.contains("last-run.log"), log_available);
                    assert_eq!(
                        body.contains("signal 11"),
                        cause.starts_with("window timeout: ")
                    );
                    assert!(!body.contains("private"));
                    assert!(body.len() < 300, "{} characters: {body}", body.len());
                }
            }
        }
    }

    #[test]
    fn capture_guidance_reports_only_unresolved_counts() {
        assert_eq!(
            capture_review_guidance(0),
            "Review launch commands and validate before loading."
        );
        for count in [1, 2, usize::MAX] {
            let body = capture_review_guidance(count);
            assert!(body.contains(&format!("Launch commands needing editing: {count}.")));
            assert!(body.ends_with("validate before loading."));
            assert!(body.len() < 120);
        }
    }

    #[test]
    fn partial_layout_advice_requires_a_successful_spawn_and_no_log() {
        let mut diagnostics = without_log("load");
        let failed_spawn = diagnostics.failure_notification("spawn application: private command");
        assert!(failed_spawn.contains("Check its executable"));
        assert!(!failed_spawn.contains("Windows may remain"));
        assert!(!failed_spawn.contains("nothing changed"));

        diagnostics.spawned(42);
        let body = diagnostics.failure_notification("window timeout: private selector");
        assert!(body.contains("Windows may remain. Inspect before retrying."));
        assert!(!body.contains("last-run.log"));
        assert!(!body.contains("private"));
    }

    #[test]
    fn profile_progress_keeps_names_private_and_survives_loader_stages() {
        use crate::profile::WorkspaceTarget;

        let mut diagnostics = without_log("profile load");
        diagnostics.stage("profile: validation");
        let validation = diagnostics.failure_notification("private YAML");
        assert!(!validation.contains("Profile stopped"));
        assert!(!validation.contains("completed"));

        diagnostics.profile_started(3);
        diagnostics.stage("workspace preflight");
        let initial = diagnostics.failure_notification("get tree: failed");
        assert!(initial.contains("Profile preflight stopped (0/3 completed)."));
        diagnostics.profile_workspace(
            &WorkspaceTarget::Name("<b>private</b>\n".into()),
            "secret",
            3,
        );
        let preflight = diagnostics.failure_notification("profile target occupied: private");
        assert!(preflight.contains("Profile preflight stopped at entry 3 (0/3 completed)."));
        assert!(!preflight.contains("Windows may remain"));
        assert!(!preflight.contains("private"));
        assert!(!preflight.contains("secret"));

        diagnostics.profile_workspace(&WorkspaceTarget::Number(4), "secret", 1);
        diagnostics.stage("workspace selection");
        let selection = diagnostics.failure_notification("workspace command rejected: private");
        assert!(selection.contains("Profile stopped at workspace 4 (0/3 completed)."));
        assert!(!selection.contains("Windows may remain"));
        assert!(!selection.contains("nothing changed"));

        diagnostics.spawned(42);
        diagnostics.stage("workspace completion verification");
        let unverified = diagnostics.failure_notification("unexpected workspace change Focus");
        assert!(unverified.contains("0/3 completed"));
        diagnostics.profile_workspace_completed(104);
        diagnostics.profile_workspace(&WorkspaceTarget::Name("sensitive name".into()), "secret", 2);
        diagnostics.stage("launch leaf=private workspace=105");
        let partial = diagnostics.failure_notification("spawn application: private");
        assert!(partial.contains("Profile stopped at entry 2 (1/3 completed)."));
        assert!(
            partial
                .contains("Windows may remain across profile workspaces. Inspect before retrying.")
        );
        assert!(!partial.contains("sensitive"));
        assert!(!partial.contains("private"));
        assert!(!partial.contains("secret"));
        assert!(!partial.contains("last-run.log"));
        assert!(partial.len() < 400, "{partial}");
    }

    #[test]
    fn timeout_diagnostics_and_notifications_use_only_known_exit_status() {
        for (status, expected_detail, expected_notification) in [
            (
                Some(ExitStatus::from_raw(1 << 8)),
                Some("Launch process exited with code 1."),
                true,
            ),
            (
                Some(ExitStatus::from_raw(11)),
                Some("Launch process terminated by signal 11."),
                true,
            ),
            (
                Some(ExitStatus::from_raw(0)),
                Some("Launch process exited successfully (delegation is allowed)."),
                false,
            ),
            (None, None, false), // Still running, or no status has been observed.
            (Some(ExitStatus::from_raw(0x7f)), None, false), // No exit code or signal available.
        ] {
            for log_available in [false, true] {
                let mut diagnostics = Diagnostics {
                    operation: "load",
                    log_file: log_available.then(|| File::open("/dev/null").unwrap()),
                    log_path: None,
                    started_at: Instant::now(),
                    current_stage: "wait".into(),
                    profile_context: None,
                    application_spawned: true,
                    timeout_process_status: None,
                    sizing_summary: SizingSummary::default(),
                };
                let detail = format!("no new windows observed; leaf={}", "x".repeat(10000));
                let error = diagnostics.window_timeout(&detail, status);
                assert!(error.starts_with(&format!("window timeout: {detail}")));
                if let Some(context) = expected_detail {
                    assert!(error.ends_with(context), "{error}");
                } else {
                    assert_eq!(error, format!("window timeout: {detail}"));
                }

                // Exercise the same structured context and body path as report_failure,
                // including the layout wrapper applied by its caller.
                let body =
                    diagnostics.failure_notification(&format!("layout \"development\": {error}"));
                assert!(body.starts_with("Window timed out."));
                assert_eq!(
                    body.contains("Launch process"),
                    expected_notification,
                    "{body}"
                );
                if expected_notification {
                    assert!(body.contains(expected_detail.unwrap()), "{body}");
                }
                assert_eq!(
                    body.contains("See last-run.log for details."),
                    log_available
                );
                assert!(body.len() < 360, "{body}");
                assert!(!body.contains("leaf="));
            }
        }
    }

    #[test]
    fn failure_notifications_summarize_recognized_causes() {
        for (error, expected) in [
            (
                "layout \"development\": window timeout: elapsed=10000ms observed=[]",
                "Window timed out. Check launch command, exact app ID/class, and single-instance behavior.",
            ),
            (
                "layout \"development\": workspace focus changed from target id=7",
                "The target workspace lost focus. Keep it focused while loading.",
            ),
            (
                "snapshot: focus left target id=7",
                "The target workspace lost focus. Keep it focused while loading.",
            ),
            (
                "connect to SWAYSOCK: connection refused",
                "Unable to communicate with Sway. Check that Sway is running and its IPC connection is available.",
            ),
            (
                "pre-menu workspace snapshot: get tree: connection closed",
                "Unable to communicate with Sway. Check that Sway is running and its IPC connection is available.",
            ),
            (
                "spawn application: executable missing; child PATH and cwd",
                "Unable to start an application. Check its executable, command, working directory, and permissions.",
            ),
            (
                "target workspace must be completely empty, including tiled/floating containers",
                "The target workspace is occupied. Use a workspace with no windows or empty containers.",
            ),
            (
                "unexpected failure: window timeout: untrusted text",
                "Unable to finish loading the layout. Inspect workspace state and diagnostics before retrying.",
            ),
        ] {
            assert_eq!(failure_body("load", error, "wait", false), expected);
            assert_eq!(
                failure_body("load", error, "wait", true),
                format!("{expected}\n\nSee last-run.log for details.")
            );
        }
    }

    #[test]
    fn runtime_causes_survive_layout_and_profile_context_without_copying_details() {
        #[rustfmt::skip]
        let cases = [
            ("placement failure: wrong workspace: matching window id=20", "opened on another workspace"),
            ("placement failure: unknown workspace: matching window id=20", "workspace could not be determined"),
            ("matching window was in the scratchpad: id=20", "opened in the scratchpad instead"),
            ("ambiguous new windows id=20 and id=21", "Multiple new windows match"),
            ("matching window was floating/fullscreen: id=20", "Loading requires tiled windows"),
            ("matching window has unusable tiled state: id=20", "unknown fullscreen state"),
            ("unexpected target-workspace window id=20", "An unexpected window appeared"),
            ("unexpected floating/fullscreen/scratchpad window id=20", "An unexpected window appeared"),
            ("preexisting window id=20 entered target workspace", "A preexisting window entered"),
            ("snapshot: focused workspace id=11 differs from pre-menu workspace id=10", "target workspace lost focus"),
            ("focus did not stay on container id=21 after 3 restorations within the target workspace", "Focus kept moving between windows"),
            ("target workspace id=10 moved to another output", "target workspace changed outputs"),
            ("snapshot: target workspace id=10 changed output from id=2 to id=3", "target workspace changed outputs"),
            ("target workspace was destroyed", "target workspace is no longer available"),
            ("profile target occupied: workspace 3", "All profile targets must be empty"),
            ("profile output mismatch: workspace 3", "different output than configured"),
            ("profile output unavailable: workspace 3", "output required by the profile is not active"),
            ("profile target changed: workspace 3", "Workspace identity, focus, or output changed"),
            ("unexpected workspace change Focus id=10", "Workspace identity, focus, or output changed"),
            ("workspace reload: revalidation required", "configuration reloaded"),
            ("workspace snapshot: ambiguous workspace identities or numbers", "inconsistent workspace identities or focus"),
            ("workspace selection: expected focus event was not observed", "workspace selection could not be verified"),
            ("spawn application: missing executable", "Unable to start an application"),
            ("workspace command rejected: denied", "Sway rejected a command"),
            ("Sway command failed: rejected", "Sway rejected a command"),
            ("window activity id=20 during prelaunch fence", "Window activity made loading unsafe"),
            ("accepted window id=20 disappeared", "A layout window closed or changed"),
            ("new/accepted window id=20 closed", "A newly observed window closed"),
            ("matching window changed identity after a nonmatching identity: id=20", "app ID/class changed during loading"),
            ("unexpected hierarchy at id=10", "hierarchy differs from the expected layout"),
            ("command deadline/channel failure: timeout", "Unable to communicate with Sway"),
            ("snapshot: event fence deadline expired", "Unable to communicate with Sway"),
            ("event reader disconnected; history cannot be trusted", "Unable to communicate with Sway"),
        ];
        for (cause, expected) in cases {
            let detail = format!("{cause}: /private/<b>secret</b>");
            for (operation, error) in [
                ("load", format!("layout \"work\": {detail}")),
                (
                    "profile load",
                    format!("profile \"work\": layout construction: {detail}"),
                ),
                (
                    "profile load",
                    format!("profile \"work\": workspace selection: {detail}"),
                ),
            ] {
                let body = failure_body(operation, &error, "runtime", false);
                assert!(body.contains(expected), "{error}: {body}");
                assert!(!body.contains("secret"), "{body}");
                assert!(!body.contains("/private"), "{body}");
            }
        }
    }

    #[test]
    fn validation_and_unknown_errors_do_not_guess_at_runtime_causes() {
        assert_eq!(
            failure_body(
                "profile load",
                "spawn application: untrusted parser text",
                "profile: validation",
                false
            ),
            "Unable to read or validate the profile and its layouts. Run sway-stage profile check NAME."
        );
        for operation in ["load", "profile load", "capture", "profile capture", "menu"] {
            let body = failure_body(
                operation,
                "unrecognized: spawn application: /private",
                "runtime",
                false,
            );
            assert!(!body.contains("start an application"));
            assert!(!body.contains("/private"));
        }
    }

    #[test]
    fn notifications_omit_long_names_paths_and_untrusted_markup() {
        let long_name = "a".repeat(10000);
        let timeout = format!("layout \"{long_name}\": window timeout: leaf=<b>private</b>");
        let body = failure_body("load", &timeout, "wait", true);
        assert!(body.starts_with("Window timed out."));
        assert!(!body.contains(&long_name));
        assert!(!body.contains("private"));

        let stage = format!("load {long_name:?}: validation");
        assert_eq!(
            failure_body(
                "load",
                "/very/long/private/path: YAML details",
                &stage,
                false
            ),
            "Unable to read or validate the selected layout. Run sway-stage check NAME."
        );
        assert_eq!(
            failure_body(
                "capture",
                "workspace <b>private</b>: unsupported tree",
                "",
                false
            ),
            "Unable to capture the workspace."
        );
    }

    #[test]
    fn menu_notifications_do_not_copy_untrusted_details_or_offer_an_old_log() {
        use crate::menu::{Error, FailureKind};

        let untrusted_detail = format!("/private/{}<b>secret</b>", "x".repeat(10000));
        for kind in [
            FailureKind::InvalidLayout,
            FailureKind::InvalidProfile,
            FailureKind::Snapshot,
            FailureKind::LayoutDiscovery,
            FailureKind::ProfileDiscovery,
            FailureKind::NoActions,
            FailureKind::NoLayouts,
            FailureKind::NoValidLayouts,
            FailureKind::Capture(None),
            FailureKind::ProfileCapture(None),
            FailureKind::FuzzelStart,
            FailureKind::FuzzelResponse,
            FailureKind::DraftSave,
            FailureKind::DirectoryRead,
        ] {
            let error = Error::new(kind, untrusted_detail.clone());
            let body = menu_failure_body(&error);
            assert!(body.len() < 160, "{body}");
            assert!(!body.contains("private"), "{body}");
            assert!(!body.contains("secret"), "{body}");
            assert!(!body.contains("last-run.log"), "{body}");
            assert_eq!(error.detail, untrusted_detail);
        }
    }

    #[test]
    fn failed_log_writes_do_not_offer_the_log_as_details() {
        let mut diagnostics = Diagnostics {
            operation: "load",
            log_file: Some(OpenOptions::new().write(true).open("/dev/full").unwrap()),
            log_path: Some(PathBuf::from("/dev/full")),
            started_at: Instant::now(),
            current_stage: "wait".into(),
            profile_context: None,
            application_spawned: true,
            timeout_process_status: None,
            sizing_summary: SizingSummary::default(),
        };
        let error = "window timeout: elapsed=10000ms observed=[id=42 app_id=wrong]";
        let terminal = diagnostics.report_failure(error.into());
        assert!(terminal.contains(error));
        assert!(terminal.contains("inspect windows before retrying"));
        assert!(diagnostics.log_file.is_none());
        let notification = diagnostics.failure_notification(error);
        assert!(!notification.contains("last-run.log"));
        assert!(notification.contains("Windows may remain"));
    }

    #[test]
    fn non_launch_failures_notify_without_replacing_the_launch_log() {
        let directory =
            env::temp_dir().join(format!("sway-stage-diagnostics-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("last-run.log");
        let mut diagnostics = Diagnostics::for_test(path.clone());
        diagnostics.stage("launch");
        diagnostics.spawned(42);

        let launch_log = fs::read_to_string(&path).unwrap();
        assert!(launch_log.contains("spawned pid=42"), "{launch_log}");

        // Capture, or menu before layout selection, reports without any log access.
        notify_failure("capture", "capture: get tree: connection closed").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), launch_log);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);

        let detail = "window timeout: elapsed=10000ms observed=[id=42 app_id=wrong]";
        let terminal = diagnostics.report_failure(detail.into());
        assert!(terminal.contains(detail));
        assert!(terminal.contains("launch:"));
        assert!(terminal.contains("inspect windows before retrying"));
        assert!(terminal.contains(path.to_str().unwrap()));
        let failed_log = fs::read_to_string(&path).unwrap();
        assert!(failed_log.contains(detail));
        assert!(failed_log.contains("inspect windows before retrying"));

        fs::remove_dir_all(&directory).unwrap();
    }
}
