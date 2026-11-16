//! Two-output profiles whose entries name the output they expect. sway-stage must refuse a
//! target on another output, or an output that is not there, during the read-only preflight
//! of every target; refuse a target that Sway creates on another output right after selecting
//! it, before any of its launches; and select an existing workspace on the other output
//! without replacing it. Pure scenario data and decisions; lifecycle owns execution and
//! guarded cleanup.

use crate::headless::{OUTPUT_NAME, SECOND_OUTPUT_INITIAL_WORKSPACE, SECOND_OUTPUT_NAME};
use crate::ipc::{NodeType, Output};
use crate::observation::{Snapshot, WorkspaceIdentity, walk};
use crate::process::CompletedProcess;
use crate::profile::{ExpectedWorkspaceEvent, wrap_launches};
use crate::scenarios::{Scenario, standard_scenarios};

/// Absent workspaces that no Sway rule places, so Sway creates them on the focused output.
/// Every case that creates them starts on the fixture's workspace, on the first output.
pub const UNPLACED_WORKSPACE: &str = "4";
pub const SECOND_UNPLACED_WORKSPACE: &str = "5";
/// An output that no headless session has.
pub const UNAVAILABLE_OUTPUT: &str = "HEADLESS-9";

/// In suite order. Each case starts and ends in the two-output baseline, so any selection of
/// them can run in this order before the cross-output profile.
pub const CASES: [Case; 5] = [
    Case::PreflightMismatch,
    Case::PreflightUnavailable,
    Case::CreatedMismatch,
    Case::LaterCreatedMismatch,
    Case::ExistingOtherOutput,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    /// The second entry expects the first output for the second output's existing initial
    /// workspace. The valid first entry must not be selected either.
    PreflightMismatch,
    /// The second entry expects an output the compositor does not have for an absent
    /// workspace. The valid first entry must not be selected either.
    PreflightUnavailable,
    /// The only entry expects the second output for an absent workspace, which Sway creates
    /// on the focused first output.
    CreatedMismatch,
    /// The first entry completes on a workspace created on the first output, where Sway then
    /// also creates the second entry's target, which expects the second output.
    LaterCreatedMismatch,
    /// The only entry selects the second output's existing, empty initial workspace there.
    ExistingOtherOutput,
}

/// How sway-stage must end a case's load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Refused by the read-only check of every target, before any selection.
    PreflightRefusal,
    /// Refused right after selecting the last entry's target, before any of its launches.
    SelectionRefusal,
    Success,
}

/// One profile entry: its target, the output it expects, and its layout.
#[derive(Debug, PartialEq, Eq)]
pub struct OutputEntry {
    pub workspace: &'static str,
    pub expected_output: &'static str,
    /// The layout name the profile references.
    pub layout_name: &'static str,
    /// The standard scenario whose layout that is.
    pub standard_layout: &'static str,
}

/// Valid wherever it appears: an absent workspace that Sway creates on the focused first
/// output, which the entry expects.
const FIRST_OUTPUT_ENTRY: OutputEntry = OutputEntry {
    workspace: UNPLACED_WORKSPACE,
    expected_output: OUTPUT_NAME,
    layout_name: "profile-first",
    standard_layout: "horizontal-pair",
};

