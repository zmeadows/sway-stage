//! Best-effort host launch commands from bounded process metadata reads.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

// A modest per-window bound; read one extra byte to detect truncation.
const MAX_CMDLINE_BYTES: usize = 64 * 1024;
const MAX_FLATPAK_INFO_BYTES: usize = 64 * 1024;
const MAX_ENVIRON_BYTES: usize = 64 * 1024;
const MAX_STAT_BYTES: usize = 4096;

/// An inaccessible or vanished process is an ordinary inference miss, never a capture
/// error. Neither I/O errors nor argument contents escape this boundary into diagnostics.
pub fn read_process_command(pid: i32) -> Option<Vec<String>> {
    if pid <= 0 {
        return None;
    }

    let process_directory = PathBuf::from(format!("/proc/{pid}"));
    with_stable_process(
        || read_process_identity(&process_directory, pid),
        || {
            let metadata = File::open(process_directory.join("root/.flatpak-info"));
            infer_process_command(metadata, || {
                let environment = File::open(process_directory.join("environ"));
                infer_host_command(
                    pid,
                    environment,
                    || std::fs::read_link(process_directory.join("exe")).ok(),
                    || {
                        let file = File::open(process_directory.join("cmdline")).ok()?;
                        read_command_line(file)
                    },
                )
            })
        },
    )
}

#[derive(Clone, PartialEq, Eq)]
struct ProcessIdentity {
    start_time_ticks: u64,
    executable: PathBuf,
    executable_device: u64,
    executable_inode: u64,
}

/// Bracket all inference reads, including Flatpak's, with the same process and executable
/// evidence. This detects observed replacement, not a stale PID already supplied by Sway,
/// same-executable execs, or changes made and undone between observations.
fn with_stable_process(
    mut read_identity: impl FnMut() -> Option<ProcessIdentity>,
    infer_command: impl FnOnce() -> Option<Vec<String>>,
) -> Option<Vec<String>> {
    let before = read_identity()?;
    let command = infer_command()?;
    let after = read_identity()?;
    (before == after).then_some(command)
}

fn read_process_identity(process_directory: &Path, pid: i32) -> Option<ProcessIdentity> {
    let start_time_ticks = read_start_time(File::open(process_directory.join("stat")).ok()?, pid)?;
    let executable = std::fs::read_link(process_directory.join("exe")).ok()?;
    let metadata = std::fs::metadata(process_directory.join("exe")).ok()?;
    Some(ProcessIdentity {
        start_time_ticks,
        executable,
        executable_device: metadata.dev(),
        executable_inode: metadata.ino(),
    })
}

fn read_start_time(reader: impl Read, pid: i32) -> Option<u64> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_STAT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if pid <= 0 || bytes.len() > MAX_STAT_BYTES || bytes.last() != Some(&b'\n') {
        return None;
    }

    // comm is parenthesized but can itself contain spaces, parentheses, newlines, and
    // non-UTF-8 bytes. Only the numeric prefix and the fields AFTER its final ')' are text.
    let pid_end = bytes.iter().position(|byte| *byte == b' ')?;
    if bytes[..pid_end] != *pid.to_string().as_bytes() || bytes.get(pid_end + 1) != Some(&b'(') {
        return None;
    }
    let comm_end = bytes.iter().rposition(|byte| *byte == b')')?;
    if comm_end <= pid_end + 1 || bytes.get(comm_end + 1) != Some(&b' ') {
        return None;
    }
    let fields = std::str::from_utf8(&bytes[comm_end + 2..]).ok()?;
    // The tail begins at field 3 (state); starttime is field 22.
    let start_time = fields.split_ascii_whitespace().nth(19)?;
    if start_time.is_empty() || !start_time.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    start_time.parse().ok()
}

#[derive(Debug, PartialEq)]
enum LauncherEvidence {
    Absent,
    Command(Vec<String>),
    Unresolved,
}

pub(super) fn infer_host_command(
    pid: i32,
    environment: io::Result<impl Read>,
    read_executable: impl FnOnce() -> Option<PathBuf>,
    read_cmdline: impl FnOnce() -> Option<Vec<String>>,
) -> Option<Vec<String>> {
    // Failed or incomplete reads cannot establish that launcher evidence is absent.
    let bytes = read_environment(environment.ok()?)?;
    let environment = LauncherEnvironment::parse(&bytes)?;
    match desktop_launcher(&environment, pid) {
        LauncherEvidence::Command(command) => return Some(command),
        LauncherEvidence::Unresolved => return None,
        LauncherEvidence::Absent => {}
    }

    if environment.unsupported_snap {
        return None;
    }
    // ARGV0 alone has uses outside AppImage and establishes no package identity.
    if environment.appimage.is_none() && environment.appdir.is_none() {
        return read_cmdline();
    }

    let executable = read_executable()?;
    // procfs appends this suffix to an unlinked executable. A new file at its former
    // path cannot corroborate the running package binary.
    if executable
        .as_os_str()
        .as_encoded_bytes()
        .ends_with(b" (deleted)")
    {
        return None;
    }

    appimage_command(&environment, &executable)
}

