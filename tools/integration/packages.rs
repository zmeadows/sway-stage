//! Real package fixtures: dynamic discovery and independent launch provenance. No code here
//! calls production inference. A retained proc descriptor, launch ancestry, and package
//! evidence must all agree; selectors and newly appearing windows never prove ownership.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use crate::ipc::{Shell, TreeNode, WindowChange, WindowEvent};
use crate::layout::{class, is_window};
use crate::observation::walk;
use crate::process;
use crate::scenarios::{ConfiguredNode, ConfiguredWindow, Scenario, Selector};

pub const SCENARIOS: [&str; 2] = ["appimage-inference", "flatpak-inference"];

#[derive(Clone, Debug)]
pub enum Package {
    AppImage(PathBuf),
    Flatpak {
        launcher: PathBuf,
        application: String,
    },
}

impl Package {
    /// Absence alone skips. Invalid configured packages are errors, never missing fixtures.
    pub fn configured(
        name: &str,
        image: Option<&Path>,
        application: Option<&str>,
    ) -> Result<Option<Self>, String> {
        match name {
            "appimage-inference" => image
                .map(normalize_appimage)
                .transpose()
                .map(|path| path.map(Self::AppImage)),
            "flatpak-inference" => application
                .map(|application| {
                    validate_flatpak_id(application)?;
                    Ok(Self::Flatpak {
                        launcher: process::find_executable("flatpak")?,
                        application: application.into(),
                    })
                })
                .transpose(),
            _ => Err("unknown package scenario".into()),
        }
    }

    pub fn require_available(&self) -> Result<(), String> {
        match self {
            Self::AppImage(path) => {
                normalize_appimage(path)?;
            }
            Self::Flatpak {
                launcher,
                application,
            } => {
                let mut command = Command::new(launcher);
                command.args(["info", "--show-ref", application]);
                let reference = process::run(
                    command,
                    "configured Flatpak must be installed",
                    crate::ipc::IPC_TIMEOUT,
                )?;
                let components: Vec<_> = reference.trim().split('/').collect();
                if components.len() != 4 || components[0] != "app" || components[1] != application {
                    return Err(
                        "installed Flatpak reference does not identify the configured application"
                            .into(),
                    );
                }
            }
        }
        Ok(())
    }

    pub fn command(&self) -> Command {
        match self {
            Self::AppImage(path) => Command::new(path),
            Self::Flatpak {
                launcher,
                application,
            } => {
                let mut command = Command::new(launcher);
                command.args(["run", application]);
                command
            }
        }
    }
}

/// Invoke the resolved image itself, so ARGV0 cannot select another binary through a
/// symlink alias. APPIMAGE should then independently report this same resolved host path.
pub fn normalize_appimage(path: &Path) -> Result<PathBuf, String> {
    let resolved = fs::canonicalize(path)
        .map_err(|error| format!("resolve AppImage {}: {error}", path.display()))?;
    let metadata = fs::metadata(&resolved).map_err(|error| format!("stat AppImage: {error}"))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.permissions().mode() & 0o111 == 0 {
        return Err("AppImage must be a nonempty executable file".into());
    }
    resolved.to_str().ok_or("AppImage path must be UTF-8")?;
    let mut header = [0; 11];
    File::open(&resolved)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|error| format!("read AppImage header: {error}"))?;
    if &header[..4] != b"\x7fELF" || &header[8..10] != b"AI" || ![1, 2].contains(&header[10]) {
        return Err(
            "configured executable lacks an AppImage runtime header (wrappers unsupported)".into(),
        );
    }
    Ok(resolved)
}

pub fn validate_flatpak_id(application: &str) -> Result<(), String> {
    // Flatpak IDs require at least three ASCII components and permit hyphens only in
    // the final component, including its first character. Keep this CLI check independent
    // of production inference; refs, options, and partial IDs are not accepted.
    let components: Vec<_> = application.split('.').collect();
    let has_invalid_component = components
        .iter()
        .enumerate()
        .any(|(component_index, part)| {
            let final_component = component_index + 1 == components.len();
            part.is_empty()
                || !part.starts_with(|c: char| {
                    c.is_ascii_alphabetic() || c == '_' || (final_component && c == '-')
                })
                || !part.bytes().all(|c| {
                    c.is_ascii_alphanumeric() || c == b'_' || (final_component && c == b'-')
                })
        });

    if application.len() > 255 || components.len() < 3 || has_invalid_component {
        return Err(
            "--flatpak requires a complete application ID, such as org.gnome.Calculator".into(),
        );
    }
    Ok(())
}

