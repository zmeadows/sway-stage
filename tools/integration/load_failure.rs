//! Standalone loads that must fail partway: one whose second window never appears, and one
//! whose first executable does not exist. Either failure counts as a pass only after the
//! harness has independently verified sway-stage's report, which applications started, and
//! that everything already on the desktop was left in place. The decisions here are pure;
//! lifecycle runs the cases against the private compositor.

use crate::process::CompletedProcess;
use crate::profile::wrap_launches;
use crate::scenarios::{ConfiguredNode, Scenario, standard_scenarios};

pub const CASES: [Case; 2] = [Case::PartialLaunch, Case::MissingExecutable];

/// The partial case's window wait. Its second window never appears, so the load always
/// waits this long; Foot opens the first window well within it.
pub const PARTIAL_WINDOW_TIMEOUT_MS: u64 = 5_000;

/// The file name of the missing case's executable, inside a private directory that the
/// runner creates and verifies does not contain it.
pub const MISSING_EXECUTABLE_FILE_NAME: &str = "missing-application";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    /// Three windows; the second runs a command that opens no window, so the load stops at
    /// its window deadline with the first window open and the third never started.
    PartialLaunch,
    /// Two windows; the first names an executable that does not exist, so its spawn fails
    /// before any application starts.
    MissingExecutable,
}

impl Case {
    pub fn name(self) -> &'static str {
        match self {
            Self::PartialLaunch => "layout-partial-failure",
            Self::MissingExecutable => "layout-missing-executable",
        }
    }
}

/// A layout that sway-stage must fail to construct, and what it must leave behind.
pub struct FailingLoad {
    /// What the configuration asks sway-stage to build.
    pub layout: Scenario,
    /// The windows sway-stage accepts before failing, as a layout of their own: the first
    /// window alone in the partial case, and none when the first spawn fails.
    pub accepted_part: Option<Scenario>,
    /// What the launch recorder must hold afterwards: one line per started launch, in order.
    pub expected_launch_record: String,
}

/// The case's layout. Recorded launches go through `launch_script` into `launch_record`.
/// The missing case's first window runs `missing_executable` directly, never through the
/// recorder, so that sway-stage's own spawn is what fails.
pub fn failing_load(
    case: Case,
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
    missing_executable: &str,
) -> FailingLoad {
    let unique_id = format!("{run_id}-{}", case.name());
    let base_name = match case {
        Case::PartialLaunch => "uneven-siblings",
        Case::MissingExecutable => "horizontal-pair",
    };
    let mut layout = standard_scenarios(&unique_id, foot, sleep)
        .into_iter()
        .find(|scenario| scenario.name == base_name)
        .expect("standard failing-load base scenario");
    layout.name = case.name();

    let ConfiguredNode::Group { children, .. } = &mut layout.layout else {
        unreachable!("both failing-load bases have a split root")
    };
    match case {
        Case::PartialLaunch => {
            layout.window_timeout_ms = PARTIAL_WINDOW_TIMEOUT_MS;
            // A windowless command that exits at once: nothing is left to clean up, and
            // sway-stage still waits out the deadline because exiting may be delegation.
            children[1].first_leaf_mut().command = vec![sleep.into(), "0".into()];
            wrap_launches(
                &mut layout.layout,
                layout.name,
                launch_script,
                launch_record,
            );
        }
        Case::MissingExecutable => {
            children[0].first_leaf_mut().command = vec![missing_executable.into()];
            wrap_launches(&mut children[1], layout.name, launch_script, launch_record);
        }
    }

    let accepted_part = match case {
        Case::PartialLaunch => {
            let first_leaf = layout.layout.leaves()[0].clone();
            let mut accepted_part = layout.clone();
            accepted_part.layout = ConfiguredNode::Window(first_leaf);
            accepted_part.expected_focus = "window1";
            accepted_part.configured_focus = Some("window1");
            Some(accepted_part)
        }
        Case::MissingExecutable => None,
    };
    let expected_launch_record = match case {
        Case::PartialLaunch => format!("{0}-window1\n{0}-window2\n", layout.name),
        Case::MissingExecutable => String::new(),
    };

    FailingLoad {
        layout,
        accepted_part,
        expected_launch_record,
    }
}

/// sway-stage's own report of the case's failure: exit status 1, no stdout, and the
/// diagnostic of the failing path.
///
/// The partial case must report the second window's deadline and that it stopped without
/// cleanup or relaunch. The missing case must report the spawn failure of the first window
/// and no window timeout. sway-stage adds the stop advice only after an application has
/// started, so its absence also shows that no launch preceded the failure.
pub fn verify_failure_report(case: Case, output: &CompletedProcess) -> Result<(), String> {
    let stderr = output.stderr.as_str();
    // sway-stage names the failing stage, then the layout, then the cause.
    let layout_context = format!("layout {:?}: ", case.name());
    let stopped_without_cleanup = stderr.contains("stopped without cleanup or relaunch");
    let reported = match case {
        Case::PartialLaunch => {
            stderr.contains(&format!(
                "wait leaf=\"window2\": {layout_context}window timeout: "
            )) && stopped_without_cleanup
        }
        Case::MissingExecutable => {
            stderr.contains("launch leaf=\"window1\" workspace=")
                && stderr.contains(&format!(": {layout_context}spawn application: "))
                && stderr.contains("check executable, child PATH and cwd")
                && !stderr.contains("window timeout")
                && !stopped_without_cleanup
        }
    };

    if output.status.code() != Some(1) || !output.stdout.trim().is_empty() || !reported {
        let expected = match case {
            Case::PartialLaunch => "a window timeout for window2 after window1",
            Case::MissingExecutable => "a spawn failure for window1 before any launch",
        };
        return Err(format!(
            "expected {expected} (exit 1), got {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "load_failure_tests.rs"]
mod tests;
