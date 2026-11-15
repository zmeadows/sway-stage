//! Starts layout applications outside the Sway launch context of whatever started
//! sway-stage, and supervises each one only while its window is awaited.
//!
//! Sway records the focused workspace for every `exec`, such as a keybinding that runs
//! `sway-stage menu`. It places the first window that presents the exported activation
//! token, or whose process descends from the process it started, on that recorded
//! workspace, which is not the target that sway-stage selected later. So an application
//! receives no inherited activation variables, and it is created only after its parent has
//! been reparented away from sway-stage:
//!
//! ```text
//! sway-stage ── intermediate (exits at once)
//!               └─ supervisor (reparented to init or a subreaper)
//!                  └─ application
//! ```
//!
//! Only a parent can collect an exit status, so the supervisor reports the application's
//! own PID and, if it exits while sway-stage still waits for its window, its wait status.
//! The supervisor exits as soon as sway-stage stops listening, its time limit passes, or
//! the application exits. A still-running application is then reparented again. Without
//! Linux 5.9's `close_range` or 5.3's pidfds, it exits at once and reports no status.

use crate::diagnostics::Diagnostics;
use std::collections::BTreeMap;
use std::env;
use std::io::{self, ErrorKind, PipeReader, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

/// Sway exports both variables for every `exec`. Each names that `exec`'s launch context.
pub(crate) const ACTIVATION_ENVIRONMENT_KEYS: [&str; 2] =
    ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"];

/// The supervisor writes at most two records, each one native-endian `i32` written whole:
/// the application's PID, then its raw wait status.
const STATUS_RECORD_BYTES: usize = size_of::<i32>();

unsafe extern "C" {
    /// glibc 2.34+: `fork` without atfork handlers. Unlike `fork`, POSIX.1-2024 lists it as
    /// async-signal-safe, which the forked child of a multithreaded process requires.
    fn _Fork() -> libc::pid_t;
}

/// Builds the application's command without spawning it. Arguments stay separate words;
/// the environment is inherited without activation variables, then overridden by `env`.
pub fn application_command(
    argv: &[String],
    cwd: Option<&PathBuf>,
    environment: &BTreeMap<String, String>,
) -> Result<Command, String> {
    let parent_cwd =
        env::current_dir().map_err(|error| format!("read current directory: {error}"))?;
    let effective_cwd = cwd.map_or_else(|| parent_cwd.clone(), |cwd| parent_cwd.join(cwd));
    // Resolve explicit relative executables ourselves: Command's relative-program plus
    // current_dir behavior is platform dependent. Bare names retain child PATH lookup.
    let executable = if argv[0].contains('/') {
        effective_cwd.join(&argv[0])
    } else {
        PathBuf::from(&argv[0])
    };

    let mut command = Command::new(executable);
    command.args(&argv[1..]).current_dir(effective_cwd);
    for activation_key in ACTIVATION_ENVIRONMENT_KEYS {
        command.env_remove(activation_key);
    }
    command
        .envs(environment)
        // A terminal interrupt directed at the launcher must not also kill the app.
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

enum Supervision {
    /// The supervisor may still report an exit status.
    Listening,
    Exited(ExitStatus),
    /// The supervisor stopped without a status: its time limit passed, it could not
    /// supervise, or it was killed. The application may still be running.
    EndedWithoutStatus,
}

/// A started application. Dropping it closes the status pipe, which ends supervision.
pub struct LaunchedApplication {
    status_reader: PipeReader,
    supervision: Supervision,
}

impl LaunchedApplication {
    /// Starts `command` detached from sway-stage, supervised for at most
    /// `supervision_limit`, and records the launch as spawned. An error means that no
    /// application started.
    pub fn start(
        mut command: Command,
        supervision_limit: Duration,
        diagnostics: &mut Diagnostics,
    ) -> Result<Self, String> {
        let (mut status_reader, status_writer) =
            io::pipe().map_err(|error| format!("create launch status pipe: {error}"))?;
        let status_fd = status_writer.as_raw_fd();
        // Rust's runtime keeps descriptors 0 through 2 open, so a new pipe never uses
        // them, where the child's null stdio would replace it.
        if status_fd <= libc::STDERR_FILENO {
            return Err(format!(
                "launch status pipe received standard descriptor {status_fd}"
            ));
        }
        let supervision_limit_ms =
            libc::c_int::try_from(supervision_limit.as_millis()).unwrap_or(libc::c_int::MAX);

        // SAFETY: std runs the closure only in its forked child, which is what
        // `detach_and_supervise` requires, and it uses only async-signal-safe functions.
        unsafe {
            command.pre_exec(move || detach_and_supervise(status_fd, supervision_limit_ms));
        }
        let spawn_result = command.spawn();
        // The supervisor must hold the only write end, so that its exit is seen as EOF.
        drop(status_writer);
        // spawn returns once the application has exec'd, or with the error that prevented
        // it, because the application holds std's error pipe until its exec.
        let mut intermediate = spawn_result.map_err(|error| {
            format!("spawn application: {error}; check executable, child PATH and cwd")
        })?;

        // The supervisor wrote the PID before closing its copy of std's error pipe, so it
        // is already readable unless the supervisor was killed.
        let application_pid = match read_status_record(&mut status_reader) {
            Ok(pid) => u32::try_from(pid).ok(),
            Err(_) => None,
        };
        diagnostics.spawned(application_pid);

        // The intermediate exited at once; reap it so that no zombie remains.
        if let Err(error) = intermediate.wait() {
            diagnostics.record(&format!("reap launch intermediate: {error}"));
        }

        Ok(Self {
            status_reader,
            supervision: Supervision::Listening,
        })
    }

    /// The application's exit status, once the supervisor has reported it.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        match self.supervision {
            Supervision::Exited(status) => Some(status),
            Supervision::Listening | Supervision::EndedWithoutStatus => None,
        }
    }

    /// Records the application's exit, or the end of its supervision, without waiting.
    pub fn record_exit_if_reported(&mut self, diagnostics: &mut Diagnostics) -> Result<(), String> {
        if !matches!(self.supervision, Supervision::Listening) {
            return Ok(());
        }

        let mut status_pipe = libc::pollfd {
            fd: self.status_reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd and a zero timeout; poll only inspects the descriptor.
        let ready = unsafe { libc::poll(&mut status_pipe, 1, 0) };
        if ready < 0 {
            return Err(format!(
                "inspect launch status pipe: {}",
                io::Error::last_os_error()
            ));
        }
        if ready == 0 {
            return Ok(());
        }

        // The status is written whole and last, so a readable pipe has it or is at EOF.
        match read_status_record(&mut self.status_reader) {
            Ok(wait_status) => {
                let status = ExitStatus::from_raw(wait_status);
                self.supervision = Supervision::Exited(status);
                diagnostics.record(&format!(
                    "launch process exited: {status}; continuing window observation (delegation is allowed)"
                ));
            }
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
                self.supervision = Supervision::EndedWithoutStatus;
                diagnostics.record(
                    "launch supervision ended without an exit status; continuing window observation",
                );
            }
            Err(error) => return Err(format!("read launch status: {error}")),
        }

        Ok(())
    }
}

