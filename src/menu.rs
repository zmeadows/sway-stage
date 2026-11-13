//! Optional synchronous Fuzzel interface and private, non-overwriting saves of captured
//! drafts into the layouts directory.

use crate::draft_file::{DraftSaveError, save_draft};
#[cfg(test)]
use crate::draft_file::{publish_draft, reject_existing_layout, validate_draft_name};
use crate::{capture, config, diagnostics, discovery, profile, profile_capture};
use std::fs;
#[cfg(test)]
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Pre-load failures carry a notification category separately from technical diagnostics.
#[derive(Debug)]
pub struct Error {
    pub kind: FailureKind,
    pub detail: String,
}

#[derive(Debug)]
pub enum FailureKind {
    Snapshot,
    LayoutDiscovery,
    ProfileDiscovery,
    NoActions,
    InvalidProfile(String),
    NoLayouts,
    NoValidLayouts,
    InvalidLayout(String),
    Capture,
    ProfileCapture,
    FuzzelStart,
    FuzzelResponse,
    DraftSave,
    DirectoryRead,
}

impl Error {
    pub fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

#[derive(Debug)]
pub enum MenuAction {
    Capture,
    CaptureProfile,
    Layout(String),
    Profile(String),
}

/// Eligibility uses only the saved tree. Discovery failures disable one category;
/// a valid independent action keeps the selector usable.
pub fn select_action(
    snapshot: Result<&capture::Snapshot, &String>,
    layouts_dir: Option<PathBuf>,
    profiles_dir: Option<PathBuf>,
) -> Result<Option<MenuAction>, Error> {
    let mut choices = Vec::new();
    let mut unavailable = Vec::new();
    match snapshot {
        Ok(snapshot) if snapshot.workspace_is_empty() => {
            let layouts = discovery::LayoutDirectory::resolve(layouts_dir.clone())
                .and_then(|directory| directory.discover());
            match layouts {
                Ok(discovery) => {
                    for (name, path) in discovery.layouts {
                        choices.push((MenuAction::Layout(name), config::read(&path).is_ok()));
                    }
                    if choices.is_empty() {
                        unavailable.push(Error::new(
                            FailureKind::NoLayouts,
                            "no layouts available; add a layout or profile before loading",
                        ));
                    }
                }
                Err(error) => unavailable.push(Error::new(FailureKind::LayoutDiscovery, error)),
            }
        }
        Ok(snapshot) => match snapshot.render(false) {
            Ok(_) => choices.push((MenuAction::Capture, true)),
            Err(error) => unavailable.push(Error::new(
                FailureKind::Capture,
                format!("capture unavailable: {error}"),
            )),
        },
        Err(error) => unavailable.push(Error::new(
            FailureKind::Snapshot,
            format!("pre-menu workspace snapshot: {error}"),
        )),
    }

    if let Ok(snapshot) = snapshot {
        match profile_capture::prepare(snapshot, &[], true, false) {
            Ok(_) => {
                let profile_capture_index =
                    if matches!(choices.first(), Some((MenuAction::Capture, _))) {
                        1
                    } else {
                        0
                    };
                choices.insert(profile_capture_index, (MenuAction::CaptureProfile, true));
            }
            Err(error) => unavailable.push(Error::new(
                FailureKind::ProfileCapture,
                format!("profile capture unavailable: {error}"),
            )),
        }
    }

    match profile::discover_names(profiles_dir.clone()) {
        Ok(names) => {
            for name in names {
                let is_valid =
                    profile::read_named(&name, profiles_dir.clone(), layouts_dir.clone()).is_ok();
                choices.push((MenuAction::Profile(name), is_valid));
            }
        }
        Err(error) => unavailable.push(Error::new(FailureKind::ProfileDiscovery, error)),
    }

    if !choices.iter().any(|(_, is_valid)| *is_valid) {
        let mut error = if choices.is_empty() && !unavailable.is_empty() {
            unavailable.remove(0)
        } else if choices
            .iter()
            .all(|(action, _)| matches!(action, MenuAction::Layout(_)))
        {
            Error::new(
                FailureKind::NoValidLayouts,
                "No valid layouts found. Run sway-stage check NAME to inspect a layout.",
            )
        } else {
            Error::new(
                FailureKind::NoActions,
                "No usable actions found. Run sway-stage check NAME or sway-stage profile check NAME to inspect configurations.",
            )
        };
        for other in unavailable {
            error.detail.push_str(&format!("; {}", other.detail));
        }
        return Err(error);
    }

    // Directory failures remain visible even when another category is usable.
    for error in unavailable {
        if matches!(
            error.kind,
            FailureKind::LayoutDiscovery | FailureKind::ProfileDiscovery
        ) {
            let _ = writeln!(io::stderr().lock(), "sway-stage: menu: {}", error.detail);
        }
    }

    let mut input = String::new();
    for (action, is_valid) in &choices {
        if !is_valid {
            input.push_str("[invalid] ");
        }
        match action {
            MenuAction::Capture => input.push_str("Capture current workspace…"),
            MenuAction::CaptureProfile => input.push_str("Capture all populated workspaces…"),
            MenuAction::Layout(name) => input.push_str(&format!("Layout: {name}")),
            MenuAction::Profile(name) => input.push_str(&format!("Profile: {name}")),
        }
        input.push('\n');
    }

    let output_name = snapshot.ok().and_then(capture::Snapshot::output_name);
    let Some(selection) = fuzzel(
        "--prompt=Sway Stage: ",
        Some(&input),
        output_name,
        None,
        true,
    )?
    else {
        return Ok(None);
    };
    let selected_index = selection.parse::<usize>().ok();
    let (action, is_valid) = selected_index
        .and_then(|index| choices.into_iter().nth(index))
        .ok_or_else(|| {
            Error::new(
                FailureKind::FuzzelResponse,
                "selection: Fuzzel returned an index that was not offered",
            )
        })?;

    if !is_valid {
        return Err(match action {
            MenuAction::Layout(name) => invalid_layout(&name, None),
            MenuAction::Profile(name) => invalid_profile(&name, None),
            MenuAction::Capture | MenuAction::CaptureProfile => {
                unreachable!("capture eligibility is validated before selection")
            }
        });
    }
    Ok(Some(action))
}

pub fn invalid_layout(name: &str, cause: Option<String>) -> Error {
    let mut detail =
        format!("Layout '{name}' is invalid. Run sway-stage check {name} for details.");
    if let Some(cause) = cause {
        detail.push_str(&format!(" {cause}"));
    }
    Error::new(FailureKind::InvalidLayout(name.to_owned()), detail)
}

pub fn invalid_profile(name: &str, cause: Option<String>) -> Error {
    let mut detail =
        format!("Profile '{name}' is invalid. Run sway-stage profile check {name} for details.");
    if let Some(cause) = cause {
        detail.push_str(&format!(" {cause}"));
    }
    Error::new(FailureKind::InvalidProfile(name.to_owned()), detail)
}

#[derive(Debug)]
pub enum DirectorySelection {
    Base,
    Child(PathBuf),
}

/// Row indices keep actions distinct even when the base label equals a child name.
pub fn select_directory(
    root: &Path,
    display_path: &Path,
    output_name: Option<&str>,
) -> Result<Option<DirectorySelection>, Error> {
    let mut choices = vec![(
        format!("Base directory ({})", display_path.display()),
        DirectorySelection::Base,
    )];
    for (name, path) in directory_choices(root)? {
        choices.push((name, DirectorySelection::Child(path)));
    }

    // Configured paths may contain controls; escape them to preserve one row per choice.
    let mut input = String::new();
    for (label, _) in &choices {
        for character in label.chars() {
            if character.is_control() {
                input.extend(character.escape_default());
            } else {
                input.push(character);
            }
        }
        input.push('\n');
    }

    let Some(selection) = fuzzel(
        "--prompt=Launch in: ",
        Some(&input),
        output_name,
        None,
        true,
    )?
    else {
        return Ok(None);
    };

    let selected_index = selection.parse::<usize>().ok();
    let selected_choice = selected_index.and_then(|index| choices.into_iter().nth(index));
    let (_, action) = selected_choice.ok_or_else(|| {
        Error::new(
            FailureKind::FuzzelResponse,
            "directory selection: Fuzzel returned an index that was not offered",
        )
    })?;
    Ok(Some(action))
}

fn directory_choices(root: &Path) -> Result<Vec<(String, PathBuf)>, Error> {
    let entries = fs::read_dir(root).map_err(|error| {
        Error::new(
            FailureKind::DirectoryRead,
            format!("read project directory {}: {error}", root.display()),
        )
    })?;
    let mut choices = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            Error::new(
                FailureKind::DirectoryRead,
                format!(
                    "read entry in project directory {}: {error}",
                    root.display()
                ),
            )
        })?;

        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') || name.chars().any(char::is_control) {
            continue;
        }

        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            Error::new(
                FailureKind::DirectoryRead,
                format!("inspect project entry {}: {error}", path.display()),
            )
        })?;
        // Follow directory symlinks, including external targets; ignore unusable links.
        let is_directory = file_type.is_dir() || (file_type.is_symlink() && path.is_dir());
        if is_directory {
            choices.push((name.to_owned(), path));
        }
    }

    choices.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(choices)
}

