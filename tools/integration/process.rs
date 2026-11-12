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
use std::ffi::OsStr;
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
    find_in_search_path(name, &search_path)
}

/// `find_on_path` over an explicit `PATH`-style list, so tests need not change `PATH`.
pub fn find_in_search_path(name: &str, search_path: &OsStr) -> Option<PathBuf> {
    for directory in env::split_paths(search_path) {
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
    run_bounded(command, operation, timeout, false)?.require_success(operation)
}

/// Only the read-only remaining-window query may run after an interrupt. It still has
/// output limits, a deadline, and direct-child shutdown on failure.
pub fn run_diagnostic(command: Command, timeout: Duration) -> Result<String, String> {
    run_bounded(command, "remaining-window tree query", timeout, true)?
        .require_success("remaining-window tree query")
}

/// The same bounded runner, with exit interpretation left to an explicit refusal check.
/// Timeouts, output failures, and interruption still return errors, never an exit result.
pub fn run_with_status(
    command: Command,
    operation: &str,
    timeout: Duration,
) -> Result<CompletedProcess, String> {
    run_bounded(command, operation, timeout, false)
}

#[derive(Debug)]
pub struct CompletedProcess {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl CompletedProcess {
    fn require_success(self, operation: &str) -> Result<String, String> {
        if !self.status.success() {
            return Err(format!(
                "{operation}: {}: {}",
                self.status,
                self.stderr.trim()
            ));
        }
        Ok(self.stdout)
    }
}

fn run_bounded(
    mut command: Command,
    operation: &str,
    timeout: Duration,
    diagnostic: bool,
) -> Result<CompletedProcess, String> {
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

    Ok(CompletedProcess {
        status,
        stdout: String::from_utf8(stdout.bytes)
            .map_err(|_| format!("{operation}: stdout is not UTF-8"))?,
        stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
    })
}

/// Launch a package fixture without collecting long-lived output or ever killing it.
/// The success path closes only its independently verified Sway window.
pub fn spawn_application(mut command: Command) -> Result<Child, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    spawn(command, "package launch", false)
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

/// Private terminal-child mode; deliberately bypasses the desktop harness entry point.
pub const PROBE_FLAG: &str = "--integration-process-probe";

/// Inspect only direct children of the independently owned terminal, across all its
/// threads. Never search unrelated processes or infer that the terminal retains child
/// argv/cwd. Inspection errors fail the scenario without granting any signal authority.
pub fn verify_probe(
    proc_root: &Path,
    terminal_pid: i32,
    leaf: &crate::scenarios::ConfiguredWindow,
) -> Result<i32, String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        check_interruption()?;
        let children = terminal_children(proc_root, terminal_pid)?;
        if !children.is_empty() {
            if children.len() != 1 {
                return Err("process probe requires exactly one direct terminal child".into());
            }
            let pid = children[0];
            verify_probe_state(proc_root, pid, leaf)?;
            return Ok(pid);
        }
        if Instant::now() >= deadline {
            return Err("process probe child did not appear within two seconds".into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn terminal_children(proc_root: &Path, terminal_pid: i32) -> Result<Vec<i32>, String> {
    if terminal_pid <= 0 {
        return Err("process probe has invalid terminal PID".into());
    }
    let tasks = fs::read_dir(proc_root.join(format!("{terminal_pid}/task")))
        .map_err(|error| format!("inspect terminal threads: {error}"))?;
    let mut children = std::collections::BTreeSet::new();
    for task in tasks {
        let task = task.map_err(|error| format!("inspect terminal thread: {error}"))?;
        let text = fs::read_to_string(task.path().join("children"))
            .map_err(|error| format!("inspect terminal children: {error}"))?;
        for child in text.split_whitespace() {
            let pid = child
                .parse::<i32>()
                .ok()
                .filter(|pid| *pid > 0)
                .ok_or("invalid terminal child PID")?;
            children.insert(pid);
        }
    }
    Ok(children.into_iter().collect())
}

fn verify_probe_state(
    proc_root: &Path,
    pid: i32,
    leaf: &crate::scenarios::ConfiguredWindow,
) -> Result<(), String> {
    let process_directory = proc_root.join(pid.to_string());
    let argv = fs::read(process_directory.join("cmdline"))
        .map_err(|error| format!("inspect probe argv: {error}"))?;
    let expected_argv: Vec<u8> = leaf.command[2..]
        .iter()
        .flat_map(|argument| argument.bytes().chain(std::iter::once(0)))
        .collect();
    if argv != expected_argv {
        return Err("probe literal argument boundaries or contents differ".into());
    }

    let cwd = fs::read_link(process_directory.join("cwd"))
        .map_err(|error| format!("inspect probe cwd: {error}"))?;
    let expected_cwd = leaf.cwd.as_ref().ok_or("probe cwd missing")?;
    let expected_cwd =
        fs::canonicalize(expected_cwd).map_err(|error| format!("resolve probe cwd: {error}"))?;
    if cwd != expected_cwd {
        return Err("probe working directory differs from configured absolute cwd".into());
    }

    let environment = fs::read(process_directory.join("environ"))
        .map_err(|error| format!("inspect probe environment override: {error}"))?;
    for (key, value) in &leaf.env {
        let expected = format!("{key}={value}");
        let prefix = format!("{key}=");
        let matching: Vec<&[u8]> = environment
            .split(|byte| *byte == 0)
            .filter(|entry| entry.starts_with(prefix.as_bytes()))
            .collect();
        if matching != [expected.as_bytes()] {
            return Err("probe environment override differs".into());
        }
    }
    Ok(())
}

/// Window cleanup must finish first. Observe exit only; never signal a discovered PID.
pub fn wait_probe_exit(pid: i32) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        check_interruption()?;
        match fs::symlink_metadata(format!("/proc/{pid}")) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("inspect probe exit: {error}")),
            Ok(_) if Instant::now() >= deadline => {
                return Err("probe still exists after verified window cleanup".into());
            }
            Ok(_) => thread::sleep(POLL_INTERVAL),
        }
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use crate::fixtures::standard_scenario;
    use std::os::unix::fs::symlink;

    #[test]
    fn probe_inspection_checks_only_the_terminal_child_and_test_values() {
        let root = env::temp_dir().join(format!("sway-stage-probe-test-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let proc_root = root.join("proc");
        let child = proc_root.join("42");
        fs::create_dir_all(&child).unwrap();
        let task = proc_root.join("41/task/41");
        fs::create_dir_all(&task).unwrap();
        fs::write(task.join("children"), "42 ").unwrap();
        fs::create_dir(root.join("process cwd")).unwrap();
        let mut scenario = standard_scenario("process-execution");
        crate::scenarios::configure_process_execution(
            &mut scenario,
            Path::new("/test/helper"),
            &root.join("process cwd"),
            "unique",
        )
        .unwrap();
        let leaf = scenario.layout.leaves()[0];
        let argv: Vec<u8> = leaf.command[2..]
            .iter()
            .flat_map(|arg| arg.bytes().chain([0]))
            .collect();
        fs::write(child.join("cmdline"), &argv).unwrap();
        fs::write(
            child.join("environ"),
            b"UNRELATED=private\0SWAY_STAGE_IT_unique=unique override\0",
        )
        .unwrap();
        symlink(root.join("process cwd"), child.join("cwd")).unwrap();
        assert_eq!(verify_probe(&proc_root, 41, leaf).unwrap(), 42);
        assert!(
            verify_probe(&proc_root, 99, leaf)
                .unwrap_err()
                .contains("terminal threads")
        );
        fs::write(task.join("children"), "42 43").unwrap();
        assert!(
            verify_probe(&proc_root, 41, leaf)
                .unwrap_err()
                .contains("exactly one")
        );
        fs::write(task.join("children"), "invalid").unwrap();
        assert!(
            terminal_children(&proc_root, 41)
                .unwrap_err()
                .contains("invalid")
        );

        fs::write(child.join("cmdline"), argv.strip_suffix(&[0]).unwrap()).unwrap();
        assert!(
            verify_probe_state(&proc_root, 42, leaf)
                .unwrap_err()
                .contains("argument boundaries")
        );
        fs::write(child.join("cmdline"), argv).unwrap();
        fs::write(child.join("environ"), b"UNRELATED=private\0").unwrap();
        let error = verify_probe_state(&proc_root, 42, leaf).unwrap_err();
        assert!(error.contains("environment override"));
        assert!(!error.contains("private"));
        fs::remove_file(child.join("environ")).unwrap();
        assert!(
            verify_probe_state(&proc_root, 42, leaf)
                .unwrap_err()
                .contains("inspect probe environment")
        );
        fs::remove_file(child.join("cwd")).unwrap();
        assert!(
            verify_probe_state(&proc_root, 42, leaf)
                .unwrap_err()
                .contains("inspect probe cwd")
        );
        fs::remove_file(child.join("cmdline")).unwrap();
        assert!(
            verify_probe_state(&proc_root, 42, leaf)
                .unwrap_err()
                .contains("inspect probe argv")
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn default_completion_still_rejects_nonzero_status_and_retains_diagnostics() {
        let completed = |code| CompletedProcess {
            status: ExitStatus::from_raw(code << 8),
            stdout: "output".into(),
            stderr: "reason".into(),
        };
        assert_eq!(completed(0).require_success("check").unwrap(), "output");
        assert!(
            completed(1)
                .require_success("load")
                .unwrap_err()
                .contains("reason")
        );
        assert_eq!(completed(1).status.code(), Some(1));
        // A signal cannot masquerade as the expected exit code.
        assert_eq!(ExitStatus::from_raw(9).code(), None);
    }

    #[test]
    fn capture_stream_limits_and_end_of_output_apply_without_starting_processes() {
        let (parent, mut peer) = UnixStream::pair().unwrap();
        let mut capture = CapturedOutput::new("stderr", parent, 3).unwrap();
        assert!(!capture.read_chunk().unwrap());
        peer.write_all(b"abc").unwrap();
        assert!(capture.read_chunk().unwrap());
        peer.write_all(b"d").unwrap();
        assert!(
            capture
                .read_chunk()
                .unwrap_err()
                .contains("exceeded 3 bytes")
        );
        drop(peer);
        assert!(capture.read_chunk().unwrap());
        assert!(capture.closed);
    }
}
