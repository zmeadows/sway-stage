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

/// The additional header of an `--infer-commands` draft.
const INFERENCE_HEADER: &str = "\
# Review recovered commands before executing or sharing: process arguments are
# best-effort, may contain secrets, and may not reproduce the original application.
";

fn app_id(scenario: &str, layout_id: &str) -> String {
    format!("sway-stage-it-{RUN_ID}-{scenario}-{layout_id}")
}

/// horizontal-pair measured at 1150:770 pixels.
fn pair_measurements() -> LayoutMeasurements {
    LayoutMeasurements {
        shares: BTreeMap::from([
            (vec![0], 100.0 * 1150.0 / 1920.0),
            (vec![1], 100.0 * 770.0 / 1920.0),
        ]),
        sizing_notes: Vec::new(),
    }
}

/// A default capture of horizontal-pair as sway-stage prints it.
fn pair_draft() -> String {
    format!(
        "{DRAFT_HEADER}\
version: 1
focus: window2
layout:
  split: horizontal
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
        app_id("horizontal-pair", "window1"),
        app_id("horizontal-pair", "window2"),
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
    let filled = verify_capture(&pair_draft(), &scenario, &pair_measurements()).unwrap();

    let expected = pair_draft()
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
    let id = |layout_id: &str| app_id("mixed-nesting", layout_id);
    // Quoted identifiers are equivalent YAML and must still compare equal.
    let draft = format!(
        "{DRAFT_HEADER}\
version: 1
focus: 'window3'
layout:
  split: horizontal
  children:
    - id: window1
      weight: 40.0
      command: null
      match:
        app_id: {}
    - split: vertical
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
fn capture_differences_fail_with_the_offending_line() {
    let scenario = standard_scenario("horizontal-pair");
    let window1 = app_id("horizontal-pair", "window1");
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
        ("split: horizontal", "split: vertical", "capture value"),
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
        let draft = pair_draft().replacen(original, replacement, 1);
        assert_ne!(draft, pair_draft(), "{original:?} is not in the draft");
        let error = verify_capture(&draft, &scenario, &pair_measurements()).unwrap_err();
        assert_contains(&error, expected);
    }

    let extra_field =
        pair_draft().replace("version: 1\n", "version: 1\nwindow_timeout_ms: 20000\n");
    let error = verify_capture(&extra_field, &scenario, &pair_measurements()).unwrap_err();
    assert_contains(&error, "capture draft has unexpected fields/structure");

    let unmeasured = LayoutMeasurements {
        shares: BTreeMap::new(),
        sizing_notes: Vec::new(),
    };
    let error = verify_capture(&pair_draft(), &scenario, &unmeasured).unwrap_err();
    assert_eq!(error, "no measured share for layout.children[0]");
}

/// An `--infer-commands` capture of horizontal-pair whose recovered commands are the
/// given lines, each already indented as a block sequence item.
fn inferred_pair_draft(first_command: &str, second_command: &str) -> String {
    pair_draft()
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
    let window1 = app_id("horizontal-pair", "window1");
    let window2 = app_id("horizontal-pair", "window2");
    let inferred = inferred_pair_draft(
        &foot_command(&[&format!("--app-id={window1}")]),
        &foot_command(&["--app-id", &format!("\"{window2}\"")]),
    );
    verify_inferred_capture(&inferred, &pair_draft(), &scenario.layout).unwrap();
}

#[test]
fn inferred_capture_mismatches_fail_without_revealing_arguments() {
    let scenario = standard_scenario("horizontal-pair");
    let window1 = app_id("horizontal-pair", "window1");
    let window2 = app_id("horizontal-pair", "window2");
    let secret = "--token=hunter2";
    let correct_first = foot_command(&[&format!("--app-id={window1}"), secret]);
    let correct_second = foot_command(&[&format!("--app-id={window2}")]);

    let swapped = inferred_pair_draft(&correct_second, &correct_first);
    let missing_value = inferred_pair_draft(&correct_first, &foot_command(&["--app-id"]));
    let other_option = inferred_pair_draft(
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
            inferred_pair_draft(&correct_first, "        - ''\n"),
            "inferred capture has an empty executable",
        ),
        (
            inferred_pair_draft(&correct_first, ""),
            "inferred capture has an empty executable",
        ),
        (
            // Inference missed the second window, so its placeholder remains.
            pair_draft().replacen(
                "      command: null\n",
                &format!("      command:\n{correct_first}"),
                1,
            ),
            "inferred capture lacks a command array",
        ),
        (
            inferred_pair_draft(&correct_first, &correct_second)
                .replace("weight: 40.1", "weight: 40.2"),
            "inference changed capture hierarchy, order, sizing, matchers, IDs, or focus",
        ),
        (
            inferred_pair_draft(&correct_first, &correct_second)
                .replace("focus: window2", "focus: window1"),
            "inference changed capture hierarchy",
        ),
        (
            // A misindented argument is not part of the command sequence.
            inferred_pair_draft(&correct_first, &correct_second).replace(
                "        - /usr/bin/sleep\n        - '300'\n      match",
                "      - '300'\n      match",
            ),
            "inference changed capture hierarchy",
        ),
    ];

    for (inferred, expected) in cases {
        let error =
            verify_inferred_capture(&inferred, &pair_draft(), &scenario.layout).unwrap_err();
        assert_contains(&error, expected);
        assert!(!error.contains("hunter2") && !error.contains("/usr/bin"));
    }
}

#[test]
fn inferred_capture_needs_one_command_per_window() {
    let scenario = standard_scenario("single-window");
    let window1 = app_id("single-window", "window1");
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