/// Returns a candidate only when global events and the tree agree on one new safe window.
/// Delayed PID/selector metadata may resolve until the bounded discovery deadline.
pub fn discover<'a>(
    tree: &'a TreeNode,
    events: &[WindowEvent],
    preexisting: &HashSet<i64>,
    target_id: i64,
) -> Result<Option<&'a TreeNode>, String> {
    let new_ids: Vec<_> = events
        .iter()
        .filter(|event| event.change == WindowChange::New)
        .map(|event| event.container.id)
        .collect();
    if new_ids.len() > 1 || new_ids.iter().any(|id| preexisting.contains(id)) {
        return Err(
            "multiple, repeated, or preexisting new-window events; leave windows untouched".into(),
        );
    }

    for event in events {
        match event.change {
            WindowChange::Title | WindowChange::Urgent | WindowChange::Mark => {}
            WindowChange::New | WindowChange::Focus if new_ids.contains(&event.container.id) => {}
            _ => return Err("unexpected window activity during package discovery".into()),
        }
    }

    let placed_nodes = walk(tree);
    let candidates: Vec<_> = placed_nodes
        .iter()
        .filter(|placed| is_window(placed.node) && !preexisting.contains(&placed.node.id))
        .collect();
    if candidates.len() > 1
        || candidates.iter().any(|placed| {
            placed
                .workspace
                .is_none_or(|workspace| workspace.id != target_id)
                || placed.unsafe_state
        })
    {
        return Err(
            "additional, foreign-workspace, floating, or fullscreen package candidate".into(),
        );
    }
    let Some(candidate) = candidates.first() else {
        return Ok(None);
    };
    if new_ids != [candidate.node.id] {
        return Err("new package window lacks exact fresh creation-event evidence".into());
    }
    if candidate.node.pid.is_none_or(|pid| pid <= 0) || selector(candidate.node).is_err() {
        return Ok(None);
    }
    Ok(Some(candidate.node))
}

pub fn selector(window: &TreeNode) -> Result<Selector, String> {
    match window.shell {
        Some(Shell::XdgShell) if window.window.is_none() => window
            .app_id
            .as_ref()
            .filter(|id| !id.is_empty())
            .cloned()
            .map(Selector::AppId),
        Some(Shell::Xwayland) if window.app_id.is_none() && window.window.is_some() => {
            class(window)
                .filter(|class| !class.is_empty())
                .map(|class| Selector::Class(class.into()))
        }
        _ => None,
    }
    .ok_or_else(|| "window lacks an unambiguous native app ID or Xwayland class".into())
}

/// Created only after discovery; never write guessed selectors into a loader configuration.
pub fn captured_scenario(name: &'static str, selector: Selector) -> Scenario {
    Scenario {
        name,
        layout: ConfiguredNode::Window(ConfiguredWindow {
            layout_id: "window1".into(),
            weight: 1,
            command: Vec::new(),
            selector,
            env: Vec::new(),
            cwd: None,
            requires_process_evidence: false,
            desktop_launcher: None,
        }),
        expected_focus: "window1",
        configured_focus: None,
        window_timeout_ms: 60_000,
        extension: None,
        skip: None,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
}

fn file_identity(path: &Path) -> Result<FileIdentity, String> {
    let metadata = fs::metadata(path).map_err(|_| "cannot stat package/process file")?;
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.len(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
    })
}

/// Retaining /proc/PID prevents later reads from following a reused numeric PID.
struct ProcessHandle {
    directory: File,
    pid: i32,
    start: u64,
}

