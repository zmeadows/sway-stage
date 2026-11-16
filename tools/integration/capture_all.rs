//! `profile capture NAME --all` of a real desktop across both outputs.
//!
//! The harness populates two workspaces beside the fixture's numeric `1`, each with a
//! layout that sway-stage loads and the harness verifies: first the exact name `notes`
//! on the first output, then the numeric `3`, which Sway's rule creates on the second.
//! In creation order, and in the tree's output order, `notes` comes before `3`; the profile
//! must instead list numbers first. One capture saves `3` while the second output shows it
//! and focus stays on `1`, so its selected window comes only from Sway's child-focus
//! history. Another capture adds the empty `2`, shown on the second output, which `--all`
//! must skip.
//!
//! Capture only reads Sway's tree. The harness compares what it writes with its own
//! measurements and requires the desktop unchanged. The decisions here are pure; lifecycle
//! owns execution and guarded cleanup.

use std::path::{Path, PathBuf};

use crate::headless::{
    FIRST_OUTPUT_INITIAL_WORKSPACE, SECOND_OUTPUT_ASSIGNED_WORKSPACE,
    SECOND_OUTPUT_INITIAL_WORKSPACE,
};
use crate::observation::{Snapshot, WorkspaceIdentity, walk};
use crate::process::CompletedProcess;
use crate::profile::{ExpectedWorkspaceEvent, wrap_launches};
use crate::scenarios::{Scenario, standard_scenarios};

pub const SCENARIO: &str = "profile-capture-all-outputs";

/// Both captures write a profile with this name, each into its own directories.
pub const PROFILE_NAME: &str = "captured-all";

/// An exact name, which Sway numbers -1. No rule places it, so Sway creates it on the
/// focused first output.
pub const NAMED_WORKSPACE: &str = "notes";
/// Sway's rule creates this numeric workspace on the second output.
pub const SECOND_OUTPUT_WORKSPACE: &str = SECOND_OUTPUT_ASSIGNED_WORKSPACE;
/// Sway's rule creates this one on the second output too. It stays empty and shown there
/// during the second capture.
pub const EMPTY_WORKSPACE: &str = SECOND_OUTPUT_INITIAL_WORKSPACE;

/// Both captures' targets in profile order: the fixture's workspace, then the other numeric
/// workspace, then the name, though the harness creates the name first.
pub const EXPECTED_TARGETS: [&str; 3] = [
    FIRST_OUTPUT_INITIAL_WORKSPACE,
    SECOND_OUTPUT_WORKSPACE,
    NAMED_WORKSPACE,
];

/// The populated workspaces' layouts: `[notes, 3]`, each recorded in `launch_record`. Both
/// select a window other than their first, so a guessed first window differs from the
/// selection that capture must read from Sway's focus history.
pub fn layouts(
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
) -> [Scenario; 2] {
    let unique_id = format!("{run_id}-{SCENARIO}");
    let standard = standard_scenarios(&unique_id, foot, sleep);
    let take_layout = |standard_name: &str, layout_name: &'static str| {
        let mut layout = standard
            .iter()
            .find(|scenario| scenario.name == standard_name)
            .cloned()
            .expect("a standard capture-all layout");
        layout.name = layout_name;
        wrap_launches(
            &mut layout.layout,
            layout.name,
            launch_script,
            launch_record,
        );
        layout
    };

    [
        // Two weighted columns, with focus on the first window of the second.
        take_layout("sibling-nested-groups", "capture-all-named"),
        // A weighted split holding tabs, whose second tab, a nested split, holds the focus.
        take_layout("mixed-native-nesting", "capture-all-second-output"),
    ]
}

