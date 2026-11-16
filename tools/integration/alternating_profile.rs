//! A successful profile whose four entries alternate between the two outputs, starting on
//! the second output's existing, empty initial workspace and then creating one workspace on
//! each output in turn. Every layout must stay exactly as constructed while later entries
//! select workspaces on the other output and on its own. Every window shares one app ID,
//! so each entry's selectors match every earlier entry's windows, which it must not adopt.
//! Pure scenario data and decisions; lifecycle owns execution and guarded cleanup.

use crate::headless::{
    ALTERNATING_FIRST_OUTPUT_WORKSPACE, ALTERNATING_LAST_FIRST_OUTPUT_WORKSPACE,
    ALTERNATING_SECOND_OUTPUT_WORKSPACE, OUTPUT_NAME, SECOND_OUTPUT_INITIAL_WORKSPACE,
    SECOND_OUTPUT_NAME,
};
use crate::ipc::Output;
use crate::observation::WorkspaceIdentity;
use crate::output_expectation::OutputEntry;
use crate::profile::{ExpectedWorkspaceEvent, output_shows, wrap_launches};
use crate::scenarios::{Scenario, standard_scenarios, use_one_foot_app_id};

pub const SCENARIO: &str = "profile-alternating-outputs";

/// In profile order. The fixture's workspace, focused when the load starts, is on the first
/// output, so every selection changes outputs. Only the first target exists beforehand: it
/// is populated before Sway hides it, so its identity survives the whole load.
pub const ENTRIES: [OutputEntry; 4] = [
    OutputEntry {
        workspace: SECOND_OUTPUT_INITIAL_WORKSPACE,
        expected_output: SECOND_OUTPUT_NAME,
        layout_name: "profile-first",
        standard_layout: "horizontal-pair",
    },
    OutputEntry {
        workspace: ALTERNATING_FIRST_OUTPUT_WORKSPACE,
        expected_output: OUTPUT_NAME,
        layout_name: "profile-second",
        standard_layout: "vertical-pair",
    },
    // Weighted, on the smaller second output.
    OutputEntry {
        workspace: ALTERNATING_SECOND_OUTPUT_WORKSPACE,
        expected_output: SECOND_OUTPUT_NAME,
        layout_name: "profile-third",
        standard_layout: "uneven-siblings",
    },
    // Nested and weighted; its final focus is the profile's.
    OutputEntry {
        workspace: ALTERNATING_LAST_FIRST_OUTPUT_WORKSPACE,
        expected_output: OUTPUT_NAME,
        layout_name: "profile-fourth",
        standard_layout: "mixed-nesting",
    },
];

/// Each entry's layout, in entry order, with launches recorded by `wrap_launches`. Every
/// window has one app ID unique to the scenario and run.
pub fn layouts(
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
) -> Vec<Scenario> {
    let unique_id = format!("{run_id}-{SCENARIO}");
    let standard = standard_scenarios(&unique_id, foot, sleep);
    let shared_app_id = format!("sway-stage-it-{unique_id}-shared");

    let mut layouts = Vec::new();
    for entry in &ENTRIES {
        let mut layout = standard
            .iter()
            .find(|scenario| scenario.name == entry.standard_layout)
            .cloned()
            .expect("standard alternating-profile layout");
        layout.name = entry.layout_name;
        use_one_foot_app_id(&mut layout, &shared_app_id, foot, sleep);
        wrap_launches(
            &mut layout.layout,
            layout.name,
            launch_script,
            launch_record,
        );
        layouts.push(layout);
    }
    layouts
}

/// The load's exact workspace events, from the fixture's workspace through `targets` in
/// entry order. Sway announces each created target before focus reaches it. It removes
/// nothing: every workspace that focus leaves, and every workspace that a selection hides
/// on its output, is the fixture's or a completed layout's, and holds windows.
pub fn load_events<'a>(
    fixture_workspace: &'a WorkspaceIdentity,
    targets: &'a [WorkspaceIdentity],
) -> Vec<ExpectedWorkspaceEvent<'a>> {
    let mut expected = Vec::new();
    let mut previous = fixture_workspace;
    for (entry, target) in ENTRIES.iter().zip(targets) {
        if entry.workspace != SECOND_OUTPUT_INITIAL_WORKSPACE {
            expected.push(ExpectedWorkspaceEvent::Init(target));
        }
        expected.push(ExpectedWorkspaceEvent::Focus {
            from_id: previous.id,
            to: target,
        });
        previous = target;
    }
    expected
}

/// The workspace events of one cleanup return, in which focus moves from the emptied
/// `chain[destination_index + 1]` back to `chain[destination_index]`. `chain` is the
/// fixture's workspace followed by the targets in entry order, so consecutive workspaces
/// are on alternate outputs, and every workspace after the destination is already empty.
///
/// The workspace that focus leaves stays, because its own output still shows it. Sway
/// removes only the workspace the destination's output showed until now: the emptied one
/// two places later, if there is one.
pub fn cleanup_return_events(
    chain: &[WorkspaceIdentity],
    destination_index: usize,
) -> Vec<ExpectedWorkspaceEvent<'_>> {
    let mut expected = vec![ExpectedWorkspaceEvent::Focus {
        from_id: chain[destination_index + 1].id,
        to: &chain[destination_index],
    }];
    if let Some(hidden) = chain.get(destination_index + 2) {
        expected.push(ExpectedWorkspaceEvent::Empty(hidden));
    }
    expected
}

/// Once the load completes, each output shows the last target that the profile placed on it.
pub fn verify_shown_workspaces(outputs: &[Output]) -> Result<(), String> {
    for output_name in [OUTPUT_NAME, SECOND_OUTPUT_NAME] {
        let last_target_there = ENTRIES
            .iter()
            .rev()
            .find(|entry| entry.expected_output == output_name)
            .expect("the profile has entries on both outputs")
            .workspace;
        if !output_shows(outputs, output_name, last_target_there) {
            return Err(format!(
                "{output_name} does not show workspace {last_target_there}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "alternating_profile_tests.rs"]
mod tests;
