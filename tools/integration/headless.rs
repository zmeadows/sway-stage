//! A private, disposable headless Sway compositor for integration checks.
//!
//! Isolation is established by construction, then verified before anything runs against
//! the compositor:
//!
//! - The compositor and every program run against it receive an environment built from
//!   nothing. Only `PATH` comes from the launcher, so the real desktop's Wayland display,
//!   X11 display, Sway socket, and D-Bus session bus cannot be inherited.
//! - Runtime, home, XDG, and temporary directories live in one exclusively created,
//!   owner-only session directory. Sway binds the IPC socket path named there, and its
//!   Wayland socket appears in the private runtime directory.
//! - Readiness requires those private sockets and exactly the session's configured
//!   `HEADLESS-N` outputs, active and at their configured sizes. Any other output name would
//!   mean the wrong compositor.
//!
//! Only the session's own compositor, a direct child, is ever signalled. Its Wayland
//! clients exit with it and are observed, never signalled. A failure preserves the session
//! directory, including the compositor log, for diagnosis.

use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt};
use std::path::{self, Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::ipc::{IPC_TIMEOUT, Output, Rect, Swaymsg, TreeNode};
use crate::process;

pub const OUTPUT_NAME: &str = "HEADLESS-1";
/// The first virtual output's logical area, at scale 1.
pub const VIRTUAL_OUTPUT_RECT: Rect = Rect {
    x: 0,
    y: 0,
    width: 2560,
    height: 1440,
};
/// Only the two-output session has this output: right of the first, and smaller.
pub const SECOND_OUTPUT_NAME: &str = "HEADLESS-2";
pub const SECOND_OUTPUT_RECT: Rect = Rect {
    x: 2560,
    y: 0,
    width: 1920,
    height: 1080,
};
/// The two-output session's Sway rules put these initial workspaces on the two outputs...
pub const FIRST_OUTPUT_INITIAL_WORKSPACE: &str = "1";
pub const SECOND_OUTPUT_INITIAL_WORKSPACE: &str = "2";
/// ...and create this one there when it is first selected.
pub const SECOND_OUTPUT_ASSIGNED_WORKSPACE: &str = "3";
/// The alternating profile's created targets. It selects each from a workspace on the other
/// output, where Sway would otherwise create it, so each needs its own rule.
pub const ALTERNATING_FIRST_OUTPUT_WORKSPACE: &str = "6";
pub const ALTERNATING_SECOND_OUTPUT_WORKSPACE: &str = "7";
pub const ALTERNATING_LAST_FIRST_OUTPUT_WORKSPACE: &str = "8";
/// The two-output session's `workspace NAME output OUTPUT` rules. Sway creates any other
/// workspace on the focused output.
pub const TWO_OUTPUT_WORKSPACE_RULES: [(&str, &str); 6] = [
    (FIRST_OUTPUT_INITIAL_WORKSPACE, OUTPUT_NAME),
    (SECOND_OUTPUT_INITIAL_WORKSPACE, SECOND_OUTPUT_NAME),
    (SECOND_OUTPUT_ASSIGNED_WORKSPACE, SECOND_OUTPUT_NAME),
    (ALTERNATING_FIRST_OUTPUT_WORKSPACE, OUTPUT_NAME),
    (ALTERNATING_SECOND_OUTPUT_WORKSPACE, SECOND_OUTPUT_NAME),
    (ALTERNATING_LAST_FIRST_OUTPUT_WORKSPACE, OUTPUT_NAME),
];

/// The virtual outputs of one session's compositor. Every scenario runs with one output,
/// except the dedicated two-output profile scenarios, which need a second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputSetup {
    OneOutput,
    TwoOutputs,
}

impl OutputSetup {
    /// Each output's name and logical area, in name order.
    pub fn outputs(self) -> &'static [(&'static str, Rect)] {
        match self {
            Self::OneOutput => &[(OUTPUT_NAME, VIRTUAL_OUTPUT_RECT)],
            Self::TwoOutputs => &[
                (OUTPUT_NAME, VIRTUAL_OUTPUT_RECT),
                (SECOND_OUTPUT_NAME, SECOND_OUTPUT_RECT),
            ],
        }
    }

    /// Keeps a two-output session's directory distinct from the one-output session's,
    /// which a single run may also create.
    fn directory_label(self) -> &'static str {
        match self {
            Self::OneOutput => "",
            Self::TwoOutputs => "two-outputs-",
        }
    }
}

