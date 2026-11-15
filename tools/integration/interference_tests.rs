use super::*;
use crate::fixtures::{FOOT, RUN_ID, SLEEP};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

const LAUNCH_SCRIPT: &str = "/private path/record-launch.sh";
const LAUNCH_RECORD: &str = "/private path/launches";
const INJECTED_WINDOW_SCRIPT: &str = "/private/injected-window.sh";
const TARGET_ID: i64 = 6;
const INJECTED_WINDOW_ID: i64 = 10;

fn injector(sway_commands: &[String]) -> Vec<String> {
    injector_command(
        "/private/inject.sh",
        "/usr/bin/swaymsg",
        "/private/sway-ipc.sock",
        "/private/injected-commands",
        sway_commands,
    )
}

fn fixture(case: Case) -> InterferingLoad {
    let sway_commands = sway_commands(case, "1", "2", Path::new(INJECTED_WINDOW_SCRIPT)).unwrap();
    interfering_load(
        case,
        RUN_ID,
        FOOT,
        SLEEP,
        LAUNCH_SCRIPT,
        LAUNCH_RECORD,
        &injector(&sway_commands),
    )
}

fn completed(code: i32, stderr: &str) -> CompletedProcess {
    CompletedProcess {
        status: ExitStatus::from_raw(code << 8),
        stdout: String::new(),
        stderr: stderr.into(),
    }
}

/// sway-stage's stderr for a failure during window2's wait of `case`'s layout.
fn wait_report(case: Case, detail: &str) -> String {
    format!(
        "sway-stage: wait leaf=\"window2\": layout \"{}\": {detail}; stopped without cleanup \
         or relaunch; inspect windows before retrying; log: /private/last-run.log\n",
        case.name()
    )
}

#[test]
fn the_cases_are_independently_selectable() {
    let names = CASES.map(Case::name);
    for (index, name) in names.iter().enumerate() {
        assert!(name.starts_with("interference-"));
        for (other_index, other) in names.iter().enumerate() {
            assert!(
                index == other_index || !other.contains(name),
                "{name} in {other}"
            );
        }
    }
}

#[test]
fn switches_leave_the_target_and_only_the_return_selects_it_again() {
    assert_eq!(Case::SwitchAway.switch_destinations("1", "2"), ["1"]);
    assert_eq!(
        Case::SwitchAndReturn.switch_destinations("1", "2"),
        ["1", "2"]
    );
    assert!(
        Case::UnexpectedWindow
            .switch_destinations("1", "2")
            .is_empty()
    );
}

#[test]
fn each_case_sends_one_validated_request() {
    let script = Path::new(INJECTED_WINDOW_SCRIPT);
    assert_eq!(
        sway_commands(Case::SwitchAway, "1", "2", script).unwrap(),
        ["workspace --no-auto-back-and-forth \"1\""]
    );
    // Both switches in one command list, so no other client's query falls between them.
    assert_eq!(
        sway_commands(Case::SwitchAndReturn, "1", "2", script).unwrap(),
        ["workspace --no-auto-back-and-forth \"1\"; workspace --no-auto-back-and-forth \"2\""]
    );
    assert_eq!(
        sway_commands(Case::UnexpectedWindow, "1", "2", script).unwrap(),
        ["exec /bin/sh /private/injected-window.sh"]
    );

    assert!(sway_commands(Case::SwitchAway, "1\"; kill", "2", script).is_err());
    assert!(sway_commands(Case::SwitchAndReturn, "1", "2; kill", script).is_err());
    assert!(
        sway_commands(
            Case::UnexpectedWindow,
            "1",
            "2",
            Path::new("/private path/window.sh")
        )
        .is_err()
    );
}

