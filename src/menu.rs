//! Optional synchronous Fuzzel interface and private, non-overwriting saves of captured
//! drafts into the layouts directory.

use crate::{capture, diagnostics, discovery};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const CAPTURE_OPTION: &str = "Capture current layout…";
// Leave room for the five-byte .yaml suffix within Linux's usual 255-byte component limit.
const MAX_DRAFT_NAME_BYTES: usize = 250;

pub enum Action {
    Load(String),
    Capture,
}

pub enum AvailableAction {
    Load,
    Capture,
}

pub fn select(
    available: AvailableAction,
    layouts_dir: Option<PathBuf>,
    output_name: Option<&str>,
) -> Result<Option<Action>, String> {
    let options: Vec<(String, Action)> = match available {
        AvailableAction::Load => {
            let directory = discovery::LayoutDirectory::resolve(layouts_dir)?;
            let layouts = directory.discover()?;
            if layouts.is_empty() {
                return Err(format!(
                    "no layouts available in {}; add a layout before loading",
                    directory.path.display()
                ));
            }
            layouts
                .into_keys()
                .map(|name| (format!("Load: {name}"), Action::Load(name)))
                .collect()
        }
        AvailableAction::Capture => vec![(CAPTURE_OPTION.into(), Action::Capture)],
    };

    let mut input = String::new();
    for (label, _) in &options {
        input.push_str(label);
        input.push('\n');
    }
    let Some(selection) = fuzzel("--prompt=Layout: ", Some(&input), output_name)? else {
        return Ok(None);
    };
    options
        .into_iter()
        .find(|(label, _)| label == &selection)
        .map(|(_, action)| Some(action))
        .ok_or_else(|| {
            "selection: Fuzzel returned an option that was not offered; use dmenu text output"
                .into()
        })
}

/// Names and paths stay paired: typed Fuzzel input never becomes a filesystem path.
pub fn select_directory(root: &Path, output_name: Option<&str>) -> Result<Option<PathBuf>, String> {
    let choices = directory_choices(root)?;
    let input = choices
        .iter()
        .map(|(name, _)| format!("{name}\n"))
        .collect::<String>();
    let Some(selection) = fuzzel("--prompt=Directory: ", Some(&input), output_name)? else {
        return Ok(None);
    };
    let (_, directory) = choices
        .into_iter()
        .find(|(name, _)| name == &selection)
        .ok_or("directory selection: Fuzzel returned a name that was not offered")?;
    Ok(Some(directory))
}

fn directory_choices(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let entries = fs::read_dir(root)
        .map_err(|error| format!("read project directory {}: {error}", root.display()))?;
    let mut choices = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "read entry in project directory {}: {error}",
                root.display()
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
        let file_type = entry
            .file_type()
            .map_err(|error| format!("inspect project entry {}: {error}", path.display()))?;
        // Follow directory symlinks, including external targets; ignore unusable links.
        let is_directory = file_type.is_dir() || (file_type.is_symlink() && path.is_dir());
        if is_directory {
            choices.push((name.to_owned(), path));
        }
    }
    choices.sort_by(|left, right| left.0.cmp(&right.0));
    if choices.is_empty() {
        return Err(format!("no selectable directories in {}", root.display()));
    }
    Ok(choices)
}

/// Saves into the configured layouts directory without requiring discovery.
/// The draft is not validated: placeholders stay until the user edits and checks it.
pub fn capture_draft(
    snapshot: &capture::Snapshot,
    layouts_dir: Option<PathBuf>,
    infer_commands: bool,
) -> Result<(), String> {
    // The action prompt has exited. Render the pre-menu state without querying Sway again.
    let draft = snapshot
        .render(infer_commands)
        .map_err(|error| format!("capture: {error}"))?;
    let Some(name) = fuzzel("--prompt-only=Draft name: ", None, snapshot.output_name())? else {
        return Ok(());
    };

    let layouts_directory = discovery::LayoutDirectory::resolve(layouts_dir)?;
    let path = save_draft(&layouts_directory.path, &name, &draft.yaml)?;
    diagnostics::notify_draft_saved(&path, &draft.commands);
    Ok(())
}

/// Returns one exact line, or cancellation. Fuzzel 1.11 introduced prompt-only and
/// documented dmenu cancellation as status 2; status 1 is an execution failure.
fn fuzzel(
    prompt: &str,
    input: Option<&str>,
    output_name: Option<&str>,
) -> Result<Option<String>, String> {
    let mut command = Command::new("fuzzel");
    command.args(["--dmenu", "--no-exit-on-keyboard-focus-loss"]);
    if input.is_some() {
        command.arg("--no-sort");
    }
    if let Some(output_name) = output_name {
        command.arg(format!("--output={output_name}"));
    }
    let mut child = command
        .arg(prompt)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start Fuzzel: {error}; install Fuzzel 1.11 or newer on PATH"))?;

    // Read stdout while feeding stdin: a large layout list must not deadlock if Fuzzel
    // returns a selection before consuming all the options. Always close stdin and reap.
    let (output, input_result) = std::thread::scope(|scope| {
        let writer = child.stdin.take().map(|mut stdin| {
            scope.spawn(move || stdin.write_all(input.expect("options for piped stdin").as_bytes()))
        });
        let output = child.wait_with_output();
        let input_result = match writer {
            Some(writer) => writer
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("Fuzzel input writer panicked"))),
            None => Ok(()),
        };
        (output, input_result)
    });
    let output = output.map_err(|error| format!("wait for Fuzzel: {error}"))?;
    if output.status.code() == Some(2) {
        if !output.stdout.is_empty() {
            return Err("Fuzzel returned unexpected output while canceling".into());
        }
        return Ok(None);
    }
    if !output.status.success() {
        return Err(format!(
            "Fuzzel failed: {}; check its stderr and configuration",
            output.status
        ));
    }
    input_result.map_err(|error| format!("write Fuzzel options: {error}"))?;

    let text = String::from_utf8(output.stdout).map_err(|_| "Fuzzel output is not UTF-8")?;
    let line = text
        .strip_suffix('\n')
        .ok_or("Fuzzel output must be one newline-terminated line")?;
    if line.chars().any(char::is_control) {
        return Err("Fuzzel output must be a single line without control characters".into());
    }
    Ok(Some(line.to_owned()))
}

