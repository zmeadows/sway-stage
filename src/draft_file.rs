//! Private staging and exclusive publication of editable configuration drafts.

use crate::discovery;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

// Linux's usual 255-byte filename component, less the .yaml suffix.
const MAX_DRAFT_NAME_BYTES: usize = 250;

pub(crate) fn validate_draft_name(name: &str) -> Result<(), String> {
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

#[derive(Debug)]
pub(crate) enum DraftSaveError {
    InvalidName(String),
    NameConflict(String),
    Operational(String),
}

impl std::fmt::Display for DraftSaveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(message)
            | Self::NameConflict(message)
            | Self::Operational(message) => formatter.write_str(message),
        }
    }
}

impl From<String> for DraftSaveError {
    fn from(message: String) -> Self {
        Self::Operational(message)
    }
}

pub(crate) fn save_draft(
    layouts_directory: &Path,
    name: &str,
    draft_yaml: &str,
) -> Result<PathBuf, DraftSaveError> {
    validate_draft_name(name).map_err(DraftSaveError::InvalidName)?;

    let destination_path = layouts_directory.join(format!("{name}.yaml"));
    let alternate_layout_path = layouts_directory.join(format!("{name}.yml"));
    reject_existing_layout(&destination_path)?;
    reject_existing_layout(&alternate_layout_path)?;

    let partial_path = stage_draft(layouts_directory, draft_yaml)?;

    if let Err(error) = publish_staged(&partial_path, &destination_path, &alternate_layout_path) {
        let message = format!(
            "{error}; unsaved staging file retained at {}",
            partial_path.display()
        );
        if matches!(error, DraftSaveError::NameConflict(_)) {
            // A retry must still report the retained staging file for inspection.
            let _ = writeln!(io::stderr().lock(), "sway-stage: {message}");
            return Err(DraftSaveError::NameConflict(message));
        }
        return Err(DraftSaveError::Operational(message));
    }

    // Publication succeeded. Cleanup is secondary and never removes the saved draft.
    if let Err(error) = fs::remove_file(&partial_path) {
        let _ = writeln!(
            io::stderr().lock(),
            "sway-stage: saved draft; cannot remove staging file {}: {error}",
            partial_path.display()
        );
    }

    Ok(destination_path)
}

/// Sync all bytes before exposing a .yaml name. The hard link is exclusive even when
/// another saver creates the destination after the caller's initial collision check.
#[cfg(test)]
pub(crate) fn publish_draft(
    partial_file: &mut File,
    partial_path: &Path,
    destination_path: &Path,
    alternate_layout_path: &Path,
    draft_yaml: &str,
) -> Result<(), DraftSaveError> {
    write_draft(partial_file, draft_yaml)?;

    publish_staged(partial_path, destination_path, alternate_layout_path)
}

/// Expose complete, synced bytes under a new name, never replacing an occupied path.
pub(crate) fn publish_staged(
    partial_path: &Path,
    destination_path: &Path,
    alternate_layout_path: &Path,
) -> Result<(), DraftSaveError> {
    reject_existing_layout(alternate_layout_path)?;
    fs::hard_link(partial_path, destination_path).map_err(|error| {
        let message = format!(
            "publish draft {} without overwriting: {error}",
            destination_path.display()
        );
        if error.kind() == io::ErrorKind::AlreadyExists {
            DraftSaveError::NameConflict(message)
        } else {
            DraftSaveError::Operational(message)
        }
    })
}

pub(crate) fn reject_existing_layout(path: &Path) -> Result<(), DraftSaveError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(DraftSaveError::NameConflict(format!(
            "configuration already exists at {}; choose another name",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DraftSaveError::Operational(format!(
            "inspect layout path {}: {error}",
            path.display()
        ))),
    }
}

/// Prepare the complete file outside discovery's extensions. Failures retain the private
/// partial path for inspection; callers never roll back potentially visible files.
pub(crate) fn stage_draft(
    layouts_directory: &Path,
    draft_yaml: &str,
) -> Result<PathBuf, DraftSaveError> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(layouts_directory)
        .map_err(|error| {
            format!(
                "create layouts directory {}: {error}",
                layouts_directory.display()
            )
        })?;

    // Stage outside discovery's extensions so list, check, and the menu never see it. A
    // write/sync/publication failure leaves a clearly named partial file for manual
    // inspection, never a partial .yaml layout.
    // Do not roll back by unlinking a path whose ownership may have changed on failure.
    static NEXT_STAGING_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let partial_path = layouts_directory.join(format!(
        ".sway-stage-{}-{}.partial",
        std::process::id(),
        NEXT_STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed)
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

    write_draft(&mut partial_file, draft_yaml).map_err(|error| {
        format!(
            "{error}; staging file retained at {}",
            partial_path.display()
        )
    })?;
    Ok(partial_path)
}

fn write_draft(file: &mut File, yaml: &str) -> Result<(), String> {
    file.write_all(yaml.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("write draft: {error}"))
}