#[test]
fn the_injector_receives_its_requests_as_separate_literal_arguments() {
    let requests = vec!["first request".to_owned(), "second; request".to_owned()];
    assert_eq!(
        injector(&requests),
        [
            "/bin/sh",
            "/private/inject.sh",
            "/usr/bin/swaymsg",
            "/private/sway-ipc.sock",
            "/private/injected-commands",
            "first request",
            "second; request",
        ]
    );
    assert_eq!(
        expected_injector_record(&requests),
        "first request\nsecond; request\n"
    );
    // Every request names the private socket explicitly and must succeed.
    assert!(INJECTOR_SCRIPT.contains("\"$swaymsg\" -s \"$socket\" -t command"));
    assert!(INJECTOR_SCRIPT.contains("|| exit 1"));
}

#[test]
fn the_second_window_is_the_injector_and_the_others_are_recorded_foot_windows() {
    for case in CASES {
        let load = fixture(case);
        let name = case.name();
        assert_eq!(load.layout.name, name);

        let leaves = load.layout.layout.leaves();
        assert_eq!(leaves.len(), 3);
        for leaf in &leaves {
            assert_eq!(
                leaf.command[..4],
                [
                    "/bin/sh".to_owned(),
                    LAUNCH_SCRIPT.to_owned(),
                    format!("{name}-{}", leaf.layout_id),
                    LAUNCH_RECORD.to_owned(),
                ]
            );
        }
        assert_eq!(leaves[0].command[4], FOOT);
        assert_eq!(leaves[1].command[4..6], ["/bin/sh", "/private/inject.sh"]);
        assert_eq!(leaves[2].command[4], FOOT);

        let ConfiguredNode::Window(accepted_window) = &load.accepted_part.layout else {
            panic!("the accepted part is the first window alone");
        };
        assert_eq!(accepted_window.command, leaves[0].command);
        assert_eq!(load.accepted_part.expected_focus, "window1");
        assert_eq!(
            load.expected_launch_record,
            format!("{name}-window1\n{name}-window2\n")
        );
        assert_eq!(
            load.observed_target.is_some(),
            case == Case::UnexpectedWindow
        );
    }
}

#[test]
fn the_injected_window_is_foreign_to_the_layout_and_follows_the_first_window() {
    let load = fixture(Case::UnexpectedWindow);
    let observed_target = load.observed_target.expect("an injected window");
    let observed_leaves = observed_target.layout.leaves();
    let [first_window, injected_window] = observed_leaves.as_slice() else {
        panic!("the first window, then the injected window");
    };

    assert_eq!(first_window.layout_id, "window1");
    assert_eq!(first_window.command, load.layout.layout.leaves()[0].command);
    assert_eq!(injected_window.layout_id, INJECTED_LAYOUT_ID);
    assert!(injected_window.requires_process_evidence);
    assert_eq!(injected_window.command[0], FOOT);
    assert!(
        injected_window
            .command
            .contains(&format!("--app-id={}", injected_window.app_id))
    );
    assert!(injected_window.app_id.contains(RUN_ID));
    for configured in load.layout.layout.leaves() {
        assert_ne!(configured.app_id, injected_window.app_id);
    }
    assert_eq!(observed_target.expected_focus, INJECTED_LAYOUT_ID);
}

#[test]
fn either_report_of_the_focus_loss_counts_for_the_switch_cases() {
    for case in [Case::SwitchAway, Case::SwitchAndReturn] {
        for detail in [
            format!("workspace focus changed from target id={TARGET_ID}"),
            format!("snapshot: focus left target id={TARGET_ID}; after 5ms waiting"),
        ] {
            verify_failure_report(
                case,
                &completed(1, &wait_report(case, &detail)),
                TARGET_ID,
                None,
            )
            .unwrap();
        }
    }
}

