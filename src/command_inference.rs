//! Best-effort host launch commands from bounded process metadata reads.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read};

// A modest per-window bound; read one extra byte to detect truncation.
const MAX_CMDLINE_BYTES: usize = 64 * 1024;
const MAX_FLATPAK_INFO_BYTES: usize = 64 * 1024;

/// An inaccessible or vanished process is an ordinary inference miss, never a capture
/// error. Neither I/O errors nor argument contents escape this boundary into diagnostics.
pub fn read_process_command(pid: i32) -> Option<Vec<String>> {
    if pid <= 0 {
        return None;
    }

    let metadata = File::open(format!("/proc/{pid}/root/.flatpak-info"));
    infer_process_command(metadata, || {
        let file = File::open(format!("/proc/{pid}/cmdline")).ok()?;
        read_command_line(file)
    })
}

pub(super) fn infer_process_command(
    metadata: io::Result<impl Read>,
    read_cmdline: impl FnOnce() -> Option<Vec<String>>,
) -> Option<Vec<String>> {
    match metadata {
        Ok(reader) => read_flatpak_command(reader),
        // Only absence permits host argv inference. Permission and other errors leave
        // identity uncertain; copying sandbox argv then could produce an unusable command.
        Err(error) if error.kind() == io::ErrorKind::NotFound => read_cmdline(),
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
