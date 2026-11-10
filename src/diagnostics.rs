//! Launch diagnostics and bounded, best-effort notifications.
//!
//! `last-run.log` is reserved for layout launches: only `Diagnostics::start` creates or
//! replaces it. Operations that never load a selected layout report through
//! `notify_failure`, which leaves the previous launch's log in place.

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
    file: Option<File>,
    path: Option<PathBuf>,
    started: Instant,
    stage: String,
    /// Set only by `spawned`, after `Command::spawn` succeeded for this invocation.
    launched: bool,
}

impl Diagnostics {
    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf) -> Self {
        let (file, path) = open_log(path).unwrap();
        Self {
            operation: "load",
            file: Some(file),
            path: Some(path),
            started: Instant::now(),
            stage: "test".into(),
            launched: false,
        }
    }

    /// Replaces the last-run log. Call only for an invocation that loads a selected layout:
    /// `load`, or the picker once a layout is selected.
    pub fn start(operation: &'static str) -> Self {
        let mut diagnostics = Self {
            operation,
            file: None,
            path: None,
            started: Instant::now(),
            stage: "validation".into(),
            launched: false,
        };
        match state_path().and_then(open_log) {
            Ok((file, path)) => {
                diagnostics.file = Some(file);
                diagnostics.path = Some(path);
            }
            Err(error) => stderr(&format!("sway-stage: logging unavailable: {error}")),
        }
        diagnostics
    }

    pub fn stage(&mut self, stage: impl Into<String>) {
        self.stage = stage.into();
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
            self.started.elapsed().as_millis(),
            self.stage.escape_debug(),
            message.escape_debug(),
        );
        if let Some(file) = &mut self.file {
            if let Err(error) = file.write_all(line.as_bytes()) {
                stderr(&format!(
                    "sway-stage: logging failed: {error}; using stderr"
                ));
                self.file = None;
            } else {
                return;
            }
        }
        let _ = io::stderr().lock().write_all(line.as_bytes());
    }

    /// Records a successful `Command::spawn`. A spawn that failed started nothing.
    pub fn spawned(&mut self, pid: u32) {
        self.launched = true;
        self.record(&format!("spawned pid={pid}"));
    }

    /// Logs and notifies a fatal error, returning the primary message for stderr.
    pub fn failure(&mut self, error: String) -> String {
        let mut message = format!("{}: {error}", self.stage);
        // Validation, connection, prelaunch and failed-spawn errors leave nothing behind.
        if self.launched {
            message
                .push_str("; stopped without cleanup or relaunch; inspect windows before retrying");
        }
        self.record(&message);
        if let Some(path) = &self.path {
            message.push_str(&format!("; log: {}", path.display()));
        }
        if let Err(error) = notify_failure(self.operation, &message) {
            self.record(&format!("secondary notification failure: {error}"));
        }
        message
    }

    pub fn sizing_warnings(&mut self, warnings: &[String]) {
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
    notify(
        "critical",
        &format!("sway-stage {operation} failed"),
        message,
    )
    .inspect_err(|error| stderr(&format!("sway-stage: notification unavailable: {error}")))
}

fn stderr(message: &str) {
    let _ = writeln!(io::stderr().lock(), "{message}");
}

fn state_path() -> Result<PathBuf, String> {
    let base = if let Some(xdg) = env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        PathBuf::from(xdg)
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
    if !base.is_absolute() {
        return Err("XDG_STATE_HOME must be absolute".into());
    }
    Ok(base.join("sway-stage/last-run.log"))
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
            result => {
                // Only the optional notification helper is terminated, never an application.
                let _ = child.kill();
                let _ = child.wait();
                return Err(match result {
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

        // Capture, or a picker before its selection, reports without any log access.
        notify_failure("capture", "capture: get tree: connection closed").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), launch_log);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(&directory).unwrap();
    }
}