#[test]
fn the_unexpected_window_must_be_refused_by_its_own_id() {
    let case = Case::UnexpectedWindow;
    let report = wait_report(
        case,
        &format!(
            "unexpected target-workspace window id={INJECTED_WINDOW_ID}: expected AppId(\"x\"), \
             observed id={INJECTED_WINDOW_ID}; refusing adoption"
        ),
    );
    verify_failure_report(
        case,
        &completed(1, &report),
        TARGET_ID,
        Some(INJECTED_WINDOW_ID),
    )
    .unwrap();

    assert!(verify_failure_report(case, &completed(1, &report), TARGET_ID, Some(11)).is_err());
    assert!(verify_failure_report(case, &completed(1, &report), TARGET_ID, None).is_err());
}

#[test]
fn other_failures_never_count_as_the_interference() {
    let case = Case::SwitchAway;
    let focus_loss = format!("workspace focus changed from target id={TARGET_ID}");
    let refused = |output: CompletedProcess, target_id: i64| {
        verify_failure_report(case, &output, target_id, None).is_err()
    };

    // A window timeout, even one that mentions a focus change, is not the expected failure.
    let timeout = wait_report(
        case,
        &format!("window timeout: after 20000ms; {focus_loss}"),
    );
    assert!(refused(completed(1, &timeout), TARGET_ID));
    // Another workspace's focus loss, another leaf's wait, or another layout.
    assert!(refused(completed(1, &wait_report(case, &focus_loss)), 7));
    let other_leaf = wait_report(case, &focus_loss).replace("window2", "window3");
    assert!(refused(completed(1, &other_leaf), TARGET_ID));
    let other_layout = wait_report(Case::SwitchAndReturn, &focus_loss);
    assert!(refused(completed(1, &other_layout), TARGET_ID));
    // An unrelated failure classification during the same wait.
    let inconsistent = wait_report(case, "snapshot: workspace/tree focus is inconsistent");
    assert!(refused(completed(1, &inconsistent), TARGET_ID));
    // The stop advice, exit status 1, and empty stdout are all required.
    let without_advice = wait_report(case, &focus_loss).replace("stopped without cleanup", "");
    assert!(refused(completed(1, &without_advice), TARGET_ID));
    assert!(refused(
        completed(0, &wait_report(case, &focus_loss)),
        TARGET_ID
    ));
    let mut with_stdout = completed(1, &wait_report(case, &focus_loss));
    with_stdout.stdout = "loaded\n".into();
    assert!(refused(with_stdout, TARGET_ID));
}

const PRIMARY_ID: i64 = 4;
const SECOND_TARGET_ID: i64 = 9;

fn primary() -> WorkspaceIdentity {
    WorkspaceIdentity {
        id: PRIMARY_ID,
        name: "2".into(),
    }
}

fn interfering_profile_fixture() -> InterferingProfile {
    let sway_commands = vec![switch_command("2").unwrap()];
    interfering_profile(
        RUN_ID,
        FOOT,
        SLEEP,
        LAUNCH_SCRIPT,
        LAUNCH_RECORD,
        &injector(&sway_commands),
    )
}

/// sway-stage's stderr for a profile failure, after its progress, at `stage_and_detail`.
fn profile_report(progress: &str, stage_and_detail: &str) -> String {
    format!(
        "sway-stage: notification unavailable: notify-send: exit status: 1\nsway-stage: profile \
         load: {progress}; {stage_and_detail}; application launches may have left partial \
         layouts; stopped without cleanup or relaunch; inspect windows before retrying; log: \
         /private/last-run.log\n"
    )
}

fn second_entry_report(detail: &str) -> String {
    profile_report(
        &format!(
            "workspace 3, layout \"profile-second\"; completed 1 workspaces [(2, {PRIMARY_ID})]"
        ),
        &format!(
            "wait leaf=\"window2\": profile \"{PROFILE_SECOND_ENTRY_SCENARIO}\": layout \
             construction: {detail}"
        ),
    )
}

fn completion_report(detail: &str) -> String {
    profile_report(
        "workspace 2, layout \"profile-first\"; completed 0 workspaces []",
        &format!(
            "workspace completion verification: profile \"{PROFILE_COMPLETION_SCENARIO}\": \
             {detail}"
        ),
    )
}

