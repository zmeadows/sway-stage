//! Launch diagnostics and bounded, best-effort notifications.
//!
//! `last-run.log` is reserved for layout launches: only `Diagnostics::start` creates or
//! replaces it. Operations that never load a selected layout report through
//! `notify_failure` or `notify_menu_failure`, leaving the previous launch's log in place.

use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Diagnostics {
    /// The invoked command, which names failure notifications.
    operation: &'static str,
    log_file: Option<File>,
    log_path: Option<PathBuf>,
    started_at: Instant,
    current_stage: String,
    /// Set only by `spawned`, after `Command::spawn` succeeded for this invocation.
    application_spawned: bool,
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
            application_spawned: false,
        }
    }

    /// Replaces the last-run log. Call only for an invocation that loads a selected layout:
    /// `load`, or `menu` once a layout is selected.
    pub fn start(operation: &'static str) -> Self {
        let mut diagnostics = Self {
            operation,
            log_file: None,
            log_path: None,
            started_at: Instant::now(),
            current_stage: "validation".into(),
            application_spawned: false,
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
            "{}.{:03} +{}ms [{}] {}\n",
            timestamp.as_secs(),
            timestamp.subsec_millis(),
            self.started_at.elapsed().as_millis(),
            self.current_stage.escape_debug(),
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

    /// Logs and notifies a fatal error, returning the primary message for stderr.
    pub fn report_failure(&mut self, error: String) -> String {
        let mut message = format!("{}: {error}", self.current_stage);
        // Validation, connection, prelaunch and failed-spawn errors leave nothing behind.
        if self.application_spawned {
            message
                .push_str("; stopped without cleanup or relaunch; inspect windows before retrying");
        }

        self.record(&message);

        if let Some(log_path) = &self.log_path {
            message.push_str(&format!("; log: {}", log_path.display()));
        }
        let notification = failure_body(
            self.operation,
            &error,
            &self.current_stage,
            self.log_file.is_some(),
        );
        if let Err(error) = send_failure_notification(self.operation, &notification) {
            self.record(&format!("secondary notification failure: {error}"));
        }

        message
    }

    pub fn report_sizing_warnings(&mut self, warnings: &[String]) {
        if warnings.is_empty() {
            return;
        }

        let message = warnings.join("\n");
        stderr(&format!("sway-stage: warning: {message}"));
        if let Err(error) = notify("normal", "sway-stage sizing warning", &message) {
            self.record(&format!("secondary warning notification failure: {error}"));
        }
    }
}

/// Attempts a critical notification for any operation's fatal error without touching the
/// last-run log. A notification failure is secondary: it is reported to stderr and
/// returned, and the caller still reports `message` as the primary error.
pub fn notify_failure(operation: &str, message: &str) -> Result<(), String> {
    send_failure_notification(operation, &failure_body(operation, message, "", false))
}

/// Menu preparation never creates a launch log, so these summaries never offer one.
pub fn notify_menu_failure(error: &crate::menu::Error) -> Result<(), String> {
    send_failure_notification("menu", &menu_failure_body(error))
}

fn menu_failure_body(error: &crate::menu::Error) -> String {
    use crate::menu::FailureKind;

    let summary = match &error.kind {
        FailureKind::Snapshot => return failure_body("menu", &error.detail, "", false),
        FailureKind::LayoutDiscovery => {
            "Cannot list saved layouts. Check the layouts directory and filenames."
        }
        FailureKind::NoLayouts => {
            "No saved layouts found. Add a YAML file to your layouts directory."
        }
        FailureKind::NoValidLayouts => {
            "No valid layouts found. Run sway-stage check NAME to inspect a layout."
        }
        FailureKind::InvalidLayout(name) => {
            return format!("Layout '{name}' is invalid. Run sway-stage check {name} for details.");
        }
        FailureKind::Capture => {
            "Cannot capture this workspace. Check floating or fullscreen windows, containers, and window focus."
        }
        FailureKind::FuzzelStart => {
            "Cannot start Fuzzel. Check that Fuzzel 1.11+ is installed and executable."
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

fn send_failure_notification(operation: &str, message: &str) -> Result<(), String> {
    notify(
        "critical",
        &format!("sway-stage {operation} failed"),
        message,
    )
    .inspect_err(|error| stderr(&format!("sway-stage: notification unavailable: {error}")))
}

/// Summarize only recognized error prefixes. Never copy arbitrary diagnostic text or
/// user-controlled names into the desktop body; terminal and log output retain them.
fn failure_body(operation: &str, error: &str, stage: &str, log_available: bool) -> String {
    // Loading adds a validated ASCII layout name around the underlying launch error.
    let mut cause = error;
    if let Some(layout_error) = error.strip_prefix("layout \"")
        && let Some((name, inner)) = layout_error.split_once("\": ")
        && crate::discovery::valid_name(name)
    {
        cause = inner;
    }
    for context in ["pre-menu workspace snapshot: ", "capture: "] {
        if let Some(inner) = cause.strip_prefix(context) {
            cause = inner;
        }
    }

    let summary = if stage.ends_with(": validation") {
        "Unable to read or validate the selected layout. Check its configuration."
    } else if cause.starts_with("window timeout: ") {
        "The expected window was not ready in time. Check its launch command or window selector."
    } else if cause.starts_with("workspace focus changed from target id=")
        || cause.starts_with("snapshot: focus left target id=")
    {
        "Loading stopped because the target workspace lost focus."
    } else if cause.starts_with("target workspace must be completely empty") {
        "The target workspace is occupied. Load on an empty workspace."
    } else if cause.starts_with("spawn application: ") {
        "Unable to start an application. Check its launch command and working directory."
    } else if [
        "connect: SWAYSOCK ",
        "connect to SWAYSOCK: ",
        "subscribe: ",
        "subscribe to workspace/window/tick: ",
        "get tree: ",
        "get workspaces: ",
        "read Sway event: ",
        "command IPC ",
        "snapshot deadline/channel failure: ",
        "tree deadline/channel failure: ",
    ]
    .iter()
    .any(|prefix| cause.starts_with(prefix))
    {
        "Unable to communicate with Sway."
    } else {
        match operation {
            "load" => "Unable to finish loading the layout.",
            "capture" => "Unable to capture the workspace. See terminal output for details.",
            _ => "Unable to complete the menu action.",
        }
    };

    if log_available {
        format!("{summary}\n\nSee last-run.log for details.")
    } else {
        summary.to_owned()
    }
}

/// Best-effort save confirmation, independent of launch diagnostics.
pub fn notify_draft_saved(name: &str) {
    let _ = notify(
        "normal",
        "sway-stage draft saved",
        &format!("Saved draft '{name}'."),
    )
    .inspect_err(|error| stderr(&format!("sway-stage: notification unavailable: {error}")));
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

    #[test]
    fn failure_notifications_summarize_recognized_causes() {
        for (error, expected) in [
            (
                "layout \"development\": window timeout: elapsed=10000ms observed=[]",
                "The expected window was not ready in time. Check its launch command or window selector.",
            ),
            (
                "layout \"development\": workspace focus changed from target id=7",
                "Loading stopped because the target workspace lost focus.",
            ),
            (
                "snapshot: focus left target id=7",
                "Loading stopped because the target workspace lost focus.",
            ),
            (
                "connect to SWAYSOCK: connection refused",
                "Unable to communicate with Sway.",
            ),
            (
                "pre-menu workspace snapshot: get tree: connection closed",
                "Unable to communicate with Sway.",
            ),
            (
                "spawn application: executable missing; child PATH and cwd",
                "Unable to start an application. Check its launch command and working directory.",
            ),
            (
                "target workspace must be completely empty, including tiled/floating containers",
                "The target workspace is occupied. Load on an empty workspace.",
            ),
            (
                "unexpected failure: window timeout: untrusted text",
                "Unable to finish loading the layout.",
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
    fn notifications_omit_long_names_paths_and_untrusted_markup() {
        let long_name = "a".repeat(10000);
        let timeout = format!("layout \"{long_name}\": window timeout: leaf=<b>private</b>");
        let body = failure_body("load", &timeout, "wait", true);
        assert!(body.starts_with("The expected window was not ready in time."));
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
            "Unable to read or validate the selected layout. Check its configuration."
        );
        assert_eq!(
            failure_body(
                "capture",
                "workspace <b>private</b>: unsupported tree",
                "",
                false
            ),
            "Unable to capture the workspace. See terminal output for details."
        );
    }

    #[test]
    fn menu_notifications_do_not_copy_untrusted_details_or_offer_an_old_log() {
        use crate::menu::{Error, FailureKind};

        let untrusted_detail = format!("/private/{}<b>secret</b>", "x".repeat(10000));
        for kind in [
            FailureKind::Snapshot,
            FailureKind::LayoutDiscovery,
            FailureKind::NoLayouts,
            FailureKind::NoValidLayouts,
            FailureKind::Capture,
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
            application_spawned: true,
        };
        let error = "window timeout: elapsed=10000ms observed=[id=42 app_id=wrong]";
        let terminal = diagnostics.report_failure(error.into());
        assert!(terminal.contains(error));
        assert!(terminal.contains("inspect windows before retrying"));
        assert!(diagnostics.log_file.is_none());
        assert!(
            !failure_body("load", error, "wait", diagnostics.log_file.is_some())
                .contains("last-run.log")
        );
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