/// An independent reading of what `--all` must capture, in profile order: every populated
/// ordinary workspace, numeric ones by number, then exact names in byte order. Each is
/// named as a profile target: its number, or its exact name. `get_workspaces` never
/// reports Sway's hidden scratchpad workspace, and an empty workspace is skipped.
pub fn expected_capture_targets(observed: &Snapshot) -> Result<Vec<String>, String> {
    let mut numeric_targets = Vec::new();
    let mut named_targets = Vec::new();
    for workspace in &observed.workspaces {
        let node = walk(&observed.tree)
            .into_iter()
            .map(|placed| placed.node)
            .find(|node| node.id == workspace.id)
            .ok_or_else(|| format!("workspace {:?} is missing from the tree", workspace.name))?;
        let populated = !node.nodes.is_empty() || !node.floating_nodes.is_empty();
        if !populated {
            continue;
        }

        if workspace.num >= 0 {
            numeric_targets.push(workspace.num);
        } else {
            named_targets.push(workspace.name.clone());
        }
    }

    numeric_targets.sort_unstable();
    named_targets.sort_unstable();
    let mut targets: Vec<String> = numeric_targets
        .iter()
        .map(|number| number.to_string())
        .collect();
    targets.extend(named_targets);
    Ok(targets)
}

/// Selecting the second output's workspace from `named` on the first output. Sway creates it
/// there unless it already is `shown_before`, the empty workspace the second output showed
/// at the start, which an earlier cross-output profile leaves as `3`. Otherwise that empty
/// workspace is now hidden, so Sway removes it. `named` stays shown on the first output.
pub fn second_output_selection_events<'a>(
    named: &'a WorkspaceIdentity,
    second_output: &'a WorkspaceIdentity,
    shown_before: &'a WorkspaceIdentity,
) -> Vec<ExpectedWorkspaceEvent<'a>> {
    let focus = ExpectedWorkspaceEvent::Focus {
        from_id: named.id,
        to: second_output,
    };
    if shown_before == second_output {
        return vec![focus];
    }
    vec![
        ExpectedWorkspaceEvent::Init(second_output),
        focus,
        ExpectedWorkspaceEvent::Empty(shown_before),
    ]
}

/// Cleanup's first selection: the second output's populated workspace, from the fixture's
/// workspace on the first. It hides the empty workspace, which Sway removes.
pub fn cleanup_selection_events<'a>(
    fixture_workspace: &'a WorkspaceIdentity,
    second_output: &'a WorkspaceIdentity,
    empty: &'a WorkspaceIdentity,
) -> Vec<ExpectedWorkspaceEvent<'a>> {
    vec![
        ExpectedWorkspaceEvent::Focus {
            from_id: fixture_workspace.id,
            to: second_output,
        },
        ExpectedWorkspaceEvent::Empty(empty),
    ]
}

/// The captured files' paths, in sway-stage's publication order: the layouts, then the
/// profile.
pub fn captured_file_paths(
    layouts_directory: &Path,
    profiles_directory: &Path,
    layout_names: &[String],
) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = layout_names
        .iter()
        .map(|layout_name| layouts_directory.join(format!("{layout_name}.yaml")))
        .collect();
    paths.push(profiles_directory.join(format!("{PROFILE_NAME}.yaml")));
    paths
}

/// A successful inference-disabled capture prints nothing on stdout. On stderr it names the
/// profile, reports every command unresolved, and lists exactly the files it published, in
/// order.
pub fn verify_capture_report(
    completed: &CompletedProcess,
    published_paths: &[PathBuf],
    unresolved_commands: usize,
) -> Result<(), String> {
    if !completed.status.success() {
        return Err(format!(
            "profile capture failed: {}: {}",
            completed.status,
            completed.stderr.trim()
        ));
    }
    if !completed.stdout.is_empty() {
        return Err("profile capture emitted unexpected stdout".into());
    }

    let mut expected_report = format!(
        "sway-stage: captured profile {PROFILE_NAME:?}. Command inference disabled; \
         unresolved commands: {unresolved_commands}. Manual editing required.\n"
    );
    for path in published_paths {
        expected_report.push_str(&format!("  {}\n", path.display()));
    }
    if completed.stderr != expected_report {
        return Err(format!(
            "profile capture report differs: expected {expected_report:?}, observed {:?}",
            completed.stderr
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "capture_all_tests.rs"]
mod tests;