impl Case {
    pub fn name(self) -> &'static str {
        match self {
            Self::PreflightMismatch => "profile-output-mismatch-refusal",
            Self::PreflightUnavailable => "profile-output-unavailable-refusal",
            Self::CreatedMismatch => "profile-created-output-mismatch",
            Self::LaterCreatedMismatch => "profile-later-created-output-mismatch",
            Self::ExistingOtherOutput => "profile-existing-other-output",
        }
    }

    pub fn outcome(self) -> Outcome {
        match self {
            Self::PreflightMismatch | Self::PreflightUnavailable => Outcome::PreflightRefusal,
            Self::CreatedMismatch | Self::LaterCreatedMismatch => Outcome::SelectionRefusal,
            Self::ExistingOtherOutput => Outcome::Success,
        }
    }

    /// The entries in profile order. A refused case fails at its last entry.
    pub fn entries(self) -> &'static [OutputEntry] {
        match self {
            Self::PreflightMismatch => &[
                FIRST_OUTPUT_ENTRY,
                OutputEntry {
                    workspace: SECOND_OUTPUT_INITIAL_WORKSPACE,
                    expected_output: OUTPUT_NAME,
                    layout_name: "profile-second",
                    standard_layout: "vertical-pair",
                },
            ],
            Self::PreflightUnavailable => &[
                FIRST_OUTPUT_ENTRY,
                OutputEntry {
                    workspace: SECOND_UNPLACED_WORKSPACE,
                    expected_output: UNAVAILABLE_OUTPUT,
                    layout_name: "profile-second",
                    standard_layout: "vertical-pair",
                },
            ],
            Self::CreatedMismatch => &[OutputEntry {
                workspace: UNPLACED_WORKSPACE,
                expected_output: SECOND_OUTPUT_NAME,
                layout_name: "profile-first",
                standard_layout: "horizontal-pair",
            }],
            Self::LaterCreatedMismatch => &[
                FIRST_OUTPUT_ENTRY,
                OutputEntry {
                    workspace: SECOND_UNPLACED_WORKSPACE,
                    expected_output: SECOND_OUTPUT_NAME,
                    layout_name: "profile-second",
                    standard_layout: "vertical-pair",
                },
            ],
            // Three weighted windows measure sizing on the smaller second output.
            Self::ExistingOtherOutput => &[OutputEntry {
                workspace: SECOND_OUTPUT_INITIAL_WORKSPACE,
                expected_output: SECOND_OUTPUT_NAME,
                layout_name: "profile-first",
                standard_layout: "uneven-siblings",
            }],
        }
    }

    /// The entry that sway-stage refuses, or `None` when the case succeeds.
    pub fn refused_entry(self) -> Option<&'static OutputEntry> {
        if self.outcome() == Outcome::Success {
            return None;
        }
        self.entries().last()
    }
}