fn read_environment(reader: impl Read) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    if reader
        .take(MAX_ENVIRON_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > MAX_ENVIRON_BYTES
        || (!bytes.is_empty() && bytes.last() != Some(&0))
    {
        return None;
    }

    Some(bytes)
}

#[derive(Default)]
struct LauncherEnvironment<'a> {
    desktop_file: Option<&'a [u8]>,
    launched_pid: Option<&'a [u8]>,
    appimage: Option<&'a [u8]>,
    appdir: Option<&'a [u8]>,
    argv0: Option<&'a [u8]>,
    unsupported_snap: bool,
}

impl<'a> LauncherEnvironment<'a> {
    /// Extract only launcher fields. Unrelated bytes remain opaque; duplicate or malformed
    /// supported fields fail closed without including environment contents in diagnostics.
    fn parse(bytes: &'a [u8]) -> Option<Self> {
        let mut environment = Self::default();
        for entry in bytes.split(|byte| *byte == 0) {
            let separator = entry.iter().position(|byte| *byte == b'=');
            let key = separator.map_or(entry, |position| &entry[..position]);
            let destination = match key {
                b"GIO_LAUNCHED_DESKTOP_FILE" => &mut environment.desktop_file,
                b"GIO_LAUNCHED_DESKTOP_FILE_PID" => &mut environment.launched_pid,
                b"APPIMAGE" => &mut environment.appimage,
                b"APPDIR" => &mut environment.appdir,
                b"ARGV0" => &mut environment.argv0,
                b"SNAP"
                | b"SNAP_NAME"
                | b"SNAP_INSTANCE_NAME"
                | b"SNAP_APP_NAME"
                | b"SNAP_INSTANCE_KEY" => {
                    // Presence alone excludes sandbox argv, regardless of the value.
                    environment.unsupported_snap = true;
                    continue;
                }
                _ => continue,
            };
            if destination.replace(&entry[separator? + 1..]).is_some() {
                return None;
            }
        }
        Some(environment)
    }
}

/// GLib's PID field distinguishes a direct launch from metadata inherited by children.
fn desktop_launcher(environment: &LauncherEnvironment<'_>, pid: i32) -> LauncherEvidence {
    let desktop_file = environment.desktop_file;
    let launched_pid = environment.launched_pid;
    if desktop_file.is_none() && launched_pid.is_none() {
        return LauncherEvidence::Absent;
    }
    let Some(recorded_pid) = launched_pid
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|text| text.parse::<i32>().ok())
        .filter(|pid| *pid > 0)
    else {
        return LauncherEvidence::Unresolved;
    };
    if recorded_pid != pid {
        return LauncherEvidence::Absent;
    }

    let Some(desktop_file) = desktop_file.and_then(|bytes| std::str::from_utf8(bytes).ok()) else {
        return LauncherEvidence::Unresolved;
    };
    let path = Path::new(desktop_file);
    if !path.is_absolute()
        || path
            .extension()
            .is_none_or(|extension| extension != "desktop")
        || !path.metadata().is_ok_and(|metadata| metadata.is_file())
    {
        return LauncherEvidence::Unresolved;
    }
    // Opening verifies readability without interpreting Exec or any other desktop keys.
    // Recheck the opened file too, since the path can change between filesystem calls.
    let usable = File::open(path)
        .and_then(|file| file.metadata())
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0);
    if !usable {
        return LauncherEvidence::Unresolved;
    }

    LauncherEvidence::Command(vec!["gio".into(), "launch".into(), desktop_file.into()])
}

/// A mounted executable corroborates package membership, not original arguments or the
/// behavior of AppRun. A different invocation name could select another bundled binary.
fn appimage_command(
    environment: &LauncherEnvironment<'_>,
    executable: &Path,
) -> Option<Vec<String>> {
    let image_text = std::str::from_utf8(environment.appimage?).ok()?;
    let image = Path::new(image_text);
    let appdir = Path::new(std::str::from_utf8(environment.appdir?).ok()?);
    if !image.is_absolute() || !appdir.is_absolute() || !executable.is_absolute() {
        return None;
    }

    // APPIMAGE is documented as resolved. Do not substitute a symlink or a file inside
    // another temporary AppImage mount for the persistent host executable.
    if image.canonicalize().ok()? != image
        || image
            .components()
            .any(|part| part.as_os_str().as_encoded_bytes().starts_with(b".mount_"))
    {
        return None;
    }
    let mounted_directory = appdir.canonicalize().ok()?;
    let mounted_executable = executable.canonicalize().ok()?;
    if !mounted_directory.is_dir()
        || image.starts_with(&mounted_directory)
        || !mounted_executable.starts_with(&mounted_directory)
        || mounted_executable == mounted_directory
    {
        return None;
    }

    let metadata = image.metadata().ok()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.permissions().mode() & 0o111 == 0 {
        return None;
    }
    let opened_metadata = File::open(image).ok()?.metadata().ok()?;
    if !opened_metadata.is_file()
        || opened_metadata.len() == 0
        || opened_metadata.permissions().mode() & 0o111 == 0
    {
        return None;
    }

    if let Some(invocation) = environment.argv0 {
        let invocation = std::str::from_utf8(invocation).ok()?;
        let image_basename = image.file_name()?.to_str()?;
        let relative_invocation = format!("./{image_basename}");
        // Other paths need the original cwd or might name a symlink that changes AppRun's
        // behavior. Do not normalize or resolve them into an apparently equivalent launch.
        if invocation != image_text
            && invocation != image_basename
            && invocation != relative_invocation
        {
            return None;
        }
    }

    Some(vec![image_text.into()])
}

