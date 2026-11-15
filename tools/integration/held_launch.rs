//! Interference that the harness itself performs while sway-stage waits for a held launch.
//!
//! One configured window's command first runs `HOLD_SCRIPT`, which keeps that application
//! from starting until the harness releases or stops it through files in the case's
//! private directory. sway-stage starts the held command only after accepting every
//! earlier window, so once the launch recorder shows it, the harness knows that those
//! windows are accepted and that sway-stage is waiting, with nothing able to map. Only
//! then does the harness interfere, by exact container ID over its own connection to the
//! private compositor, and observe each effect through fenced snapshots before its next
//! step. The decisions here are pure; lifecycle runs the cases.

use crate::ipc::{WindowChange, WindowEvent};
use crate::ownership::{is_harmless, require_quiet};
use crate::process::CompletedProcess;
use crate::profile::wrap_launches;
use crate::scenarios::{ConfiguredNode, GroupLayout, Scenario, standard_scenarios};

pub const CASES: [Case; 3] = [
    Case::FocusRestored,
    Case::FocusUnstable,
    Case::AcceptedWindowClosed,
];

/// sway-stage restores focus inside the target at most this many times per launch. The
/// harness states the production limit independently, so changing that limit fails these
/// scenarios instead of silently adjusting them.
pub const FOCUS_RESTORATION_LIMIT: usize = 3;

/// How often the held script looks for the harness's decision.
pub const HOLD_POLL_INTERVAL_MS: u64 = 50;

/// The window the harness focuses in a focus case: accepted, but not the insertion focus.
pub const DISRUPTED_LAYOUT_ID: &str = "window1";
/// The window sway-stage must keep focused while the third window launches beside it.
pub const INSERTION_LAYOUT_ID: &str = "window2";
/// The accepted window the harness closes in the close case.
pub const CLOSED_LAYOUT_ID: &str = "window1";
/// The window whose launch is held until the harness releases it in the successful case.
pub const RELEASED_LAYOUT_ID: &str = "window3";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    /// While the third window is held, the harness focuses the first window once.
    /// sway-stage must restore focus to the second, and the load then completes.
    FocusRestored,
    /// The harness moves focus to the first window once more than sway-stage restores it.
    /// After its last restoration, sway-stage must stop with focus left where it was moved.
    FocusUnstable,
    /// While the second window is held, the harness closes the first, accepted window.
    AcceptedWindowClosed,
}

impl Case {
    pub fn name(self) -> &'static str {
        match self {
            Self::FocusRestored => "interference-focus-restored",
            Self::FocusUnstable => "interference-focus-unstable",
            Self::AcceptedWindowClosed => "interference-accepted-window-closed",
        }
    }

    /// The index among the layout's three windows of the one whose launch is held.
    fn held_window_index(self) -> usize {
        match self {
            Self::FocusRestored | Self::FocusUnstable => 2,
            Self::AcceptedWindowClosed => 1,
        }
    }

    /// How often the harness moves focus away from the insertion window.
    pub fn focus_changes(self) -> usize {
        match self {
            Self::FocusRestored => 1,
            Self::FocusUnstable => FOCUS_RESTORATION_LIMIT + 1,
            Self::AcceptedWindowClosed => 0,
        }
    }

    /// How many of those focus changes sway-stage must restore before it continues or stops.
    pub fn restorations(self) -> usize {
        self.focus_changes().min(FOCUS_RESTORATION_LIMIT)
    }

    /// Only the successful case lets the held application start.
    pub fn releases_hold(self) -> bool {
        self == Self::FocusRestored
    }
}