/// Each entry's layout, in entry order, with launches recorded by `wrap_launches` and app IDs
/// unique to the case and run.
pub fn layouts(
    case: Case,
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
) -> Vec<Scenario> {
    let unique_id = format!("{run_id}-{}", case.name());
    let standard = standard_scenarios(&unique_id, foot, sleep);

    let mut layouts = Vec::new();
    for entry in case.entries() {
        let mut layout = standard
            .iter()
            .find(|scenario| scenario.name == entry.standard_layout)
            .cloned()
            .expect("standard output-expectation layout");
        layout.name = entry.layout_name;
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

/// The case's profile file.
pub fn configuration(case: Case) -> String {
    profile_configuration(case.entries())
}

/// A profile file with each entry's numeric target, layout, and expected output.
pub fn profile_configuration(entries: &[OutputEntry]) -> String {
    let mut yaml = "version: 1\nworkspaces:\n".to_owned();
    for entry in entries {
        yaml.push_str(&format!(
            "  - workspace: {}\n    layout: {}\n    output: {}\n",
            entry.workspace, entry.layout_name, entry.expected_output
        ));
    }
    yaml
}

/// The workspace events of a load that leaves the fixture's workspace for each of `targets`
/// in turn. A created target is announced before focus reaches it. No workspace is removed:
/// the fixture's keeps its windows, an earlier target keeps its layout, and a refused target
/// stays focused. An existing target on the other output does not hide the fixture's.
pub fn selection_events<'a>(
    case: Case,
    fixture_workspace: &'a WorkspaceIdentity,
    targets: &'a [WorkspaceIdentity],
) -> Vec<ExpectedWorkspaceEvent<'a>> {
    let targets_are_created = case != Case::ExistingOtherOutput;

    let mut expected = Vec::new();
    let mut previous = fixture_workspace;
    for target in targets {
        if targets_are_created {
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

/// `UNAVAILABLE_OUTPUT` is neither among the compositor's outputs nor in its tree.
pub fn require_unavailable_output(observed: &Snapshot, outputs: &[Output]) -> Result<(), String> {
    let listed = outputs
        .iter()
        .any(|output| output.name == UNAVAILABLE_OUTPUT);
    let in_tree = walk(&observed.tree).iter().any(|placed| {
        placed.node.node_type == NodeType::Output
            && placed.node.name.as_deref() == Some(UNAVAILABLE_OUTPUT)
    });
    if listed || in_tree {
        return Err(format!(
            "output {UNAVAILABLE_OUTPUT} must not exist in the two-output session"
        ));
    }
    Ok(())
}

/// sway-stage's report of a refused case: exit 1 and nothing on stdout. Stderr names the
/// refused entry, exactly the `completed_targets` that received their layouts, the phase that
/// refused it, the profile, its specific output classification with the outputs it names, and
/// whether any application launched. The other phase or output classification, or an
/// occupancy refusal, is a wrong answer, even beside the right one.
pub fn verify_refusal_report(
    case: Case,
    process: &CompletedProcess,
    completed_targets: &[WorkspaceIdentity],
) -> Result<(), String> {
    let refused = case
        .refused_entry()
        .expect("only refused cases have a refusal report");
    let workspace = refused.workspace;
    let expected_output = refused.expected_output;

    let (phase, wrong_phase) = match case.outcome() {
        Outcome::PreflightRefusal => ("workspace preflight", "workspace selection"),
        Outcome::SelectionRefusal => ("workspace selection", "workspace preflight"),
        Outcome::Success => unreachable!("a successful case has no refused entry"),
    };
    let completed_list = completed_targets
        .iter()
        .map(|target| format!("({}, {})", target.name, target.id))
        .collect::<Vec<_>>()
        .join(", ");
    let progress = format!(
        "profile load: workspace {workspace}, layout {:?}; completed {} workspaces \
         [{completed_list}]; {phase}: profile {:?}: ",
        refused.layout_name,
        completed_targets.len(),
        case.name()
    );

    // A target found on the wrong output names both; an absent one only the expected output.
    let (classification, wrong_classification) = match case {
        Case::PreflightMismatch => (
            format!(
                "profile output mismatch: workspace {workspace}: expected output \
                 {expected_output:?}, observed output {SECOND_OUTPUT_NAME:?};"
            ),
            "profile output unavailable",
        ),
        Case::PreflightUnavailable => (
            format!(
                "profile output unavailable: workspace {workspace}: expected output \
                 {expected_output:?} is not active; workspace does not yet exist;"
            ),
            "profile output mismatch",
        ),
        Case::CreatedMismatch | Case::LaterCreatedMismatch => (
            format!(
                "profile output mismatch: workspace {workspace}: expected output \
                 {expected_output:?}, observed output {OUTPUT_NAME:?};"
            ),
            "profile output unavailable",
        ),
        Case::ExistingOtherOutput => unreachable!("a successful case has no refused entry"),
    };

    let launch_advice_correct = if completed_targets.is_empty() {
        process.stderr.contains("; no applications launched")
            && !process
                .stderr
                .contains("application launches may have left")
    } else {
        process
            .stderr
            .contains("; application launches may have left partial layouts")
            && process
                .stderr
                .contains("; stopped without cleanup or relaunch")
    };
    let refused_as_expected = process.status.code() == Some(1)
        && process.stdout.trim().is_empty()
        && process.stderr.contains(&progress)
        && process.stderr.contains(&classification)
        && !process.stderr.contains(wrong_classification)
        && !process.stderr.contains(wrong_phase)
        && !process.stderr.contains("profile target occupied")
        && launch_advice_correct;
    if !refused_as_expected {
        return Err(format!(
            "expected {progress:?} and {classification:?} (exit 1), got {}: {}",
            process.status,
            process.stderr.trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "output_expectation_tests.rs"]
mod tests;