#[test]
fn the_profile_scenarios_are_distinct_interference_and_profile_scenarios() {
    for name in [PROFILE_SECOND_ENTRY_SCENARIO, PROFILE_COMPLETION_SCENARIO] {
        assert!(name.starts_with("profile-") && name.ends_with("-interference"));
        for case in CASES {
            assert!(!case.name().contains(name) && !name.contains(case.name()));
        }
    }
    assert!(!PROFILE_SECOND_ENTRY_SCENARIO.contains(PROFILE_COMPLETION_SCENARIO));
    assert!(!PROFILE_COMPLETION_SCENARIO.contains(PROFILE_SECOND_ENTRY_SCENARIO));
}

#[test]
fn the_second_entry_runs_the_injector_after_a_complete_first_entry() {
    let profile = interfering_profile_fixture();
    assert_eq!(profile.first.name, "profile-first");
    assert_eq!(profile.second.name, "profile-second");

    let first_leaves = profile.first.layout.leaves();
    assert_eq!(first_leaves.len(), 2);
    for leaf in &first_leaves {
        assert_eq!(leaf.command[2], format!("profile-first-{}", leaf.layout_id));
        assert_eq!(leaf.command[4], FOOT);
    }
    let second_leaves = profile.second.layout.leaves();
    assert_eq!(second_leaves.len(), 3);
    assert_eq!(second_leaves[0].command[4], FOOT);
    assert_eq!(
        second_leaves[1].command[4..6],
        ["/bin/sh", "/private/inject.sh"]
    );
    assert_eq!(
        second_leaves[1].command.last().unwrap(),
        "workspace --no-auto-back-and-forth \"2\""
    );
    assert_eq!(second_leaves[2].command[4], FOOT);
    for first_leaf in &first_leaves {
        for second_leaf in &second_leaves {
            assert_ne!(first_leaf.app_id, second_leaf.app_id);
        }
    }

    let ConfiguredNode::Window(accepted_window) = &profile.second_accepted_part.layout else {
        panic!("the second entry's accepted part is its first window alone");
    };
    assert_eq!(accepted_window.command, second_leaves[0].command);
    assert_eq!(profile.second_accepted_part.expected_focus, "window1");
    assert_eq!(
        profile.expected_launch_record,
        "profile-first-window1\nprofile-first-window2\nprofile-second-window1\n\
         profile-second-window2\n"
    );
}

#[test]
fn the_second_entry_must_lose_focus_of_its_own_target_after_one_completed_workspace() {
    for detail in [
        format!("workspace focus changed from target id={SECOND_TARGET_ID}"),
        format!("snapshot: focus left target id={SECOND_TARGET_ID}"),
    ] {
        let report = completed(1, &second_entry_report(&detail));
        verify_profile_failure_report(&report, &primary(), "3", SECOND_TARGET_ID).unwrap();
        assert!(verify_profile_failure_report(&report, &primary(), "3", PRIMARY_ID).is_err());
        assert!(verify_profile_failure_report(&report, &primary(), "0", SECOND_TARGET_ID).is_err());
        let other_primary = WorkspaceIdentity {
            id: PRIMARY_ID + 1,
            name: "2".into(),
        };
        assert!(
            verify_profile_failure_report(&report, &other_primary, "3", SECOND_TARGET_ID).is_err()
        );
    }

    let focus_loss = format!("workspace focus changed from target id={SECOND_TARGET_ID}");
    let refused = |stderr: &str, code: i32| {
        verify_profile_failure_report(&completed(code, stderr), &primary(), "3", SECOND_TARGET_ID)
            .is_err()
    };
    assert!(refused(&second_entry_report(&focus_loss), 0));
    assert!(refused(
        &second_entry_report(&format!("window timeout: after 20000ms; {focus_loss}")),
        1
    ));
    let none_completed = second_entry_report(&focus_loss).replace(
        &format!("completed 1 workspaces [(2, {PRIMARY_ID})]"),
        "completed 0 workspaces []",
    );
    assert!(refused(&none_completed, 1));
    let other_leaf = second_entry_report(&focus_loss).replace("window2", "window3");
    assert!(refused(&other_leaf, 1));
    let without_advice =
        second_entry_report(&focus_loss).replace("may have left partial layouts; ", "");
    assert!(refused(&without_advice, 1));
}

