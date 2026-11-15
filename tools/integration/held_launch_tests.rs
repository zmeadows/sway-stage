use super::*;
use crate::fixtures::{FOOT, RUN_ID, SLEEP, window_event};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

const LAUNCH_SCRIPT: &str = "/private path/record-launch.sh";
const LAUNCH_RECORD: &str = "/private path/launches";
const DISRUPTED_ID: i64 = 10;
const INSERTION_ID: i64 = 11;
const HELD_ID: i64 = 12;

fn hold_prefix() -> Vec<String> {
    hold_command(
        "/private path/hold.sh",
        "/private path/release",
        "/private path/stop",
        "/private path/hold-outcome",
        SLEEP,
        20_000,
    )
}

fn fixture(case: Case) -> HeldLoad {
    held_load(
        case,
        RUN_ID,
        FOOT,
        SLEEP,
        LAUNCH_SCRIPT,
        LAUNCH_RECORD,
        &hold_prefix(),
    )
}

fn focus(container_id: i64) -> WindowEvent {
    window_event("focus", container_id, None)
}

/// Window events without reported app IDs, from `(change, container ID)` pairs.
fn events(history: &[(&str, i64)]) -> Vec<WindowEvent> {
    history
        .iter()
        .map(|&(change, container_id)| window_event(change, container_id, None))
        .collect()
}

fn completed(code: i32, stderr: &str) -> CompletedProcess {
    CompletedProcess {
        status: ExitStatus::from_raw(code << 8),
        stdout: String::new(),
        stderr: stderr.into(),
    }
}

/// sway-stage's stderr for a failure during `leaf`'s wait of `case`'s layout.
fn wait_report(case: Case, leaf: &str, detail: &str) -> String {
    format!(
        "sway-stage: wait leaf=\"{leaf}\": layout \"{}\": {detail}; stopped without cleanup \
         or relaunch; inspect windows before retrying; log: /private/last-run.log\n",
        case.name()
    )
}

#[test]
fn the_cases_are_independently_selectable_interference_scenarios() {
    let names = CASES.map(Case::name);
    for (index, name) in names.iter().enumerate() {
        assert!(name.starts_with("interference-"));
        for (other_index, other) in names.iter().enumerate() {
            assert!(
                index == other_index || !other.contains(name),
                "{name} in {other}"
            );
        }
        for stage2_case in crate::interference::CASES {
            assert!(!stage2_case.name().contains(name) && !name.contains(stage2_case.name()));
        }
    }
}

#[test]
fn the_hold_waits_for_a_file_decision_within_the_window_timeout() {
    assert_eq!(
        hold_prefix(),
        [
            "/bin/sh",
            "/private path/hold.sh",
            "/private path/release",
            "/private path/stop",
            "/private path/hold-outcome",
            SLEEP,
            "400",
        ]
    );
    // The application starts only through the final exec, after the release is recorded.
    assert!(HOLD_SCRIPT.contains("while [ ! -e \"$release\" ]"));
    assert!(HOLD_SCRIPT.ends_with("printf 'released\\n' >> \"$outcome\" || exit 1\nexec \"$@\"\n"));
    assert!(HOLD_SCRIPT.contains("printf 'stopped\\n' >> \"$outcome\"\n        exit 0"));
    assert!(HOLD_SCRIPT.contains("\"$sleep\" 0.05"));
    assert_eq!(HOLD_POLL_INTERVAL_MS, 50);
    assert_eq!(expected_hold_outcome(Case::FocusRestored), "released\n");
    assert_eq!(expected_hold_outcome(Case::FocusUnstable), "stopped\n");
    assert_eq!(
        expected_hold_outcome(Case::AcceptedWindowClosed),
        "stopped\n"
    );
}

#[test]
fn only_the_held_window_waits_and_every_launch_is_recorded() {
    for (case, held_index) in [
        (Case::FocusRestored, 2),
        (Case::FocusUnstable, 2),
        (Case::AcceptedWindowClosed, 1),
    ] {
        let load = fixture(case);
        let name = case.name();
        assert_eq!(load.case, case);
        assert_eq!(load.layout.name, name);

        let leaves = load.layout.layout.leaves();
        assert_eq!(leaves.len(), 3);
        for (index, leaf) in leaves.iter().enumerate() {
            assert_eq!(
                leaf.command[..4],
                [
                    "/bin/sh".to_owned(),
                    LAUNCH_SCRIPT.to_owned(),
                    format!("{name}-{}", leaf.layout_id),
                    LAUNCH_RECORD.to_owned(),
                ]
            );
            let application = if index == held_index {
                assert_eq!(leaf.command[4..11], hold_prefix());
                &leaf.command[11..]
            } else {
                &leaf.command[4..]
            };
            assert_eq!(application[0], FOOT);
            assert!(application.contains(&format!("--app-id={}", leaf.app_id)));
        }

        let expected_record: String = leaves[..=held_index]
            .iter()
            .map(|leaf| format!("{name}-{}\n", leaf.layout_id))
            .collect();
        assert_eq!(load.launch_record_through_hold, expected_record);
    }
}

