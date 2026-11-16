use std::collections::HashSet;

use super::*;
use crate::fixtures::{FOOT, RUN_ID, SLEEP};
use crate::scenarios::{ConfiguredNode, GroupLayout};

const LAUNCH_SCRIPT: &str = "/private path/record-launch.sh";
const LAUNCH_RECORD: &str = "/private path/launches";

#[test]
fn each_case_loads_a_weighted_split_named_after_it_with_recorded_launches() {
    for case in CASES {
        let layout = layout(case, RUN_ID, FOOT, SLEEP, LAUNCH_SCRIPT, LAUNCH_RECORD);
        assert_eq!(layout.name, case.name());
        assert!(case.name().contains("second-output"), "{}", case.name());

        // Unequal weights on the root split, so measured sizes depend on the output's area.
        let ConfiguredNode::Group {
            layout: root_layout,
            children,
            ..
        } = &layout.layout
        else {
            panic!("{} has a group root", layout.name);
        };
        assert_eq!(*root_layout, GroupLayout::Horizontal, "{}", layout.name);
        let weights: HashSet<u32> = children.iter().map(ConfiguredNode::weight).collect();
        assert!(weights.len() > 1, "{}: {weights:?}", layout.name);

        for leaf in layout.layout.leaves() {
            assert_eq!(
                leaf.command[..4],
                [
                    "/bin/sh".to_owned(),
                    LAUNCH_SCRIPT.to_owned(),
                    format!("{}-{}", layout.name, leaf.layout_id),
                    LAUNCH_RECORD.to_owned(),
                ]
            );
            assert!(leaf.app_id.contains(case.name()), "{}", leaf.app_id);
        }
    }

    // The direct case's final focus is inside its nested group, not on a root child.
    let direct = layout(
        Case::Direct,
        RUN_ID,
        FOOT,
        SLEEP,
        LAUNCH_SCRIPT,
        LAUNCH_RECORD,
    );
    assert_eq!(direct.expected_focus, "window3");
    assert_eq!(direct.configured_focus, Some("window3"));
}

#[test]
fn the_decoy_records_its_launch_beside_the_menu_layout_under_its_own_app_ids() {
    let selected = layout(
        Case::Menu,
        RUN_ID,
        FOOT,
        SLEEP,
        LAUNCH_SCRIPT,
        LAUNCH_RECORD,
    );
    let decoy = menu_decoy_layout(RUN_ID, FOOT, SLEEP, LAUNCH_SCRIPT, LAUNCH_RECORD);
    assert_eq!(decoy.name, MENU_DECOY_LAYOUT);

    let [decoy_leaf] = decoy.layout.leaves()[..] else {
        panic!("the decoy has one window");
    };
    assert_eq!(decoy_leaf.command[3], LAUNCH_RECORD);
    assert_eq!(
        decoy_leaf.command[2],
        format!("{MENU_DECOY_LAYOUT}-{}", decoy_leaf.layout_id)
    );
    // A decoy window could never pass as one of the selected layout's.
    for selected_leaf in selected.layout.leaves() {
        assert_ne!(selected_leaf.app_id, decoy_leaf.app_id);
    }
}

#[test]
fn the_menu_offers_profile_capture_then_both_layouts_then_the_profile() {
    assert_eq!(
        menu_rows(),
        [
            "Capture session profile…",
            "Layout: layout-second-output-decoy",
            "Layout: layout-second-output-menu",
            "Profile: layout-second-output-menu",
        ]
    );

    // The selected row is offered once, between the decoy and the profile.
    let rows = menu_rows();
    let selection = menu_selection();
    let positions: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| **row == selection)
        .map(|(row_index, _)| row_index)
        .collect();
    assert_eq!(positions, [2]);
    assert!(MENU_DECOY_LAYOUT < Case::Menu.name());
}

#[test]
fn the_offered_profile_references_the_menu_layout() {
    assert_eq!(
        menu_profile_configuration(),
        "version: 1\nworkspaces:\n  - workspace: 3\n    layout: layout-second-output-menu\n"
    );
}
