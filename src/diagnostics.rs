//! Launch diagnostics and bounded, best-effort notifications.
//!
//! `last-run.log` is reserved for layout launches: only `Diagnostics::start` creates or
//! replaces it. Operations that never load a selected layout report through
//! `notify_capture_failure` or `notify_menu_failure`, leaving the previous launch's log in place.

use crate::failure::{Cause, Failure, launch_exit_context};
use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Diagnostics {
    /// The operation being performed, independent of whether it was selected in the menu.
    operation: &'static str,
    log_file: Option<File>,
    /// Where `log_file` writes. Cleared with it when a write fails, so a failure message
    /// never offers a log that stopped receiving its records.
    log_path: Option<PathBuf>,
    started_at: Instant,
    /// Where the attempt is, for notifications and urgency; the stage adds log detail.
    phase: Phase,
    current_stage: String,
    profile_context: Option<ProfileProgress>,
    /// Set only by `spawned`, after `Command::spawn` succeeded for this invocation.
    application_spawned: bool,
    sizing_summary: SizingSummary,
}

/// The part of an attempt that failed. Together with the failure's cause, it decides
/// whether a failure is a safe refusal; it also selects validation and preflight wording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Reading and validating configuration files, before any IPC.
    Validation,
    /// The profile coordinator's read-only examination of every target.
    ProfilePreflight,
    /// The profile coordinator's selection of one target workspace.
    ProfileSelection,
    /// The ordinary loader, from its subscription through its prelaunch emptiness check,
    /// launches, and arrangement.
    Construction,
    /// The profile coordinator's verification after one layout's construction.
    ProfileCompletion,
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
            phase: Phase::Validation,
            current_stage: "test".into(),
            profile_context: None,
            application_spawned: false,
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
            phase: Phase::Validation,
            current_stage: "validation".into(),
            profile_context: None,
            application_spawned: false,
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

    /// Profile coordination keeps one session across phases; every load starts in validation.
    pub fn enter_phase(&mut self, phase: Phase) {
        self.phase = phase;
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
                self.log_path = None;
            } else {
                return;
            }
        }

        let _ = io::stderr().lock().write_all(line.as_bytes());
    }

    /// Records a started application. A spawn that failed started nothing. The PID is
    /// unknown only if the launch supervisor died before reporting it.
    pub fn spawned(&mut self, application_pid: Option<u32>) {
        self.application_spawned = true;
        match application_pid {
            Some(pid) => self.record(&format!("spawned pid={pid}")),
            None => self.record("spawned an application whose pid was not reported"),
        }
    }

    /// Logs and notifies a fatal error, returning the primary message for stderr.
    pub fn report_failure(&mut self, failure: Failure) -> String {
        let mut message = format!("{}: {}", self.current_stage, failure.detail);
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
        let notification = self.failure_notification(&failure);
        let urgency = self.failure_urgency(&failure);
        if let Err(error) = send_failure_notification(self.operation, &notification, urgency) {
            self.record(&format!("secondary notification failure: {error}"));
        }

        message
    }

    /// Built only from the phase, typed cause, and progress counts. Failure details can
    /// contain paths, names, commands, and parser text, so they never enter the body.
    fn failure_notification(&self, failure: &Failure) -> String {
        let mut notification = self.failure_summary(failure).to_owned();
        if let Some(Cause::WindowTimeout {
            launch_exit: Some(status),
        }) = failure.cause
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
            notification.push_str(&progress.notification(self.phase == Phase::ProfilePreflight));
        }
        with_log_reference(notification, self.log_file.is_some())
    }

    /// Validation details contain paths and arbitrary parser text, so validation uses one
    /// fixed summary. A runtime failure uses its cause's summary, or the generic one.
    fn failure_summary(&self, failure: &Failure) -> &'static str {
        if self.phase == Phase::Validation {
            return if self.operation == "profile load" {
                "Unable to read or validate the profile and its layouts. Run sway-stage profile check NAME."
            } else {
                "Unable to read or validate the selected layout. Run sway-stage check NAME."
            };
        }

        match failure.cause {
            Some(cause) => cause.summary(),
            None => generic_failure_summary(self.operation),
        }
    }

    /// Only refusals verified before anything changed are safe: invalid files, and occupied
    /// or misplaced targets found by the initial read-only checks. IPC failures and
    /// contradictory workspace history remain critical even before a launch, and every
    /// failure after a launch is critical.
    fn failure_urgency(&self, failure: &Failure) -> &'static str {
        if self.application_spawned {
            return "critical";
        }

        let safe_refusal = match self.phase {
            Phase::Validation => true,
            Phase::ProfilePreflight => matches!(
                failure.cause,
                Some(
                    Cause::ProfileTargetOccupied
                        | Cause::ProfileOutputMismatch
                        | Cause::ProfileOutputUnavailable
                )
            ),
            // The loader checks emptiness before its first launch.
            Phase::Construction => failure.cause == Some(Cause::TargetOccupied),
            Phase::ProfileSelection | Phase::ProfileCompletion => false,
        };

        if safe_refusal { "normal" } else { "critical" }
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

