//! One frozen Sway observation, complete in-memory drafts, then profile-last publication.

use crate::capture::{self, CommandCounts};
use crate::profile::{self, MAX_WORKSPACES, WorkspaceTarget};
use crate::{discovery, draft_file};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

// Leave room for -NNN.yaml within Linux's usual 255-byte filename component.
const MAX_PROFILE_NAME_BYTES: usize = 246;

fn validate_name(name: &str) -> Result<(), String> {
    if !discovery::valid_name(name) || name.len() > MAX_PROFILE_NAME_BYTES {
        return Err(format!(
            "profile capture name: use [A-Za-z0-9][A-Za-z0-9_-]*, at most {MAX_PROFILE_NAME_BYTES} ASCII characters to allow -NNN.yaml"
        ));
    }
    Ok(())
}

fn parse_targets(arguments: &[String], all: bool) -> Result<Vec<WorkspaceTarget>, String> {
    let has_explicit_targets = !arguments.is_empty();
    if all == has_explicit_targets {
        return Err("select workspace targets or --all, exclusively".into());
    }
    if arguments.len() > MAX_WORKSPACES {
        return Err(format!(
            "at most {MAX_WORKSPACES} workspace targets are supported"
        ));
    }

    let mut targets = Vec::new();
    let mut unique = HashSet::new();
    for argument in arguments {
        let target = if argument.bytes().all(|byte| byte.is_ascii_digit()) && !argument.is_empty() {
            WorkspaceTarget::Number(
                argument
                    .parse()
                    .map_err(|_| format!("workspace {argument:?}: invalid numeric value"))?,
            )
        } else {
            WorkspaceTarget::Name(argument.clone())
        };
        target
            .validate()
            .map_err(|error| format!("workspace {argument:?}: {error}"))?;
        if !unique.insert(target.clone()) {
            return Err(format!("duplicate workspace target {target}"));
        }
        targets.push(target);
    }
    Ok(targets)
}

#[derive(Serialize)]
struct DraftProfile {
    version: u32,
    workspaces: Vec<DraftEntry>,
}

#[derive(Serialize)]
struct DraftEntry {
    workspace: WorkspaceTarget,
    layout: String,
}

/// The vector order is publication order: all layouts, followed by their profile.
struct DraftFile {
    destination: PathBuf,
    yaml: String,
}

pub fn capture(
    name: &str,
    arguments: &[String],
    all: bool,
    infer_commands: bool,
    layouts_dir: Option<PathBuf>,
    profiles_dir: Option<PathBuf>,
) -> Result<(), String> {
    validate_name(name)?;
    let targets = parse_targets(arguments, all)?;
    let layouts_directory = discovery::LayoutDirectory::resolve(layouts_dir)?.path;
    let (profiles_directory, _) = profile::profile_directory(profiles_dir)?;

    let snapshot = capture::snapshot().map_err(|error| format!("snapshot: {error}"))?;
    let selected = snapshot
        .select_workspaces(&targets, all)
        .map_err(|error| format!("selection: {error}"))?;
    let mut files = Vec::new();
    let mut entries = Vec::new();
    let mut commands = CommandCounts {
        total: 0,
        inferred: 0,
        inference_requested: infer_commands,
    };
    for (workspace_index, (target, workspace_id)) in selected.into_iter().enumerate() {
        let draft = snapshot
            .render_workspace(workspace_id, infer_commands)
            .map_err(|error| format!("render workspace {target}: {error}"))?;
        commands.total += draft.commands.total;
        commands.inferred += draft.commands.inferred;
        let layout_name = format!("{name}-{:03}", workspace_index + 1);
        files.push(DraftFile {
            destination: layouts_directory.join(format!("{layout_name}.yaml")),
            yaml: draft.yaml,
        });
        entries.push(DraftEntry {
            workspace: target,
            layout: layout_name,
        });
    }

    let profile_yaml = serde_saphyr::to_string_with_options(
        &DraftProfile {
            version: 1,
            workspaces: entries,
        },
        serde_saphyr::ser_options! { compact_list_indent: false },
    )
    .map_err(|error| format!("render profile: {error}"))?;
    files.push(DraftFile {
        destination: profiles_directory.join(format!("{name}.yaml")),
        yaml: format!("# Captured drafts: edit unresolved commands/matches, then run `sway-stage profile check {name}`.\n{profile_yaml}"),
    });

    publish_files(&files).map_err(|error| format!("publication: {error}"))?;

    // Reporting failures cannot undo a completed publication.
    let mut stderr = io::stderr().lock();
    let _ = writeln!(
        stderr,
        "sway-stage: captured profile {name:?}. {}",
        commands.summary()
    );
    for file in &files {
        let _ = writeln!(stderr, "  {}", file.destination.display());
    }
    Ok(())
}

fn preflight_files(files: &[DraftFile]) -> Result<(), String> {
    for file in files {
        draft_file::reject_existing_layout(&file.destination).map_err(|error| error.to_string())?;
        draft_file::reject_existing_layout(&file.destination.with_extension("yml"))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn publish_files(files: &[DraftFile]) -> Result<(), String> {
    preflight_files(files)?;

    let mut staging_paths = Vec::new();
    for file in files {
        let directory = file
            .destination
            .parent()
            .expect("generated path has a directory");
        match draft_file::stage_draft(directory, &file.yaml) {
            Ok(path) => staging_paths.push(path),
            Err(error) => {
                return Err(format!(
                    "{error}; no configuration published; staging files may remain: {}",
                    display_paths(&staging_paths)
                ));
            }
        }
    }

    publish_prepared(files, &staging_paths)?;
    for path in staging_paths {
        if let Err(error) = fs::remove_file(&path) {
            let _ = writeln!(
                io::stderr().lock(),
                "sway-stage: saved capture; cannot remove staging file {}: {error}",
                path.display()
            );
        }
    }
    Ok(())
}

/// Kept separate so the ordering and failure behavior can be tested with real files,
/// including a destination occupied after preparation. Never unlink published paths.
fn publish_prepared(files: &[DraftFile], staging_paths: &[PathBuf]) -> Result<(), String> {
    for (file_index, (file, partial_path)) in files.iter().zip(staging_paths).enumerate() {
        if let Err(error) = draft_file::publish_staged(
            partial_path,
            &file.destination,
            &file.destination.with_extension("yml"),
        ) {
            let published_paths: Vec<_> = files[..file_index]
                .iter()
                .map(|file| file.destination.clone())
                .collect();
            return Err(format!(
                "{error}; generated profile was not published; generated files may remain: {}; staging files retained: {}",
                display_paths(&published_paths),
                display_paths(staging_paths)
            ));
        }
    }
    Ok(())
}

fn display_paths(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return "none".into();
    }
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
#[path = "profile_capture_tests.rs"]
mod tests;
