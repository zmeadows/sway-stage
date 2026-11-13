use super::*;
use crate::fixtures::{RUN_ID, assert_contains, standard_scenario};
use std::collections::BTreeMap;

/// The comment header `sway-stage capture` prints before every draft.
const DRAFT_HEADER: &str = "\
# Layout draft from `sway-stage capture`. Before it can run, replace every
# `command: null` with the executable and arguments that open that window, and any
# `match: null` with exactly one exact `app_id` or `class`. Weights are percentages of
# the enclosing split. Validate the edited file with `sway-stage check NAME`.
";

/// The additional header of an inferred draft.
const INFERENCE_HEADER: &str = "\
# Review recovered commands before executing or sharing: process arguments are
# best-effort, may contain secrets, and may not reproduce the original application.
";

fn scenario_app_id(scenario: &str, layout_id: &str) -> String {
    format!("sway-stage-it-{RUN_ID}-{scenario}-{layout_id}")
}

/// horizontal-pair measured at 1150:770 pixels.
fn horizontal_pair_measurements() -> LayoutMeasurements {
    LayoutMeasurements {
        shares: BTreeMap::from([
            (vec![0], 100.0 * 1150.0 / 1920.0),
            (vec![1], 100.0 * 770.0 / 1920.0),
        ]),
        sizing_notes: Vec::new(),
    }
}

/// An inference-disabled capture of horizontal-pair as sway-stage prints it.
fn horizontal_pair_draft() -> String {
    format!(
        "{DRAFT_HEADER}\
version: 1
focus: window2
layout:
  group: splith
  children:
    - id: window1
      weight: 59.9
      command: null
      match:
        app_id: {}
    - id: window2
      weight: 40.1
      command: null
      match:
        app_id: {}
",
        scenario_app_id("horizontal-pair", "window1"),
        scenario_app_id("horizontal-pair", "window2"),
    )
}

#[test]
fn capture_lines_drop_comments_blank_lines_and_trailing_space() {
    let draft = "# header\n\nversion: 1  \n  # indented comment\n   \nlayout:\n";

    assert_eq!(capture_lines(draft), ["version: 1", "layout:"]);
}

