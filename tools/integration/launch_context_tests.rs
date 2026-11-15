use serde_json::json;

use super::*;
use crate::fixtures::assert_contains;
use crate::ipc::WorkspaceEvent;
use crate::profile::verify_workspace_events;

#[test]
fn cases_have_distinct_names_and_entry_modes_and_expect_the_known_bug() {
    let names: Vec<&str> = CASES.iter().map(|case| case.name()).collect();
    assert_eq!(
        names,
        [
            "profile-exec-context",
            "profile-exec-ancestry-only",
            "profile-exec-token-only"
        ]
    );
    let modes: Vec<&str> = CASES.iter().map(|case| case.entry_mode()).collect();
    assert_eq!(modes, ["inherited", "without-token", "detached"]);
    for case in CASES {
        assert_eq!(case.expected_placement(), Placement::RecreatedOrigin);
    }
}

#[test]
fn shell_quoting_keeps_every_byte_literal() {
    assert_eq!(shell_quote(""), "''");
    assert_eq!(shell_quote("plain"), "'plain'");
    assert_eq!(shell_quote("it's"), r"'it'\''s'");
    assert_eq!(
        shell_quote("a b $HOME ; $(x) `y` \"z\" \\ *"),
        "'a b $HOME ; $(x) `y` \"z\" \\ *'"
    );
}

#[test]
fn entry_scripts_quote_every_value_and_select_one_mode() {
    let command = vec![
        "/build/sway-stage".to_owned(),
        "--layouts-dir".to_owned(),
        "/tmp/run/it's layouts".to_owned(),
        "profile".to_owned(),
        "load".to_owned(),
        "profile-exec-context".to_owned(),
    ];
    let script = entry_script(Case::TokenOnly, "/tmp/run/evidence", &command);

    assert!(script.starts_with(
        "#!/bin/sh\nmode='detached'\nevidence='/tmp/run/evidence'\n\
         set -- '/build/sway-stage' '--layouts-dir' '/tmp/run/it'\\''s layouts' 'profile' \
         'load' 'profile-exec-context'\n\n"
    ));
    assert!(script.ends_with(ENTRY_SCRIPT_BODY));
    for mode in ["inherited)", "without-token)", "detached)"] {
        assert_eq!(script.matches(mode).count(), 1, "{mode}");
    }
    // The script records presence only, and only the token-less branch removes it.
    assert!(!script.contains("$XDG_ACTIVATION_TOKEN"));
    assert_eq!(script.matches("unset XDG_ACTIVATION_TOKEN").count(), 1);
    assert_contains(&script, "( run \"$@\" & )");
    assert_contains(
        &script,
        "mv \"$evidence/status.partial\" \"$evidence/status\"",
    );
}

#[test]
fn the_profile_has_one_entry_for_the_target() {
    assert_eq!(
        profile_configuration(Case::AncestryOnly, "3"),
        "version: 1\nworkspaces:\n  - workspace: 3\n    layout: profile-exec-ancestry-only\n"
    );
}

#[test]
fn the_layout_is_one_recorded_foot_window() {
    let layout = layout(
        Case::ContextInherited,
        "run",
        "/usr/bin/foot",
        "/usr/bin/sleep",
        "/tmp/record.sh",
        "/tmp/launches",
    );
    assert_eq!(layout.name, "profile-exec-context");
    let leaves = layout.layout.leaves();
    let [leaf] = leaves.as_slice() else {
        panic!("one window");
    };
    assert_eq!(
        leaf.app_id,
        "sway-stage-it-run-profile-exec-context-single-window-window1"
    );
    assert_eq!(
        leaf.command[..6],
        [
            "/bin/sh",
            "/tmp/record.sh",
            "profile-exec-context-window1",
            "/tmp/launches",
            "/usr/bin/foot",
            "--app-id=sway-stage-it-run-profile-exec-context-single-window-window1",
        ]
    );
}

fn outcome(exit_code: i32, stdout: &str, stderr: &str, context: &str) -> ExecOutcome {
    ExecOutcome {
        exit_code,
        stdout: stdout.into(),
        stderr: stderr.into(),
        context: context.into(),
    }
}

/// The shape of sway-stage's profile failure for a misplaced window.
const REFUSAL: &str = "sway-stage: profile load: workspace 3, layout \"profile-exec-context\"; \
    completed 0 workspaces []; launch window1: placement failure: wrong workspace: matching \
    window id=42 (workspace=Some(41) scratchpad=false floating=false); expected workspace=40; \
    check Sway assignment rules and activation context; stopped without cleanup or relaunch; \
    inspect windows before retrying\n";