impl ProcessHandle {
    fn open(pid: i32) -> Result<Self, String> {
        if pid <= 0 {
            return Err("invalid process PID".into());
        }
        let directory =
            File::open(format!("/proc/{pid}")).map_err(|_| "cannot retain package process")?;
        let mut handle = Self {
            directory,
            pid,
            start: 0,
        };
        handle.start = handle.stat()?.1;
        Ok(handle)
    }

    fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
    }

    fn stat(&self) -> Result<(i32, u64), String> {
        parse_stat(&read_text(&self.path().join("stat"))?, self.pid)
    }

    fn require_present(&self) -> Result<(), String> {
        if self.stat()?.1 != self.start {
            return Err("package process identity changed".into());
        }
        Ok(())
    }
}

fn parse_stat(stat: &str, pid: i32) -> Result<(i32, u64), String> {
    let (prefix, tail) = stat
        .rsplit_once(") ")
        .ok_or("malformed package process stat")?;
    if !prefix.starts_with(&format!("{pid} (")) {
        return Err("process PID changed".into());
    }
    let fields: Vec<_> = tail.split_whitespace().collect();
    if fields.len() < 20 || matches!(fields[0], "Z" | "X" | "x") {
        return Err("package process disappeared or exited".into());
    }
    Ok((
        fields[1].parse().map_err(|_| "invalid parent PID")?,
        fields[19]
            .parse()
            .map_err(|_| "invalid process start time")?,
    ))
}

/// Evidence for one direct package launch. The caller keeps its returned Child unreaped
/// while verifying ancestry, so the root PID cannot be recycled. Neither value kills on
/// drop; failure must leave the application alone.
pub struct Launch {
    root: ProcessHandle,
    package: Package,
    image_identity: Option<FileIdentity>,
}

impl Launch {
    pub fn start(package: Package) -> Result<(Child, Self), String> {
        let image_identity = match &package {
            Package::AppImage(path) => Some(file_identity(path)?),
            Package::Flatpak { .. } => None,
        };
        let child = process::spawn_application(package.command())?;
        let root = ProcessHandle::open(child.id() as i32).map_err(|reason| {
            format!(
                "{reason}; package launch PID={} retained without termination",
                child.id()
            )
        })?;
        Ok((
            child,
            Self {
                root,
                package,
                image_identity,
            },
        ))
    }

    pub fn accept(&self, window: &TreeNode) -> Result<Ownership, String> {
        let process = ProcessHandle::open(window.pid.ok_or("package window lacks PID")?)?;
        let executable = fs::read_link(process.path().join("exe"))
            .map_err(|_| "cannot read package executable")?;
        let executable_identity = file_identity(&process.path().join("exe"))?;
        let mut ownership = Ownership {
            process,
            executable,
            executable_identity,
            container_id: window.id,
            selector: selector(window)?,
            evidence: None,
            expected_command: Vec::new(),
        };
        let (evidence, command) = ownership.read_package(self)?;
        ownership.evidence = Some(evidence);
        ownership.expected_command = command;
        ownership.verify(self, window)?;
        Ok(ownership)
    }
}

#[derive(PartialEq, Eq)]
enum PackageEvidence {
    AppImage {
        image: String,
        appdir: PathBuf,
        mount: String,
    },
    Flatpak {
        metadata: String,
        namespace: PathBuf,
    },
}

pub struct Ownership {
    process: ProcessHandle,
    executable: PathBuf,
    executable_identity: FileIdentity,
    container_id: i64,
    selector: Selector,
    evidence: Option<PackageEvidence>,
    pub expected_command: Vec<String>,
}

impl Ownership {
    pub fn verify(&self, launch: &Launch, window: &TreeNode) -> Result<(), String> {
        if window.id != self.container_id
            || window.pid != Some(self.process.pid)
            || selector(window)? != self.selector
        {
            return Err(
                "package window container/process/selector changed; refusing cleanup".into(),
            );
        }
        self.require_process_identity()?;
        require_ancestry(&self.process, &launch.root)?;
        let (evidence, command) = self.read_package(launch)?;
        if self.evidence.as_ref() != Some(&evidence) || command != self.expected_command {
            return Err("package provenance changed; refusing cleanup".into());
        }
        require_ancestry(&self.process, &launch.root)?;
        self.require_process_identity()
    }

