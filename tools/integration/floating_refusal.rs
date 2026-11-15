//! Refusals of a target whose only occupant is one floating window: a standalone load on it,
//! and a profile that names it as the second target behind an empty, focused first target.
//!
//! The harness opens that window itself through Sway's `exec` and then makes it floating
//! with one exact-ID command, so neither the window nor its floating state comes from
//! sway-stage. The decisions here are pure; lifecycle runs the cases against the private
//! compositor.

use crate::ipc::{Floating, NodeType, ScratchpadState, Shell, WindowChange, WindowEvent};
use crate::layout::is_window;
use crate::observation::{Snapshot, walk};
use crate::ownership::{require_process_command, require_quiet};
use crate::profile::wrap_launches;
use crate::scenarios::{ConfiguredWindow, Scenario, standard_scenarios};

pub const CASES: [Case; 2] = [Case::Layout, Case::Profile];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    /// `load` on the focused primary, which holds only the floating window.
    Layout,
    /// `profile load` of the empty, focused workspace `3`, then the primary, which holds
    /// only the floating window. Preflight must refuse before constructing the first entry.
    Profile,
}

impl Case {
    pub fn name(self) -> &'static str {
        match self {
            Self::Layout => "layout-floating-refusal",
            Self::Profile => "profile-floating-refusal",
        }
    }
}

/// The harness's own Foot window that occupies the refused target, as a one-window layout
/// whose app ID is unique to the run and case. Its launch arguments are ownership evidence.
pub fn floating_window(case: Case, run_id: &str, foot: &str, sleep: &str) -> Scenario {
    let unique_id = format!("{run_id}-{}-floating", case.name());
    let mut window = standard_scenarios(&unique_id, foot, sleep)
        .into_iter()
        .find(|scenario| scenario.name == "single-window")
        .expect("standard single-window scenario");
    window.name = case.name();
    window.layout.first_leaf_mut().requires_process_evidence = true;
    window
}

/// The valid layouts sway-stage is asked to load and must refuse without launching: one for
/// the standalone case, and the profile's two entries in order. Every launch is recorded
/// through `launch_script`, so a launch that should never happen leaves evidence.
pub fn refused_layouts(
    case: Case,
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
) -> Vec<Scenario> {
    let unique_id = format!("{run_id}-{}", case.name());
    let standard = standard_scenarios(&unique_id, foot, sleep);
    let take_layout = |standard_name: &str, layout_name: &'static str| {
        let mut layout = standard
            .iter()
            .find(|scenario| scenario.name == standard_name)
            .cloned()
            .expect("standard refused layout");
        layout.name = layout_name;
        wrap_launches(
            &mut layout.layout,
            layout_name,
            launch_script,
            launch_record,
        );
        layout
    };

    match case {
        Case::Layout => vec![take_layout("horizontal-pair", case.name())],
        Case::Profile => vec![
            take_layout("horizontal-pair", "profile-first"),
            take_layout("vertical-pair", "profile-second"),
        ],
    }
}

/// The window events of making `window_id` floating: Sway's floating event for that window,
/// and possibly its focus, besides harmless title, urgency, and mark changes.
pub fn require_float_events(window_events: &[WindowEvent], window_id: i64) -> Result<(), String> {
    for event in window_events {
        let is_this_window = event.container.id == window_id;
        let expected = matches!(event.change, WindowChange::Floating | WindowChange::Focus);
        if is_this_window && expected {
            continue;
        }
        require_quiet(std::slice::from_ref(event), None)?;
    }
    Ok(())
}

/// Requires workspace `workspace_id` to hold no tiled container and exactly one floating
/// container: the window `window_id`, made floating by the user, with `leaf`'s exact app ID,
/// as a native Wayland view that is neither fullscreen nor in the scratchpad, with `leaf`'s
/// launch arguments where it requires them. No other window anywhere may have that app ID.
pub fn verify_floating_only(
    observed: &Snapshot,
    workspace_id: i64,
    window_id: i64,
    leaf: &ConfiguredWindow,
) -> Result<(), String> {
    observed.validate()?;
    let placed_nodes = walk(&observed.tree);
    let Some(workspace) = placed_nodes
        .iter()
        .map(|placed| placed.node)
        .find(|node| node.id == workspace_id && node.node_type == NodeType::Workspace)
    else {
        return Err(format!("workspace id={workspace_id} disappeared"));
    };

    let floating_ids: Vec<i64> = workspace
        .floating_nodes
        .iter()
        .map(|node| node.id)
        .collect();
    if !workspace.nodes.is_empty() || floating_ids != [window_id] {
        return Err(format!(
            "workspace id={workspace_id} must hold only floating window id={window_id}: \
             tiled={} floating={floating_ids:?}",
            workspace.nodes.len()
        ));
    }

    let window = &workspace.floating_nodes[0];
    let is_native_wayland = window.shell == Some(Shell::XdgShell) && window.window.is_none();
    let has_floating_state = window.node_type == NodeType::FloatingCon
        && window.floating == Some(Floating::UserOn)
        && window.fullscreen_mode == Some(0)
        && window.scratchpad_state == Some(ScratchpadState::None);
    if !is_window(window)
        || !window.nodes.is_empty()
        || window.app_id.as_deref() != Some(leaf.app_id.as_str())
        || !is_native_wayland
        || !has_floating_state
    {
        return Err(format!(
            "floating window id={window_id} identity or state differs: type={:?} app_id={:?} \
             floating={:?} fullscreen={:?} scratchpad={:?}",
            window.node_type,
            window.app_id,
            window.floating,
            window.fullscreen_mode,
            window.scratchpad_state
        ));
    }
    if leaf.requires_process_evidence {
        require_process_command(window, leaf)?;
    }

    let same_app_id_elsewhere = placed_nodes.iter().any(|placed| {
        placed.node.id != window_id && placed.node.app_id.as_deref() == Some(leaf.app_id.as_str())
    });
    if same_app_id_elsewhere {
        return Err(format!(
            "another window has the floating window's app ID {:?}",
            leaf.app_id
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "floating_refusal_tests.rs"]
mod tests;
