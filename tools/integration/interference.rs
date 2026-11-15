//! Deliberate interference while sway-stage constructs a standalone layout.
//!
//! The layout's second window is not an application: its configured command is
//! `INJECTOR_SCRIPT`, which sends prepared Sway commands through the private socket and then
//! exits without opening a window of its own. sway-stage starts it only after accepting the
//! first window, so the interference always falls inside construction, during the second
//! window's wait, without any timed sleep. No window can ever match the second leaf, so
//! nothing races with the interference: sway-stage must stop because of it, and the third
//! window never launches. The decisions here are pure; lifecycle runs the cases against the
//! private compositor and verifies independently what each injected command did.

use std::path::Path;

use crate::ipc::{exec_command, switch_command};
use crate::process::CompletedProcess;
use crate::profile::wrap_launches;
use crate::scenarios::{ConfiguredNode, GroupLayout, Scenario, standard_scenarios};

pub const CASES: [Case; 3] = [
    Case::SwitchAway,
    Case::SwitchAndReturn,
    Case::UnexpectedWindow,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    /// Selects the fixture's workspace and stays there.
    SwitchAway,
    /// Selects the fixture's workspace, then the target again, in one Sway command request.
    /// Sway serves no other client between a request's commands, so no query sway-stage
    /// makes can see the fixture's workspace focused: only the event history shows the
    /// switch, and the final focus alone shows nothing wrong.
    SwitchAndReturn,
    /// Opens a Foot window with an app ID no layout window has, through Sway's own `exec`,
    /// which places it on the focused target.
    UnexpectedWindow,
}

impl Case {
    pub fn name(self) -> &'static str {
        match self {
            Self::SwitchAway => "interference-switch-away",
            Self::SwitchAndReturn => "interference-switch-and-return",
            Self::UnexpectedWindow => "interference-unexpected-window",
        }
    }

    /// The workspaces the injector selects, in order: the switches the harness expects.
    pub fn switch_destinations<'a>(self, fixture: &'a str, target: &'a str) -> Vec<&'a str> {
        match self {
            Self::SwitchAway => vec![fixture],
            Self::SwitchAndReturn => vec![fixture, target],
            Self::UnexpectedWindow => Vec::new(),
        }
    }
}

/// Arguments: swaymsg, the private IPC socket, a record file, then the Sway command requests
/// to send in order. Every request goes through the explicitly named socket and must
/// succeed; `swaymsg` exits nonzero when Sway rejects any command in it. Each success is
/// recorded before the next request, and the first failure stops the script, so the record
/// shows exactly which requests Sway accepted. The script opens no window and leaves no
/// process behind.
pub const INJECTOR_SCRIPT: &str = "\
#!/bin/sh
swaymsg=$1 socket=$2 record=$3
shift 3
for sway_command do
    \"$swaymsg\" -s \"$socket\" -t command \"$sway_command\" >/dev/null || exit 1
    printf '%s\\n' \"$sway_command\" >> \"$record\" || exit 1
done
";

/// The command that runs `INJECTOR_SCRIPT` with `sway_commands`.
pub fn injector_command(
    script: &str,
    swaymsg: &str,
    socket: &str,
    record: &str,
    sway_commands: &[String],
) -> Vec<String> {
    let mut command = vec![
        "/bin/sh".into(),
        script.into(),
        swaymsg.into(),
        socket.into(),
        record.into(),
    ];
    command.extend_from_slice(sway_commands);
    command
}

/// The case's one Sway command request, built from the same validated constructors the
/// harness uses for its own commands. Several switches form one command list, so Sway runs
/// them without serving any other client in between. `injected_window_script` is what
/// Sway's `exec` runs to open the unexpected window.
pub fn sway_commands(
    case: Case,
    fixture_workspace: &str,
    target_workspace: &str,
    injected_window_script: &Path,
) -> Result<Vec<String>, String> {
    if case == Case::UnexpectedWindow {
        return Ok(vec![exec_command(injected_window_script)?]);
    }
    let switches = case
        .switch_destinations(fixture_workspace, target_workspace)
        .into_iter()
        .map(switch_command)
        .collect::<Result<Vec<String>, String>>()?;
    Ok(vec![switches.join("; ")])
}

/// What the injector's record holds once every request succeeded: one line per request.
pub fn expected_injector_record(sway_commands: &[String]) -> String {
    sway_commands
        .iter()
        .map(|sway_command| format!("{sway_command}\n"))
        .collect()
}

/// The layout ID the harness gives the injected window in its own model of the target.
pub const INJECTED_LAYOUT_ID: &str = "injected-window";