/// Saves into the configured layouts directory without requiring discovery.
/// The draft is not validated: placeholders stay until the user edits and checks it.
pub fn capture_draft(
    snapshot: &capture::Snapshot,
    layouts_dir: Option<PathBuf>,
    infer_commands: bool,
) -> Result<(), Error> {
    // Render the pre-menu state without querying Sway again.
    let draft = snapshot
        .render(infer_commands)
        .map_err(|error| Error::new(FailureKind::Capture, format!("capture: {error}")))?;
    let mut placeholder = None;
    loop {
        let Some(name) = fuzzel(
            "--prompt-only=Capture layout as: ",
            None,
            snapshot.output_name(),
            placeholder.as_deref(),
            false,
        )?
        else {
            return Ok(());
        };

        let layouts_directory = discovery::LayoutDirectory::resolve(layouts_dir.clone())
            .map_err(|error| Error::new(FailureKind::DraftSave, error))?;
        match save_draft(&layouts_directory.path, &name, &draft.yaml) {
            Ok(_) => {
                diagnostics::notify_draft_saved(&name);
                return Ok(());
            }
            Err(error @ (DraftSaveError::InvalidName(_) | DraftSaveError::NameConflict(_))) => {
                placeholder = Some(name_error_placeholder(&name, &error));
            }
            Err(error) => return Err(Error::new(FailureKind::DraftSave, error.to_string())),
        }
    }
}