/// Every session directory's name starts with this; `is_session_ipc_socket` relies on it.
const SESSION_DIRECTORY_PREFIX: &str = "sway-stage-headless-";
const RUNTIME_DIRECTORY_NAME: &str = "runtime";
const IPC_SOCKET_NAME: &str = "sway-ipc.sock";

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Clients normally exit as soon as their compositor connection closes.
const CLIENT_EXIT_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// `sockaddr_un.sun_path` holds 108 bytes including the terminating NUL. Sway copies a
/// longer `SWAYSOCK` with `strncpy`, silently binding a truncated path instead.
const MAX_SOCKET_PATH_BYTES: usize = 107;

/// Variable names and values for a child that inherits nothing else.
type Environment = Vec<(&'static str, OsString)>;

/// The owner-only directory holding everything one session creates. It is removed only
/// after verified success; any failure leaves it, including the compositor log.
pub struct SessionDirectory {
    root: PathBuf,
}

impl SessionDirectory {
    /// Creates a new directory under the temporary directory. Creation is exclusive, so
    /// the IPC socket path cannot exist yet: Sway binds `SWAYSOCK` only when nothing is
    /// there, and would otherwise silently choose a different path.
    pub fn create(setup: OutputSetup) -> Result<Self, String> {
        let parent = path::absolute(env::temp_dir())
            .map_err(|error| format!("temporary directory: {error}"))?;
        let started_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("system clock: {error}"))?
            .as_secs();
        let directory = Self {
            root: parent.join(format!(
                "{SESSION_DIRECTORY_PREFIX}{}{started_seconds}-{}",
                setup.directory_label(),
                std::process::id()
            )),
        };
        directory.require_socket_paths_fit()?;

        create_private_directory(&directory.root)?;
        for private_directory in [directory.runtime(), directory.home(), directory.temporary()] {
            create_private_directory(&private_directory)?;
        }
        Ok(directory)
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Only after the session stopped cleanly and every check passed.
    pub fn remove(self) -> Result<(), String> {
        fs::remove_dir_all(&self.root)
            .map_err(|error| format!("remove session directory {}: {error}", self.root.display()))
    }

    /// `XDG_RUNTIME_DIR`, holding the IPC and Wayland sockets.
    fn runtime(&self) -> PathBuf {
        self.root.join(RUNTIME_DIRECTORY_NAME)
    }

    /// `HOME` and every XDG base directory, so the real Foot configuration and sway-stage
    /// launch log are neither read nor replaced.
    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// `TMPDIR`.
    fn temporary(&self) -> PathBuf {
        self.root.join("tmp")
    }

    fn ipc_socket(&self) -> PathBuf {
        self.runtime().join(IPC_SOCKET_NAME)
    }

    fn configuration(&self) -> PathBuf {
        self.root.join("sway.config")
    }

    fn compositor_log(&self) -> PathBuf {
        self.root.join("sway.log")
    }

    /// The last tree observed before shutdown; after a failure, the only remaining record
    /// of the invisible compositor's windows.
    fn final_tree(&self) -> PathBuf {
        self.root.join("final-tree.json")
    }

    /// The IPC socket path is longer than any `wayland-N` path Sway chooses beside it.
    fn require_socket_paths_fit(&self) -> Result<(), String> {
        let socket_path_bytes = self.ipc_socket().as_os_str().len();
        if socket_path_bytes > MAX_SOCKET_PATH_BYTES {
            return Err(format!(
                "the session's IPC socket path would be {socket_path_bytes} bytes, over the \
                 {MAX_SOCKET_PATH_BYTES}-byte Unix socket limit; set TMPDIR to a shorter directory"
            ));
        }
        Ok(())
    }
}

/// A running private compositor whose sockets and virtual output have been verified.
pub struct HeadlessSession {
    /// Our direct child. `Drop` stops it if `stop` was never reached.
    compositor: Child,
    compositor_log: PathBuf,
    final_tree: PathBuf,
    swaymsg_executable: PathBuf,
    ipc_socket: PathBuf,
    sway: Swaymsg,
    wayland_display: OsString,
    client_environment: Environment,
}

impl HeadlessSession {
    /// Starts the compositor and waits until it is verifiably ready. On failure, the
    /// compositor has already been stopped.
    pub fn start(directory: &SessionDirectory, setup: OutputSetup) -> Result<Self, String> {
        let sway_executable = process::find_executable("sway")?;
        let swaymsg_executable = process::find_executable("swaymsg")?;
        let setpriv_executable = process::find_executable("setpriv")?;
        let search_path = env::var_os("PATH").unwrap_or_default();

        let configuration_path = directory.configuration();
        create_private_file(&configuration_path)?
            .write_all(sway_configuration(setup).as_bytes())
            .map_err(|error| format!("write {}: {error}", configuration_path.display()))?;
        let log = create_private_file(&directory.compositor_log())?;

        // If this process dies without `stop`, for example from SIGKILL or a closed
        // terminal, the kernel kills the compositor; in its own process group it would
        // otherwise run forever. The death signal follows the spawning thread, so the
        // compositor must be started from the main thread.
        let mut command = Command::new(setpriv_executable);
        command
            .args(["--pdeathsig", "KILL", "--"])
            .arg(sway_executable)
            .arg("--config")
            .arg(&configuration_path)
            .current_dir(&directory.root)
            .env_clear()
            .envs(compositor_environment(directory, &search_path, setup));
        let mut compositor = process::spawn_with_log(command, "start headless Sway", &log)?;

        let (sway, wayland_display) = match wait_until_ready(directory, setup, &mut compositor) {
            Ok(ready) => ready,
            Err(error) => {
                // Never leave a half-started compositor behind. Its log stays for diagnosis.
                if let Err(stop_error) = process::kill_and_reap(&mut compositor, "headless Sway") {
                    return Err(format!("{error}; {stop_error}"));
                }
                return Err(error);
            }
        };

        let client_environment = client_environment(directory, &search_path, &wayland_display);
        Ok(Self {
            compositor,
            compositor_log: directory.compositor_log(),
            final_tree: directory.final_tree(),
            swaymsg_executable,
            ipc_socket: directory.ipc_socket(),
            sway,
            wayland_display,
            client_environment,
        })
    }

    pub fn compositor_pid(&self) -> u32 {
        self.compositor.id()
    }

    pub fn wayland_display(&self) -> &OsStr {
        &self.wayland_display
    }

    /// The only way to run a program against this compositor. The session's environment
    /// replaces the launcher's entirely, so nothing from the real desktop leaks through.
    pub fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command.env_clear().envs(
            self.client_environment
                .iter()
                .map(|(name, value)| (*name, value)),
        );
        command
    }

    /// Stops the compositor on every outcome, then observes, without signalling them,
    /// that its clients exited with it. Returns every problem found along the way.
    pub fn stop(mut self) -> Result<(), String> {
        let mut problems = Vec::new();

        // Record the clients first: once the compositor is gone, nothing can name them. The
        // same reply is kept as the final tree. After an interrupt no new query may start,
        // so neither is recorded and the clients go unverified.
        let client_pids = if process::interruption_requested() {
            BTreeSet::new()
        } else {
            let recorded = self
                .sway
                .get_tree_json()
                .and_then(|tree_json| record_final_tree(&tree_json, &self.final_tree));
            match recorded {
                Ok(client_pids) => client_pids,
                Err(error) => {
                    problems.push(format!("record session clients: {error}"));
                    BTreeSet::new()
                }
            }
        };

        if let Err(error) = self.stop_compositor() {
            problems.push(error);
        }
        if let Err(error) = wait_for_clients_to_exit(&client_pids) {
            problems.push(error);
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    /// Gracefully through the private socket unless interrupted; otherwise, or after the
    /// deadline, SIGKILL to our direct child.
    fn stop_compositor(&mut self) -> Result<(), String> {
        let already_exited = self
            .compositor
            .try_wait()
            .map_err(|error| format!("wait for headless Sway: {error}"))?;
        if let Some(status) = already_exited {
            return Err(format!(
                "headless Sway exited before shutdown ({status}); see {}",
                self.compositor_log.display()
            ));
        }

        if process::interruption_requested() {
            process::kill_and_reap(&mut self.compositor, "headless Sway")?;
            return Err(
                "interrupted; headless Sway was killed and client exit was not verified".into(),
            );
        }

        let exit_request = request_compositor_exit(&self.swaymsg_executable, &self.ipc_socket);
        if let Some(status) = wait_for_exit(&mut self.compositor, SHUTDOWN_TIMEOUT)? {
            if !status.success() {
                return Err(format!(
                    "headless Sway exited with {status}; see {}",
                    self.compositor_log.display()
                ));
            }
            return Ok(());
        }

        process::kill_and_reap(&mut self.compositor, "headless Sway")?;
        let mut message = format!(
            "headless Sway did not exit within {SHUTDOWN_TIMEOUT:?} of the exit request and was killed"
        );
        if let Err(error) = exit_request {
            message.push_str(&format!(" (exit request: {error})"));
        }
        Err(message)
    }
}

impl Drop for HeadlessSession {
    /// A panic or early return must not leave the compositor running. After `stop` has
    /// reaped it, `Child::kill` sends nothing.
    fn drop(&mut self) {
        let _ = process::kill_and_reap(&mut self.compositor, "headless Sway");
    }
}

/// No Xwayland, swaynag, bar, background, decorations, or startup applications.
/// `swaynag_command` comes first so that no later configuration error can start swaynag.
fn sway_configuration(setup: OutputSetup) -> String {
    let mut configuration = String::from(
        "\
swaynag_command -
xwayland disable
default_border none
default_floating_border none
focus_follows_mouse no
",
    );
    for (name, rect) in setup.outputs() {
        let Rect {
            x,
            y,
            width,
            height,
        } = rect;
        configuration.push_str(&format!(
            "output {name} mode --custom {width}x{height} position {x} {y}\n"
        ));
    }

    // Sway's own workspace rules, as a user would configure placement: each output starts
    // with its own workspace, and Sway creates each assigned one on its output.
    if setup == OutputSetup::TwoOutputs {
        for (workspace, output) in TWO_OUTPUT_WORKSPACE_RULES {
            configuration.push_str(&format!("workspace {workspace} output {output}\n"));
        }
        // The backend decides which output Sway enables, and so focuses, first. Sway defers
        // this selection until both are enabled; it hides neither initial workspace.
        configuration.push_str(&format!(
            "workspace --no-auto-back-and-forth {FIRST_OUTPUT_INITIAL_WORKSPACE}\n"
        ));
    }
    configuration
}

/// Variables shared by the compositor and its clients, built from nothing. Only `PATH`
/// comes from the launcher; nothing names the real desktop's Wayland display, X11
/// display, Sway socket, or D-Bus session bus.
fn base_environment(directory: &SessionDirectory, search_path: &OsStr) -> Environment {
    let home = directory.home();

    // Deliberately nonexistent: an explicit address stops D-Bus clients, such as
    // notify-send, from reaching the real session bus or autolaunching one.
    let mut session_bus_address = OsString::from("unix:path=");
    session_bus_address.push(directory.runtime().join("no-session-bus"));

    vec![
        ("PATH", search_path.to_owned()),
        // Foot requires a UTF-8 locale.
        ("LANG", "C.UTF-8".into()),
        ("HOME", home.clone().into()),
        ("XDG_CONFIG_HOME", home.join(".config").into()),
        ("XDG_CACHE_HOME", home.join(".cache").into()),
        ("XDG_DATA_HOME", home.join(".local/share").into()),
        ("XDG_STATE_HOME", home.join(".local/state").into()),
        ("XDG_RUNTIME_DIR", directory.runtime().into()),
        ("TMPDIR", directory.temporary().into()),
        ("DBUS_SESSION_BUS_ADDRESS", session_bus_address),
        // Sway binds this path because it does not exist yet; clients then connect to it.
        ("SWAYSOCK", directory.ipc_socket().into()),
    ]
}

/// The wlroots headless backend with the setup's virtual outputs and no input devices.
/// Renderer selection stays automatic, as in the proven manual setup.
fn compositor_environment(
    directory: &SessionDirectory,
    search_path: &OsStr,
    setup: OutputSetup,
) -> Environment {
    let mut environment = base_environment(directory, search_path);
    environment.extend([
        ("WLR_BACKENDS", OsString::from("headless")),
        (
            "WLR_HEADLESS_OUTPUTS",
            setup.outputs().len().to_string().into(),
        ),
        ("WLR_LIBINPUT_NO_DEVICES", "1".into()),
    ]);
    environment
}

/// Clients resolve `WAYLAND_DISPLAY` within the private runtime directory.
fn client_environment(
    directory: &SessionDirectory,
    search_path: &OsStr,
    wayland_display: &OsStr,
) -> Environment {
    let mut environment = base_environment(directory, search_path);
    environment.push(("WAYLAND_DISPLAY", wayland_display.to_owned()));
    environment
}

/// Polls until the private sockets and virtual output are verified, the compositor exits,
/// or the deadline passes. The last IPC query may outlast the deadline by `IPC_TIMEOUT`.
fn wait_until_ready(
    directory: &SessionDirectory,
    setup: OutputSetup,
    compositor: &mut Child,
) -> Result<(Swaymsg, OsString), String> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        process::check_interruption()?;
        let exited = compositor
            .try_wait()
            .map_err(|error| format!("wait for headless Sway: {error}"))?;
        if let Some(status) = exited {
            return Err(format!(
                "headless Sway exited during startup ({status}); see {}",
                directory.compositor_log().display()
            ));
        }

        let not_ready_reason = match observe_readiness(directory, setup) {
            Ok(ready) => return Ok(ready),
            Err(reason) => reason,
        };
        if Instant::now() >= deadline {
            return Err(format!(
                "headless Sway was not ready within {STARTUP_TIMEOUT:?}: {not_ready_reason}; see {}",
                directory.compositor_log().display()
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// One readiness check, entirely through paths this session named.
fn observe_readiness(
    directory: &SessionDirectory,
    setup: OutputSetup,
) -> Result<(Swaymsg, OsString), String> {
    let sway = Swaymsg::at_socket(directory.ipc_socket())?;
    verify_virtual_outputs(&sway.get_outputs()?, setup)?;
    let wayland_display = find_wayland_display(&directory.runtime())?;
    Ok((sway, wayland_display))
}

/// Exactly the setup's headless outputs. A real output name means the wrong compositor;
/// the backend's 1280x720 default means the configuration has not been applied. The
/// scenario runner repeats this check before it changes anything.
pub fn verify_virtual_outputs(outputs: &[Output], setup: OutputSetup) -> Result<(), String> {
    let mut observed_names: Vec<&str> = outputs.iter().map(|output| output.name.as_str()).collect();
    observed_names.sort_unstable();
    let expected_names: Vec<&str> = setup.outputs().iter().map(|(name, _)| *name).collect();
    if observed_names != expected_names {
        return Err(format!(
            "expected only outputs {expected_names:?}, found {observed_names:?}"
        ));
    }

    for (name, expected_rect) in setup.outputs() {
        let output = outputs
            .iter()
            .find(|output| output.name == *name)
            .expect("output names were compared above");
        if !output.active {
            return Err(format!("{name} is not active"));
        }
        if output.rect != *expected_rect {
            return Err(format!(
                "{name} has {:?}, expected {expected_rect:?}",
                output.rect
            ));
        }
        if output.current_workspace.is_none() {
            return Err(format!("{name} has no workspace yet"));
        }
    }
    Ok(())
}

/// Whether `socket` is shaped like a session directory's IPC socket path. The scenario
/// runner requires this of `SWAYSOCK` before its first connection, so a runner started by
/// hand with a desktop session's environment refuses without reaching that compositor.
pub fn is_session_ipc_socket(socket: &Path) -> bool {
    let runtime = socket.parent();
    let session_directory_name = runtime
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(OsStr::to_str);

    socket.is_absolute()
        && socket.file_name() == Some(OsStr::new(IPC_SOCKET_NAME))
        && runtime.and_then(Path::file_name) == Some(OsStr::new(RUNTIME_DIRECTORY_NAME))
        && session_directory_name.is_some_and(|name| name.starts_with(SESSION_DIRECTORY_PREFIX))
}

/// Sway chooses its own `wayland-N` name, so discover it rather than assume one.
fn find_wayland_display(runtime: &Path) -> Result<OsString, String> {
    let read_error = |error: std::io::Error| format!("read private runtime directory: {error}");
    let mut displays = Vec::new();
    for entry in fs::read_dir(runtime).map_err(read_error)? {
        let entry = entry.map_err(read_error)?;
        // `DirEntry::file_type` does not follow symlinks, so only a real socket counts.
        let file_type = entry.file_type().map_err(read_error)?;
        if is_wayland_socket_name(&entry.file_name()) && file_type.is_socket() {
            displays.push(entry.file_name());
        }
    }

    match displays.as_slice() {
        [display] => Ok(display.clone()),
        [] => Err("no Wayland socket in the private runtime directory yet".into()),
        _ => Err(format!(
            "expected one Wayland socket in the private runtime directory, found {displays:?}"
        )),
    }
}

/// `wayland-` followed by a display number, excluding its `.lock` file.
fn is_wayland_socket_name(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix("wayland-"))
        .is_some_and(|number| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// Sway may close the connection before replying to `exit`, so the reply proves nothing
/// either way; the caller waits for the compositor process itself. The command goes only
/// to this session's own socket, never to one named by the environment.
fn request_compositor_exit(swaymsg_executable: &Path, ipc_socket: &Path) -> Result<(), String> {
    let mut command = Command::new(swaymsg_executable);
    command
        .arg("-s")
        .arg(ipc_socket)
        .args(["-t", "command", "exit"]);
    process::run_with_status(command, "headless Sway exit request", IPC_TIMEOUT)?;
    Ok(())
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Result<Option<ExitStatus>, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = child
            .try_wait()
            .map_err(|error| format!("wait for headless Sway: {error}"))?;
        if status.is_some() || Instant::now() >= deadline {
            return Ok(status);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Saves the raw `get_tree` reply, then returns the session clients it names. Saving is
/// best-effort: a file that cannot be written is only reported, so it never fails a run,
/// hides the failure being diagnosed, or stops client verification and shutdown.
fn record_final_tree(tree_json: &str, final_tree_path: &Path) -> Result<BTreeSet<i32>, String> {
    let saved = create_private_file(final_tree_path).and_then(|mut file| {
        file.write_all(tree_json.as_bytes())
            .map_err(|error| format!("write {}: {error}", final_tree_path.display()))
    });
    if let Err(error) = saved {
        eprintln!("      final tree not saved: {error}");
    }

    let tree: TreeNode = serde_json::from_str(tree_json)
        .map_err(|error| format!("Sway get_tree: invalid reply: {error}"))?;
    Ok(session_client_pids(&tree))
}

/// Every client process Sway reports for a view, tiled or floating.
fn session_client_pids(tree: &TreeNode) -> BTreeSet<i32> {
    let mut pids = BTreeSet::new();
    let mut pending = vec![tree];
    while let Some(node) = pending.pop() {
        if let Some(pid) = node.pid.filter(|pid| *pid > 0) {
            pids.insert(pid);
        }
        pending.extend(node.nodes.iter().chain(&node.floating_nodes));
    }
    pids
}

/// Observes only: clients are never signalled, even when they outlive the compositor.
fn wait_for_clients_to_exit(client_pids: &BTreeSet<i32>) -> Result<(), String> {
    let deadline = Instant::now() + CLIENT_EXIT_TIMEOUT;
    loop {
        let mut running = Vec::new();
        for pid in client_pids {
            match fs::symlink_metadata(format!("/proc/{pid}")) {
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(format!("inspect session client {pid}: {error}")),
                Ok(_) => running.push(pid),
            }
        }

        if running.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "session clients {running:?} were still running {CLIENT_EXIT_TIMEOUT:?} after the \
                 compositor stopped; they were not signalled"
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|error| format!("create private directory {}: {error}", path.display()))
}

fn create_private_file(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("create {}: {error}", path.display()))
}

#[cfg(test)]
#[path = "headless_tests.rs"]
mod tests;
