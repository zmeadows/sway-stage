//! Standalone layout loads on the second output, run directly and through the menu.
//!
//! From the fixture's workspace on the first output, the harness selects the second
//! output's empty initial workspace, which keeps its identity. sway-stage must then build a
//! weighted layout there, sized for that output's own logical area, without selecting,
//! creating, or moving any workspace. The menu case also requires Fuzzel's prompt on the
//! second output, and selects the layout by its offered row between a decoy layout and a
//! profile, so a wrong selection launches windows that the launch record refuses. The
//! decisions here are pure; lifecycle runs the scenarios.

use crate::headless::SECOND_OUTPUT_ASSIGNED_WORKSPACE;
use crate::profile::wrap_launches;
use crate::scenarios::{Scenario, standard_scenarios};

pub const CASES: [Case; 2] = [Case::Direct, Case::Menu];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    /// `sway-stage load`, run by the harness.
    Direct,
    /// `sway-stage menu`, started by Sway's `exec` as a keybinding would, selecting the
    /// case's layout.
    Menu,
}

impl Case {
    pub fn name(self) -> &'static str {
        match self {
            Self::Direct => "layout-second-output-direct",
            Self::Menu => "layout-second-output-menu",
        }
    }

    /// Weighted splits, so the measured sizes depend on the second output's area.
    fn standard_layout(self) -> &'static str {
        match self {
            // Nested, with its final focus inside the nested group.
            Self::Direct => "mixed-nesting",
            Self::Menu => "uneven-siblings",
        }
    }
}

/// A valid layout that the menu offers beside the selected one. Its name sorts first, so the
/// selected row is neither the first layout nor the last row.
pub const MENU_DECOY_LAYOUT: &str = "layout-second-output-decoy";

/// The case's standard layout, named after the case, with every launch recorded.
pub fn layout(
    case: Case,
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
) -> Scenario {
    recorded_standard_layout(
        case.standard_layout(),
        case.name(),
        run_id,
        foot,
        sleep,
        launch_script,
        launch_record,
    )
}

/// `single-window`, recorded in the same launch record as the menu case's layout, so a launch
/// of the decoy fails that record's verification.
pub fn menu_decoy_layout(
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
) -> Scenario {
    recorded_standard_layout(
        "single-window",
        MENU_DECOY_LAYOUT,
        run_id,
        foot,
        sleep,
        launch_script,
        launch_record,
    )
}

fn recorded_standard_layout(
    standard_name: &str,
    layout_name: &'static str,
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
) -> Scenario {
    // App IDs unique to the layout and run, as every other recorded layout has.
    let unique_id = format!("{run_id}-{layout_name}");
    let mut layout = standard_scenarios(&unique_id, foot, sleep)
        .into_iter()
        .find(|scenario| scenario.name == standard_name)
        .expect("a standard layout name");
    layout.name = layout_name;
    wrap_launches(
        &mut layout.layout,
        layout.name,
        launch_script,
        launch_record,
    );
    layout
}

/// A valid one-entry profile with the menu case's name, which the menu therefore offers. It
/// is never loaded.
pub fn menu_profile_configuration() -> String {
    format!(
        "version: 1\nworkspaces:\n  - workspace: {SECOND_OUTPUT_ASSIGNED_WORKSPACE}\n    layout: {}\n",
        Case::Menu.name()
    )
}

/// The row that the Fuzzel substitute selects.
pub fn menu_selection() -> String {
    format!("Layout: {}", Case::Menu.name())
}

/// What the menu offers on the empty, focused workspace while the fixture's populated
/// workspace is capturable: profile capture, then the layouts and the profile, each category
/// in lexical name order.
pub fn menu_rows() -> [String; 4] {
    [
        "Capture session profile…".to_owned(),
        format!("Layout: {MENU_DECOY_LAYOUT}"),
        menu_selection(),
        format!("Profile: {}", Case::Menu.name()),
    ]
}

#[cfg(test)]
#[path = "secondary_output_tests.rs"]
mod tests;