#[test]
fn focus_cases_expect_the_insertion_window_focused_and_the_close_case_the_first() {
    let restored = fixture(Case::FocusRestored);
    let accepted_leaves = restored.accepted_before_hold.layout.leaves();
    let accepted_ids: Vec<&str> = accepted_leaves
        .iter()
        .map(|leaf| leaf.layout_id.as_str())
        .collect();
    assert_eq!(accepted_ids, [DISRUPTED_LAYOUT_ID, INSERTION_LAYOUT_ID]);
    // Unsized: sway-stage applies weights only after building the whole layout.
    assert!(accepted_leaves.iter().all(|leaf| leaf.weight == 1));
    assert_eq!(
        restored.accepted_before_hold.expected_focus,
        INSERTION_LAYOUT_ID
    );
    let after = restored.expected_after_load.expect("the whole layout");
    assert_eq!(after.layout.leaves().len(), 3);
    assert_eq!(after.layout.leaves()[2].layout_id, RELEASED_LAYOUT_ID);
    assert_eq!(after.expected_focus, "window2");

    let unstable = fixture(Case::FocusUnstable);
    let after = unstable.expected_after_load.expect("the accepted windows");
    assert_eq!(after.layout.leaves().len(), 2);
    assert_eq!(after.expected_focus, DISRUPTED_LAYOUT_ID);

    let closed = fixture(Case::AcceptedWindowClosed);
    let ConfiguredNode::Window(only_window) = &closed.accepted_before_hold.layout else {
        panic!("only the first window is accepted before the hold");
    };
    assert_eq!(only_window.layout_id, CLOSED_LAYOUT_ID);
    assert_eq!(closed.accepted_before_hold.expected_focus, CLOSED_LAYOUT_ID);
    assert!(closed.expected_after_load.is_none());
}

#[test]
fn disruptions_exceed_the_restoration_limit_only_in_the_unstable_case() {
    assert_eq!(Case::FocusRestored.focus_changes(), 1);
    assert_eq!(Case::FocusRestored.restorations(), 1);
    assert_eq!(
        Case::FocusUnstable.focus_changes(),
        FOCUS_RESTORATION_LIMIT + 1
    );
    assert_eq!(Case::FocusUnstable.restorations(), FOCUS_RESTORATION_LIMIT);
    assert_eq!(Case::AcceptedWindowClosed.focus_changes(), 0);
    assert_eq!(Case::AcceptedWindowClosed.restorations(), 0);
    assert!(Case::FocusRestored.releases_hold());
    assert!(!Case::FocusUnstable.releases_hold());
    assert!(!Case::AcceptedWindowClosed.releases_hold());
}

#[test]
fn the_hold_has_started_only_once_its_own_launch_is_recorded() {
    let expected = "case-window1\ncase-window2\ncase-window3\n";
    assert!(!held_launch_started("", expected).unwrap());
    assert!(!held_launch_started("case-window1\n", expected).unwrap());
    assert!(held_launch_started(expected, expected).unwrap());
    assert!(held_launch_started("case-window2\n", expected).is_err());
    assert!(held_launch_started(&format!("{expected}case-window1\n"), expected).is_err());
}