fn read_status_record(status_reader: &mut PipeReader) -> io::Result<i32> {
    let mut record = [0; STATUS_RECORD_BYTES];
    status_reader.read_exact(&mut record)?;
    Ok(i32::from_ne_bytes(record))
}

/// Runs in the intermediate process that std forked for `Command::spawn`, and returns only
/// in the application process, which std then execs. An error returned from the
/// intermediate or the supervisor reaches `spawn` through std's error pipe.
///
/// Every process here is a fork of multithreaded sway-stage, so this code may use only
/// async-signal-safe functions: no allocation, locking, formatting, or panics.
///
/// # Safety
///
/// Call only from a `pre_exec` closure. Anywhere else, it would exit the calling process.
unsafe fn detach_and_supervise(
    status_fd: RawFd,
    supervision_limit_ms: libc::c_int,
) -> io::Result<()> {
    // SAFETY (whole function): async-signal-safe libc calls on plain values.
    let intermediate_pid = unsafe { libc::getpid() };
    match unsafe { _Fork() } {
        -1 => return Err(io::Error::last_os_error()),
        0 => {}
        _supervisor_pid => unsafe { libc::_exit(0) },
    }

    // The supervisor. Its parent is exiting; once reparented, nothing created from here on
    // descends from sway-stage, or from the process Sway's exec started.
    while unsafe { libc::getppid() } == intermediate_pid {
        unsafe { libc::sched_yield() };
    }

    let application_pid = match unsafe { _Fork() } {
        -1 => return Err(io::Error::last_os_error()),
        0 => return Ok(()),
        application_pid => application_pid,
    };
    unsafe { supervise(application_pid, status_fd, supervision_limit_ms) }
}