    fn require_process_identity(&self) -> Result<(), String> {
        self.process.require_present()?;
        let executable_link = self.process.path().join("exe");
        if fs::read_link(&executable_link).ok().as_ref() != Some(&self.executable)
            || file_identity(&executable_link)? != self.executable_identity
        {
            return Err("package executable changed; refusing cleanup".into());
        }
        Ok(())
    }

    fn read_package(&self, launch: &Launch) -> Result<(PackageEvidence, Vec<String>), String> {
        let proc_path = self.process.path();
        match &launch.package {
            Package::AppImage(image) => {
                if Some(file_identity(image)?) != launch.image_identity {
                    return Err("configured AppImage file changed".into());
                }
                let environment = read_bytes(&proc_path.join("environ"))?;
                let image_text = image.to_str().ok_or("non-UTF-8 AppImage path")?;
                let observed_image = environment_field(&environment, "APPIMAGE")?;
                let appdir = PathBuf::from(environment_field(&environment, "APPDIR")?);
                if observed_image != image_text || !appdir.is_absolute() {
                    return Err("APPIMAGE/APPDIR does not identify the configured image".into());
                }
                let appdir = fs::canonicalize(appdir).map_err(|_| "cannot resolve APPDIR mount")?;
                let mounts = read_text(&proc_path.join("mountinfo"))?;
                let mount = validate_image_mount(image, &appdir, &self.executable, &mounts)?;
                if file_identity(&appdir)?.device != self.executable_identity.device {
                    return Err(
                        "AppImage executable is on a different filesystem than APPDIR".into(),
                    );
                }
                // ARGV0 is optional on older runtimes; if supplied it must describe our
                // canonical invocation, rather than an alias with different AppRun behavior.
                if environment_has(&environment, "ARGV0")
                    && environment_field(&environment, "ARGV0")? != image_text
                {
                    return Err("AppImage ARGV0 differs from the direct resolved invocation".into());
                }
                Ok((
                    PackageEvidence::AppImage {
                        image: observed_image,
                        appdir,
                        mount,
                    },
                    vec![image_text.into()],
                ))
            }
            Package::Flatpak {
                launcher,
                application,
            } => {
                let metadata = read_text(&proc_path.join("root/.flatpak-info"))?;
                let fields = flatpak_fields(&metadata)?;
                let mut command = Command::new(launcher);
                command.args([
                    "ps",
                    "--columns=instance,application,arch,branch,pid,child-pid",
                ]);
                let instances = process::run(
                    command,
                    "read Flatpak running instances",
                    crate::ipc::IPC_TIMEOUT,
                )?;
                let instance = verify_flatpak_instance(application, &fields, &instances)?;
                let sandbox = ProcessHandle::open(instance.sandbox_pid)?;
                require_ancestry(&sandbox, &launch.root)?;
                let namespace = fs::read_link(proc_path.join("ns/mnt"))
                    .map_err(|_| "cannot inspect Flatpak window mount namespace")?;
                let sandbox_namespace = fs::read_link(sandbox.path().join("ns/mnt"))
                    .map_err(|_| "cannot inspect Flatpak instance mount namespace")?;
                if sandbox_namespace != namespace
                    || read_text(&sandbox.path().join("root/.flatpak-info"))? != metadata
                {
                    return Err("window process does not belong to the independently observed Flatpak instance".into());
                }
                sandbox.require_present()?;
                let host_namespace = fs::read_link("/proc/self/ns/mnt")
                    .map_err(|_| "cannot inspect harness mount namespace")?;
                if host_namespace == namespace {
                    return Err("Flatpak window is outside a separate mount namespace".into());
                }
                let relative_executable = self
                    .executable
                    .strip_prefix("/")
                    .map_err(|_| "Flatpak executable is not absolute")?;
                if file_identity(&proc_path.join("root").join(relative_executable))?
                    != self.executable_identity
                {
                    return Err("Flatpak executable does not belong to the observed sandbox".into());
                }
                Ok((
                    PackageEvidence::Flatpak {
                        metadata,
                        namespace,
                    },
                    instance.expected_command,
                ))
            }
        }
    }
}

