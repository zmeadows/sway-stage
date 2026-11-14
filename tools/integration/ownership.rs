//! Independent evidence that this run owns exactly the windows it launched.
//!
//! Ordered `window::new` events bind sequential launches to layout leaves; app IDs alone
//! never establish ownership. Later checks require those container IDs on the target
//! workspace in safe tiled state, as native Wayland windows with the exact app ID, and,
//! where the leaf requires it, the literal launch arguments in `/proc/PID/cmdline`.
//! Every failure here is `Uncertain`, which forbids cleanup.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::Path;

use crate::ipc::{Shell, TreeNode, WindowChange, WindowEvent};
use crate::layout::{class, is_window, outline};
use crate::observation::{Failure, walk};
use crate::scenarios::{ConfiguredNode, ConfiguredWindow};

/// Sway container ID → configured layout ID, for windows this run launched and accepted.
pub type AcceptedWindows = BTreeMap<i64, String>;

/// A close this run requested: the closing window and the owned windows that remain.
pub struct ExpectedClose<'a> {
    pub closing_window_id: i64,
    pub remaining: &'a AcceptedWindows,
}

/// Title, urgency, and mark changes alter neither structure nor identity.
fn is_harmless(change: WindowChange) -> bool {
    matches!(
        change,
        WindowChange::Title | WindowChange::Urgent | WindowChange::Mark
    )
}

/// Requires every window event to be harmless. While `expected_close` is set, the closing
/// window's close event and focus moving to a remaining owned window are expected too.
pub fn require_quiet(
    window_events: &[WindowEvent],
    expected_close: Option<&ExpectedClose>,
) -> Result<(), Failure> {
    for event in window_events {
        if is_harmless(event.change) {
            continue;
        }

        let container_id = event.container.id;
        if let Some(close) = expected_close {
            let is_requested_close =
                event.change == WindowChange::Close && container_id == close.closing_window_id;
            let is_focus_on_remaining =
                event.change == WindowChange::Focus && close.remaining.contains_key(&container_id);
            if is_requested_close || is_focus_on_remaining {
                continue;
            }
        }
        return Err(Failure::Uncertain(format!(
            "unexpected window activity: {:?} id={container_id}",
            event.change
        )));
    }
    Ok(())
}

/// Binds launches to layout leaves by the order of `window::new` events, never by app ID
/// uniqueness: sway-stage launches one leaf at a time and snapshots global window identities
/// before each spawn, so the nth new window belongs to the nth leaf. Each new window must
/// be globally new. Every other event during the load must be harmless, or a new or focus
/// event of an accepted window whose reported app ID, if any, matches its leaf.
/// `verify_ownership` then checks tree identities and any required process arguments.
pub fn bind_launches(
    window_events: &[WindowEvent],
    layout: &ConfiguredNode,
    preexisting_ids: &HashSet<i64>,
) -> Result<AcceptedWindows, Failure> {
    let mut new_window_ids = Vec::new();
    for event in window_events {
        if event.change != WindowChange::New {
            continue;
        }
        let container_id = event.container.id;
        if preexisting_ids.contains(&container_id) || new_window_ids.contains(&container_id) {
            return Err(Failure::Uncertain(format!(
                "preexisting or repeated new event: id={container_id}"
            )));
        }
        new_window_ids.push(container_id);
    }

    let leaves = layout.leaves();
    if new_window_ids.len() != leaves.len() {
        return Err(Failure::Uncertain(format!(
            "expected {} sequential new windows, observed IDs {new_window_ids:?}",
            leaves.len()
        )));
    }
    let leaves_by_container: HashMap<i64, &ConfiguredWindow> =
        new_window_ids.iter().copied().zip(leaves).collect();

    for event in window_events {
        if is_harmless(event.change) {
            continue;
        }

        let container_id = event.container.id;
        let leaf = match leaves_by_container.get(&container_id) {
            Some(leaf) if matches!(event.change, WindowChange::New | WindowChange::Focus) => leaf,
            _ => {
                return Err(Failure::Uncertain(format!(
                    "unexpected activity during load: {:?} id={container_id}",
                    event.change
                )));
            }
        };
        // Identities may arrive after the new event, so an absent one is not a mismatch.
        let reported_app_id = event.container.app_id.as_deref();
        if reported_app_id.is_some_and(|app_id| app_id != leaf.app_id) {
            return Err(Failure::Uncertain(format!(
                "event identity differs: id={container_id} app_id={reported_app_id:?}, \
                 expected {:?}",
                leaf.app_id
            )));
        }
    }

    let accepted = leaves_by_container
        .into_iter()
        .map(|(container_id, leaf)| (container_id, leaf.layout_id.clone()))
        .collect();
    Ok(accepted)
}

