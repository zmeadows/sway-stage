//! Deliberate interference while sway-stage constructs a standalone layout or a profile.
//!
//! The layout's second window is not an application: its configured command is
//! `INJECTOR_SCRIPT`, which sends prepared Sway commands through the private socket and then
//! exits without opening a window of its own. sway-stage starts it only after accepting the
//! first window, so the interference always falls inside construction, during the second
//! window's wait, without any timed sleep. No window can ever match the second leaf, so
//! nothing races with the interference: sway-stage must stop because of it, and the third
//! window never launches. A profile's second entry is interrupted the same way.
//!
//! A profile's first entry can instead send its request from a window's command just before
//! that window starts, so the construction still completes and only the profile coordinator
//! can object. The decisions here are pure; lifecycle runs the cases against the private
//! compositor and verifies independently what each injected command did.

use std::path::Path;

use crate::ipc::{exec_command, switch_command};
use crate::observation::WorkspaceIdentity;
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
    let layout = layout_with_injector(
        case.name(),
        &standard,
        launch_script,
        launch_record,
        injector,
    );
    let accepted_part = first_window_alone(&layout);
    let first_leaf = layout.layout.leaves()[0].clone();

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

/// `uneven-siblings`, named `name`, with the injector as its second window's command. Every
/// launch is recorded through `launch_script`.
fn layout_with_injector(
    name: &'static str,
    standard: &[Scenario],
    launch_script: &str,
    launch_record: &str,
    injector: &[String],
) -> Scenario {
    let mut layout = standard
        .iter()
        .find(|scenario| scenario.name == "uneven-siblings")
        .cloned()
        .expect("standard uneven-siblings scenario");
    layout.name = name;

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
    layout
}

/// Everything sway-stage accepts before the injector runs: the first window alone.
fn first_window_alone(layout: &Scenario) -> Scenario {
    let first_leaf = layout.layout.leaves()[0].clone();
    let mut accepted_part = layout.clone();
    accepted_part.layout = ConfiguredNode::Window(first_leaf);
    accepted_part.expected_focus = "window1";
    accepted_part.configured_focus = Some("window1");
    accepted_part
}

/// A two-entry profile whose second construction the injector interrupts by selecting the
/// first entry's workspace again. The coordinator has already verified and recorded the
/// first entry's completion, so this is interference during the second entry, not in the
/// coordinator's interval between the entries.
pub const PROFILE_SECOND_ENTRY_SCENARIO: &str = "profile-second-entry-interference";

/// The interfered profile's entries and what the harness expects afterwards.
pub struct InterferingProfile {
    /// The first entry's layout, `horizontal-pair`, which must complete and stay intact.
    pub first: Scenario,
    /// The second entry's layout, whose second window is the injector.
    pub second: Scenario,
    /// The second entry's first window alone: all of it that sway-stage accepts.
    pub second_accepted_part: Scenario,
    /// One line per started launch, in order: both first-entry windows, then the second
    /// entry's first window and the injector.
    pub expected_launch_record: String,
}

/// The interfered profile's layouts, each named after its position in the profile. Every
/// launch is recorded through `launch_script`; `injector` is from `injector_command`.
pub fn interfering_profile(
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
    injector: &[String],
) -> InterferingProfile {
    let unique_id = format!("{run_id}-{PROFILE_SECOND_ENTRY_SCENARIO}");
    let standard = standard_scenarios(&unique_id, foot, sleep);

    let mut first = standard
        .iter()
        .find(|scenario| scenario.name == "horizontal-pair")
        .cloned()
        .expect("standard horizontal-pair scenario");
    first.name = "profile-first";
    wrap_launches(&mut first.layout, first.name, launch_script, launch_record);

    let second = layout_with_injector(
        "profile-second",
        &standard,
        launch_script,
        launch_record,
        injector,
    );
    let second_accepted_part = first_window_alone(&second);

    // The second entry's third window never launches.
    let mut expected_launch_record = String::new();
    for leaf in first.layout.leaves() {
        expected_launch_record.push_str(&format!("{}-{}\n", first.name, leaf.layout_id));
    }
    for leaf in &second.layout.leaves()[..2] {
        expected_launch_record.push_str(&format!("{}-{}\n", second.name, leaf.layout_id));
    }

    InterferingProfile {
        first,
        second,
        second_accepted_part,
        expected_launch_record,
    }
}