fn require_ancestry(process: &ProcessHandle, root: &ProcessHandle) -> Result<(), String> {
    root.require_present()?;
    process.require_present()?;
    let mut current_pid = process.pid;
    let mut chain: Vec<(ProcessHandle, i32)> = Vec::new();
    for _ in 0..128 {
        if current_pid == root.pid {
            // Revalidate every observed edge. Reparenting or disappearance loses proof.
            for (handle, observed_parent) in chain {
                handle.require_present()?;
                if handle.stat()?.0 != observed_parent {
                    return Err("package ancestry changed".into());
                }
            }
            return root.require_present();
        }
        let ancestor = ProcessHandle::open(current_pid)?;
        if ancestor.start < root.start {
            break;
        }
        let (parent, _) = ancestor.stat()?;
        if parent <= 1 || parent == current_pid {
            break;
        }
        chain.push((ancestor, parent));
        current_pid = parent;
    }
    Err("window process is not independently descended from this launch; singleton/delegation unsupported".into())
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, String> {
    const LIMIT: u64 = 1024 * 1024;
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(LIMIT + 1).read_to_end(&mut bytes))
        .map_err(|_| "cannot read required package/process evidence")?;
    if bytes.len() as u64 > LIMIT {
        return Err("package evidence exceeded read limit".into());
    }
    Ok(bytes)
}

fn read_text(path: &Path) -> Result<String, String> {
    String::from_utf8(read_bytes(path)?).map_err(|_| "package metadata is not UTF-8".into())
}

fn environment_has(environment: &[u8], key: &str) -> bool {
    environment
        .split(|byte| *byte == 0)
        .any(|entry| entry.starts_with(format!("{key}=").as_bytes()))
}

fn environment_field(environment: &[u8], key: &str) -> Result<String, String> {
    if environment.last() != Some(&0) {
        return Err("incomplete package environment".into());
    }
    let prefix = format!("{key}=");
    let entries: Vec<_> = environment
        .split(|byte| *byte == 0)
        .filter_map(|entry| entry.strip_prefix(prefix.as_bytes()))
        .collect();
    let [entry] = entries.as_slice() else {
        return Err("missing or duplicate package environment field".into());
    };
    let text = std::str::from_utf8(entry).map_err(|_| "non-UTF-8 package environment field")?;
    if text.is_empty() {
        return Err("empty package environment field".into());
    }
    Ok(text.into())
}

/// Retain only the unique APPDIR record. A mount covering the executable below APPDIR
/// makes membership ambiguous, even if it happens to expose the same device.
fn validate_image_mount(
    image: &Path,
    appdir: &Path,
    executable: &Path,
    mounts: &str,
) -> Result<String, String> {
    if image.starts_with(appdir)
        || executable == appdir
        || !executable.starts_with(appdir)
        || executable.components().any(|part| part.as_os_str() == "..")
    {
        return Err("AppImage executable is not within its independent APPDIR mount".into());
    }

    let mut image_mount = None;
    for line in mounts.lines() {
        // Parse every record before filtering: malformed evidence cannot safely establish
        // that a mount is unrelated to APPDIR or the executable.
        let (mount, filesystem) = line.split_once(" - ").ok_or("malformed mount evidence")?;
        let mount_fields: Vec<_> = mount.split_whitespace().collect();
        let filesystem_fields: Vec<_> = filesystem.split_whitespace().collect();
        if mount_fields.len() < 6 || filesystem_fields.len() != 3 {
            return Err("malformed mount evidence".into());
        }
        let device = mount_fields[2]
            .split_once(':')
            .ok_or("malformed mount device")?;
        for number in [mount_fields[0], mount_fields[1], device.0, device.1] {
            if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("malformed mount identity".into());
            }
        }
        let root = decode_mount_path(mount_fields[3])?;
        let mountpoint = decode_mount_path(mount_fields[4])?;
        if !root.is_absolute() || !mountpoint.is_absolute() {
            return Err("mount evidence contains a relative path".into());
        }

        if mountpoint == appdir {
            let filesystem_type = filesystem_fields[0];
            if !(filesystem_type.starts_with("fuse.")
                || ["squashfs", "iso9660"].contains(&filesystem_type))
            {
                return Err("APPDIR is not a supported image filesystem mount".into());
            }
            if image_mount.replace(line.to_owned()).is_some() {
                return Err("APPDIR lacks unique AppImage mount evidence".into());
            }
        } else if mountpoint.starts_with(appdir) && executable.starts_with(&mountpoint) {
            return Err("another mount obscures the AppImage executable".into());
        }
    }

    image_mount.ok_or_else(|| "APPDIR lacks unique AppImage mount evidence".into())
}