#[test]
fn matching_default_capture_is_filled_for_check() {
    let scenario = standard_scenario("horizontal-pair");
    let filled = verify_capture(
        &horizontal_pair_draft(),
        &scenario,
        &horizontal_pair_measurements(),
    )
    .unwrap();

    let expected = horizontal_pair_draft()
        .strip_prefix(DRAFT_HEADER)
        .unwrap()
        .replace("command: null", r#"command: ["true"]"#);

    assert_eq!(filled, expected);
}

#[test]
fn nested_capture_matches_the_measured_hierarchy() {
    let scenario = standard_scenario("mixed-nesting");
    let shares = [
        (vec![0], 40.0),
        (vec![1], 30.0),
        (vec![1, 0], 59.99),
        (vec![1, 1], 40.01),
        (vec![2], 30.0),
    ];
    let measurements = LayoutMeasurements {
        shares: BTreeMap::from(shares),
        sizing_notes: Vec::new(),
    };
    let id = |layout_id: &str| scenario_app_id("mixed-nesting", layout_id);
    // Quoted identifiers are equivalent YAML and must still compare equal.
    let draft = format!(
        "{DRAFT_HEADER}\
version: 1
focus: 'window3'
layout:
  group: splith
  children:
    - id: window1
      weight: 40.0
      command: null
      match:
        app_id: {}
    - group: splitv
      weight: 30.0
      children:
        - id: window2
          weight: 59.99
          command: null
          match:
            app_id: \"{}\"
        - id: \"window3\"
          weight: 40.01
          command: null
          match:
            app_id: {}
    - id: window4
      weight: 30.0
      command: null
      match:
        app_id: {}
",
        id("window1"),
        id("window2"),
        id("window3"),
        id("window4"),
    );

    let filled = verify_capture(&draft, &scenario, &measurements).unwrap();

    assert_eq!(filled.matches(r#"command: ["true"]"#).count(), 4);
    assert!(!filled.contains('#'));
}

#[test]
fn capture_differences_report_the_failed_comparison() {
    let scenario = standard_scenario("horizontal-pair");
    let window1 = scenario_app_id("horizontal-pair", "window1");
    let cases = [
        ("focus: window2", "focus: window1", "capture value"),
        ("weight: 59.9", "weight: 59.92", "capture sizing"),
        ("weight: 59.9", "weight: NaN", "capture sizing"),
        ("weight: 59.9", "weight: inf", "capture sizing"),
        (
            "weight: 59.9",
            "weight: sixty",
            "capture weight is not numeric",
        ),
        ("group: splith", "group: splitv", "capture value"),
        ("    - id: window1", "    - id: window2", "capture value"),
        (
            "      command: null\n      match:\n        app_id: sway",
            "      command: [foot]\n      match:\n        app_id: sway",
            "capture value",
        ),
        (
            "      match:\n        app_id: sway",
            "      match:\n        class: sway",
            "capture field",
        ),
        (
            "      weight: 59.9",
            "        weight: 59.9",
            "capture indentation/arrangement",
        ),
        (
            "    - id: window1",
            "      id: window1",
            "capture indentation/arrangement",
        ),
        ("      weight: 59.9", "      share: 59.9", "capture field"),
        ("      weight: 59.9", "      weight", "capture field"),
        (
            &format!("app_id: {window1}"),
            "app_id: firefox-esr",
            "capture value",
        ),
    ];
    for (original, replacement, expected) in cases {
        let draft = horizontal_pair_draft().replacen(original, replacement, 1);
        assert_ne!(
            draft,
            horizontal_pair_draft(),
            "{original:?} is not in the draft"
        );
        let error = verify_capture(&draft, &scenario, &horizontal_pair_measurements()).unwrap_err();
        assert_contains(&error, expected);
    }

    let extra_field =
        horizontal_pair_draft().replace("version: 1\n", "version: 1\nwindow_timeout_ms: 20000\n");
    let error =
        verify_capture(&extra_field, &scenario, &horizontal_pair_measurements()).unwrap_err();

    assert_contains(&error, "capture draft has unexpected fields/structure");

    let unmeasured = LayoutMeasurements {
        shares: BTreeMap::new(),
        sizing_notes: Vec::new(),
    };
    let error = verify_capture(&horizontal_pair_draft(), &scenario, &unmeasured).unwrap_err();
    assert_eq!(error, "no measured share for layout.children[0]");
}

/// An inferred capture of horizontal-pair whose recovered commands are the
/// given lines, each already indented as a block sequence item.
fn inferred_horizontal_pair_draft(first_command: &str, second_command: &str) -> String {
    horizontal_pair_draft()
        .replacen(
            DRAFT_HEADER,
            &format!("{DRAFT_HEADER}{INFERENCE_HEADER}"),
            1,
        )
        .replacen(
            "      command: null\n",
            &format!("      command:\n{first_command}"),
            1,
        )
        .replacen(
            "      command: null\n",
            &format!("      command:\n{second_command}"),
            1,
        )
}

fn foot_command(app_id_arguments: &[&str]) -> String {
    let mut lines = vec!["        - /usr/bin/foot".to_owned()];
    lines.extend(
        app_id_arguments
            .iter()
            .map(|argument| format!("        - {argument}")),
    );
    lines.push("        - /usr/bin/sleep".into());
    lines.push("        - '300'".into());
    lines.join("\n") + "\n"
}

#[test]
fn inferred_capture_accepts_either_app_id_argument_form() {
    let scenario = standard_scenario("horizontal-pair");
    let window1 = scenario_app_id("horizontal-pair", "window1");
    let window2 = scenario_app_id("horizontal-pair", "window2");
    let inferred = inferred_horizontal_pair_draft(
        &foot_command(&[&format!("--app-id={window1}")]),
        &foot_command(&["--app-id", &format!("\"{window2}\"")]),
    );
    verify_inferred_capture(&inferred, &horizontal_pair_draft(), &scenario.layout).unwrap();
}

#[test]
fn inferred_capture_accepts_mixed_command_styles_with_literal_arguments() {
    let scenario = standard_scenario("horizontal-pair");
    let first = vec![
        "/usr/bin/foot".to_owned(),
        format!("--app-id={}", scenario_app_id("horizontal-pair", "window1")),
        "literal: [a,b] # 'quote' \\ $HOME\nnext".to_owned(),
        "".to_owned(),
        "null".to_owned(),
    ];
    let second = foot_command(&[&format!(
        "--app-id={}",
        scenario_app_id("horizontal-pair", "window2")
    )]);
    let inferred = horizontal_pair_draft()
        .replacen(
            "      command: null\n",
            &format!(
                "      command: {}\n",
                serde_json::to_string(&first).unwrap()
            ),
            1,
        )
        .replacen(
            "      command: null\n",
            &format!("      command:\n{second}"),
            1,
        );
    verify_inferred_capture(&inferred, &horizontal_pair_draft(), &scenario.layout).unwrap();
    let commands = inferred_commands(&inferred, &horizontal_pair_draft()).unwrap();

    assert_eq!(commands[0], first);
}

#[test]
fn inferred_capture_mismatches_fail_without_revealing_arguments() {
    let scenario = standard_scenario("horizontal-pair");
    let window1 = scenario_app_id("horizontal-pair", "window1");
    let window2 = scenario_app_id("horizontal-pair", "window2");
    let secret = "--token=hunter2";
    let correct_first = foot_command(&[&format!("--app-id={window1}"), secret]);
    let correct_second = foot_command(&[&format!("--app-id={window2}")]);

    let swapped = inferred_horizontal_pair_draft(&correct_second, &correct_first);
    let missing_value =
        inferred_horizontal_pair_draft(&correct_first, &foot_command(&["--app-id"]));
    let other_option = inferred_horizontal_pair_draft(
        &correct_first,
        &foot_command(&[&format!("--title={window2}")]),
    );
    let cases = [
        (
            swapped,
            "inferred command does not identify the correct Foot window: window1",
        ),
        (
            missing_value,
            "inferred command does not identify the correct Foot window: window2",
        ),
        (
            other_option,
            "inferred command does not identify the correct Foot window: window2",
        ),
        (
            inferred_horizontal_pair_draft(&correct_first, "        - ''\n"),
            "inferred capture has an empty executable",
        ),
        (
            inferred_horizontal_pair_draft(&correct_first, ""),
            "inferred capture has an empty executable",
        ),
        (
            // Inference missed the second window, so its placeholder remains.
            horizontal_pair_draft().replacen(
                "      command: null\n",
                &format!("      command:\n{correct_first}"),
                1,
            ),
            "inferred capture lacks a command array",
        ),
        (
            inferred_horizontal_pair_draft(&correct_first, &correct_second)
                .replace("weight: 40.1", "weight: 40.2"),
            "inference changed capture hierarchy, order, sizing, matchers, IDs, or focus",
        ),
        (
            inferred_horizontal_pair_draft(&correct_first, &correct_second)
                .replace("focus: window2", "focus: window1"),
            "inference changed capture hierarchy",
        ),
        (
            // A misindented argument is not part of the command sequence.
            inferred_horizontal_pair_draft(&correct_first, &correct_second).replace(
                "        - /usr/bin/sleep\n        - '300'\n      match",
                "      - '300'\n      match",
            ),
            "inference changed capture hierarchy",
        ),
    ];

    for (inferred, expected) in cases {
        let error = verify_inferred_capture(&inferred, &horizontal_pair_draft(), &scenario.layout)
            .unwrap_err();
        assert_contains(&error, expected);
        assert!(!error.contains("hunter2") && !error.contains("/usr/bin"));
    }
}

#[test]
fn inferred_capture_needs_one_command_per_window() {
    let scenario = standard_scenario("single-window");
    let window1 = scenario_app_id("single-window", "window1");
    let default_draft = format!(
        "{DRAFT_HEADER}version: 1\nfocus: window1\nlayout:\n  id: window1\n  command: null\n  match:\n    app_id: {window1}\n"
    );
    let inferred = default_draft.replace(
        "  command: null\n",
        &format!("  command:\n    - /usr/bin/foot\n    - --app-id={window1}\n"),
    );
    verify_inferred_capture(&inferred, &default_draft, &scenario.layout).unwrap();

    let pair = standard_scenario("horizontal-pair");
    let error = verify_inferred_capture(&inferred, &default_draft, &pair.layout).unwrap_err();

    assert_eq!(error, "inferred command count differs");
}

#[test]
fn desktop_capture_requires_the_exact_fixture_command_and_unchanged_structure() {
    let scenario = crate::fixtures::extended_scenario("desktop-launcher-roundtrip");
    let fixture = crate::scenarios::DesktopLauncher {
        path: "/private/test launcher.desktop".into(),
        contents: "independent fixture".into(),
        foot_command: scenario.layout.leaves()[0].command.clone(),
        launch_method: crate::scenarios::DesktopLaunchMethod::GtkLaunch,
    };
    let default = expected_draft(&scenario, &BTreeMap::new()).unwrap();
    let inferred = default.replace(
        "command: null",
        "command:\n    - gio\n    - launch\n    - /private/test launcher.desktop",
    );
    verify_desktop_launcher_capture(&inferred, &default, &fixture).unwrap();
    verify_desktop_replay_capture(&inferred, &default, &fixture).unwrap();
    verify_desktop_replay_capture(&default, &default, &fixture).unwrap();
    // GLib 2.84's replay miss cannot satisfy initial inference verification.

    assert!(verify_desktop_launcher_capture(&default, &default, &fixture).is_err());
    // Preserve the existing Foot-only assertion: gio argv cannot satisfy it.
    assert!(verify_inferred_capture(&inferred, &default, &scenario.layout).is_err());
    for bad in [
        inferred.replace("- gio", "- gtk-launch"),
        inferred.replace("- launch", "- open"),
        inferred.replace("test launcher.desktop", "different.desktop"),
        inferred.replace("    - launch\n", ""),
        inferred.replace("  match:", "    - secret\n  match:"),
        inferred.replace("focus: window1", "focus: window2"),
        inferred.replace("id: window1", "id: other"),
        default.clone(),
    ] {
        let error = verify_desktop_launcher_capture(&bad, &default, &fixture).unwrap_err();
        assert!(!error.contains("secret") && !error.contains("/private"));
        if bad != default {
            assert!(verify_desktop_replay_capture(&bad, &default, &fixture).is_err());
        }
    }
    let foot_arguments = fixture
        .foot_command
        .iter()
        .map(|argument| format!("    - {argument}"))
        .collect::<Vec<_>>()
        .join("\n");
    let foot_inferred = default.replace("command: null", &format!("command:\n{foot_arguments}"));
    assert!(verify_desktop_replay_capture(&foot_inferred, &default, &fixture).is_err());
    assert!(
        verify_desktop_replay_capture(
            &default.replace("focus: window1", "focus: window2"),
            &default,
            &fixture
        )
        .is_err()
    );
}

#[test]
fn executable_replay_edits_only_captured_placeholders_with_literal_commands() {
    let mut scenario = standard_scenario("horizontal-pair");
    scenario.layout.first_leaf_mut().command.extend([
        "two words".into(),
        "quotes ' \" and \\ newline\n $HOME ; $(literal) *".into(),
        String::new(),
    ]);
    let draft = horizontal_pair_draft().replace("id: window1", "id: 'window1'");
    let (yaml, replay) =
        executable_capture(&draft, &scenario, &horizontal_pair_measurements()).unwrap();
    let mut normalized = yaml.clone();
    for leaf in scenario.layout.leaves() {
        let serialized = json_sequence(&leaf.command);
        normalized = normalized.replacen(&format!("command: {serialized}"), "command: null", 1);
    }
    // Comments, quoted IDs and measured decimal weights are from the capture itself.

    assert_eq!(normalized, draft);
    let commands: Vec<Vec<String>> = yaml
        .lines()
        .filter_map(|line| {
            line.trim_start()
                .strip_prefix("command: ")
                .map(|scalar| serde_json::from_str(scalar).unwrap())
        })
        .collect();
    assert_eq!(
        commands,
        scenario
            .layout
            .leaves()
            .iter()
            .map(|leaf| leaf.command.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(replay.layout.leaves()[0].weight, 5990);
    assert_eq!(replay.layout.leaves()[1].weight, 4010);
    assert_eq!(scenario.layout.leaves()[0].weight, 3);
    assert_eq!(replay.expected_focus, "window2");
    assert_eq!(replay.configured_focus, Some("window2"));
    assert_eq!(
        replay.layout.leaves()[0].selector,
        scenario.layout.leaves()[0].selector
    );
    assert!(!yaml.contains("command: [\"true\"]"));
}

#[test]
fn replay_rejects_missing_extra_relocated_or_misassociated_placeholders() {
    let scenario = standard_scenario("horizontal-pair");
    for draft in [
        horizontal_pair_draft().replacen("      command: null\n", "", 1),
        horizontal_pair_draft() + "command: null\n",
        horizontal_pair_draft().replacen("      command: null", "    command: null", 1),
        horizontal_pair_draft().replace(
            "      weight: 59.9\n      command: null",
            "      command: null\n      weight: 59.9",
        ),
        horizontal_pair_draft().replacen("id: window1", "id: window2", 1),
        horizontal_pair_draft().replacen(
            &scenario_app_id("horizontal-pair", "window1"),
            &scenario_app_id("horizontal-pair", "window2"),
            1,
        ),
        horizontal_pair_draft().replacen("      command: null", "      command: [\"true\"]", 1),
    ] {
        assert!(executable_capture(&draft, &scenario, &horizontal_pair_measurements()).is_err());
    }
    let mut no_command = scenario;
    no_command.layout.first_leaf_mut().command.clear();

    assert!(
        executable_capture(
            &horizontal_pair_draft(),
            &no_command,
            &horizontal_pair_measurements()
        )
        .is_err()
    );
}

#[test]
fn nested_replay_retains_focus_shape_and_every_captured_weight() {
    let scenario = standard_scenario("capture-reload-roundtrip");
    let measurements = LayoutMeasurements {
        shares: BTreeMap::from([
            (vec![0], 40.01),
            (vec![1], 30.02),
            (vec![1, 0], 59.98),
            (vec![1, 1], 40.02),
            (vec![2], 29.97),
        ]),
        sizing_notes: Vec::new(),
    };
    let draft = expected_draft(&scenario, &measurements.shares).unwrap();
    let (yaml, replay) = executable_capture(&draft, &scenario, &measurements).unwrap();
    let ConfiguredNode::Group {
        layout, children, ..
    } = &replay.layout
    else {
        panic!("root split")
    };

    assert_eq!(*layout, crate::scenarios::GroupLayout::Horizontal);
    assert_eq!(
        children
            .iter()
            .map(ConfiguredNode::weight)
            .collect::<Vec<_>>(),
        [4001, 3002, 2997]
    );
    let ConfiguredNode::Group {
        layout, children, ..
    } = &children[1]
    else {
        panic!("nested split")
    };
    assert_eq!(*layout, crate::scenarios::GroupLayout::Vertical);
    assert_eq!(
        children
            .iter()
            .map(ConfiguredNode::weight)
            .collect::<Vec<_>>(),
        [5998, 4002]
    );
    assert_eq!(replay.expected_focus, "window3");
    assert_eq!(replay.configured_focus, Some("window3"));
    for (original, replayed) in scenario.layout.leaves().iter().zip(replay.layout.leaves()) {
        assert_eq!(original.layout_id, replayed.layout_id);
        assert_eq!(original.selector, replayed.selector);
        assert_eq!(original.command, replayed.command);
    }
    assert_eq!(
        yaml.lines()
            .filter(|line| line.trim_start().starts_with("weight:"))
            .collect::<Vec<_>>(),
        draft
            .lines()
            .filter(|line| line.trim_start().starts_with("weight:"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn duplicate_id_inference_requires_each_launches_distinct_literal_title() {
    let scenario = crate::fixtures::extended_scenario("duplicate-foot-ids");
    let shares = BTreeMap::from([(vec![0], 59.9), (vec![1], 40.1)]);
    let default = expected_draft(&scenario, &shares).unwrap();
    let leaves = scenario.layout.leaves();
    let shared_id = leaves[0].selector.value();
    let titles: Vec<_> = leaves
        .iter()
        .map(|leaf| option_values(&leaf.command, "--title")[0])
        .collect();

    assert_ne!(titles[0], titles[1]);
    let first = foot_command(&[
        &format!("--app-id={shared_id}"),
        &format!("--title={}", titles[0]),
    ]);
    let second = foot_command(&["--app-id", shared_id, "--title", titles[1]]);
    let inferred = default
        .replacen(
            "      command: null\n",
            &format!("      command:\n{first}"),
            1,
        )
        .replacen(
            "      command: null\n",
            &format!("      command:\n{second}"),
            1,
        );
    verify_inferred_capture(&inferred, &default, &scenario.layout).unwrap();
    for invalid in [
        inferred
            .replace(titles[0], "temporary-marker")
            .replace(titles[1], titles[0])
            .replace("temporary-marker", titles[1]),
        inferred.replace(titles[1], titles[0]),
        inferred.replace(&format!("        - --title={}\n", titles[0]), ""),
        inferred.replace("        - --title\n", ""),
        inferred.replace(&format!("--app-id={shared_id}"), "--app-id=wrong"),
        inferred.replace(
            &format!("--title={}", titles[0]),
            &format!("--title={} --title={}", titles[0], titles[1]),
        ),
        inferred.replace("        - /usr/bin/foot", "        - [not, a, string]"),
        inferred.replace("        - /usr/bin/foot", "        - {bad: mapping}"),
        inferred.replace("        - /usr/bin/foot", "        - null"),
        inferred.replace("        - /usr/bin/foot", "        - \"unterminated"),
    ] {
        let error = verify_inferred_capture(&invalid, &default, &scenario.layout).unwrap_err();
        assert!(!error.contains(shared_id) && !titles.iter().any(|title| error.contains(title)));
        assert!(!error.contains("/usr/bin"));
    }
    for (original, replacement) in [
        ("group: splith", "group: splitv"),
        ("weight: 59.9", "weight: 59.91"),
        ("id: window1", "id: other"),
        ("focus: window2", "focus: window1"),
        ("app_id:", "class:"),
    ] {
        assert!(
            verify_inferred_capture(
                &inferred.replacen(original, replacement, 1),
                &default,
                &scenario.layout
            )
            .is_err()
        );
    }
}

#[test]
fn unresolved_capture_requires_null_commands_success_and_independent_stderr() {
    use std::os::unix::process::ExitStatusExt;
    let scenario = crate::fixtures::extended_scenario("unresolved-launcher");
    let default = expected_draft(&scenario, &BTreeMap::new()).unwrap();
    let valid = || {
        crate::process::CompletedProcess {
        status: std::process::ExitStatus::from_raw(0),
        stdout: default.clone(),
        stderr: "sway-stage: capture completed. Commands inferred: 0; unresolved: 1. Manual editing required.\n".into(),
    }
    };
    verify_unresolved_capture(&valid(), &default).unwrap();
    for (stdout, stderr, status) in [
        (
            default.replace("command: null", "command: [secret]"),
            valid().stderr,
            0,
        ),
        (
            default.replace("focus: window1", "focus: secret"),
            valid().stderr,
            0,
        ),
        (default.replace("app_id:", "secret:"), valid().stderr, 0),
        (format!("{default}secret diagnostics\n"), valid().stderr, 0),
        (default.clone(), "secret stderr".into(), 0),
        (
            default.clone(),
            valid().stderr.replace("unresolved: 1", "unresolved: 0"),
            0,
        ),
        (
            default.clone(),
            valid().stderr.replace(" Manual editing required.", ""),
            0,
        ),
        (default.clone(), valid().stderr, 1),
    ] {
        let output = crate::process::CompletedProcess {
            status: std::process::ExitStatus::from_raw(status << 8),
            stdout,
            stderr,
        };
        let error = verify_unresolved_capture(&output, &default).unwrap_err();
        assert!(!error.contains("secret"));
    }
}

#[test]
fn ordinary_capture_mismatches_never_echo_unexpected_arguments_or_paths() {
    let scenario = standard_scenario("single-window");
    let measurements = LayoutMeasurements {
        shares: BTreeMap::new(),
        sizing_notes: Vec::new(),
    };
    let default = expected_draft(&scenario, &measurements.shares).unwrap();
    for draft in [
        default.replace("command: null", "command: [/private/secret, token]"),
        format!("{default}extra: /private/secret\n"),
        default.replace("id: window1", "id: /private/secret"),
    ] {
        let error = verify_capture(&draft, &scenario, &measurements).unwrap_err();
        assert!(!error.contains("secret") && !error.contains("token"));
    }
}

#[test]
fn package_capture_compares_exact_host_commands_and_all_other_fields() {
    let selector = Selector::AppId("observed: app \\\"name".into());
    let default = format!(
        "version: 1\nfocus: window1\nlayout:\n  id: window1\n  command: null\n  match:\n    app_id: {}\n",
        serde_json::to_string(selector.value()).unwrap()
    );
    verify_package_default_capture(&default, &selector).unwrap();
    let commands = [
        vec!["/apps/My App.AppImage".into()],
        vec![
            "flatpak".into(),
            "run".into(),
            "--branch=beta".into(),
            "--arch=aarch64".into(),
            "org.example.App".into(),
        ],
    ];
    for expected in commands {
        let block = format!(
            "command:\n{}",
            expected
                .iter()
                .map(|arg| format!("    - {}\n", serde_json::to_string(arg).unwrap()))
                .collect::<String>()
        );
        let inferred = default.replace("command: null\n", &block);
        verify_package_capture(&inferred, &default, &expected).unwrap();
        for bad in [
            inferred.replace("window1", "window2"),
            inferred.replace("version: 1", "version: 2"),
            inferred.replace("app_id:", "class:"),
            default.clone(),
            inferred.replace(
                &serde_json::to_string(&expected[0]).unwrap(),
                "\"/tmp/.mount_app/bin/app\"",
            ),
        ] {
            assert!(verify_package_capture(&bad, &default, &expected).is_err());
        }
        let mut extra = expected.clone();
        extra.push("unexpected".into());
        assert!(verify_package_capture(&inferred, &default, &extra).is_err());
    }

    assert!(
        verify_package_default_capture(
            &default.replace("command: null", "command: [unexpected]"),
            &selector
        )
        .is_err()
    );
    assert!(
        verify_package_default_capture(&default.replace("window1", "window2"), &selector).is_err()
    );
}

#[test]
fn mixed_capture_replay_aligns_only_split_child_weights_and_retains_actual_yaml() {
    let scenario = standard_scenario("mixed-capture-reload");
    let measurements = LayoutMeasurements {
        shares: BTreeMap::from([
            (vec![0], 59.75),
            (vec![1], 40.25),
            (vec![0, 1, 0], 60.01),
            (vec![0, 1, 1], 39.99),
        ]),
        sizing_notes: Vec::new(),
    };
    let draft = format!(
        "# keep this captured comment\n\n{}",
        expected_draft(&scenario, &measurements.shares).unwrap()
    )
    .replace("group: tabbed", "group: 'tabbed'");
    let (yaml, replay) = executable_capture(&draft, &scenario, &measurements).unwrap();
    let mut normalized = yaml.clone();
    for leaf in scenario.layout.leaves() {
        normalized = normalized.replacen(
            &format!("command: {}", json_sequence(&leaf.command)),
            "command: null",
            1,
        );
    }

    assert_eq!(normalized, draft);
    let ConfiguredNode::Group { children, .. } = &replay.layout else {
        panic!("root group")
    };
    assert_eq!(children[0].weight(), 5975);
    assert_eq!(children[1].weight(), 4025);
    let ConfiguredNode::Group { children: tabs, .. } = &children[0] else {
        panic!("tabs")
    };
    assert_eq!(tabs[0].weight(), 1);
    assert_eq!(tabs[1].weight(), 1);
    let ConfiguredNode::Group { children: row, .. } = &tabs[1] else {
        panic!("row")
    };
    assert_eq!(row[0].weight(), 6001);
    assert_eq!(row[1].weight(), 3999);
    assert_eq!(replay.expected_focus, "window3");
    assert_eq!(yaml.matches("weight:").count(), 4);
    for (original, replacement) in [
        ("group: 'tabbed'", "group: stacked"),
        ("group: stacked", "group: tabbed"),
        ("weight: 59.75\n", ""),
        ("weight: 60.01", "weight: 50"),
        ("group: stacked\n", "group: stacked\n          weight: 1\n"),
        ("id: window1", "id: window2"),
        ("focus: window3", "focus: window1"),
        ("app_id:", "class:"),
    ] {
        assert!(
            executable_capture(
                &draft.replacen(original, replacement, 1),
                &scenario,
                &measurements
            )
            .is_err(),
            "{original}"
        );
    }
    // A displaced split-child weight cannot compensate for a weight inside a header.
    let relocated = draft.replacen("      weight: 59.75\n", "", 1).replacen(
        "group: stacked\n",
        "group: stacked\n          weight: 59.75\n",
        1,
    );
    assert!(executable_capture(&relocated, &scenario, &measurements).is_err());
}

#[test]
fn header_only_capture_replay_needs_no_weights() {
    for name in ["root-tabbed", "root-stacked", "singleton-groups"] {
        let scenario = standard_scenario(name);
        let measurements = LayoutMeasurements {
            shares: BTreeMap::new(),
            sizing_notes: Vec::new(),
        };
        let draft = expected_draft(&scenario, &measurements.shares).unwrap();
        let (yaml, replay) = executable_capture(&draft, &scenario, &measurements).unwrap();
        assert!(!yaml.contains("weight:"));
        assert_eq!(replay.layout.leaves().len(), scenario.layout.leaves().len());
        let weighted = draft.replacen(
            "      command: null",
            "      weight: 1\n      command: null",
            1,
        );
        assert_ne!(weighted, draft);
        assert!(verify_capture(&weighted, &scenario, &measurements).is_err());
    }
}

#[test]
fn measured_mixed_replay_is_verified_against_fresh_tree_geometry() {
    use crate::fixtures::{
        TARGET_WORKSPACE_ID, accepted_windows, find, mixed_native_desktop, node_mut, parse_tree,
    };
    let scenario = standard_scenario("mixed-capture-reload");
    let owned = accepted_windows(&[
        (20, "window1"),
        (21, "window2"),
        (22, "window3"),
        (23, "window4"),
    ]);
    let mut tree = mixed_native_desktop(&scenario);
    node_mut(&mut tree, 31)["rect"]["height"] = serde_json::json!(640);
    node_mut(&mut tree, 23)["rect"]["y"] = serde_json::json!(640);
    node_mut(&mut tree, 23)["rect"]["height"] = serde_json::json!(360);
    node_mut(&mut tree, 21)["rect"]["width"] = serde_json::json!(640);
    node_mut(&mut tree, 22)["rect"]["x"] = serde_json::json!(640);
    node_mut(&mut tree, 22)["rect"]["width"] = serde_json::json!(360);
    let tree = parse_tree(tree);
    let workspace = find(&tree, TARGET_WORKSPACE_ID);
    let measured = crate::layout::verify_layout(workspace, &scenario, &owned).unwrap();

    assert_eq!(measured.sizing_notes.len(), 2);
    let draft = expected_draft(&scenario, &measured.shares).unwrap();
    let (_, replay) = executable_capture(&draft, &scenario, &measured).unwrap();
    let replayed = crate::layout::verify_layout(workspace, &replay, &owned).unwrap();
    assert!(replayed.sizing_notes.is_empty());
    assert_eq!(replayed.shares, measured.shares);
}

#[test]
fn outside_focus_replay_preserves_inactive_selection_and_rejects_changed_active_yaml() {
    use crate::fixtures::{
        TARGET_WORKSPACE_ID, accepted_windows, find, mixed_native_desktop, node_mut, parse_tree,
        set_focus,
    };
    let scenario = standard_scenario("outside-focus-capture-reload");
    assert!(scenario.captures_and_reloads());
    let owned = accepted_windows(&[
        (20, "window1"),
        (21, "window2"),
        (22, "window3"),
        (23, "window4"),
    ]);
    let mut tree = mixed_native_desktop(&scenario);
    set_focus(&mut tree, 23);
    node_mut(&mut tree, TARGET_WORKSPACE_ID)["focus"] = serde_json::json!([23, 31]);
    let tree = parse_tree(tree);
    let workspace = find(&tree, TARGET_WORKSPACE_ID);
    let measurements = crate::layout::verify_layout(workspace, &scenario, &owned).unwrap();
    let draft = expected_draft(&scenario, &measurements.shares).unwrap();
    assert!(draft.contains("focus: window4"));
    assert_eq!(draft.matches("active: 2").count(), 1);
    let (yaml, replay) = executable_capture(&draft, &scenario, &measurements).unwrap();
    assert!(yaml.contains("active: 2"));
    crate::layout::verify_layout(workspace, &replay, &owned).unwrap();

    for replacement in ["active: 1", "active: 3", "active: null", "active: '2'"] {
        let changed = draft.replace("active: 2", replacement);
        assert!(executable_capture(&changed, &scenario, &measurements).is_err());
    }
    let omitted = draft.replace("      active: 2\n", "");
    assert_ne!(omitted, draft);
    assert!(executable_capture(&omitted, &scenario, &measurements).is_err());
}