#[test]
fn a_cycle_is_the_injected_focus_then_the_restoration() {
    let cycle = |events: &[WindowEvent]| focus_cycle(events, DISRUPTED_ID, INSERTION_ID);
    assert_eq!(cycle(&[]).unwrap(), FocusCycle::Pending);
    assert_eq!(
        cycle(&[window_event("title", DISRUPTED_ID, None)]).unwrap(),
        FocusCycle::Pending
    );
    assert_eq!(
        cycle(&[focus(DISRUPTED_ID)]).unwrap(),
        FocusCycle::Disrupted
    );
    assert_eq!(
        cycle(&[
            focus(DISRUPTED_ID),
            window_event("title", INSERTION_ID, None),
            focus(INSERTION_ID),
            window_event("new", HELD_ID, None),
        ])
        .unwrap(),
        FocusCycle::Restored { end: 2 }
    );

    // A restoration without the injected change, a repeated change, focus anywhere else,
    // and any structural activity are not this cycle.
    assert!(cycle(&[focus(INSERTION_ID)]).is_err());
    assert!(cycle(&[focus(DISRUPTED_ID), focus(DISRUPTED_ID)]).is_err());
    assert!(cycle(&[focus(HELD_ID)]).is_err());
    assert!(cycle(&[focus(DISRUPTED_ID), focus(HELD_ID)]).is_err());
    assert!(cycle(&[window_event("new", HELD_ID, None)]).is_err());
    assert!(
        cycle(&[
            focus(DISRUPTED_ID),
            window_event("close", DISRUPTED_ID, None)
        ])
        .is_err()
    );
}

#[test]
fn every_restored_change_is_answered_before_the_next_and_the_last_is_refused() {
    let verify = |history: &[(&str, i64)], indexes: &[usize], restored: usize| {
        verify_focus_cycles(
            &events(history),
            indexes,
            restored,
            DISRUPTED_ID,
            INSERTION_ID,
        )
    };
    let launches = [
        ("new", DISRUPTED_ID),
        ("focus", DISRUPTED_ID),
        ("new", INSERTION_ID),
        ("focus", INSERTION_ID),
    ];

    let mut unstable = launches.to_vec();
    let mut injection_indexes = Vec::new();
    for _ in 0..FOCUS_RESTORATION_LIMIT {
        injection_indexes.push(unstable.len());
        unstable.extend([("focus", DISRUPTED_ID), ("focus", INSERTION_ID)]);
    }
    injection_indexes.push(unstable.len());
    unstable.push(("focus", DISRUPTED_ID));
    verify(&unstable, &injection_indexes, FOCUS_RESTORATION_LIMIT).unwrap();

    // One restoration too many, or one too few, fails.
    let mut restored_again = unstable.clone();
    restored_again.push(("focus", INSERTION_ID));
    assert!(verify(&restored_again, &injection_indexes, FOCUS_RESTORATION_LIMIT).is_err());
    assert!(verify(&unstable, &injection_indexes, FOCUS_RESTORATION_LIMIT + 1).is_err());
    // A change counts only if sway-stage answered it before the next was sent.
    let mut early = injection_indexes.clone();
    early[1] = injection_indexes[0] + 1;
    assert!(verify(&unstable, &early, FOCUS_RESTORATION_LIMIT).is_err());
    // Nothing else happens between a restoration and the next change.
    let mut interleaved = unstable.clone();
    interleaved.insert(injection_indexes[1], ("new", HELD_ID));
    let shifted: Vec<usize> = injection_indexes
        .iter()
        .enumerate()
        .map(|(index, &position)| if index == 0 { position } else { position + 1 })
        .collect();
    assert!(verify(&interleaved, &shifted, FOCUS_RESTORATION_LIMIT).is_err());

    // The successful case: one restored change, then the released window and final focus.
    let mut restored = launches.to_vec();
    let injected_at = restored.len();
    restored.extend([
        ("focus", DISRUPTED_ID),
        ("focus", INSERTION_ID),
        ("new", HELD_ID),
        ("focus", HELD_ID),
        ("focus", INSERTION_ID),
    ]);
    verify(&restored, &[injected_at], 1).unwrap();
    assert!(verify(&restored, &[injected_at], 0).is_err());
    assert!(verify(&restored[..injected_at + 1], &[injected_at], 1).is_err());
}

#[test]
fn the_held_window_must_open_after_its_release() {
    require_mapped_after_release(7, 7).unwrap();
    require_mapped_after_release(8, 7).unwrap();
    assert!(require_mapped_after_release(6, 7).is_err());
}

