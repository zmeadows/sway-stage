//! A standalone load interrupted by its target workspace moving to the other output.
//!
//! The layout's second window is held by `held_launch::HOLD_SCRIPT`, so once the launch
//! record shows it, sway-stage has accepted the first window and waits with nothing able to
//! map. Only then does the harness move the target workspace, which the first window's
//! exact container ID names, to the other output over its own connection to the private
//! compositor. In Sway 1.10.1 that move keeps the workspace focused and emits no focus
//! event: the window it holds is already the seat's focus. So sway-stage must stop for the
//! output change itself, not for a focus loss, and the third window never launches. The
//! decisions here are pure; lifecycle runs the scenario.

use crate::headless::{OUTPUT_NAME, SECOND_OUTPUT_NAME};
use crate::ipc::Output;
use crate::observation::{Snapshot, WorkspaceIdentity, walk};
use crate::preservation::output_ancestry;
use crate::process::CompletedProcess;
use crate::profile::{ExpectedWorkspaceEvent, output_shows, wrap_launches};
use crate::scenarios::{ConfiguredNode, Scenario, standard_scenarios};

pub const SCENARIO: &str = "interference-output-migration";

/// No rule places this workspace, so Sway creates it on the focused first output.
pub const TARGET_WORKSPACE: &str = "9";
/// Where the target starts, and where the harness moves it.
pub const ORIGINAL_OUTPUT: &str = OUTPUT_NAME;
pub const DESTINATION_OUTPUT: &str = SECOND_OUTPUT_NAME;

/// The window whose launch is held while the harness moves the target.
pub const HELD_LAYOUT_ID: &str = "window2";

/// A three-window layout with its second launch held, and what the harness expects of it.
pub struct MigratingLoad {
    /// What the configuration asks sway-stage to build.
    pub layout: Scenario,
    /// The first window alone, focused: everything sway-stage accepts before the move.
    pub accepted_before_move: Scenario,
    /// The launch record once the held launch has started: the first two windows, in order.
    pub launch_record_through_hold: String,
}

/// `uneven-siblings`, three Foot windows in one weighted horizontal split, with the second
/// window's command prefixed by `hold_prefix`, from `held_launch::hold_command`. Every launch
/// is recorded through `launch_script`.
pub fn migrating_load(
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
    hold_prefix: &[String],
) -> MigratingLoad {
    let unique_id = format!("{run_id}-{SCENARIO}");
    let mut layout = standard_scenarios(&unique_id, foot, sleep)
        .into_iter()
        .find(|scenario| scenario.name == "uneven-siblings")
        .expect("standard uneven-siblings scenario");
    layout.name = SCENARIO;

    let ConfiguredNode::Group { children, .. } = &mut layout.layout else {
        unreachable!("uneven-siblings has a split root")
    };
    let held_window = children[1].first_leaf_mut();
    let mut held_command = hold_prefix.to_vec();
    held_command.append(&mut held_window.command);
    held_window.command = held_command;
    wrap_launches(
        &mut layout.layout,
        layout.name,
        launch_script,
        launch_record,
    );

    let leaves = layout.layout.leaves();
    let launch_record_through_hold = leaves[..2]
        .iter()
        .map(|leaf| format!("{}-{}\n", layout.name, leaf.layout_id))
        .collect();
    let first_window = leaves[0].clone();

    let mut accepted_before_move = layout.clone();
    accepted_before_move.layout = ConfiguredNode::Window(first_window);
    accepted_before_move.expected_focus = "window1";
    accepted_before_move.configured_focus = Some("window1");

    MigratingLoad {
        layout,
        accepted_before_move,
        launch_record_through_hold,
    }
}

/// The two outputs' container IDs and the empty workspace the destination output shows, as
/// a well-defined starting point that does not depend on earlier scenarios: an earlier
/// cross-output profile may have replaced the second output's initial workspace and left
/// focus on its replacement.
pub struct MigrationBaseline {
    pub original_output_id: i64,
    pub destination_output_id: i64,
    /// Hidden by the move, so Sway removes it.
    pub destination_shown_workspace: WorkspaceIdentity,
}

