use serde_json::json;

use super::*;
use crate::fixtures::assert_contains;
use crate::ipc::WorkspaceEvent;
use crate::profile::verify_workspace_events;

#[test]
fn cases_have_distinct_names_entry_points_and_entry_modes() {
    let names: Vec<&str> = CASES.iter().map(|case| case.name()).collect();
    assert_eq!(
        names,
        [
            "layout-exec-direct",
            "layout-exec-menu",
            "profile-exec-context",
            "profile-exec-ancestry-only",
            "profile-exec-token-only",
            "profile-exec-menu"
        ]
    );
    let modes: Vec<&str> = CASES.iter().map(|case| case.entry_mode()).collect();
    assert_eq!(
        modes,
        [
            "inherited",
            "inherited",
            "inherited",
            "without-token",
            "detached",
            "inherited"
        ]
    );
    let profile_loads: Vec<bool> = CASES.iter().map(|case| case.loads_profile()).collect();
    assert_eq!(profile_loads, [false, false, true, true, true, true]);
    let base_layouts: Vec<&str> = CASES.iter().map(|case| case.base_layout()).collect();
    assert_eq!(
        base_layouts,
        [
            "horizontal-pair",
            "horizontal-pair",
            "single-window",
            "single-window",
            "single-window",
            "single-window"
        ]
    );

    // Only the menu cases select a row, and equal layout and profile names stay distinct.
    let selections: Vec<Option<String>> = CASES.iter().map(|case| case.menu_selection()).collect();
    assert_eq!(
        selections,
        [
            None,
            Some("Layout: layout-exec-menu".to_owned()),
            None,
            None,
            None,
            Some("Profile: profile-exec-menu".to_owned())
        ]
    );
}