/// Prepare once from the saved tree; name retries never repeat inference or IPC.
pub fn capture_profile(
    snapshot: &capture::Snapshot,
    layouts_dir: Option<PathBuf>,
    profiles_dir: Option<PathBuf>,
    infer_commands: bool,
) -> Result<(), Error> {
    let profile_error = |error| Error::new(FailureKind::ProfileCapture, error);
    let prepared =
        profile_capture::prepare(snapshot, &[], true, infer_commands).map_err(profile_error)?;
    let layouts_directory =
        discovery::LayoutDirectory::resolve(layouts_dir).map_err(profile_error)?;
    let (profiles_directory, _) =
        profile::profile_directory(profiles_dir).map_err(profile_error)?;

    let mut placeholder = None;
    loop {
        let Some(name) = fuzzel(
            "--prompt-only=Capture profile as: ",
            None,
            snapshot.output_name(),
            placeholder.as_deref(),
            false,
        )?
        else {
            return Ok(());
        };

        match prepared.save(&name, &layouts_directory.path, &profiles_directory) {
            Ok(()) => {
                diagnostics::notify_profile_saved(&name, prepared.workspace_count());
                return Ok(());
            }
            Err(error @ (DraftSaveError::InvalidName(_) | DraftSaveError::NameConflict(_))) => {
                placeholder = Some(name_error_placeholder(&name, &error));
            }
            Err(error) => return Err(profile_error(error.to_string())),
        }
    }
}

/// Returns one exact line, or cancellation. Fuzzel 1.11 introduced prompt-only and
/// documented dmenu cancellation as status 2; status 1 is an execution failure.
fn fuzzel(
    prompt: &str,
    input: Option<&str>,
    output_name: Option<&str>,
    placeholder: Option<&str>,
    return_index: bool,
) -> Result<Option<String>, Error> {
    let mut command = Command::new("fuzzel");
    command.args(["--dmenu", "--no-exit-on-keyboard-focus-loss"]);
    if input.is_some() {
        command.arg("--no-sort");
    } else {
        command.arg("--width=80");
    }
    if return_index {
        command.arg("--index");
    }
    if let Some(output_name) = output_name {
        command.arg(format!("--output={output_name}"));
    }

    if let Some(placeholder) = placeholder {
        command.arg("--placeholder").arg(placeholder);
    }

    let fuzzel_process = command
        .arg(prompt)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| {
            Error::new(
                FailureKind::FuzzelStart,
                format!("start Fuzzel: {error}; install Fuzzel 1.11 or newer on PATH"),
            )
        })?;

    read_fuzzel_response(fuzzel_process, input)
        .map_err(|error| Error::new(FailureKind::FuzzelResponse, error))
}