/// The supervisor's whole remaining life. It never returns to std's spawn code.
///
/// # Safety
///
/// Call only in the supervisor forked by `detach_and_supervise`.
unsafe fn supervise(
    application_pid: libc::pid_t,
    status_fd: RawFd,
    supervision_limit_ms: libc::c_int,
) -> ! {
    // SAFETY (whole function): async-signal-safe libc calls on plain values.
    write_status_record(status_fd, application_pid);

    // Every inherited descriptor above standard I/O goes except the status pipe's write
    // end: std's error pipe, so that spawn can return; the status pipe's read end, so that
    // sway-stage's exit is seen; and sway-stage's own files and sockets.
    let first_descriptor = (libc::STDERR_FILENO + 1) as libc::c_uint;
    let status_descriptor = status_fd as libc::c_uint;
    let closed_below = status_descriptor == first_descriptor
        || unsafe { libc::close_range(first_descriptor, status_descriptor - 1, 0) } == 0;
    let closed_above =
        unsafe { libc::close_range(status_descriptor + 1, libc::c_uint::MAX, 0) } == 0;
    if !closed_below || !closed_above {
        unsafe { libc::_exit(0) };
    }

    // Only this supervisor can reap the application, so its PID cannot have been reused
    // and the pidfd refers to the application, even if it has already exited.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, application_pid, 0) };
    if pidfd < 0 {
        unsafe { libc::_exit(0) };
    }

    // The application's exit makes its pidfd readable. sway-stage's closing of the read
    // end, by drop or by its own exit, raises POLLERR on the write end.
    let mut watched = [
        libc::pollfd {
            fd: pidfd as RawFd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: status_fd,
            events: 0,
            revents: 0,
        },
    ];
    let ready = unsafe { libc::poll(watched.as_mut_ptr(), 2, supervision_limit_ms) };
    let application_exited = watched[0].revents & libc::POLLIN != 0;
    let still_listening = watched[1].revents == 0;
    if ready > 0 && application_exited && still_listening {
        let mut wait_status = 0;
        if unsafe { libc::waitpid(application_pid, &mut wait_status, 0) } == application_pid {
            write_status_record(status_fd, wait_status);
        }
    }

    // A timeout, an interruption, or sway-stage's exit leaves a running application to be
    // reparented and reaped there.
    unsafe { libc::_exit(0) }
}

/// One whole record, never split: it is smaller than PIPE_BUF. A failed write only leaves
/// sway-stage without the record, which it reports.
fn write_status_record(status_fd: RawFd, value: i32) {
    let record = value.to_ne_bytes();
    // SAFETY: writes from a valid local buffer of its own length.
    unsafe { libc::write(status_fd, record.as_ptr().cast(), record.len()) };
}

#[cfg(test)]
#[path = "application_process_tests.rs"]
mod tests;