#[test]
fn the_first_entry_renames_its_target_before_its_second_window_starts() {
    let prefix = inject_before_launch_command(
        "/private/inject-before-launch.sh",
        "/usr/bin/swaymsg",
        "/private/sway-ipc.sock",
        "/private/injected-commands",
        "rename workspace \"2\" to \"stage-a1-renamed\"",
    );
    assert_eq!(
        prefix,
        [
            "/bin/sh",
            "/private/inject-before-launch.sh",
            "/usr/bin/swaymsg",
            "/private/sway-ipc.sock",
            "/private/injected-commands",
            "rename workspace \"2\" to \"stage-a1-renamed\"",
        ]
    );
    // The request must succeed and be recorded before the window's command replaces it.
    assert!(INJECT_BEFORE_LAUNCH_SCRIPT.contains("\"$swaymsg\" -s \"$socket\" -t command"));
    let request = INJECT_BEFORE_LAUNCH_SCRIPT.find("-t command").unwrap();
    let record = INJECT_BEFORE_LAUNCH_SCRIPT.find(">> \"$record\"").unwrap();
    let launch = INJECT_BEFORE_LAUNCH_SCRIPT.find("exec \"$@\"").unwrap();
    assert!(request < record && record < launch);

    let [first, second] =
        renaming_profile(RUN_ID, FOOT, SLEEP, LAUNCH_SCRIPT, LAUNCH_RECORD, &prefix);
    assert_eq!(first.name, "profile-first");
    assert_eq!(second.name, "profile-second");
    let first_leaves = first.layout.leaves();
    assert_eq!(first_leaves[0].command[4], FOOT);
    assert_eq!(first_leaves[1].command[4..10], prefix);
    assert_eq!(first_leaves[1].command[10], FOOT);
    for leaf in second.layout.leaves() {
        assert_eq!(
            leaf.command[2],
            format!("profile-second-{}", leaf.layout_id)
        );
        assert_eq!(leaf.command[4], FOOT);
    }
}

#[test]
fn only_the_coordinators_completion_check_may_refuse_the_rename() {
    let rename = format!("unexpected workspace change Rename id={PRIMARY_ID}");
    let report = completed(1, &completion_report(&rename));
    verify_completion_failure_report(&report, "2", PRIMARY_ID).unwrap();
    assert!(verify_completion_failure_report(&report, "3", PRIMARY_ID).is_err());
    assert!(verify_completion_failure_report(&report, "2", SECOND_TARGET_ID).is_err());

    let refused = |stderr: String| {
        verify_completion_failure_report(&completed(1, &stderr), "2", PRIMARY_ID).is_err()
    };
    // A failure inside the first construction, or a selection already attempted, is not it.
    assert!(refused(profile_report(
        "workspace 2, layout \"profile-first\"; completed 0 workspaces []",
        &format!("wait leaf=\"window2\": layout construction: {rename}"),
    )));
    assert!(refused(profile_report(
        &format!(
            "workspace 3, layout \"profile-second\"; completed 1 workspaces [(2, {PRIMARY_ID})]"
        ),
        &format!("workspace selection: {rename}"),
    )));
    assert!(refused(completion_report(&format!(
        "unexpected workspace change Focus id={PRIMARY_ID}"
    ))));
    assert!(refused(
        completion_report(&rename).replace("may have left partial layouts; ", "")
    ));
    assert!(
        verify_completion_failure_report(&completed(0, &completion_report(&rename)), "2", 4)
            .is_err()
    );
}