impl MigrationBaseline {
    /// Requires exactly two workspaces: the fixture's on the original output, and one empty
    /// workspace that the destination output shows. Focus may be on either.
    pub fn record(
        observed: &Snapshot,
        outputs: &[Output],
        fixture_workspace: &WorkspaceIdentity,
    ) -> Result<Self, String> {
        observed.validate()?;

        let workspace_names: Vec<&str> = observed
            .workspaces
            .iter()
            .map(|workspace| workspace.name.as_str())
            .collect();
        let has_fixture = observed
            .workspaces
            .iter()
            .any(|workspace| workspace.id == fixture_workspace.id);
        let others: Vec<_> = observed
            .workspaces
            .iter()
            .filter(|workspace| workspace.id != fixture_workspace.id)
            .collect();
        let [other] = others.as_slice() else {
            return Err(format!(
                "the output migration starts with the fixture's workspace and one other, \
                 found {workspace_names:?}"
            ));
        };
        if !has_fixture {
            return Err(format!(
                "the fixture's workspace is missing, found {workspace_names:?}"
            ));
        }

        let original_output_id = single_output(observed, fixture_workspace.id, ORIGINAL_OUTPUT)?;
        let destination_output_id = single_output(observed, other.id, DESTINATION_OUTPUT)?;
        let other_node = walk(&observed.tree)
            .into_iter()
            .map(|placed| placed.node)
            .find(|node| node.id == other.id)
            .expect("snapshot validation matched every workspace to the tree");
        if !other_node.nodes.is_empty() || !other_node.floating_nodes.is_empty() {
            return Err(format!("workspace {:?} must be empty", other.name));
        }
        if !output_shows(outputs, DESTINATION_OUTPUT, &other.name) {
            return Err(format!(
                "{DESTINATION_OUTPUT} must show workspace {:?}",
                other.name
            ));
        }

        Ok(Self {
            original_output_id,
            destination_output_id,
            destination_shown_workspace: WorkspaceIdentity {
                id: other.id,
                name: other.name.clone(),
            },
        })
    }
}

/// The container ID of the output named `output_name`, the only output above `workspace_id`.
pub fn single_output(
    observed: &Snapshot,
    workspace_id: i64,
    output_name: &str,
) -> Result<i64, String> {
    match output_ancestry(&observed.tree, workspace_id)?.as_slice() {
        [(output_id, Some(name))] if name == output_name => Ok(*output_id),
        ancestry => Err(format!(
            "workspace id={workspace_id} is on {ancestry:?}, expected {output_name}"
        )),
    }
}

/// The move's exact workspace events. Sway removes the empty workspace that the destination
/// output showed, which the target now replaces there, then reports the move. The original
/// output still holds the fixture's workspace, so Sway creates none there, and focus stays
/// where it was.
pub fn move_events<'a>(
    destination_shown_workspace: &'a WorkspaceIdentity,
    target: &'a WorkspaceIdentity,
) -> [ExpectedWorkspaceEvent<'a>; 2] {
    [
        ExpectedWorkspaceEvent::Empty(destination_shown_workspace),
        ExpectedWorkspaceEvent::Move(target),
    ]
}

/// Which of sway-stage's two output-change checks stopped it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputChangeReport {
    /// It read the target's move event first.
    MoveEvent,
    /// A snapshot showed the target under the other output first.
    Snapshot,
}

/// sway-stage's report: exit status 1, no stdout, the second window's wait stopped because
/// the target workspace changed outputs, and the stop advice that follows a started
/// application. sway-stage may read the move event first, or a snapshot may show the new
/// output first; either names the exact target, and the snapshot names both output IDs. A
/// focus loss, window timeout, or any other failure is never the expected one.
pub fn verify_failure_report(
    output: &CompletedProcess,
    target_workspace_id: i64,
    original_output_id: i64,
    destination_output_id: i64,
) -> Result<OutputChangeReport, String> {
    let stderr = output.stderr.as_str();
    // sway-stage names the failing stage, then the layout, then the cause.
    let wait_context = format!("wait leaf={HELD_LAYOUT_ID:?}: layout {SCENARIO:?}: ");
    let move_event =
        format!("{wait_context}target workspace id={target_workspace_id} moved to another output");
    let output_snapshot = format!(
        "{wait_context}snapshot: target workspace id={target_workspace_id} changed output from \
         id={original_output_id} to id={destination_output_id}"
    );
    let reported = match (
        stderr.contains(&move_event),
        stderr.contains(&output_snapshot),
    ) {
        (true, false) => Some(OutputChangeReport::MoveEvent),
        (false, true) => Some(OutputChangeReport::Snapshot),
        _ => None,
    };
    let other_failure = ["focus changed", "focus left", "window timeout", "preflight"]
        .iter()
        .any(|unexpected| stderr.contains(unexpected));
    let stopped_without_cleanup = stderr.contains("stopped without cleanup or relaunch");

    let stopped_as_expected = output.status.code() == Some(1)
        && output.stdout.trim().is_empty()
        && !other_failure
        && stopped_without_cleanup;
    match reported {
        Some(report) if stopped_as_expected => Ok(report),
        _ => Err(format!(
            "expected the target's output change during {HELD_LAYOUT_ID}'s wait (exit 1), got \
             {}: {}",
            output.status,
            stderr.trim()
        )),
    }
}

#[cfg(test)]
#[path = "output_migration_tests.rs"]
mod tests;
