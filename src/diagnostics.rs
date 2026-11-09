//! Last-run diagnostics and bounded, best-effort error notifications for `load` only.

use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Diagnostics {
    file: Option<File>,
    path: Option<PathBuf>,
    started: Instant,
    stage: String,
}

impl Diagnostics {
    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf) -> Self {
        let (file, path) = open_log(path).unwrap();
        Self {
            file: Some(file),
            path: Some(path),
            started: Instant::now(),
            stage: "test".into(),
        }
    }

    pub fn start() -> Self {
        let mut diagnostics = Self {
            file: None,
            path: None,
            started: Instant::now(),
            stage: "validation".into(),
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

    pub fn failure(&mut self, error: String) -> String {
        let mut message = format!("{}: {error}", self.stage);
        message.push_str("; stopped without cleanup or relaunch; inspect windows before retrying");
        self.record(&message);
        if let Some(path) = &self.path {
            message.push_str(&format!("; log: {}", path.display()));
        }
        if let Err(error) = notify("critical", "sway-stage load failed", &message) {
            self.record(&format!("secondary notification failure: {error}"));
            stderr(&format!("sway-stage: notification unavailable: {error}"));
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
