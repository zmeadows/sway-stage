//! Bounded child processes: `swaymsg` helpers and the programs the harness runs.
//!
//! Executables are explicit absolute paths and every argument remains a separate argv
//! element; nothing runs through a shell. Child stdout and stderr are Unix socket pairs
//! rather than pipes, because the parent's end of a socket can be read without blocking
//! or with a timeout. A deadline therefore always bounds collection, even when a
//! descendant keeps an output stream open.
//!
//! The only signal sent is SIGKILL to one of the harness's own direct children, to stop
//! the event helper or an overdue run. It never reaches that child's descendants, such as
//! applications started by `sway-stage load`, and never targets a window.

use std::env;
use std::fs;
use std::io::{ErrorKind, Read};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(5);
// SIGKILL cannot be caught or ignored; only an uninterruptible kernel wait delays exit.
const KILL_REAP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_STDOUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 1024 * 1024;
const READ_CHUNK_BYTES: usize = 64 * 1024;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// The handler only records a request. Desktop decisions and child shutdown happen in
/// normal control flow. Repeated SIGINTs cannot bypass the guarded failure path.
pub fn install_sigint_handler() -> Result<(), String> {
    signal_hook::flag::register(signal_hook::consts::SIGINT, &INTERRUPTED)
        .map(|_| ())
        .map_err(|error| format!("install SIGINT handler: {error}"))
}

pub fn interruption_requested() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

pub fn check_interruption() -> Result<(), String> {
    require_not_interrupted(interruption_requested())
}

fn require_not_interrupted(requested: bool) -> Result<(), String> {
    if requested {
        return Err("interrupted; focus history is no longer trusted".into());
    }
    Ok(())
}

/// Resolves a command name on `PATH` or an explicit path to a canonical executable.
pub fn find_executable(name: &str) -> Result<PathBuf, String> {
    let candidate = if name.contains('/') {
        Some(PathBuf::from(name)).filter(|path| is_executable_file(path))
    } else {
        find_on_path(name)
    }
    .ok_or_else(|| format!("required executable not found or not executable: {name}"))?;
    fs::canonicalize(&candidate)
        .map_err(|error| format!("resolve {}: {error}", candidate.display()))
}

/// Returns the first executable file `name` in an absolute `PATH` directory, without
/// resolving symlinks, so a wrapper or multi-call link keeps the name it is invoked by.
/// Relative `PATH` entries are skipped so the current directory can never supply one.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let search_path = env::var_os("PATH").unwrap_or_default();
    for directory in env::split_paths(&search_path) {
        if !directory.is_absolute() {
            continue;
        }

        let candidate = directory.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Runs `command` to completion within `timeout` and returns its stdout, which must be
/// UTF-8. Stdin is null. Environment changes the caller made on `command` apply to this
/// child only. A nonzero exit reports the child's stderr. On expiry or a capture failure,
/// the direct child is killed and reaped before the error is returned.
pub fn run(command: Command, operation: &str, timeout: Duration) -> Result<String, String> {
    run_bounded(command, operation, timeout, false)
}

/// Only the read-only remaining-window query may run after an interrupt. It still has
/// output limits, a deadline, and direct-child shutdown on failure.
pub fn run_diagnostic(command: Command, timeout: Duration) -> Result<String, String> {
    run_bounded(command, "remaining-window tree query", timeout, true)
}

fn run_bounded(
    mut command: Command,
    operation: &str,
    timeout: Duration,
    diagnostic: bool,
) -> Result<String, String> {
    let (stdout_socket, child_stdout) = socket_pair(operation)?;
    let (stderr_socket, child_stderr) = socket_pair(operation)?;
    let mut stdout = CapturedOutput::new("stdout", stdout_socket, MAX_STDOUT_BYTES)
        .map_err(|error| format!("{operation}: {error}"))?;
    let mut stderr = CapturedOutput::new("stderr", stderr_socket, MAX_STDERR_BYTES)
        .map_err(|error| format!("{operation}: {error}"))?;

    command
        .stdin(Stdio::null())
        .stdout(child_stdout)
        .stderr(child_stderr);
    let mut child = spawn(command, operation, diagnostic)?;

    let status = match collect_until_exit(&mut child, &mut stdout, &mut stderr, timeout, diagnostic)
    {
        Ok(status) => status,
        Err(error) => {
            let mut message = format!("{operation}: {error}");
            if let Err(stop_error) = kill_and_reap(&mut child, operation) {
                message.push_str(&format!("; {stop_error}"));
            }
            return Err(message);
        }
    };

    if !status.success() {
        let diagnostics = String::from_utf8_lossy(&stderr.bytes);
        return Err(format!("{operation}: {status}: {}", diagnostics.trim()));
    }
    String::from_utf8(stdout.bytes).map_err(|_| format!("{operation}: stdout is not UTF-8"))
}

/// Starts a long-running helper whose stdout arrives on the returned blocking socket. Its
/// stdin and stderr are null. The caller owns the child and stops it with `kill_and_reap`.
pub fn spawn_with_output(
    mut command: Command,
    operation: &str,
) -> Result<(Child, UnixStream), String> {
    let (output, child_stdout) = socket_pair(operation)?;
    command
        .stdin(Stdio::null())
        .stdout(child_stdout)
        .stderr(Stdio::null());
    let child = spawn(command, operation, false)?;
    Ok((child, output))
}

/// Sends SIGKILL to one of the harness's own direct children and reaps it within a bound.
/// Never use this for applications or windows.
pub fn kill_and_reap(child: &mut Child, description: &str) -> Result<(), String> {
    // Signalling an exited but unreaped child is harmless: its PID cannot be reused yet.
    // After a successful reap, `Child::kill` sends nothing.
    child
        .kill()
        .map_err(|error| format!("stop {description}: {error}"))?;

    let deadline = Instant::now() + KILL_REAP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                return Err(format!(
                    "{description} (pid {}) was not reaped within {KILL_REAP_TIMEOUT:?} of SIGKILL",
                    child.id()
                ));
            }
            Err(error) => return Err(format!("reap {description}: {error}")),
        }
    }
}