#[test]
fn only_an_exported_launch_context_counts() {
    outcome(1, "", "", LAUNCH_CONTEXT_EXPORTED)
        .require_launch_context()
        .unwrap();
    for context in [
        "",
        "activation_token=absent\nstartup_id=present\n",
        "activation_token=present\nstartup_id=absent\n",
        "activation_token=present\n",
    ] {
        assert!(
            outcome(1, "", "", context)
                .require_launch_context()
                .is_err(),
            "{context:?}"
        );
    }
}

#[test]
fn a_misplacement_refusal_must_name_the_window_its_workspace_and_the_target() {
    let refused = outcome(1, "", REFUSAL, LAUNCH_CONTEXT_EXPORTED);
    refused.require_misplacement_refusal(42, 41, 40).unwrap();

    // Another window, workspace, or target; a different exit; stdout; or a missing summary.
    for (window_id, workspace_id, target_id) in [(43, 41, 40), (42, 4, 40), (42, 41, 4)] {
        assert!(
            refused
                .require_misplacement_refusal(window_id, workspace_id, target_id)
                .is_err()
        );
    }
    for wrong in [
        outcome(0, "", REFUSAL, LAUNCH_CONTEXT_EXPORTED),
        outcome(2, "", REFUSAL, LAUNCH_CONTEXT_EXPORTED),
        outcome(1, "loaded\n", REFUSAL, LAUNCH_CONTEXT_EXPORTED),
        outcome(
            1,
            "",
            &REFUSAL.replace(
                "completed 0 workspaces []",
                "completed 1 workspaces [(3, 40)]",
            ),
            LAUNCH_CONTEXT_EXPORTED,
        ),
        outcome(
            1,
            "",
            &REFUSAL.replace("stopped without cleanup", "stopped"),
            LAUNCH_CONTEXT_EXPORTED,
        ),
    ] {
        assert!(wrong.require_misplacement_refusal(42, 41, 40).is_err());
    }
}

#[test]
fn success_requires_exit_zero_and_no_stdout() {
    outcome(0, "", "", LAUNCH_CONTEXT_EXPORTED)
        .require_success()
        .unwrap();
    assert!(outcome(1, "", REFUSAL, "").require_success().is_err());
    assert!(outcome(0, "loaded\n", "", "").require_success().is_err());
}

fn workspace_event(
    change: &str,
    workspace_id: i64,
    name: &str,
    old_id: Option<i64>,
) -> WorkspaceEvent {
    let old = old_id.map(|id| json!({"type": "workspace", "id": id, "name": "old", "num": -1}));
    serde_json::from_value(json!({
        "change": change,
        "current": {"type": "workspace", "id": workspace_id, "name": name, "num": name.parse::<i32>().unwrap()},
        "old": old,
    }))
    .expect("a workspace event in Sway's format")
}

#[test]
fn workspace_events_distinguish_a_recreated_origin_from_a_correct_placement() {
    let origin = WorkspaceIdentity {
        id: 10,
        name: "2".into(),
    };
    let target = WorkspaceIdentity {
        id: 40,
        name: "3".into(),
    };
    let recreated = WorkspaceIdentity {
        id: 41,
        name: "2".into(),
    };
    // Selecting the target creates it and removes the empty origin.
    let selection = || {
        vec![
            workspace_event("init", 40, "3", None),
            workspace_event("focus", 40, "3", Some(10)),
            workspace_event("empty", 10, "2", None),
        ]
    };
    let with_last = |last: WorkspaceEvent| {
        let mut events = selection();
        events.push(last);
        events
    };
    let misplaced = with_last(workspace_event("init", 41, "2", None));

    let correct_expectation = expected_workspace_events(&origin, &target, None);
    let bug_expectation = expected_workspace_events(&origin, &target, Some(&recreated));
    verify_workspace_events(&selection(), &correct_expectation).unwrap();
    verify_workspace_events(&misplaced, &bug_expectation).unwrap();
    assert!(verify_workspace_events(&selection(), &bug_expectation).is_err());
    assert!(verify_workspace_events(&misplaced, &correct_expectation).is_err());

    // Recreation must be a new identity, after the origin's removal.
    let reused = with_last(workspace_event("init", 10, "2", None));
    assert!(verify_workspace_events(&reused, &bug_expectation).is_err());
    let early = vec![
        workspace_event("init", 40, "3", None),
        workspace_event("focus", 40, "3", Some(10)),
        workspace_event("init", 41, "2", None),
        workspace_event("empty", 10, "2", None),
    ];
    assert!(verify_workspace_events(&early, &bug_expectation).is_err());
}