fn read_fuzzel_response(
    mut fuzzel_process: std::process::Child,
    input: Option<&str>,
) -> Result<Option<String>, String> {
    // Read stdout while feeding stdin: a large layout list must not deadlock if Fuzzel
    // returns a selection before consuming all the options. Always close stdin and reap.
    let (output_result, write_options_result) = std::thread::scope(|scope| {
        let input_writer = fuzzel_process.stdin.take().map(|mut stdin| {
            scope.spawn(move || stdin.write_all(input.expect("options for piped stdin").as_bytes()))
        });

        let fuzzel_output = fuzzel_process.wait_with_output();
        let write_options_result = match input_writer {
            Some(input_writer) => input_writer
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("Fuzzel input writer panicked"))),
            None => Ok(()),
        };
        (fuzzel_output, write_options_result)
    });

    let fuzzel_output = output_result.map_err(|error| format!("wait for Fuzzel: {error}"))?;
    if fuzzel_output.status.code() == Some(2) {
        if !fuzzel_output.stdout.is_empty() {
            return Err("Fuzzel returned unexpected output while canceling".into());
        }
        return Ok(None);
    }
    if !fuzzel_output.status.success() {
        return Err(format!(
            "Fuzzel failed: {}; check its stderr and configuration",
            fuzzel_output.status
        ));
    }

    write_options_result.map_err(|error| format!("write Fuzzel options: {error}"))?;

    let output_text =
        String::from_utf8(fuzzel_output.stdout).map_err(|_| "Fuzzel output is not UTF-8")?;
    let selected_line = output_text
        .strip_suffix('\n')
        .ok_or("Fuzzel output must be one newline-terminated line")?;
    if selected_line.chars().any(char::is_control) {
        return Err("Fuzzel output must be a single line without control characters".into());
    }

    Ok(Some(selected_line.to_owned()))
}