/// A layout that sway-stage must stop constructing, and what the harness expects afterwards.
pub struct InterferingLoad {
    /// What the configuration asks sway-stage to build.
    pub layout: Scenario,
    /// The first window alone: everything sway-stage accepts before the interference.
    pub accepted_part: Scenario,
    /// Only for the unexpected window: the target as the harness expects to find it, the
    /// first window and then the injected window, which Sway places beside it and focuses.
    /// Its second leaf is the injected window, whose app ID no configured window has.
    pub observed_target: Option<Scenario>,
    /// One line per started launch, in order: the first window and the injector.
    pub expected_launch_record: String,
}

/// The case's three-window layout. Every launch is recorded through `launch_script`; the
/// second window's command is `injector`, from `injector_command`.
pub fn interfering_load(
    case: Case,
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
    injector: &[String],
) -> InterferingLoad {
    let unique_id = format!("{run_id}-{}", case.name());
    let standard = standard_scenarios(&unique_id, foot, sleep);
    let mut layout = standard
        .iter()
        .find(|scenario| scenario.name == "uneven-siblings")
        .cloned()
        .expect("standard uneven-siblings scenario");
    layout.name = case.name();

    let ConfiguredNode::Group { children, .. } = &mut layout.layout else {
        unreachable!("uneven-siblings has a split root")
    };
    children[1].first_leaf_mut().command = injector.to_vec();
    wrap_launches(
        &mut layout.layout,
        layout.name,
        launch_script,
        launch_record,
    );

    let first_leaf = layout.layout.leaves()[0].clone();
    let mut accepted_part = layout.clone();
    accepted_part.layout = ConfiguredNode::Window(first_leaf.clone());
    accepted_part.expected_focus = "window1";
    accepted_part.configured_focus = Some("window1");

    let observed_target = if case == Case::UnexpectedWindow {
        // Another standard scenario's window, so its app ID names neither this layout nor
        // its leaves. Its literal launch arguments are its ownership evidence.
        let mut injected_window = standard
            .iter()
            .find(|scenario| scenario.name == "single-window")
            .expect("standard single-window scenario")
            .layout
            .leaves()[0]
            .clone();
        injected_window.layout_id = INJECTED_LAYOUT_ID.into();
        injected_window.requires_process_evidence = true;

        // Sway gives a window mapped beside the only window half of the split.
        let mut first_window = first_leaf;
        first_window.weight = 1;
        injected_window.weight = 1;
        let mut observed_target = accepted_part.clone();
        observed_target.layout = ConfiguredNode::Group {
            layout: GroupLayout::Horizontal,
            active: 1,
            weight: 1,
            children: vec![
                ConfiguredNode::Window(first_window),
                ConfiguredNode::Window(injected_window),
            ],
        };
        observed_target.expected_focus = INJECTED_LAYOUT_ID;
        observed_target.configured_focus = Some(INJECTED_LAYOUT_ID);
        Some(observed_target)
    } else {
        None
    };

    InterferingLoad {
        expected_launch_record: format!("{0}-window1\n{0}-window2\n", layout.name),
        layout,
        accepted_part,
        observed_target,
    }
}

/// sway-stage's own report of the interference: exit status 1, no stdout, the second
/// window's wait stopped by the case's failure, and the stop advice that follows a started
/// application. A window timeout, or any other failure, is never the expected one.
///
/// A switch is a focus loss of the target `target_workspace_id`, whether sway-stage reads
/// the focus event first or a snapshot already shows another workspace focused. The
/// unexpected window is refused by its container ID, `injected_window_id`.
pub fn verify_failure_report(
    case: Case,
    output: &CompletedProcess,
    target_workspace_id: i64,
    injected_window_id: Option<i64>,
) -> Result<(), String> {
    let stderr = output.stderr.as_str();
    // sway-stage names the failing stage, then the layout, then the cause.
    let wait_context = format!("wait leaf=\"window2\": layout {:?}: ", case.name());
    let reported = match (case, injected_window_id) {
        (Case::SwitchAway | Case::SwitchAndReturn, None) => {
            let focus_event = format!(
                "{wait_context}workspace focus changed from target id={target_workspace_id}"
            );
            let focused_elsewhere =
                format!("{wait_context}snapshot: focus left target id={target_workspace_id}");
            stderr.contains(&focus_event) || stderr.contains(&focused_elsewhere)
        }
        (Case::UnexpectedWindow, Some(window_id)) => {
            stderr.contains(&format!(
                "{wait_context}unexpected target-workspace window id={window_id}: "
            )) && stderr.contains("refusing adoption")
        }
        _ => false,
    };
    let stopped_without_cleanup = stderr.contains("stopped without cleanup or relaunch");

    if output.status.code() != Some(1)
        || !output.stdout.trim().is_empty()
        || !reported
        || !stopped_without_cleanup
        || stderr.contains("window timeout")
    {
        let expected = match case {
            Case::SwitchAway | Case::SwitchAndReturn => "a focus loss during window2's wait",
            Case::UnexpectedWindow => "the injected window refused during window2's wait",
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
#[path = "interference_tests.rs"]
mod tests;
