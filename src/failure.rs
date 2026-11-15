//! Runtime failures of Sway observation, layout loading, and profile loading.
//!
//! The code that detects a failure knows what it means, so it chooses the `Cause` there.
//! Wrapping adds context to the detail without changing the cause, and nothing ever derives
//! a cause from error text: text can change, and it can contain names, paths, or other
//! untrusted details that only resemble a known failure.

use std::fmt;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

/// What a runtime failure means for the user, grouped by the action that can resolve it.
/// Each cause has one fixed desktop summary that never includes failure details.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// The ordinary loader's frozen target was not completely empty before any launch.
    TargetOccupied,
    /// A profile target was not completely empty.
    ProfileTargetOccupied,
    /// A profile target exists on another output than the profile expects.
    ProfileOutputMismatch,
    /// A missing profile target's expected output is not active.
    ProfileOutputUnavailable,
    /// The window deadline passed. The application's exit status, when reported, adds
    /// context such as an early crash.
    WindowTimeout {
        launch_exit: Option<ExitStatus>,
    },
    WindowOnOtherWorkspace,
    WindowInScratchpad,
    WindowWorkspaceUnknown,
    AmbiguousWindows,
    WindowNotTiled,
    UnexpectedWindow,
    PreexistingWindow,
    NewWindowClosed,
    WindowIdentityChanged,
    /// An accepted or matched window closed or changed identity, placement, or state.
    LayoutWindowChanged,
    /// Window activity made ownership or emptiness uncertain.
    WindowActivity,
    HierarchyMismatch,
    FocusLost,
    /// Focus kept moving within the target after bounded restorations.
    FocusUnstable,
    OutputChanged,
    TargetUnavailable,
    /// Workspace identity, focus, or output changed during profile coordination.
    ProfileWorkspaceChanged,
    ConfigurationReloaded,
    SelectionUnverified,
    InconsistentWorkspaces,
    ApplicationStart,
    CommandRejected,
    SwayIpc,
}

impl Cause {
    pub fn summary(self) -> &'static str {
        match self {
            Self::TargetOccupied => {
                "The target workspace is occupied. Use a workspace with no windows or empty containers."
            }
            Self::ProfileTargetOccupied => {
                "A profile target is occupied. All profile targets must be empty, including floating windows and empty containers."
            }
            Self::ProfileOutputMismatch => {
                "A profile workspace is on a different output than configured. Check the profile's output expectations and Sway workspace placement rules."
            }
            Self::ProfileOutputUnavailable => {
                "An output required by the profile is not active. Connect or enable it, or update the profile's output expectation."
            }
            Self::WindowTimeout { .. } => {
                "Window timed out. Check launch command, exact app ID/class, and single-instance behavior."
            }
            Self::WindowOnOtherWorkspace => {
                "A window for this layout opened on another workspace. Check Sway's window assignment rules."
            }
            Self::WindowInScratchpad => {
                "A window for this layout opened in the scratchpad instead of the target workspace. Check Sway's scratchpad rules."
            }
            Self::WindowWorkspaceUnknown => {
                "A matching window's workspace could not be determined. Inspect workspace state before retrying."
            }
            Self::AmbiguousWindows => {
                "Multiple new windows match the expected application. Use a more specific app ID/class or a command that opens one window."
            }
            Self::WindowNotTiled => {
                "Loading requires tiled windows. A match is floating/fullscreen or has unknown fullscreen state; check app settings and Sway rules."
            }
            Self::UnexpectedWindow => {
                "An unexpected window appeared in the target workspace. Check the layout's app ID/class and Sway rules."
            }
            Self::PreexistingWindow => {
                "A preexisting window entered the target workspace. Keep other windows off the target while loading."
            }
            Self::NewWindowClosed => {
                "A newly observed window closed before its placement could be verified. Check application behavior and retry."
            }
            Self::WindowIdentityChanged => {
                "A window's app ID/class changed during loading. Use a stable, exact selector."
            }
            Self::LayoutWindowChanged => {
                "A layout window closed or changed identity, placement, or tiled state. Keep layout windows unchanged until loading finishes."
            }
            Self::WindowActivity => {
                "Window activity made loading unsafe. Keep windows unchanged until loading finishes."
            }
            Self::HierarchyMismatch => {
                "The workspace hierarchy differs from the expected layout. Check Sway rules and keep windows unchanged while loading."
            }
            Self::FocusLost => "The target workspace lost focus. Keep it focused while loading.",
            Self::FocusUnstable => {
                "Focus kept moving between windows on the target workspace. Avoid moving the pointer or focus there until loading finishes."
            }
            Self::OutputChanged => {
                "The target workspace changed outputs. Keep it on its original output while loading."
            }
            Self::TargetUnavailable => {
                "The target workspace is no longer available. Check workspace state and retry."
            }
            Self::ProfileWorkspaceChanged => {
                "Workspace identity, focus, or output changed during profile loading. Keep workspaces unchanged until loading finishes."
            }
            Self::ConfigurationReloaded => {
                "Sway's configuration reloaded during loading. Retry after the reload finishes."
            }
            Self::SelectionUnverified => {
                "Sway's workspace selection could not be verified. Check for workspace changes and retry."
            }
            Self::InconsistentWorkspaces => {
                "Sway reported inconsistent workspace identities or focus. Retry when workspace state is stable."
            }
            Self::ApplicationStart => {
                "Unable to start an application. Check its executable, command, working directory, and permissions."
            }
            Self::CommandRejected => {
                "Sway rejected a command or returned an unexpected reply. Check Sway's configuration and compatibility."
            }
            Self::SwayIpc => {
                "Unable to communicate with Sway. Check that Sway is running and its IPC connection is available."
            }
        }
    }
}

/// Complete technical detail for stderr and the log, with the cause when its producer
/// classified it. An unclassified failure (`cause: None`) gets the operation's generic
/// notification; `From<String>` creates one, so unclassified errors still propagate with `?`.
#[derive(Clone, Debug)]
pub struct Failure {
    pub cause: Option<Cause>,
    pub detail: String,
}

impl Failure {
    pub fn new(cause: Cause, detail: impl Into<String>) -> Self {
        Self {
            cause: Some(cause),
            detail: detail.into(),
        }
    }

    /// The detail keeps the observed process status without another poll.
    pub fn window_timeout(wait_detail: &str, launch_exit: Option<ExitStatus>) -> Self {
        let mut detail = format!("window timeout: {wait_detail}");
        if let Some(status) = launch_exit
            && let Some(context) = launch_exit_context(status)
        {
            detail.push_str(&format!(". {context}"));
        }

        Self::new(Cause::WindowTimeout { launch_exit }, detail)
    }

    /// Prefixes the detail; the cause is unchanged.
    pub fn context(mut self, context: impl fmt::Display) -> Self {
        self.detail = format!("{context}: {}", self.detail);
        self
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl From<String> for Failure {
    fn from(detail: String) -> Self {
        Self {
            cause: None,
            detail,
        }
    }
}

impl From<&str> for Failure {
    fn from(detail: &str) -> Self {
        detail.to_owned().into()
    }
}

/// Shared by the timeout detail and its notification, so both describe the same status.
pub fn launch_exit_context(status: ExitStatus) -> Option<String> {
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
