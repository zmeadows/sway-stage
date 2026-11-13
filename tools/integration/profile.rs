//! Independent profile fixtures and observation decisions. No production parser or
//! construction code is used here; lifecycle owns execution and guarded cleanup.

use std::collections::HashSet;

use crate::ipc::{TreeNode, WindowChange, WindowEvent, workspace_number};
use crate::observation::{Failure, Snapshot, WorkspaceIdentity, walk};
use crate::ownership::{AcceptedWindows, bind_launches, require_quiet};
use crate::preservation::output_ancestry;
use crate::scenarios::{Scenario, foreign_fixture_scenario, standard_scenarios};

pub const CASES: [Case; 3] = [Case::Success, Case::Occupied, Case::Partial];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Case {
    Success,
    Occupied,
    Partial,
}

impl Case {
    pub fn name(self) -> &'static str {
        match self {
            Self::Success => "profile-success",
            Self::Occupied => "profile-occupied-refusal",
            Self::Partial => "profile-partial-failure",
        }
    }
}

pub fn workspace_eligibility(primary: &str, foreign: &str, second: &str) -> Result<(), String> {
    for number in [primary, foreign, second] {
        workspace_number(number)?;
    }
    if primary == foreign || primary == second || foreign == second {
        return Err("primary, foreign, and profile workspaces must all differ".into());
    }
    Ok(())
}

/// The wrapper records *launches*, including launches that never create a window. It
/// preserves the configured executable and every argument through a literal exec.
pub const LAUNCH_SCRIPT: &str =
    "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$2\" || exit 1\nshift 2\nexec \"$@\"\n";

pub fn layouts(
    case: Case,
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
) -> [Scenario; 2] {
    let unique_id = format!("{run_id}-{}", case.name());
    let mut pairs = standard_scenarios(&unique_id, foot, sleep).into_iter();
    let mut first = pairs
        .find(|scenario| scenario.name == "horizontal-pair")
        .expect("standard horizontal-pair scenario");
    let mut second = pairs
        .find(|scenario| scenario.name == "vertical-pair")
        .expect("standard vertical-pair scenario");
    first.name = "profile-first";
    second.name = "profile-second";
    if case == Case::Partial {
        second = foreign_fixture_scenario(&unique_id, foot, sleep);
        second.name = "profile-second";
        second.window_timeout_ms = 500;
        second.layout.first_leaf_mut().command = vec![sleep.into(), "0".into()];
    }

    for scenario in [&mut first, &mut second] {
        wrap_launches(
            &mut scenario.layout,
            scenario.name,
            launch_script,
            launch_record,
        );
    }
    [first, second]
}

fn wrap_launches(
    node: &mut crate::scenarios::ConfiguredNode,
    name: &str,
    script: &str,
    record: &str,
) {
    match node {
        crate::scenarios::ConfiguredNode::Window(window) => {
            let mut command = vec![
                "/bin/sh".into(),
                script.into(),
                format!("{name}-{}", window.layout_id),
                record.into(),
            ];
            command.append(&mut window.command);
            window.command = command;
        }
        crate::scenarios::ConfiguredNode::Group { children, .. } => {
            for child in children {
                wrap_launches(child, name, script, record);
            }
        }
    }
}

pub fn configuration(primary: &str, second: &str) -> String {
    format!(
        "version: 1\nworkspaces:\n  - workspace: {primary}\n    layout: profile-first\n  - workspace: {second}\n    layout: profile-second\n"
    )
}

/// Existing targets keep their identity/output; missing ones may be created on any
/// existing output according to the user's Sway rules. Move events are rejected separately.
pub struct Target {
    pub workspace: WorkspaceIdentity,
    output: Vec<(i64, Option<String>)>,
}

impl Target {
    pub fn record(observed: &Snapshot, number: &str) -> Result<Option<Self>, Failure> {
        let candidates: Vec<_> = observed
            .workspaces
            .iter()
            .filter(|workspace| workspace.num.to_string() == number || workspace.name == number)
            .collect();
        match candidates.as_slice() {
            [] => Ok(None),
            [workspace] if workspace.name == number => Ok(Some(Self {
                workspace: WorkspaceIdentity {
                    id: workspace.id,
                    name: workspace.name.clone(),
                },
                output: output_ancestry(&observed.tree, workspace.id)?,
            })),
            _ => Err(Failure::Uncertain(
                "ambiguous or renamed profile workspace".into(),
            )),
        }
    }

    pub fn verify<'a>(&self, observed: &'a Snapshot) -> Result<&'a TreeNode, Failure> {
        let workspace = walk(&observed.tree)
            .into_iter()
            .map(|placed| placed.node)
            .find(|node| {
                node.id == self.workspace.id && node.name.as_ref() == Some(&self.workspace.name)
            })
            .ok_or_else(|| {
                Failure::Uncertain("profile target identity changed/disappeared".into())
            })?;
        if output_ancestry(&observed.tree, workspace.id)? != self.output {
            return Err(Failure::Uncertain(
                "profile target output ancestry changed".into(),
            ));
        }
        Ok(workspace)
    }
}

/// Workspace switches may change global focus and focus-stack metadata. Every identity,
/// wrapper, window state, and rectangle must otherwise survive; selection is verified
/// separately with the independent layout model and the accepted focus events.
pub fn require_preserved_layout(before: &TreeNode, after: &TreeNode) -> Result<(), Failure> {
    let mut baseline = crate::layout::signature(before);
    let mut current = crate::layout::signature(after);
    baseline.ignore_focus();
    current.ignore_focus();
    if baseline != current {
        return Err(Failure::Uncertain(
            "profile layout changed across cleanup workspace switch".into(),
        ));
    }
    Ok(())
}

/// Bind each side of the sole expected switch independently. In particular, a second
/// layout cannot launch before that switch, and the first cannot receive later activity.
pub fn bind_ordered_launches(
    events: &[WindowEvent],
    switch_index: usize,
    layouts: &[Scenario; 2],
    preexisting: &HashSet<i64>,
    partial: bool,
) -> Result<[AcceptedWindows; 2], Failure> {
    let (first_events, second_events) = events
        .split_at_checked(switch_index)
        .ok_or_else(|| Failure::Uncertain("invalid profile transition boundary".into()))?;
    let first = bind_launches(first_events, &layouts[0].layout, preexisting)?;
    let expected_focus = first
        .iter()
        .find(|(_, leaf)| leaf.as_str() == layouts[0].expected_focus)
        .map(|(&id, _)| id);
    let last_focus = first_events
        .iter()
        .rev()
        .find(|event| event.change == WindowChange::Focus)
        .map(|event| event.container.id);
    if last_focus != expected_focus {
        return Err(Failure::Uncertain(
            "first profile layout did not select its final focus before switching".into(),
        ));
    }

    let mut before_second = preexisting.clone();
    before_second.extend(first.keys());
    let second = if partial {
        require_quiet(second_events, None)?;
        AcceptedWindows::new()
    } else {
        bind_launches(second_events, &layouts[1].layout, &before_second)?
    };
    Ok([first, second])
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