fn name_error_placeholder(name: &str, error: &DraftSaveError) -> String {
    // Bound display length and escape controls without changing the submitted name.
    const MAX_DISPLAY_CHARACTERS: usize = 60;
    let mut displayed_name = String::new();
    for character in name.chars().take(MAX_DISPLAY_CHARACTERS) {
        if character.is_control() {
            displayed_name.extend(character.escape_default());
        } else {
            displayed_name.push(character);
        }
    }
    if name.chars().count() > MAX_DISPLAY_CHARACTERS {
        displayed_name.push('…');
    }
    if displayed_name.is_empty() {
        displayed_name.push_str("(empty name)");
    }

    match error {
        DraftSaveError::InvalidName(reason) => format!(
            "{displayed_name} is invalid — {}",
            reason.strip_prefix("draft name: ").unwrap_or(reason)
        ),
        DraftSaveError::NameConflict(_) => {
            format!("{displayed_name} already exists — enter another name")
        }
        DraftSaveError::Operational(_) => unreachable!("operational errors are not retried"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Directory {
        directory_path: PathBuf,
    }

    impl Directory {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "sway-stage-menu-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self {
                directory_path: path,
            }
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.directory_path).unwrap();
        }
    }

    #[test]
    fn directory_enumeration_errors_include_the_root() {
        let directory = Directory::new();
        for root in [
            directory.directory_path.join("missing"),
            directory.directory_path.join("file"),
        ] {
            fs::write(directory.directory_path.join("file"), "not a directory").unwrap();
            let error = directory_choices(&root).unwrap_err();
            assert!(matches!(error.kind, FailureKind::DirectoryRead));
            assert!(error.detail.contains("read project directory"), "{error:?}");
            assert!(error.detail.contains(root.to_str().unwrap()), "{error:?}");
        }
    }

    #[test]
    fn draft_names_use_the_layout_grammar_and_leave_room_for_the_suffix() {
        for name in ["development", "A_1-2", "0", &"a".repeat(250)] {
            validate_draft_name(name).unwrap();
        }
        for name in [
            "",
            "_name",
            "-name",
            "a.b",
            "a/b",
            "a\\b",
            "..",
            " name",
            "name ",
            "é",
            &"a".repeat(251),
        ] {
            assert!(validate_draft_name(name).is_err());
        }
    }

    #[test]
    fn error_placeholders_bound_names_and_escape_control_characters() {
        let name = "a".repeat(251);
        let error = DraftSaveError::InvalidName(validate_draft_name(&name).unwrap_err());
        let placeholder = name_error_placeholder(&name, &error);
        assert_eq!(
            placeholder,
            format!(
                "{}… is invalid — maximum length is 250 ASCII characters",
                "a".repeat(60)
            )
        );

        let name = "bad\tname\0";
        let error = DraftSaveError::InvalidName(validate_draft_name(name).unwrap_err());
        let placeholder = name_error_placeholder(name, &error);
        assert!(placeholder.starts_with(r"bad\tname\u{0} is invalid"));
        assert!(!placeholder.chars().any(char::is_control));
    }

    #[test]
    fn saves_are_complete_and_do_not_change_existing_directory_permissions() {
        let directory = Directory::new();
        fs::set_permissions(&directory.directory_path, fs::Permissions::from_mode(0o750)).unwrap();
        let draft = "# keep all output\ncommand: null\nmatch: null\n";
        let path = save_draft(&directory.directory_path, "draft", draft).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), draft);
        assert_eq!(
            fs::metadata(&directory.directory_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
        assert_eq!(fs::read_dir(&directory.directory_path).unwrap().count(), 1);
    }

    #[test]
    fn both_extensions_and_dangling_symlinks_are_collisions() {
        for suffix in ["yaml", "yml"] {
            let directory = Directory::new();
            let existing = directory.directory_path.join(format!("draft.{suffix}"));
            fs::write(&existing, "original").unwrap();
            assert!(
                save_draft(&directory.directory_path, "draft", "replacement")
                    .unwrap_err()
                    .to_string()
                    .contains("already exists")
            );
            assert_eq!(fs::read_to_string(&existing).unwrap(), "original");
            fs::remove_file(&existing).unwrap();
            symlink("absent", &existing).unwrap();
            assert!(save_draft(&directory.directory_path, "draft", "replacement").is_err());
            assert!(
                fs::symlink_metadata(&existing)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    #[test]
    fn exclusive_publication_refuses_a_destination_created_after_the_initial_check() {
        let directory = Directory::new();
        let path = directory.directory_path.join("draft.yaml");
        let alternate = path.with_extension("yml");
        reject_existing_layout(&path).unwrap();
        let partial = directory.directory_path.join("staging.partial");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .unwrap();
        fs::write(&path, "concurrent draft").unwrap();
        assert!(matches!(
            publish_draft(&mut file, &partial, &path, &alternate, "new draft"),
            Err(DraftSaveError::NameConflict(_))
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "concurrent draft");
        assert_eq!(fs::read_to_string(&partial).unwrap(), "new draft");
    }

    #[test]
    fn unrelated_publication_errors_are_operational() {
        let directory = Directory::new();
        let partial = directory.directory_path.join("staging.partial");
        let mut file = File::create(&partial).unwrap();
        let destination = directory.directory_path.join("missing/draft.yaml");
        let error = publish_draft(
            &mut file,
            &partial,
            &destination,
            &destination.with_extension("yml"),
            "new draft",
        )
        .unwrap_err();
        assert!(matches!(error, DraftSaveError::Operational(_)));
        assert!(error.to_string().contains("publish draft"));
        assert_eq!(fs::read_to_string(&partial).unwrap(), "new draft");
        assert!(!destination.exists());
    }

    #[test]
    fn failed_writes_never_publish_a_yaml_file() {
        let directory = Directory::new();
        let path = directory.directory_path.join("draft.yaml");
        let partial = directory.directory_path.join("staging.partial");
        fs::write(&partial, "").unwrap();
        let mut full = OpenOptions::new().write(true).open("/dev/full").unwrap();
        let error = publish_draft(
            &mut full,
            &partial,
            &path,
            &path.with_extension("yml"),
            "draft",
        )
        .unwrap_err();
        assert!(matches!(error, DraftSaveError::Operational(_)));
        assert!(error.to_string().contains("write draft"));
        assert!(!path.exists());
        assert!(partial.exists());
    }
}