/// sway-stage's report of the interrupted profile: exit status 1, no stdout, the second
/// entry's progress with exactly the first workspace completed, a focus loss of the second
/// target `second_target_id` during its second window's wait, and the stop advice that
/// follows started applications. A window timeout, or any other failure, is never it.
pub fn verify_profile_failure_report(
    output: &CompletedProcess,
    first_target: &WorkspaceIdentity,
    second_target_name: &str,
    second_target_id: i64,
) -> Result<(), String> {
    let stderr = output.stderr.as_str();
    let progress = format!(
        "profile load: workspace {second_target_name}, layout \"profile-second\"; completed 1 \
         workspaces [({}, {})]; ",
        first_target.name, first_target.id
    );
    let wait_context = format!(
        "{progress}wait leaf=\"window2\": profile {PROFILE_SECOND_ENTRY_SCENARIO:?}: layout \
         construction: "
    );
    let focus_event =
        format!("{wait_context}workspace focus changed from target id={second_target_id}");
    let focused_elsewhere =
        format!("{wait_context}snapshot: focus left target id={second_target_id}");
    let reported = stderr.contains(&focus_event) || stderr.contains(&focused_elsewhere);
    let stopped_without_cleanup = stderr.contains(
        "application launches may have left partial layouts; stopped without cleanup or relaunch",
    );

    if output.status.code() != Some(1)
        || !output.stdout.trim().is_empty()
        || !reported
        || !stopped_without_cleanup
        || stderr.contains("window timeout")
    {
        return Err(format!(
            "expected a focus loss during the second entry's window2 wait after one completed \
             workspace (exit 1), got {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    Ok(())
}

/// A two-entry profile whose first entry renames its own target while it loads: the first
/// entry's second window renames the focused workspace before Foot starts. The loader knows
/// its target only by container ID, so the first construction completes. Only the profile
/// coordinator, which checks every workspace event that arrives while a layout loads,
/// refuses the rename, at its completion verification and so before selecting the second
/// target. The rename happens during the first construction; it is the coordinator's own
/// check, not the timing, that this case places at the boundary between the entries.
pub const PROFILE_COMPLETION_SCENARIO: &str = "profile-completion-interference";

/// Arguments: swaymsg, the private IPC socket, a record file, and one Sway command request,
/// then a window's own command. The request goes through the explicitly named socket and
/// must succeed; it is recorded before the window's command replaces the script, and a
/// failure exits before the window starts.
pub const INJECT_BEFORE_LAUNCH_SCRIPT: &str = "\
#!/bin/sh
swaymsg=$1 socket=$2 record=$3 sway_command=$4
shift 4
\"$swaymsg\" -s \"$socket\" -t command \"$sway_command\" >/dev/null || exit 1
printf '%s\\n' \"$sway_command\" >> \"$record\" || exit 1
exec \"$@\"
";

/// The command prefix that runs `INJECT_BEFORE_LAUNCH_SCRIPT` with `sway_command` before a
/// window's own command.
pub fn inject_before_launch_command(
    script: &str,
    swaymsg: &str,
    socket: &str,
    record: &str,
    sway_command: &str,
) -> Vec<String> {
    vec![
        "/bin/sh".into(),
        script.into(),
        swaymsg.into(),
        socket.into(),
        record.into(),
        sway_command.into(),
    ]
}

/// The renaming profile's layouts in profile order: `horizontal-pair`, whose second window's
/// command is prefixed by `inject_prefix`, then `vertical-pair`, which never launches. Every
/// launch is recorded through `launch_script`.
pub fn renaming_profile(
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
    inject_prefix: &[String],
) -> [Scenario; 2] {
    let unique_id = format!("{run_id}-{PROFILE_COMPLETION_SCENARIO}");
    let standard = standard_scenarios(&unique_id, foot, sleep);
    let take_layout = |standard_name: &str, layout_name: &'static str| {
        let mut layout = standard
            .iter()
            .find(|scenario| scenario.name == standard_name)
            .cloned()
            .expect("standard renaming profile scenario");
        layout.name = layout_name;
        layout
    };

    let mut first = take_layout("horizontal-pair", "profile-first");
    let mut second = take_layout("vertical-pair", "profile-second");
    let ConfiguredNode::Group { children, .. } = &mut first.layout else {
        unreachable!("horizontal-pair has a split root")
    };
    let renaming_window = children[1].first_leaf_mut();
    let mut command = inject_prefix.to_vec();
    command.append(&mut renaming_window.command);
    renaming_window.command = command;

    for layout in [&mut first, &mut second] {
        wrap_launches(
            &mut layout.layout,
            layout.name,
            launch_script,
            launch_record,
        );
    }
    [first, second]
}

/// sway-stage's report of the renamed first target: exit status 1, no stdout, the first
/// entry's progress with no workspace completed, the coordinator's refusal of the rename of
/// `renamed_workspace_id` at its completion verification, and the stop advice that follows
/// started applications. A failure inside the construction, or any other, is never it.
pub fn verify_completion_failure_report(
    output: &CompletedProcess,
    first_target_name: &str,
    renamed_workspace_id: i64,
) -> Result<(), String> {
    let stderr = output.stderr.as_str();
    let refused = format!(
        "profile load: workspace {first_target_name}, layout \"profile-first\"; completed 0 \
         workspaces []; workspace completion verification: profile \
         {PROFILE_COMPLETION_SCENARIO:?}: unexpected workspace change Rename \
         id={renamed_workspace_id}"
    );
    let stopped_without_cleanup = stderr.contains(
        "application launches may have left partial layouts; stopped without cleanup or relaunch",
    );

    if output.status.code() != Some(1)
        || !output.stdout.trim().is_empty()
        || !stderr.contains(&refused)
        || !stopped_without_cleanup
        || stderr.contains("layout construction")
    {
        return Err(format!(
            "expected the coordinator to refuse the rename after the first construction (exit \
             1), got {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    Ok(())
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