/// Takes `command` by value so it is dropped on return. A `Command` keeps its own copies
/// of the child's socket ends, and end of output is observable only once they are closed.
fn spawn(mut command: Command, operation: &str, diagnostic: bool) -> Result<Child, String> {
    // Executables are resolved beforehand; never rely on PATH lookup at spawn time.
    if !Path::new(command.get_program()).is_absolute() {
        return Err(format!("{operation}: executable must be an absolute path"));
    }
    if !diagnostic {
        check_interruption()?;
    }

    // A terminal sends SIGINT to its foreground process group. Isolate each direct child
    // before exec so Ctrl-C reaches the harness, not sway-stage's launched applications.
    // Never signal the group: cancellation kills only this direct child by Child's PID.
    command.process_group(0);
    command
        .spawn()
        .map_err(|error| format!("{operation}: start: {error}"))
}

/// Returns the parent's end and the child's end, ready to be a child's stdout or stderr.
fn socket_pair(operation: &str) -> Result<(UnixStream, Stdio), String> {
    let (parent_end, child_end) = UnixStream::pair()
        .map_err(|error| format!("{operation}: create output socket: {error}"))?;
    Ok((parent_end, Stdio::from(OwnedFd::from(child_end))))
}

/// Reads both outputs while polling for exit, so a child never blocks on a full socket.
fn collect_until_exit(
    child: &mut Child,
    stdout: &mut CapturedOutput,
    stderr: &mut CapturedOutput,
    timeout: Duration,
    diagnostic: bool,
) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + timeout;
    let mut exit_status = None;
    loop {
        if !diagnostic {
            check_interruption()?;
        }
        let stdout_progressed = stdout.read_chunk()?;
        let stderr_progressed = stderr.read_chunk()?;
        if exit_status.is_none() {
            exit_status = child.try_wait().map_err(|error| format!("wait: {error}"))?;
        }

        if let Some(status) = exit_status
            && stdout.closed
            && stderr.closed
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(match exit_status {
                Some(_) => format!("exited, but its output stayed open for {timeout:?}"),
                None => format!("did not exit within {timeout:?}"),
            });
        }
        if !stdout_progressed && !stderr_progressed {
            thread::sleep(POLL_INTERVAL);
        }
    }
}

/// One child output stream, read without blocking and retained up to a byte limit.
struct CapturedOutput {
    name: &'static str,
    socket: UnixStream,
    limit: usize,
    bytes: Vec<u8>,
    /// Set once the stream reports end of file: every writer has closed it.
    closed: bool,
}

impl CapturedOutput {
    fn new(name: &'static str, socket: UnixStream, limit: usize) -> Result<Self, String> {
        socket
            .set_nonblocking(true)
            .map_err(|error| format!("configure {name} socket: {error}"))?;
        Ok(Self {
            name,
            socket,
            limit,
            bytes: Vec::new(),
            closed: false,
        })
    }

    /// Reads at most one chunk without blocking. Returns whether data or end of file arrived.
    fn read_chunk(&mut self) -> Result<bool, String> {
        if self.closed {
            return Ok(false);
        }

        let mut chunk = [0; READ_CHUNK_BYTES];
        loop {
            match self.socket.read(&mut chunk) {
                Ok(0) => {
                    self.closed = true;
                    return Ok(true);
                }
                Ok(count) => {
                    if self.bytes.len() + count > self.limit {
                        return Err(format!("{} exceeded {} bytes", self.name, self.limit));
                    }
                    self.bytes.extend_from_slice(&chunk[..count]);
                    return Ok(true);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(format!("read {}: {error}", self.name)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn interruption_is_a_stop_request_even_when_reported_repeatedly() {
        assert!(super::require_not_interrupted(false).is_ok());
        for _ in 0..2 {
            assert!(
                super::require_not_interrupted(true)
                    .unwrap_err()
                    .contains("interrupted")
            );
        }
    }
}
