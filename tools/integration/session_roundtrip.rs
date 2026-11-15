//! The four-workspace session round-trip, modeled on the workflow that first showed Sway's
//! launch context misplacing a profile's windows: four populated workspaces are captured
//! into a profile, their windows are closed, and the profile is then loaded through the
//! menu, started by Sway's `exec` as a keybinding would, from another, occupied workspace.
//!
//! Sway itself opens the original windows, one `exec` at a time, after giving each empty
//! workspace its layout, so sway-stage is never both the fixture's constructor and the
//! system under test. Every window keeps Foot's default app ID; a distinct literal title
//! argument per window is its launch evidence. The decisions here are pure; lifecycle runs
//! the scenario against the private compositor.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::Path;

use crate::ipc::TreeNode;
use crate::launch_context::shell_quote;
use crate::observation::{Snapshot, WorkspaceIdentity};
use crate::ownership::{AcceptedWindows, verify_ownership};
use crate::profile::{ExpectedWorkspaceEvent, Target};
use crate::scenarios::Scenario;

pub const SCENARIO: &str = "profile-session-roundtrip";

/// The captured profile's name, which the menu row names.
pub const PROFILE_NAME: &str = "session";

/// The round-trip's workspaces in capture and profile order, one per
/// `session_roundtrip_layouts` entry. The runner uses none of them otherwise.
pub const WORKSPACES: [&str; 4] = ["5", "6", "7", "8"];

/// One of the round-trip's workspaces, or one target of a profile verified workspace by
/// workspace, in one construction cycle, with the windows accepted there.
pub struct SessionWorkspace {
    pub target: Target,
    pub layout: Scenario,
    pub accepted: AcceptedWindows,
}

/// The IDs `verify_ownership` may pass over while checking `workspaces[workspace_index]`:
/// everything that existed before the cycle, and the windows owned on the cycle's other
/// workspaces. Every round-trip window shares one app ID, as do both entries of a shared
/// profile layout, so without the latter each would be an unowned candidate on the wrong
/// workspace.
pub fn known_ids_besides(
    workspaces: &[SessionWorkspace],
    workspace_index: usize,
    preexisting: &HashSet<i64>,
) -> HashSet<i64> {
    let mut known_ids = preexisting.clone();
    for (other_index, other) in workspaces.iter().enumerate() {
        if other_index != workspace_index {
            known_ids.extend(other.accepted.keys());
        }
    }
    known_ids
}

/// Requires every one of `workspaces` to hold exactly its own accepted windows, as their
/// leaves, with process evidence, and no other window with their app ID anywhere.
pub fn verify_session_ownership(
    tree: &TreeNode,
    workspaces: &[SessionWorkspace],
    preexisting: &HashSet<i64>,
) -> Result<(), String> {
    for (workspace_index, workspace) in workspaces.iter().enumerate() {
        let known_ids = known_ids_besides(workspaces, workspace_index, preexisting);
        verify_ownership(
            tree,
            workspace.target.workspace.id,
            &workspace.layout.layout,
            &known_ids,
            &workspace.accepted,
        )
        .map_err(|error| format!("workspace {}: {error}", workspace.target.workspace.name))?;
    }
    Ok(())
}

/// The script Sway's `exec` runs to open one original window. It replaces itself with Foot,
/// so the window's process carries exactly the leaf's literal arguments.
pub fn window_launch_script(command: &[String]) -> String {
    let quoted_command: Vec<String> = command
        .iter()
        .map(|argument| shell_quote(argument))
        .collect();
    format!("#!/bin/sh\nexec {}\n", quoted_command.join(" "))
}

/// What the menu offers on the fixture's populated, capturable workspace once the original
/// workspaces are gone: capture of that workspace, capture of the session, in which it is
/// the only populated workspace, and the captured profile.
pub fn menu_rows() -> [String; 3] {
    [
        "Capture workspace layout…".to_owned(),
        "Capture session profile…".to_owned(),
        format!("Profile: {PROFILE_NAME}"),
    ]
}

/// Selecting `destination` while the original workspaces are opened: Sway creates it and
/// moves focus from `origin`, which it removes only when `origin_removed`, because the
/// origin was empty.
pub fn selection_events<'a>(
    origin: &'a WorkspaceIdentity,
    destination: &'a WorkspaceIdentity,
    origin_removed: bool,
) -> Vec<ExpectedWorkspaceEvent<'a>> {
    let mut expected = vec![
        ExpectedWorkspaceEvent::Init(destination),
        ExpectedWorkspaceEvent::Focus {
            from_id: origin.id,
            to: destination,
        },
    ];
    if origin_removed {
        expected.push(ExpectedWorkspaceEvent::Empty(origin));
    }
    expected
}

/// The reload's exact workspace events: each target created and selected in profile order,
/// first from the occupied `origin`, then from the previous target, which its new windows
/// keep. No workspace is removed, and none is recreated for a window.
pub fn reload_events<'a>(
    origin: &'a WorkspaceIdentity,
    targets: &'a [WorkspaceIdentity],
) -> Vec<ExpectedWorkspaceEvent<'a>> {
    let mut expected = Vec::new();
    let mut previous = origin;
    for target in targets {
        expected.extend(selection_events(previous, target, false));
        previous = target;
    }
    expected
}

/// Leaving `emptied` after its verified cleanup for the occupied `destination`: focus moves,
/// and Sway removes the empty workspace.
pub fn return_events<'a>(
    emptied: &'a WorkspaceIdentity,
    destination: &'a WorkspaceIdentity,
) -> Vec<ExpectedWorkspaceEvent<'a>> {
    vec![
        ExpectedWorkspaceEvent::Focus {
            from_id: emptied.id,
            to: destination,
        },
        ExpectedWorkspaceEvent::Empty(emptied),
    ]
}

/// Requires exactly the workspaces named `expected_names`, so no other workspace was created
/// or kept.
pub fn require_workspace_names(observed: &Snapshot, expected_names: &[&str]) -> Result<(), String> {
    let observed_names: BTreeSet<&str> = observed
        .workspaces
        .iter()
        .map(|workspace| workspace.name.as_str())
        .collect();
    let expected_names: BTreeSet<&str> = expected_names.iter().copied().collect();
    if observed_names != expected_names {
        return Err(format!(
            "expected exactly workspaces {expected_names:?}, observed {observed_names:?}"
        ));
    }
    Ok(())
}

/// Requires `directory` to hold exactly the regular files `expected_file_names`, so capture
/// wrote every expected file and nothing else.
pub fn require_directory_files(
    directory: &Path,
    expected_file_names: &[String],
) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("read {}: {error}", directory.display()))?;
    let mut observed_file_names = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("read {}: {error}", directory.display()))?;
        let is_file = entry
            .file_type()
            .map_err(|error| format!("inspect {}: {error}", entry.path().display()))?
            .is_file();
        let file_name = entry.file_name().into_string().map_err(|file_name| {
            format!("{}: non-UTF-8 entry {file_name:?}", directory.display())
        })?;
        if !is_file {
            return Err(format!(
                "{}: unexpected entry {file_name}",
                directory.display()
            ));
        }
        observed_file_names.insert(file_name);
    }

    let expected_file_names: BTreeSet<String> = expected_file_names.iter().cloned().collect();
    if observed_file_names != expected_file_names {
        return Err(format!(
            "{}: expected exactly {expected_file_names:?}, observed {observed_file_names:?}",
            directory.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "session_roundtrip_tests.rs"]
mod tests;