pub(super) fn infer_process_command(
    metadata: io::Result<impl Read>,
    read_host_command: impl FnOnce() -> Option<Vec<String>>,
) -> Option<Vec<String>> {
    match metadata {
        Ok(reader) => read_flatpak_command(reader),
        // Only absence permits host launcher/argv inference. Other errors leave
        // identity uncertain; copying sandbox argv then could produce an unusable command.
        Err(error) if error.kind() == io::ErrorKind::NotFound => read_host_command(),
        Err(_) => None,
    }
}

fn read_flatpak_command(reader: impl Read) -> Option<Vec<String>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_FLATPAK_INFO_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_FLATPAK_INFO_BYTES {
        return None;
    }
    flatpak_command(std::str::from_utf8(&bytes).ok()?)
}

/// Read only the identity fields of Flatpak's key file. Unsupported escapes in these
/// fields are rejected rather than decoded or guessed; unrelated values stay opaque.
fn flatpak_command(metadata: &str) -> Option<Vec<String>> {
    if metadata.contains('\0') {
        return None;
    }

    let mut section = "";
    let mut sections = HashSet::new();
    let mut keys = HashSet::new();
    let mut application_id = None;
    let mut branch = None;
    let mut arch = None;
    for line in metadata.lines() {
        let line = line.trim_matches([' ', '\t', '\r']);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            section = line.strip_prefix('[')?.strip_suffix(']')?;
            if section.is_empty() || section.contains(['[', ']']) || !sections.insert(section) {
                return None;
            }
            continue;
        }

        let (key, field_value) = line.split_once('=')?;
        let key = key.trim_matches([' ', '\t']);
        let field_value = field_value.trim_matches([' ', '\t']);
        if section.is_empty() || key.is_empty() || !keys.insert((section, key)) {
            return None;
        }
        match (section, key) {
            ("Application", "name") => application_id = Some(field_value),
            ("Instance", "branch") => branch = Some(field_value),
            ("Instance", "arch") => arch = Some(field_value),
            _ => {}
        }
    }

    // Runtime-only sandboxes have [Runtime], not [Application]. Conflicting groups
    // are ambiguous and must never turn a runtime name into an application launch.
    let application_id = application_id?;
    if sections.contains("Runtime") || !valid_flatpak_id(application_id) {
        return None;
    }

    let mut command = vec!["flatpak".into(), "run".into()];
    for (option, field_value) in [("--branch", branch), ("--arch", arch)] {
        if let Some(field_value) = field_value {
            // A conservative subset of reference components, with no key-file escapes,
            // whitespace, path separators, or option-like leading punctuation.
            if !field_value.starts_with(|c: char| c.is_ascii_alphanumeric())
                || !field_value
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            {
                return None;
            }
            command.push(format!("{option}={field_value}"));
        }
    }
    command.push(application_id.into());
    Some(command)
}

/// Flatpak IDs have at least three nonempty ASCII components, at most 255 bytes,
/// no leading digits, and hyphens only in the final component.
fn valid_flatpak_id(application_id: &str) -> bool {
    if application_id.len() > 255 {
        return false;
    }
    let components: Vec<&str> = application_id.split('.').collect();
    if components.len() < 3 {
        return false;
    }
    for (component_index, component) in components.iter().enumerate() {
        let allow_hyphen = component_index == components.len() - 1;
        let valid_initial =
            |c: u8| c.is_ascii_alphabetic() || c == b'_' || (allow_hyphen && c == b'-');
        if !component.bytes().next().is_some_and(valid_initial)
            || !component
                .bytes()
                .all(|c| valid_initial(c) || c.is_ascii_digit())
        {
            return false;
        }
    }
    true
}

fn read_command_line(reader: impl Read) -> Option<Vec<String>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_CMDLINE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.is_empty() || bytes.len() > MAX_CMDLINE_BYTES || bytes.last() != Some(&0) {
        return None;
    }

    // Remove exactly the final terminator: any other adjacent NULs are empty arguments.
    let arguments: Vec<String> = bytes[..bytes.len() - 1]
        .split(|byte| *byte == 0)
        .map(|argument| String::from_utf8(argument.to_vec()))
        .collect::<Result<_, _>>()
        .ok()?;
    if arguments[0].is_empty() {
        return None;
    }
    Some(arguments)
}

#[cfg(test)]
#[path = "command_inference_tests.rs"]
mod tests;