#[test]
fn after_the_close_only_the_closed_window_closes_once() {
    let verify = |history: &[(&str, i64)], closed_window_id: i64| {
        verify_closed_window_history(&events(history), 2, closed_window_id)
    };
    let history = [
        ("new", DISRUPTED_ID),
        ("focus", DISRUPTED_ID),
        ("title", DISRUPTED_ID),
        ("close", DISRUPTED_ID),
    ];
    verify(&history, DISRUPTED_ID).unwrap();

    assert!(verify(&history[..2], DISRUPTED_ID).is_err());
    assert!(verify(&history, INSERTION_ID).is_err());
    for later in ["new", "close", "move", "floating", "focus"] {
        let mut extended = history.to_vec();
        extended.push((later, HELD_ID));
        assert!(verify(&extended, DISRUPTED_ID).is_err(), "{later}");
    }
    let mut closed_twice = history.to_vec();
    closed_twice.push(("close", DISRUPTED_ID));
    assert!(verify(&closed_twice, DISRUPTED_ID).is_err());
}

#[test]
fn unstable_focus_must_be_refused_for_the_insertion_window_after_the_limit() {
    let case = Case::FocusUnstable;
    let detail = format!(
        "focus did not stay on container id={INSERTION_ID} after 3 restorations within the \
         target workspace"
    );
    let report = wait_report(case, "window3", &detail);
    verify_failure_report(case, &completed(1, &report), INSERTION_ID).unwrap();

    assert!(verify_failure_report(case, &completed(1, &report), DISRUPTED_ID).is_err());
    assert!(verify_failure_report(case, &completed(0, &report), INSERTION_ID).is_err());
    let other_leaf = wait_report(case, "window2", &detail);
    assert!(verify_failure_report(case, &completed(1, &other_leaf), INSERTION_ID).is_err());
    let other_limit = report.replace("after 3 restorations", "after 4 restorations");
    assert!(verify_failure_report(case, &completed(1, &other_limit), INSERTION_ID).is_err());
    for unrelated in [
        format!("window timeout: after 20000ms; {detail}"),
        format!("workspace focus changed from target id=6; {detail}"),
        "hierarchy mismatch".to_owned(),
    ] {
        let report = wait_report(case, "window3", &unrelated);
        assert!(
            verify_failure_report(case, &completed(1, &report), INSERTION_ID).is_err(),
            "{unrelated}"
        );
    }
}

#[test]
fn the_closed_window_must_be_reported_by_its_own_id_during_the_second_wait() {
    let case = Case::AcceptedWindowClosed;
    for detail in [
        format!(
            "accepted window/group id={DISRUPTED_ID} closed or changed \
             floating/fullscreen/ownership state"
        ),
        format!("accepted window id={DISRUPTED_ID} disappeared"),
    ] {
        let report = wait_report(case, "window2", &detail);
        verify_failure_report(case, &completed(1, &report), DISRUPTED_ID).unwrap();
        assert!(verify_failure_report(case, &completed(1, &report), INSERTION_ID).is_err());
        let third_wait = wait_report(case, "window3", &detail);
        assert!(verify_failure_report(case, &completed(1, &third_wait), DISRUPTED_ID).is_err());
        let without_advice = report.replace("stopped without cleanup", "");
        assert!(verify_failure_report(case, &completed(1, &without_advice), DISRUPTED_ID).is_err());
    }

    let new_window_closed = wait_report(
        case,
        "window2",
        &format!("new/accepted window id={DISRUPTED_ID} closed; cannot verify"),
    );
    assert!(verify_failure_report(case, &completed(1, &new_window_closed), DISRUPTED_ID).is_err());
    // The successful case has no failure to report.
    let unstable = wait_report(Case::FocusRestored, "window3", "anything");
    assert!(verify_failure_report(Case::FocusRestored, &completed(1, &unstable), 1).is_err());
}

#[test]
fn the_log_must_number_each_restoration_of_the_insertion_window() {
    let line = |restoration: usize, id: i64| {
        format!(
            "1.000 +5ms [restore focus [con_id={id}] focus] focus moved within the target \
             workspace; restoring id={id} (restoration {restoration} of 3)\n"
        )
    };
    let three: String = (1..=3).map(|n| line(n, INSERTION_ID)).collect();
    verify_restoration_log(&three, INSERTION_ID, 3).unwrap();
    verify_restoration_log(&line(1, INSERTION_ID), INSERTION_ID, 1).unwrap();
    verify_restoration_log("", INSERTION_ID, 0).unwrap();

    assert!(verify_restoration_log(&three, INSERTION_ID, 2).is_err());
    assert!(verify_restoration_log(&line(1, INSERTION_ID), INSERTION_ID, 3).is_err());
    assert!(verify_restoration_log(&line(1, DISRUPTED_ID), INSERTION_ID, 1).is_err());
    assert!(verify_restoration_log(&line(2, INSERTION_ID), INSERTION_ID, 1).is_err());
}