#[test]
fn each_entry_point_has_its_own_command_line() {
    let command = |case: Case| sway_stage_arguments(case, "/build/sway-stage", "/run/l", "/run/p");
    let directories = [
        "/build/sway-stage",
        "--layouts-dir",
        "/run/l",
        "--profiles-dir",
        "/run/p",
    ];
    for (case, entry_point) in [
        (Case::LayoutLoad, vec!["load", "layout-exec-direct"]),
        (Case::MenuLayout, vec!["menu"]),
        (
            Case::ContextInherited,
            vec!["profile", "load", "profile-exec-context"],
        ),
        (
            Case::TokenOnly,
            vec!["profile", "load", "profile-exec-token-only"],
        ),
        (Case::MenuProfile, vec!["menu"]),
    ] {
        let mut expected = directories.to_vec();
        expected.extend(entry_point);
        assert_eq!(command(case), expected, "{case:?}");
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
    let script = entry_script(Case::TokenOnly, "/tmp/run/evidence", None, &command);

    assert!(script.starts_with(
        "#!/bin/sh\nmode='detached'\nevidence='/tmp/run/evidence'\nfuzzel_directory=''\n\
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
fn only_a_menu_entry_script_puts_the_fuzzel_substitute_first_on_path() {
    let command = vec!["/build/sway-stage".to_owned(), "menu".to_owned()];
    let script = entry_script(
        Case::MenuProfile,
        "/tmp/run/evidence",
        Some("/tmp/run/fuzzel substitute"),
        &command,
    );
    assert!(script.starts_with(
        "#!/bin/sh\nmode='inherited'\nevidence='/tmp/run/evidence'\n\
         fuzzel_directory='/tmp/run/fuzzel substitute'\nset -- '/build/sway-stage' 'menu'\n\n"
    ));
    // The prefix is applied only when the directory is set, before sway-stage starts.
    assert_contains(
        &script,
        "if [ -n \"$fuzzel_directory\" ]; then\n    PATH=$fuzzel_directory:$PATH\n    export PATH\nfi",
    );
    assert!(script.find("export PATH").unwrap() < script.find("case $mode in").unwrap());
}

#[test]
fn the_fuzzel_substitute_quotes_its_selection_and_selects_by_index() {
    let script = fuzzel_substitute_script("Layout: it's", "/tmp/run/evidence");
    assert!(
        script.starts_with(
            "#!/bin/sh\nselection='Layout: it'\\''s'\nevidence='/tmp/run/evidence'\n\n"
        )
    );
    assert!(script.ends_with(FUZZEL_SUBSTITUTE_BODY));
    // Noclobber makes a second prompt fail, and an absent or repeated row exits 1.
    assert!(script.contains("set -C\n"));
    assert_contains(&script, "[ -z \"$selected_index\" ] || exit 1");
    assert_contains(&script, "[ -n \"$selected_index\" ] || exit 1");
    // It reads every row before answering, and prints only the index.
    assert!(
        script.find("done").unwrap()
            < script
                .rfind("printf '%s\\n' \"$selected_index\"\n")
                .unwrap()
    );
}

fn menu_evidence(arguments: &str, rows: &str, selected_index: &str) -> MenuEvidence {
    MenuEvidence {
        arguments: arguments.into(),
        rows: rows.into(),
        selected_index: selected_index.into(),
    }
}

const MENU_ARGUMENTS: &str = "--dmenu\n--no-exit-on-keyboard-focus-loss\n--no-sort\n--index\n--output=HEADLESS-1\n--prompt=> \n";

#[test]
fn menu_evidence_requires_the_prompt_the_rows_and_the_case_selection() {
    let rows = "Capture session profile…\nLayout: layout-exec-menu\nProfile: layout-exec-menu\n";
    menu_evidence(MENU_ARGUMENTS, rows, "1\n")
        .verify(Case::MenuLayout)
        .unwrap();
    let rows = "Capture session profile…\nLayout: profile-exec-menu\nProfile: profile-exec-menu\n";
    menu_evidence(MENU_ARGUMENTS, rows, "2\n")
        .verify(Case::MenuProfile)
        .unwrap();

    // Another row, a different prompt or output, or other offered rows all fail.
    assert!(
        menu_evidence(MENU_ARGUMENTS, rows, "1\n")
            .verify(Case::MenuProfile)
            .is_err()
    );
    let other_output = MENU_ARGUMENTS.replace("HEADLESS-1", "HEADLESS-2");
    let without_index = MENU_ARGUMENTS.replace("--index\n", "");
    for arguments in [other_output.as_str(), without_index.as_str(), ""] {
        assert!(
            menu_evidence(arguments, rows, "2\n")
                .verify(Case::MenuProfile)
                .is_err(),
            "{arguments:?}"
        );
    }
    for other_rows in [
        "Layout: profile-exec-menu\nProfile: profile-exec-menu\n",
        "Capture session profile…\nLayout: profile-exec-menu\n[invalid] Profile: profile-exec-menu\n",
        "Capture session profile…\nLayout: profile-exec-menu\nProfile: profile-exec-menu\nProfile: other\n",
    ] {
        assert!(
            menu_evidence(MENU_ARGUMENTS, other_rows, "2\n")
                .verify(Case::MenuProfile)
                .is_err(),
            "{other_rows:?}"
        );
    }
}

#[test]
fn the_profile_has_one_entry_for_the_target() {
    assert_eq!(
        profile_configuration(Case::AncestryOnly, "3"),
        "version: 1\nworkspaces:\n  - workspace: 3\n    layout: profile-exec-ancestry-only\n"
    );
}

#[test]
fn profile_cases_load_one_recorded_window_and_layout_cases_two() {
    let profile_layout = layout(
        Case::ContextInherited,
        "run",
        "/usr/bin/foot",
        "/usr/bin/sleep",
        "/tmp/record.sh",
        "/tmp/launches",
    );
    assert_eq!(profile_layout.name, "profile-exec-context");
    let leaves = profile_layout.layout.leaves();
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

    let menu_layout = layout(
        Case::MenuLayout,
        "run",
        "/usr/bin/foot",
        "/usr/bin/sleep",
        "/tmp/record.sh",
        "/tmp/launches",
    );
    assert_eq!(menu_layout.name, "layout-exec-menu");
    let records: Vec<&str> = menu_layout
        .layout
        .leaves()
        .iter()
        .map(|leaf| leaf.command[2].as_str())
        .collect();
    assert_eq!(
        records,
        ["layout-exec-menu-window1", "layout-exec-menu-window2"]
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

/// The shape of sway-stage's profile failure, as from a misplaced window.
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
fn success_requires_exit_zero_and_no_stdout() {
    outcome(0, "", "", LAUNCH_CONTEXT_EXPORTED)
        .require_success()
        .unwrap();
    assert!(
        outcome(1, "", REFUSAL, LAUNCH_CONTEXT_EXPORTED)
            .require_success()
            .is_err()
    );
    assert!(
        outcome(0, "loaded\n", "", LAUNCH_CONTEXT_EXPORTED)
            .require_success()
            .is_err()
    );
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
fn workspace_events_require_the_target_and_no_recreated_origin() {
    let origin = WorkspaceIdentity {
        id: 10,
        name: "2".into(),
    };
    let target = WorkspaceIdentity {
        id: 40,
        name: "3".into(),
    };
    // Selecting the target creates it and removes the empty origin.
    let selection = || {
        vec![
            workspace_event("init", 40, "3", None),
            workspace_event("focus", 40, "3", Some(10)),
            workspace_event("empty", 10, "2", None),
        ]
    };
    let expected = expected_workspace_events(&origin, &target);
    verify_workspace_events(&selection(), &expected).unwrap();

    // The misplacement's recreation of the origin, for the window, is an extra event.
    let mut recreated_origin = selection();
    recreated_origin.push(workspace_event("init", 41, "2", None));
    assert!(verify_workspace_events(&recreated_origin, &expected).is_err());

    // Another target, or a switch from elsewhere, also differs.
    let other_target = vec![
        workspace_event("init", 41, "3", None),
        workspace_event("focus", 41, "3", Some(10)),
        workspace_event("empty", 10, "2", None),
    ];
    assert!(verify_workspace_events(&other_target, &expected).is_err());
    let other_origin = vec![
        workspace_event("init", 40, "3", None),
        workspace_event("focus", 40, "3", Some(11)),
        workspace_event("empty", 10, "2", None),
    ];
    assert!(verify_workspace_events(&other_origin, &expected).is_err());
}