fn validate_draft_name(name: &str) -> Result<(), String> {
    if !discovery::valid_name(name) {
        return Err(
            "draft name: use [A-Za-z0-9][A-Za-z0-9_-]* without spaces or an extension".into(),
        );
    }
    if name.len() > MAX_DRAFT_NAME_BYTES {
        return Err(format!(
            "draft name: maximum length is {MAX_DRAFT_NAME_BYTES} ASCII characters"
        ));
    }
    Ok(())
}

fn save_draft(directory: &Path, name: &str, draft: &str) -> Result<PathBuf, String> {
    validate_draft_name(name)?;
    let path = directory.join(format!("{name}.yaml"));
    let alternate = directory.join(format!("{name}.yml"));
    reject_existing_layout(&path)?;
    reject_existing_layout(&alternate)?;

    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .map_err(|error| format!("create layouts directory {}: {error}", directory.display()))?;

    // Stage outside discovery's extensions so list, check, and the menu never see it. A
    // write/sync/publication failure leaves a clearly named partial file for manual
    // inspection, never a partial .yaml layout.
    // Do not roll back by unlinking a path whose ownership may have changed on failure.
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let partial_path = directory.join(format!(
        ".sway-stage-{}-{}.partial",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut partial_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial_path)
        .map_err(|error| {
            format!(
                "create draft staging file {}: {error}",
                partial_path.display()
            )
        })?;
    if let Err(error) = publish_draft(&mut partial_file, &partial_path, &path, &alternate, draft) {
        return Err(format!(
            "{error}; unsaved staging file retained at {}",
            partial_path.display()
        ));
    }

    // Publication succeeded. Cleanup is secondary and never removes the saved draft.
    if let Err(error) = fs::remove_file(&partial_path) {
        let _ = writeln!(
            io::stderr().lock(),
            "sway-stage: saved draft; cannot remove staging file {}: {error}",
            partial_path.display()
        );
    }
    Ok(path)
}

/// Sync all bytes before exposing a .yaml name. The hard link is exclusive even when
/// another saver creates the destination after the caller's initial collision check.
fn publish_draft(
    partial_file: &mut File,
    partial_path: &Path,
    path: &Path,
    alternate: &Path,
    draft: &str,
) -> Result<(), String> {
    partial_file
        .write_all(draft.as_bytes())
        .and_then(|()| partial_file.sync_all())
        .map_err(|error| format!("write draft: {error}"))?;
    reject_existing_layout(alternate)?;
    fs::hard_link(partial_path, path).map_err(|error| {
        format!(
            "publish draft {} without overwriting: {error}",
            path.display()
        )
    })
}

fn reject_existing_layout(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(format!(
            "layout already exists at {}; choose another name",
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("inspect layout path {}: {error}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Directory(PathBuf);

    impl Directory {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "sway-stage-menu-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn directory_enumeration_errors_include_the_root() {
        let directory = Directory::new();
        for root in [directory.0.join("missing"), directory.0.join("file")] {
            fs::write(directory.0.join("file"), "not a directory").unwrap();
            let error = directory_choices(&root).unwrap_err();
            assert!(error.contains("read project directory"), "{error}");
            assert!(error.contains(root.to_str().unwrap()), "{error}");
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
    fn saves_are_complete_and_do_not_change_existing_directory_permissions() {
        let directory = Directory::new();
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o750)).unwrap();
        let draft = "# keep all output\ncommand: null\nmatch: null\n";
        let path = save_draft(&directory.0, "draft", draft).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), draft);
        assert_eq!(
            fs::metadata(&directory.0).unwrap().permissions().mode() & 0o777,
            0o750
        );
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    }

    #[test]
    fn both_extensions_and_dangling_symlinks_are_collisions() {
        for suffix in ["yaml", "yml"] {
            let directory = Directory::new();
            let existing = directory.0.join(format!("draft.{suffix}"));
            fs::write(&existing, "original").unwrap();
            assert!(
                save_draft(&directory.0, "draft", "replacement")
                    .unwrap_err()
                    .contains("already exists")
            );
            assert_eq!(fs::read_to_string(&existing).unwrap(), "original");
            fs::remove_file(&existing).unwrap();
            symlink("absent", &existing).unwrap();
            assert!(save_draft(&directory.0, "draft", "replacement").is_err());
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
        let path = directory.0.join("draft.yaml");
        let alternate = path.with_extension("yml");
        reject_existing_layout(&path).unwrap();
        let partial = directory.0.join("staging.partial");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .unwrap();
        fs::write(&path, "concurrent draft").unwrap();
        assert!(publish_draft(&mut file, &partial, &path, &alternate, "new draft").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "concurrent draft");
        assert_eq!(fs::read_to_string(&partial).unwrap(), "new draft");
    }

    #[test]
    fn failed_writes_never_publish_a_yaml_file() {
        let directory = Directory::new();
        let path = directory.0.join("draft.yaml");
        let partial = directory.0.join("staging.partial");
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
        assert!(error.contains("write draft"));
        assert!(!path.exists());
        assert!(partial.exists());
    }
}