/// A failure without a classified cause gets its operation's generic summary. Never
/// inspect the failure's detail to find a more specific one.
fn generic_failure_summary(operation: &str) -> &'static str {
    match operation {
        "load" => {
            "Unable to finish loading the layout. Inspect workspace state and diagnostics before retrying."
        }
        "profile load" => {
            "Unable to finish loading the profile. Inspect workspace state and diagnostics before retrying."
        }
        "capture" => "Unable to capture the workspace.",
        "profile capture" => "Unable to capture the profile.",
        _ => "Unable to complete the menu action.",
    }
}

/// Attempts a normal-urgency notification for a capture's fatal error without touching
/// the last-run log. Capture summaries are selected where the arrangement, save, or Sway
/// failure is known. Detailed workspace names, paths, and recovered commands never enter
/// the notification. A notification failure is secondary: it is reported to stderr and
/// returned, and the caller still reports the capture error as the primary error.
pub fn notify_capture_failure(
    operation: &str,
    error: &crate::capture::Error,
) -> Result<(), String> {
    let summary = error
        .summary
        .unwrap_or_else(|| generic_failure_summary(operation));
    send_failure_notification(operation, summary, "normal")
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
        FailureKind::Snapshot(summary) => summary.unwrap_or(generic_failure_summary("menu")),
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
        FailureKind::Capture(summary) => summary.unwrap_or(generic_failure_summary("capture")),
        FailureKind::ProfileCapture(summary) => {
            summary.unwrap_or(generic_failure_summary("profile capture"))
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

fn with_log_reference(summary: String, log_available: bool) -> String {
    if log_available {
        format!("{summary}\n\nSee last-run.log for details.")
    } else {
        summary
    }
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
    use crate::profile::WorkspaceTarget;
    use std::collections::HashSet;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    /// Every cause, so each one's notification is checked.
    const ALL_CAUSES: [Cause; 28] = [
        Cause::TargetOccupied,
        Cause::ProfileTargetOccupied,
        Cause::ProfileOutputMismatch,
        Cause::ProfileOutputUnavailable,
        Cause::WindowTimeout { launch_exit: None },
        Cause::WindowOnOtherWorkspace,
        Cause::WindowInScratchpad,
        Cause::WindowWorkspaceUnknown,
        Cause::AmbiguousWindows,
        Cause::WindowNotTiled,
        Cause::UnexpectedWindow,
        Cause::PreexistingWindow,
        Cause::NewWindowClosed,
        Cause::WindowIdentityChanged,
        Cause::LayoutWindowChanged,
        Cause::WindowActivity,
        Cause::HierarchyMismatch,
        Cause::FocusLost,
        Cause::FocusUnstable,
        Cause::OutputChanged,
        Cause::TargetUnavailable,
        Cause::ProfileWorkspaceChanged,
        Cause::ConfigurationReloaded,
        Cause::SelectionUnverified,
        Cause::InconsistentWorkspaces,
        Cause::ApplicationStart,
        Cause::CommandRejected,
        Cause::SwayIpc,
    ];

    /// Detail text that names private things and imitates other failures' wording.
    const UNTRUSTED_DETAIL: &str = "profile target occupied: window timeout: spawn application: /private/<b>secret</b> \"name\" --token=hidden";

    fn without_log(operation: &'static str) -> Diagnostics {
        Diagnostics {
            operation,
            log_file: None,
            log_path: None,
            started_at: Instant::now(),
            phase: Phase::Construction,
            current_stage: "runtime".into(),
            profile_context: None,
            application_spawned: false,
            sizing_summary: SizingSummary::default(),
        }
    }

    fn assert_no_details(body: &str) {
        for private in ["private", "secret", "name", "token", "hidden", "<b>"] {
            assert!(!body.contains(private), "{private}: {body}");
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
        let mut diagnostics = without_log("load");
        diagnostics.enter_phase(Phase::Validation);
        assert_eq!(
            diagnostics.failure_urgency(&"untrusted parser text".into()),
            "normal"
        );

        // The loader's occupied target is refused before any launch.
        diagnostics.enter_phase(Phase::Construction);
        let occupied = Failure::new(Cause::TargetOccupied, "occupied");
        assert_eq!(diagnostics.failure_urgency(&occupied), "normal");
        for cause in [
            Cause::FocusLost,
            Cause::OutputChanged,
            Cause::SwayIpc,
            Cause::ProfileTargetOccupied,
        ] {
            let failure = Failure::new(cause, "detail");
            assert_eq!(
                diagnostics.failure_urgency(&failure),
                "critical",
                "{cause:?}"
            );
        }

        diagnostics.operation = "profile load";
        diagnostics.profile_started(2);
        let profile_refusals = [
            Cause::ProfileTargetOccupied,
            Cause::ProfileOutputMismatch,
            Cause::ProfileOutputUnavailable,
        ];
        for cause in profile_refusals {
            let refusal = Failure::new(cause, "detail").context("profile \"private\"");
            diagnostics.enter_phase(Phase::ProfilePreflight);
            assert_eq!(diagnostics.failure_urgency(&refusal), "normal", "{cause:?}");

            // The same finding after preflight follows a workspace switch.
            for phase in [
                Phase::ProfileSelection,
                Phase::Construction,
                Phase::ProfileCompletion,
            ] {
                diagnostics.enter_phase(phase);
                assert_eq!(
                    diagnostics.failure_urgency(&refusal),
                    "critical",
                    "{phase:?}"
                );
            }
        }

        diagnostics.enter_phase(Phase::ProfilePreflight);
        for cause in [
            Some(Cause::SwayIpc),
            Some(Cause::InconsistentWorkspaces),
            Some(Cause::ProfileWorkspaceChanged),
            Some(Cause::WindowActivity),
            Some(Cause::TargetOccupied),
            None,
        ] {
            let failure = Failure {
                cause,
                detail: "detail".into(),
            };
            assert_eq!(
                diagnostics.failure_urgency(&failure),
                "critical",
                "{cause:?}"
            );
        }

        // Anything may remain once an application started.
        diagnostics.spawned(Some(42));
        let refusal = Failure::new(Cause::ProfileTargetOccupied, "detail");
        assert_eq!(diagnostics.failure_urgency(&refusal), "critical");
        diagnostics.enter_phase(Phase::Construction);
        assert_eq!(diagnostics.failure_urgency(&occupied), "critical");
    }

    #[test]
    fn text_resembling_a_known_failure_is_never_classified() {
        // Before typed causes, these prefixes selected summaries and safe-refusal urgency.
        for (phase, imitation) in [
            (
                Phase::ProfilePreflight,
                "profile target occupied: workspace 3",
            ),
            (
                Phase::ProfilePreflight,
                "profile output mismatch: workspace 3",
            ),
            (
                Phase::ProfilePreflight,
                "profile output unavailable: workspace 3",
            ),
            (
                Phase::Construction,
                "target workspace must be completely empty",
            ),
            (Phase::Construction, "window timeout: elapsed=10000ms"),
            (Phase::Construction, "spawn application: missing"),
            (Phase::ProfileSelection, "get tree: connection closed"),
        ] {
            for operation in ["load", "profile load"] {
                let mut diagnostics = without_log(operation);
                diagnostics.profile_started(1);
                diagnostics.enter_phase(phase);
                let failure = Failure::from(imitation).context("layout construction");

                assert_eq!(
                    diagnostics.failure_urgency(&failure),
                    "critical",
                    "{imitation}"
                );
                assert_eq!(
                    diagnostics.failure_summary(&failure),
                    generic_failure_summary(operation),
                    "{imitation}"
                );
            }
        }

        // An uncertain-state cause keeps its own summary and urgency whatever its text says.
        let mut diagnostics = without_log("profile load");
        diagnostics.profile_started(1);
        diagnostics.enter_phase(Phase::ProfilePreflight);
        let failure = Failure::new(Cause::SwayIpc, "profile target occupied: workspace 3");
        assert_eq!(diagnostics.failure_urgency(&failure), "critical");
        assert_eq!(
            diagnostics.failure_summary(&failure),
            Cause::SwayIpc.summary()
        );
    }

    #[test]
    fn notification_categories_depend_only_on_the_cause() {
        let mut summaries = HashSet::new();
        for cause in ALL_CAUSES {
            let summary = cause.summary();
            assert!(summaries.insert(summary), "duplicate summary: {summary}");
            assert!(summary.len() < 160, "{summary}");
            assert_no_details(summary);

            for operation in ["load", "profile load"] {
                let mut bodies = HashSet::new();
                for detail in ["", "short wording", UNTRUSTED_DETAIL] {
                    // Wrapping adds context to the detail and never changes the cause.
                    for failure in [
                        Failure::new(cause, detail),
                        Failure::new(cause, detail).context("layout \"work\""),
                        Failure::new(cause, detail)
                            .context("layout construction")
                            .context("profile \"work\""),
                        Failure::new(cause, detail)
                            .context("workspace selection")
                            .context("profile \"work\""),
                    ] {
                        assert_eq!(failure.cause, Some(cause));
                        let diagnostics = without_log(operation);
                        let body = diagnostics.failure_notification(&failure);
                        assert!(body.starts_with(summary), "{body}");
                        assert_no_details(&body);
                        bodies.insert(body);
                    }
                }
                assert_eq!(bodies.len(), 1, "{cause:?}: {bodies:?}");
            }
        }
        for operation in ["load", "profile load", "capture", "profile capture", "menu"] {
            assert!(!summaries.contains(generic_failure_summary(operation)));
        }
    }

    #[test]
    fn unclassified_and_validation_failures_use_fixed_summaries() {
        for operation in ["load", "profile load"] {
            let mut diagnostics = without_log(operation);
            let unclassified = Failure::from(UNTRUSTED_DETAIL);
            assert_eq!(
                diagnostics.failure_notification(&unclassified),
                generic_failure_summary(operation)
            );

            // Validation details are parser text and paths, whatever their cause.
            diagnostics.enter_phase(Phase::Validation);
            let expected = if operation == "profile load" {
                "Unable to read or validate the profile and its layouts. Run sway-stage profile check NAME."
            } else {
                "Unable to read or validate the selected layout. Run sway-stage check NAME."
            };
            for failure in [
                unclassified.clone(),
                Failure::new(Cause::ApplicationStart, UNTRUSTED_DETAIL),
            ] {
                assert_eq!(diagnostics.failure_notification(&failure), expected);
                assert_eq!(diagnostics.failure_urgency(&failure), "normal");
            }
        }
        for operation in ["capture", "profile capture", "menu"] {
            let summary = generic_failure_summary(operation);
            assert!(summary.starts_with("Unable to"), "{summary}");
        }
    }

    #[test]
    fn combined_profile_failures_keep_cause_progress_windows_and_log_concise() {
        for target in [
            WorkspaceTarget::Number(999_999_999),
            WorkspaceTarget::Name("private name".into()),
        ] {
            for log_available in [false, true] {
                for failure in [
                    Failure::window_timeout("private", Some(ExitStatus::from_raw(11))),
                    Failure::new(Cause::WindowWorkspaceUnknown, "private"),
                    Failure::new(Cause::WindowNotTiled, "private"),
                    Failure::new(Cause::AmbiguousWindows, "id=20 private"),
                    Failure::new(Cause::ApplicationStart, "private"),
                ] {
                    let mut diagnostics = without_log("profile load");
                    diagnostics.profile_started(256);
                    diagnostics.profile_workspace(&WorkspaceTarget::Number(1), "private", 1);
                    diagnostics.spawned(Some(42));
                    diagnostics.profile_workspace_completed(101);
                    diagnostics.profile_workspace(&target, "private", 256);

                    diagnostics.log_file = log_available.then(|| File::open("/dev/null").unwrap());
                    let failure = failure.context("layout construction");
                    let body = diagnostics.failure_notification(&failure);
                    let cause = failure.cause.unwrap();
                    assert!(body.starts_with(cause.summary()));
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
                        matches!(cause, Cause::WindowTimeout { .. })
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
        let failed_spawn = diagnostics.failure_notification(&Failure::new(
            Cause::ApplicationStart,
            "spawn application: private command",
        ));
        assert!(failed_spawn.contains("Check its executable"));
        assert!(!failed_spawn.contains("Windows may remain"));
        assert!(!failed_spawn.contains("nothing changed"));

        diagnostics.spawned(Some(42));
        let body =
            diagnostics.failure_notification(&Failure::window_timeout("private selector", None));
        assert!(body.contains("Windows may remain. Inspect before retrying."));
        assert!(!body.contains("last-run.log"));
        assert!(!body.contains("private"));
    }

    #[test]
    fn profile_progress_keeps_names_private_and_survives_loader_stages() {
        let mut diagnostics = without_log("profile load");
        diagnostics.enter_phase(Phase::Validation);
        diagnostics.stage("profile: validation");
        let validation = diagnostics.failure_notification(&"private YAML".into());
        assert!(!validation.contains("Profile stopped"));
        assert!(!validation.contains("completed"));

        diagnostics.profile_started(3);
        diagnostics.enter_phase(Phase::ProfilePreflight);
        diagnostics.stage("workspace preflight");
        let ipc_failure = Failure::new(Cause::SwayIpc, "get tree: failed");
        let initial = diagnostics.failure_notification(&ipc_failure);
        assert!(initial.contains("Profile preflight stopped (0/3 completed)."));
        diagnostics.profile_workspace(
            &WorkspaceTarget::Name("<b>private</b>\n".into()),
            "secret",
            3,
        );
        let occupied = Failure::new(Cause::ProfileTargetOccupied, "private");
        let preflight = diagnostics.failure_notification(&occupied);
        assert!(preflight.contains("Profile preflight stopped at entry 3 (0/3 completed)."));
        assert!(!preflight.contains("Windows may remain"));
        assert!(!preflight.contains("private"));
        assert!(!preflight.contains("secret"));

        diagnostics.profile_workspace(&WorkspaceTarget::Number(4), "secret", 1);
        diagnostics.enter_phase(Phase::ProfileSelection);
        diagnostics.stage("workspace selection");
        let rejected = Failure::new(Cause::CommandRejected, "private");
        let selection = diagnostics.failure_notification(&rejected);
        assert!(selection.contains("Profile stopped at workspace 4 (0/3 completed)."));
        assert!(!selection.contains("Windows may remain"));
        assert!(!selection.contains("nothing changed"));
        let terminal = diagnostics.report_failure(rejected.context("profile \"work\""));
        assert!(terminal.contains("completed 0 workspaces []"), "{terminal}");
        assert!(
            terminal.ends_with("; no applications launched"),
            "{terminal}"
        );

        // The loader's own stages and phase never reset profile progress.
        diagnostics.enter_phase(Phase::Construction);
        diagnostics.stage("launch leaf=private workspace=104");
        diagnostics.spawned(Some(42));
        diagnostics.enter_phase(Phase::ProfileCompletion);
        diagnostics.stage("workspace completion verification");
        let changed = Failure::new(Cause::ProfileWorkspaceChanged, "private");
        let unverified = diagnostics.failure_notification(&changed);
        assert!(unverified.contains("Profile stopped at workspace 4 (0/3 completed)."));
        diagnostics.profile_workspace_completed(104);
        diagnostics.profile_workspace(&WorkspaceTarget::Name("sensitive name".into()), "secret", 2);
        diagnostics.enter_phase(Phase::Construction);
        diagnostics.stage("launch leaf=private workspace=105");
        let spawn_failure = Failure::new(Cause::ApplicationStart, "private");
        let partial = diagnostics.failure_notification(&spawn_failure);
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

        let terminal = diagnostics.report_failure(spawn_failure.context("profile \"work\""));
        assert!(
            terminal.contains("completed 1 workspaces [(4, 104)]"),
            "{terminal}"
        );
        assert!(
            terminal.contains("application launches may have left partial layouts"),
            "{terminal}"
        );
        assert!(
            terminal.contains("inspect windows before retrying"),
            "{terminal}"
        );
        assert!(!terminal.contains("no applications launched"), "{terminal}");
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
                let mut diagnostics = without_log("load");
                diagnostics.log_file = log_available.then(|| File::open("/dev/null").unwrap());
                diagnostics.application_spawned = true;
                let wait_detail = format!("no new windows observed; leaf={}", "x".repeat(10000));
                let failure = Failure::window_timeout(&wait_detail, status);
                assert_eq!(
                    failure.cause,
                    Some(Cause::WindowTimeout {
                        launch_exit: status
                    })
                );
                let detail = &failure.detail;
                assert!(detail.starts_with(&format!("window timeout: {wait_detail}")));
                if let Some(context) = expected_detail {
                    assert!(detail.ends_with(context), "{detail}");
                } else {
                    assert_eq!(*detail, format!("window timeout: {wait_detail}"));
                }

                // The same path as report_failure, including the caller's layout context.
                let body =
                    diagnostics.failure_notification(&failure.context("layout \"development\""));
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
    fn capture_and_menu_notifications_use_only_classified_summaries() {
        use crate::menu::{Error, FailureKind};

        let classified =
            crate::capture::Error::from(Failure::new(Cause::SwayIpc, UNTRUSTED_DETAIL));
        assert_eq!(classified.summary, Some(Cause::SwayIpc.summary()));
        assert_eq!(classified.detail, UNTRUSTED_DETAIL);
        let unclassified = crate::capture::Error::from(Failure::from(UNTRUSTED_DETAIL));
        assert_eq!(unclassified.summary, None);

        let untrusted_detail = format!("/private/{}<b>secret</b>", "x".repeat(10000));
        for (kind, expected) in [
            (
                FailureKind::Snapshot(None),
                "Unable to complete the menu action.",
            ),
            (
                FailureKind::Snapshot(Some(Cause::SwayIpc.summary())),
                Cause::SwayIpc.summary(),
            ),
            (
                FailureKind::Capture(None),
                "Unable to capture the workspace.",
            ),
            (
                FailureKind::ProfileCapture(None),
                "Unable to capture the profile.",
            ),
        ] {
            let error = Error::new(kind, untrusted_detail.clone());
            assert_eq!(menu_failure_body(&error), expected);
        }
    }

    #[test]
    fn menu_notifications_do_not_copy_untrusted_details_or_offer_an_old_log() {
        use crate::menu::{Error, FailureKind};

        let untrusted_detail = format!("/private/{}<b>secret</b>", "x".repeat(10000));
        for kind in [
            FailureKind::InvalidLayout,
            FailureKind::InvalidProfile,
            FailureKind::Snapshot(None),
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
        let mut diagnostics = without_log("load");
        diagnostics.log_file = Some(OpenOptions::new().write(true).open("/dev/full").unwrap());
        diagnostics.log_path = Some(PathBuf::from("/dev/full"));
        diagnostics.application_spawned = true;
        let detail = "elapsed=10000ms observed=[id=42 app_id=wrong]";
        let failure = Failure::window_timeout(detail, None);
        let terminal = diagnostics.report_failure(failure.clone());
        assert!(terminal.contains(detail));
        assert!(terminal.contains("inspect windows before retrying"));
        assert!(!terminal.contains("; log:"), "{terminal}");
        assert!(!terminal.contains("/dev/full"), "{terminal}");
        assert!(diagnostics.log_file.is_none());
        assert!(diagnostics.log_path.is_none());
        let notification = diagnostics.failure_notification(&failure);
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
        diagnostics.enter_phase(Phase::Construction);
        diagnostics.stage("launch");
        diagnostics.spawned(Some(42));

        let launch_log = fs::read_to_string(&path).unwrap();
        assert!(launch_log.contains("spawned pid=42"), "{launch_log}");

        // Capture, or menu before layout selection, reports without any log access.
        let capture_error = crate::capture::Error::from(Failure::new(
            Cause::SwayIpc,
            "get tree: connection closed",
        ));
        notify_capture_failure("capture", &capture_error).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), launch_log);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);

        let detail = "elapsed=10000ms observed=[id=42 app_id=wrong]";
        let terminal = diagnostics.report_failure(Failure::window_timeout(detail, None));
        assert!(terminal.contains(detail));
        assert!(terminal.contains("launch:"));
        assert!(terminal.contains("inspect windows before retrying"));
        // Logging still works, so the terminal message offers the log for details.
        assert!(
            terminal.ends_with(&format!("; log: {}", path.display())),
            "{terminal}"
        );
        let failed_log = fs::read_to_string(&path).unwrap();
        assert!(failed_log.contains(detail));
        assert!(failed_log.contains("inspect windows before retrying"));

        fs::remove_dir_all(&directory).unwrap();
    }
}