/// Arguments: the release file, the stop file, the outcome record, sleep, and the number of
/// polls before giving up, then the held application's command. The script starts that
/// command only once the harness creates the release file, and exits without starting it
/// once the stop file appears or its polls run out. Each outcome is recorded before the
/// script execs or exits, so the record shows whether the application could have started
/// and that no hold is still waiting.
pub const HOLD_SCRIPT: &str = "\
#!/bin/sh
release=$1 stop=$2 outcome=$3 sleep=$4 remaining_polls=$5
shift 5
while [ ! -e \"$release\" ]; do
    if [ -e \"$stop\" ]; then
        printf 'stopped\\n' >> \"$outcome\"
        exit 0
    fi
    if [ \"$remaining_polls\" -le 0 ]; then
        printf 'expired\\n' >> \"$outcome\"
        exit 1
    fi
    remaining_polls=$((remaining_polls - 1))
    \"$sleep\" 0.05 || exit 1
done
printf 'released\\n' >> \"$outcome\" || exit 1
exec \"$@\"
";

/// The command prefix that runs `HOLD_SCRIPT` before the held application's own command.
/// The hold lasts at most `window_timeout_ms`, as sway-stage's wait for the window does.
pub fn hold_command(
    script: &str,
    release_file: &str,
    stop_file: &str,
    outcome_record: &str,
    sleep: &str,
    window_timeout_ms: u64,
) -> Vec<String> {
    let polls = window_timeout_ms / HOLD_POLL_INTERVAL_MS;
    vec![
        "/bin/sh".into(),
        script.into(),
        release_file.into(),
        stop_file.into(),
        outcome_record.into(),
        sleep.into(),
        polls.to_string(),
    ]
}

/// What the hold's outcome record holds once the case's decision took effect.
pub fn expected_hold_outcome(case: Case) -> &'static str {
    if case.releases_hold() {
        "released\n"
    } else {
        "stopped\n"
    }
}

/// A three-window layout with one held launch, and what the harness expects of it.
pub struct HeldLoad {
    pub case: Case,
    /// What the configuration asks sway-stage to build.
    pub layout: Scenario,
    /// The windows sway-stage accepts before the held launch, focused on the window that
    /// the held window must be inserted after.
    pub accepted_before_hold: Scenario,
    /// What the target must hold once sway-stage exits: the whole layout after a restored
    /// focus change; the accepted windows, with focus where the harness last moved it, after
    /// unstable focus; nothing after the accepted window closed.
    pub expected_after_load: Option<Scenario>,
    /// The launch record once the held launch has started: one line per launch, in order.
    pub launch_record_through_hold: String,
}

/// The case's layout: `uneven-siblings`, three Foot windows in one horizontal split, with
/// the held window's command prefixed by `hold_prefix`, from `hold_command`. Every launch is
/// recorded through `launch_script`.
pub fn held_load(
    case: Case,
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
    hold_prefix: &[String],
) -> HeldLoad {
    let unique_id = format!("{run_id}-{}", case.name());
    let mut layout = standard_scenarios(&unique_id, foot, sleep)
        .into_iter()
        .find(|scenario| scenario.name == "uneven-siblings")
        .expect("standard uneven-siblings scenario");
    layout.name = case.name();

    let held_window_index = case.held_window_index();
    let ConfiguredNode::Group { children, .. } = &mut layout.layout else {
        unreachable!("uneven-siblings has a split root")
    };
    let held_window = children[held_window_index].first_leaf_mut();
    let mut held_command = hold_prefix.to_vec();
    held_command.append(&mut held_window.command);
    held_window.command = held_command;
    wrap_launches(
        &mut layout.layout,
        layout.name,
        launch_script,
        launch_record,
    );

    let leaves = layout.layout.leaves();
    let launch_record_through_hold = leaves[..=held_window_index]
        .iter()
        .map(|leaf| format!("{}-{}\n", layout.name, leaf.layout_id))
        .collect();

    let accepted_before_hold = match case {
        Case::FocusRestored | Case::FocusUnstable => {
            accepted_windows(&layout, held_window_index, INSERTION_LAYOUT_ID)
        }
        Case::AcceptedWindowClosed => {
            accepted_windows(&layout, held_window_index, CLOSED_LAYOUT_ID)
        }
    };
    let expected_after_load = match case {
        Case::FocusRestored => Some(layout.clone()),
        Case::FocusUnstable => Some(accepted_windows(
            &layout,
            held_window_index,
            DISRUPTED_LAYOUT_ID,
        )),
        Case::AcceptedWindowClosed => None,
    };

    HeldLoad {
        case,
        layout,
        accepted_before_hold,
        expected_after_load,
        launch_record_through_hold,
    }
}

/// The first `count` windows of `layout` as a layout of their own, focused on `focus`.
/// sway-stage sizes splits only after building the whole layout, so two windows accepted
/// before the hold share their split equally.
fn accepted_windows(layout: &Scenario, count: usize, focus: &'static str) -> Scenario {
    let mut leaves: Vec<_> = layout.layout.leaves()[..count]
        .iter()
        .map(|&leaf| leaf.clone())
        .collect();

    let mut accepted = layout.clone();
    accepted.layout = if let [only_window] = leaves.as_slice() {
        ConfiguredNode::Window(only_window.clone())
    } else {
        for leaf in &mut leaves {
            leaf.weight = 1;
        }
        ConfiguredNode::Group {
            layout: GroupLayout::Horizontal,
            active: 1,
            weight: 1,
            children: leaves.into_iter().map(ConfiguredNode::Window).collect(),
        }
    };
    accepted.expected_focus = focus;
    accepted.configured_focus = Some(focus);
    accepted
}

/// Whether the launch record shows that the held launch has started. Every record before
/// that is a prefix of `launch_record_through_hold`; anything else, such as a launch out
/// of order or a later window, fails.
pub fn held_launch_started(
    observed_record: &str,
    launch_record_through_hold: &str,
) -> Result<bool, String> {
    if !launch_record_through_hold.starts_with(observed_record) {
        return Err(format!(
            "launch order/count differs: expected {launch_record_through_hold:?} before the \
             hold, observed {observed_record:?}"
        ));
    }
    Ok(observed_record == launch_record_through_hold)
}

/// How far one injected focus change has progressed, judged from the window events
/// observed since the harness sent it.
#[derive(Debug, PartialEq, Eq)]
pub enum FocusCycle {
    /// The injected change has not been observed yet.
    Pending,
    /// Focus moved to the disrupted window, and nothing has moved it since.
    Disrupted,
    /// sway-stage focused the insertion window again. `end` is the position of that focus
    /// event after the injection; the caller judges any later events.
    Restored { end: usize },
}

/// Reads the window events observed since one injected focus change. The first event that
/// is not a harmless title, urgency, or mark change must be the focus of
/// `disrupted_window_id`, and the next the focus of `insertion_window_id`. Nothing else may
/// happen in between: no window opens, closes, or receives focus.
pub fn focus_cycle(
    events_since_injection: &[WindowEvent],
    disrupted_window_id: i64,
    insertion_window_id: i64,
) -> Result<FocusCycle, String> {
    let mut progress = FocusCycle::Pending;
    for (position, event) in events_since_injection.iter().enumerate() {
        if is_harmless(event.change) {
            continue;
        }

        let focused_window_id = (event.change == WindowChange::Focus).then_some(event.container.id);
        progress = match (progress, focused_window_id) {
            (FocusCycle::Pending, Some(id)) if id == disrupted_window_id => FocusCycle::Disrupted,
            (FocusCycle::Disrupted, Some(id)) if id == insertion_window_id => {
                return Ok(FocusCycle::Restored { end: position });
            }
            (progress, _) => {
                return Err(format!(
                    "unexpected window activity during focus interference ({progress:?}): \
                     {:?} id={}",
                    event.change, event.container.id
                ));
            }
        };
    }
    Ok(progress)
}

/// Requires the load's complete, ordered window events to show each injected focus change
/// at `injection_indexes`, the positions among them when the harness sent it. The first
/// `restorations` injections must each be followed by sway-stage's restoration, before the
/// next injection. A later injection, the last of the unstable case, must be the final
/// activity: focus stays where the harness moved it.
pub fn verify_focus_cycles(
    window_events: &[WindowEvent],
    injection_indexes: &[usize],
    restorations: usize,
    disrupted_window_id: i64,
    insertion_window_id: i64,
) -> Result<(), String> {
    for (injection, &injected_at) in injection_indexes.iter().enumerate() {
        let next_injection = injection_indexes
            .get(injection + 1)
            .copied()
            .unwrap_or(window_events.len());
        let events_since_injection = window_events
            .get(injected_at..)
            .ok_or("invalid focus injection index")?;
        let cycle = focus_cycle(
            events_since_injection,
            disrupted_window_id,
            insertion_window_id,
        )?;

        let ordinal = injection + 1;
        match cycle {
            FocusCycle::Restored { end } if injection < restorations => {
                let restored_at = injected_at + end;
                if restored_at >= next_injection {
                    return Err(format!(
                        "focus change {ordinal} was not restored before the next one"
                    ));
                }
                // The harness injects again only once it has seen this restoration.
                if injection + 1 < injection_indexes.len() {
                    require_quiet(&window_events[restored_at + 1..next_injection], None)?;
                }
            }
            FocusCycle::Disrupted if injection >= restorations => {}
            cycle => {
                let expected = if injection < restorations {
                    "a restoration"
                } else {
                    "focus left on the disrupted window"
                };
                return Err(format!(
                    "focus change {ordinal}: expected {expected}, observed {cycle:?}"
                ));
            }
        }
    }
    Ok(())
}

/// Requires the held window's new event, at `held_window_event_index`, to follow the
/// release at `released_at`: no window could map while the harness interfered.
pub fn require_mapped_after_release(
    held_window_event_index: usize,
    released_at: usize,
) -> Result<(), String> {
    if held_window_event_index < released_at {
        return Err("the held window opened before the harness released it".into());
    }
    Ok(())
}

/// Requires everything after the harness's close request, at `close_requested_at` among the
/// load's ordered window events, to be harmless apart from exactly one close event: the
/// closed window's. No replacement opens and nothing else closes, moves, or takes focus.
pub fn verify_closed_window_history(
    window_events: &[WindowEvent],
    close_requested_at: usize,
    closed_window_id: i64,
) -> Result<(), String> {
    let events_since_close = window_events
        .get(close_requested_at..)
        .ok_or("invalid close request index")?;
    let mut closes = 0;
    for event in events_since_close {
        if is_harmless(event.change) {
            continue;
        }
        if event.change == WindowChange::Close && event.container.id == closed_window_id {
            closes += 1;
            continue;
        }
        return Err(format!(
            "unexpected window activity after the close: {:?} id={}",
            event.change, event.container.id
        ));
    }
    if closes != 1 {
        return Err(format!(
            "expected one close event for id={closed_window_id}, observed {closes}"
        ));
    }
    Ok(())
}

/// sway-stage's report of a failing case: exit status 1, no stdout, a failure in the held
/// window's wait with the stop advice that follows a started application, and no window
/// timeout.
///
/// Unstable focus must be refused for the insertion window `window_id` after exactly the
/// restoration limit. A closed window must be reported by its own `window_id` as an
/// accepted window that closed or disappeared.
pub fn verify_failure_report(
    case: Case,
    output: &CompletedProcess,
    window_id: i64,
) -> Result<(), String> {
    let stderr = output.stderr.as_str();
    let held_layout_id = format!("window{}", case.held_window_index() + 1);
    // sway-stage names the failing stage, then the layout, then the cause.
    let wait_context = format!("wait leaf={held_layout_id:?}: layout {:?}: ", case.name());
    let reported = match case {
        Case::FocusUnstable => stderr.contains(&format!(
            "{wait_context}focus did not stay on container id={window_id} after \
             {FOCUS_RESTORATION_LIMIT} restorations within the target workspace"
        )),
        Case::AcceptedWindowClosed => {
            let closed = format!(
                "{wait_context}accepted window/group id={window_id} closed or changed \
                 floating/fullscreen/ownership state"
            );
            let disappeared = format!("{wait_context}accepted window id={window_id} disappeared");
            stderr.contains(&closed) || stderr.contains(&disappeared)
        }
        Case::FocusRestored => false,
    };
    let stopped_without_cleanup = stderr.contains("stopped without cleanup or relaunch");

    if output.status.code() != Some(1)
        || !output.stdout.trim().is_empty()
        || !reported
        || !stopped_without_cleanup
        || stderr.contains("window timeout")
    {
        let expected = match case {
            Case::FocusUnstable => "unstable focus refused during window3's wait",
            Case::AcceptedWindowClosed => "the closed window1 reported during window2's wait",
            Case::FocusRestored => "no failure",
        };
        return Err(format!(
            "expected {expected} (exit 1), got {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    Ok(())
}

/// Requires sway-stage's own log to record exactly `restorations` focus restorations, each
/// of `insertion_window_id`, numbered in order against the restoration limit. The log is
/// supporting evidence; the ordered focus events are the independent proof.
pub fn verify_restoration_log(
    log: &str,
    insertion_window_id: i64,
    restorations: usize,
) -> Result<(), String> {
    let recorded = log.matches("restoring id=").count();
    let all_numbered = (1..=restorations).all(|restoration| {
        log.contains(&format!(
            "restoring id={insertion_window_id} (restoration {restoration} of \
             {FOCUS_RESTORATION_LIMIT})"
        ))
    });
    if recorded != restorations || !all_numbered {
        return Err(format!(
            "sway-stage's log records {recorded} focus restorations, expected {restorations} \
             of id={insertion_window_id}"
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "held_launch_tests.rs"]
mod tests;