fn decode_mount_path(path: &str) -> Result<PathBuf, String> {
    let mut decoded = String::new();
    let mut characters = path.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        let escape: String = characters.by_ref().take(3).collect();
        decoded.push(match escape.as_str() {
            "040" => ' ',
            "011" => '\t',
            "012" => '\n',
            "134" => '\\',
            _ => return Err("malformed mount path escape".into()),
        });
    }
    Ok(PathBuf::from(decoded))
}

/// Read the running sandbox's key file only as corroboration. Expected argv comes from
/// the independently queried Flatpak instance table, not production's metadata parser.
fn flatpak_fields(metadata: &str) -> Result<BTreeMap<(String, String), String>, String> {
    let mut section = "";
    let mut fields = BTreeMap::new();
    let mut sections = HashSet::new();
    for line in metadata
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            if !sections.insert(name) {
                return Err("duplicate Flatpak metadata section".into());
            }
            section = name;
        } else {
            let (key, value) = line.split_once('=').ok_or("malformed Flatpak metadata")?;
            if section.is_empty()
                || fields
                    .insert((section.into(), key.trim().into()), value.trim().into())
                    .is_some()
            {
                return Err("ambiguous Flatpak metadata".into());
            }
        }
    }
    if sections.contains("Runtime") {
        return Err("runtime-only or contradictory Flatpak metadata".into());
    }
    Ok(fields)
}

struct FlatpakInstance {
    sandbox_pid: i32,
    expected_command: Vec<String>,
}

fn verify_flatpak_instance(
    application: &str,
    fields: &BTreeMap<(String, String), String>,
    instances: &str,
) -> Result<FlatpakInstance, String> {
    let field = |section: &str, key: &str| {
        fields
            .get(&(section.into(), key.into()))
            .map(String::as_str)
            .ok_or("missing Flatpak application/instance metadata")
    };
    let instance_id = field("Instance", "instance-id")?;
    let rows: Vec<Vec<_>> = instances
        .lines()
        .map(|line| line.split_whitespace().collect())
        .filter(|row: &Vec<&str>| row.first() == Some(&instance_id))
        .collect();
    let [row] = rows.as_slice() else {
        return Err("Flatpak instance absent or ambiguous".into());
    };
    let [
        _,
        observed_application,
        architecture,
        branch,
        wrapper_pid,
        sandbox_pid,
    ] = row.as_slice()
    else {
        return Err("Flatpak instance table lacks the six requested columns".into());
    };
    if *observed_application != application
        || field("Application", "name")? != application
        || *architecture != field("Instance", "arch")?
        || *branch != field("Instance", "branch")?
    {
        return Err(
            "Flatpak running instance contradicts configured application or sandbox metadata"
                .into(),
        );
    }
    let positive_pid = |text: &str| {
        text.parse::<i32>()
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or("Flatpak instance lacks a usable process PID")
    };
    positive_pid(wrapper_pid)?;
    let sandbox_pid = positive_pid(sandbox_pid)?;

    for component in [architecture, branch] {
        if !component.starts_with(|c: char| c.is_ascii_alphanumeric())
            || !component
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        {
            return Err("unusable Flatpak branch/architecture".into());
        }
    }
    let expected_command = vec![
        "flatpak".into(),
        "run".into(),
        format!("--branch={branch}"),
        format!("--arch={architecture}"),
        application.into(),
    ];
    Ok(FlatpakInstance {
        sandbox_pid,
        expected_command,
    })
}

#[cfg(test)]
#[path = "packages_tests.rs"]
mod tests;