/// Requires the whole tree to show exactly the `accepted` windows, each still identified as
/// its leaf, on the target workspace in safe tiled state, with process evidence where its
/// leaf requires it. Candidates are accepted IDs, any window anywhere matching one of the
/// layout's app IDs, and any window on the target; any candidate that is not such an
/// owned window is fatal. Preexisting windows elsewhere, such as the user's other windows,
/// are expected and ignored.
pub fn verify_ownership(
    tree: &TreeNode,
    target_workspace_id: i64,
    layout: &ConfiguredNode,
    preexisting_ids: &HashSet<i64>,
    accepted: &AcceptedWindows,
) -> Result<(), Failure> {
    let leaves = layout.leaves();
    let mut verified_windows = AcceptedWindows::new();
    for placed in walk(tree) {
        let node = placed.node;
        let container_id = node.id;
        let on_target_workspace = placed
            .workspace
            .is_some_and(|workspace| workspace.id == target_workspace_id);
        let is_preexisting = preexisting_ids.contains(&container_id);
        if is_preexisting && !on_target_workspace {
            continue;
        }

        let observed_app_id = node.app_id.as_deref();
        let matches_an_app_id = leaves
            .iter()
            .any(|leaf| observed_app_id == Some(leaf.app_id.as_str()));
        let is_candidate = accepted.contains_key(&container_id)
            || matches_an_app_id
            || (on_target_workspace && is_window(node));
        if !is_candidate {
            continue;
        }

        let owned_layout_id = match accepted.get(&container_id) {
            Some(layout_id)
                if !is_preexisting
                    && on_target_workspace
                    && !placed.unsafe_state
                    && is_window(node) =>
            {
                layout_id
            }
            _ => {
                let workspace_name = placed
                    .workspace
                    .and_then(|workspace| workspace.name.as_deref());
                return Err(Failure::Uncertain(format!(
                    "ownership/placement uncertain: id={container_id} app_id={:?} class={:?} \
                     workspace={workspace_name:?}",
                    node.app_id,
                    class(node)
                )));
            }
        };
        let Some(leaf) = leaves
            .iter()
            .find(|leaf| &leaf.layout_id == owned_layout_id)
        else {
            return Err(Failure::Uncertain(format!(
                "accepted window id={container_id} names no layout leaf {owned_layout_id}"
            )));
        };

        if observed_app_id != Some(leaf.app_id.as_str()) {
            return Err(Failure::Uncertain(format!(
                "owned identity changed: id={container_id} expected app_id {:?}",
                leaf.app_id
            )));
        }
        let is_native_wayland = node.shell == Some(Shell::XdgShell) && node.window.is_none();
        if !is_native_wayland {
            return Err(Failure::Uncertain(format!(
                "expected a native Wayland window: {}",
                outline(node)
            )));
        }
        if leaf.requires_process_evidence {
            require_process_command(node, leaf)?;
        }

        verified_windows.insert(container_id, owned_layout_id.clone());
    }

    if &verified_windows != accepted {
        return Err(Failure::Uncertain(format!(
            "owned windows changed: expected {accepted:?}, observed {verified_windows:?}"
        )));
    }
    Ok(())
}

/// Requires Sway's PID for `window` to show the configured application's literal argv.
/// Recovered arguments never appear in messages.
pub fn require_process_command(window: &TreeNode, leaf: &ConfiguredWindow) -> Result<(), Failure> {
    let Some(pid) = window.pid.filter(|pid| *pid > 0) else {
        return Err(Failure::Uncertain(format!(
            "no usable Sway process identity: container={} leaf={}",
            window.id, leaf.layout_id
        )));
    };
    let refuse = |reason: &str| {
        Failure::Uncertain(format!(
            "{reason}: container={} pid={pid} leaf={}; refusing cleanup",
            window.id, leaf.layout_id
        ))
    };

    let command_line_path = format!("/proc/{pid}/cmdline");
    let Some(command_line) = read_process_bytes(Path::new(&command_line_path)) else {
        return Err(refuse("cannot verify process argv"));
    };
    if !command_line_proves_launch(&command_line, &leaf.command) {
        return Err(refuse("process argv does not prove this launch"));
    }
    Ok(())
}

fn read_process_bytes(path: &Path) -> Option<Vec<u8>> {
    const LIMIT: usize = 64 * 1024;
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= LIMIT).then_some(bytes)
}

/// Whether raw `/proc/PID/cmdline` bytes hold exactly `command`'s arguments after the
/// executable, byte for byte, with argument boundaries intact. argv[0] may differ after an
/// exec through a distribution wrapper; every other literal argument must agree, including
/// a per-leaf Foot title or the process probe's literal arguments.
fn command_line_proves_launch(command_line: &[u8], command: &[String]) -> bool {
    // The kernel terminates every argument, including the last, with NUL.
    let Some(arguments) = command_line.strip_suffix(&[0]) else {
        return false;
    };
    let observed_arguments: Vec<&[u8]> = arguments.split(|byte| *byte == 0).collect();
    let expected_arguments: Vec<&[u8]> =
        command.iter().map(|argument| argument.as_bytes()).collect();

    observed_arguments.len() == expected_arguments.len()
        && observed_arguments[1..] == expected_arguments[1..]
}

#[cfg(test)]
#[path = "ownership_tests.rs"]
mod tests;
