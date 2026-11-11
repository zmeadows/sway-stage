//! Verification of `sway-stage capture` drafts against the verified arrangement.
//!
//! This is a narrow structural comparison with capture's one known serializer format:
//! only command argument scalars use YAML decoding. A format change fails visibly, and
//! `check` on the filled draft supplies authoritative syntax and semantic validation.
//! Recovered command arguments from
//! `--infer-commands` never appear in messages. Only a verified private fixture
//! command is saved for the desktop-launcher replay.

use crate::layout::LayoutMeasurements;
use crate::scenarios::{ConfiguredNode, Scenario, Selector, expected_draft, json_sequence};

/// A captured weight is the measured share rounded to hundredths.
const WEIGHT_TOLERANCE: f64 = 0.02;

/// Fields whose generated values contain no YAML punctuation, so a serializer that quotes
/// them changes nothing a quote-stripping comparison would miss.
const IDENTIFIER_KEYS: [&str; 5] = ["id", "focus", "app_id", "class", "split"];

/// The lines of a draft that carry content: blank and comment lines are dropped and
/// trailing whitespace is removed.
pub fn capture_lines(draft: &str) -> Vec<&str> {
    draft
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty() && !line.trim_start().starts_with('#'))
        .collect()
}

/// Compares a default capture draft with the expected one, line by line: indentation and
/// sequence arrangement, keys, identifiers, focus, `command: null` placeholders, unchanged
/// selectors, and weights within 0.02 of the measured shares. Returns the draft with each
/// placeholder filled with a harmless command, ready for `sway-stage check`. Never echo
/// captured lines: even an ordinary capture could unexpectedly contain recovered arguments.
pub fn verify_capture(
    draft: &str,
    scenario: &Scenario,
    measurements: &LayoutMeasurements,
) -> Result<String, String> {
    let actual_lines = capture_lines(draft);
    let expected = expected_draft(scenario, &measurements.shares)?;
    let expected_lines: Vec<&str> = expected.lines().collect();
    if actual_lines.len() != expected_lines.len() {
        return Err("capture draft has unexpected fields/structure".into());
    }

    let mut placeholder_count = 0;
    let mut filled_lines = Vec::new();
    for (actual_line, expected_line) in actual_lines.into_iter().zip(expected_lines) {
        let actual_field = actual_line.trim_start_matches([' ', '-']);
        let expected_field = expected_line.trim_start_matches([' ', '-']);
        let actual_arrangement = &actual_line[..actual_line.len() - actual_field.len()];
        let expected_arrangement = &expected_line[..expected_line.len() - expected_field.len()];
        if actual_arrangement != expected_arrangement {
            return Err("capture indentation/arrangement".into());
        }

        let Some((key, actual_scalar)) = actual_field.split_once(':') else {
            return Err("capture field".into());
        };
        let (expected_key, expected_scalar) = expected_field
            .split_once(':')
            .unwrap_or((expected_field, ""));
        if key != expected_key {
            return Err("capture field".into());
        }
        let actual_scalar = actual_scalar.trim();
        let expected_scalar = expected_scalar.trim();

        if key == "weight" {
            let weight: f64 = actual_scalar
                .parse()
                .map_err(|_| "capture weight is not numeric".to_owned())?;
            let expected_weight: f64 = expected_scalar
                .parse()
                .map_err(|_| "expected weight is not numeric".to_owned())?;
            if !weight.is_finite() || (weight - expected_weight).abs() > WEIGHT_TOLERANCE {
                return Err("capture sizing".into());
            }
        } else {
            let actual_value = if IDENTIFIER_KEYS.contains(&key) {
                strip_quotes(actual_scalar)
            } else {
                actual_scalar
            };
            if actual_value != expected_scalar {
                return Err("capture value".into());
            }
        }

        if key == "command" {
            placeholder_count += 1;
            filled_lines.push(actual_line.replace("command: null", r#"command: ["true"]"#));
        } else {
            filled_lines.push(actual_line.to_owned());
        }
    }

    if placeholder_count != scenario.layout.leaves().len() {
        return Err("capture command placeholder count differs".into());
    }
    Ok(filled_lines.join("\n") + "\n")
}

/// Builds replay YAML only by editing the fully verified capture's null commands.
/// The independent model keeps the proven shape, selectors and focus, but takes every
/// non-root weight from the captured text. Integer hundredths preserve capture's precision
/// without changing the harness's integer-weight model or regenerating the replay YAML.
pub fn executable_capture(
    draft: &str,
    scenario: &Scenario,
    measurements: &LayoutMeasurements,
) -> Result<(String, Scenario), String> {
    verify_capture(draft, scenario, measurements)?;

    let leaves = scenario.layout.leaves();
    let mut commands = leaves.iter();
    let mut replay_lines = Vec::new();
    let mut captured_weights = Vec::new();
    for line in draft.lines() {
        let field = line.trim_start();
        if field == "command: null" {
            let leaf = commands.next().ok_or("extra replay placeholder")?;
            if leaf.command.first().is_none_or(String::is_empty) {
                return Err(format!("missing replay command for {}", leaf.layout_id));
            }
            let indent = &line[..line.len() - field.len()];
            replay_lines.push(format!("{indent}command: {}", json_sequence(&leaf.command)));
        } else {
            if let Some(scalar) = field.strip_prefix("weight: ") {
                let weight: f64 = scalar.parse().map_err(|_| "invalid replay weight")?;
                let hundredths = (weight * 100.0).round();
                // Allow binary floating-point noise, but never discard decimal precision.
                if weight <= 0.0 || weight > 100.0 || (weight * 100.0 - hundredths).abs() > 1e-6 {
                    return Err("replay weight is not a positive percentage in hundredths".into());
                }
                captured_weights.push(hundredths as u32);
            }
            replay_lines.push(line.to_owned());
        }
    }
    if commands.next().is_some() {
        return Err("missing replay placeholder".into());
    }

    let mut replay = scenario.clone();
    let mut weights = captured_weights.into_iter();
    apply_captured_weights(&mut replay.layout, true, &mut weights)?;
    if weights.next().is_some() {
        return Err("extra captured weight".into());
    }
    replay.configured_focus = Some(scenario.expected_focus);
    Ok((replay_lines.join("\n") + "\n", replay))
}

fn apply_captured_weights(
    configured: &mut ConfiguredNode,
    is_root: bool,
    weights: &mut std::vec::IntoIter<u32>,
) -> Result<(), String> {
    let weight = if is_root {
        1
    } else {
        weights.next().ok_or("missing captured weight")?
    };
    match configured {
        ConfiguredNode::Window(window) => window.weight = weight,
        ConfiguredNode::Split {
            weight: group_weight,
            children,
            ..
        } => {
            *group_weight = weight;
            for child in children {
                apply_captured_weights(child, false, weights)?;
            }
        }
    }
    Ok(())
}

/// Removes one leading and one trailing quote of either kind, independently.
fn strip_quotes(scalar: &str) -> &str {
    let scalar = scalar.strip_prefix('"').unwrap_or(scalar);
    let scalar = scalar.strip_suffix('"').unwrap_or(scalar);
    let scalar = scalar.strip_prefix('\'').unwrap_or(scalar);
    scalar.strip_suffix('\'').unwrap_or(scalar)
}

/// Compares an `--infer-commands` draft with the already verified default draft. Only the
/// serializer's block command sequences are recognized; with each replaced by
/// `command: null`, the drafts must agree exactly, so inference changed no hierarchy, order,
/// sizing, selector, ID, or focus. Each recovered Foot command must carry its leaf's app ID,
/// as `--app-id=ID` or as `--app-id ID`. Shared selectors also require the corresponding
/// literal title option from the independently configured launch, never Sway's title.
pub fn verify_inferred_capture(
    inferred_draft: &str,
    default_draft: &str,
    layout: &ConfiguredNode,
) -> Result<(), String> {
    let commands = inferred_commands(inferred_draft, default_draft)?;

    let leaves = layout.leaves();
    if commands.len() != leaves.len() {
        return Err("inferred command count differs".into());
    }
    for (arguments, leaf) in commands.iter().zip(&leaves) {
        let Selector::AppId(app_id) = &leaf.selector else {
            return Err(format!(
                "inferred command checks need an app ID selector: {}",
                leaf.layout_id
            ));
        };
        if option_values(arguments, "--app-id") != [app_id.as_str()] {
            return Err(format!(
                "inferred command does not identify the correct Foot window: {}",
                leaf.layout_id
            ));
        }
        let shared_selector = leaves
            .iter()
            .filter(|other| other.selector == leaf.selector)
            .count()
            > 1;
        if shared_selector {
            let expected_titles = option_values(&leaf.command, "--title");
            let titles = option_values(arguments, "--title");
            if expected_titles.len() != 1 || titles != expected_titles {
                return Err(format!(
                    "inferred command has incorrect instance title: {}",
                    leaf.layout_id
                ));
            }
        }
    }
    Ok(())
}

/// A dynamic package fixture has exactly one root window. Decode its arbitrary observed
/// selector as YAML rather than assuming the simple identifiers of our Foot fixtures.
pub fn verify_package_default_capture(draft: &str, selector: &Selector) -> Result<(), String> {
    let actual: serde_json::Value = serde_saphyr::from_str(draft)
        .map_err(|_| "package capture is not a valid YAML document")?;
    let expected = serde_json::json!({
        "version": 1,
        "focus": "window1",
        "layout": {
            "id": "window1", "command": null,
            "match": { (selector.kind()): selector.value() }
        }
    });
    if actual != expected {
        return Err(
            "ordinary package capture differs from the independently observed window".into(),
        );
    }
    Ok(())
}

/// Package commands are derived from independently observed package evidence. Exact argv
/// equality excludes mount/sandbox executables, extra arguments, and unresolved commands.
pub fn verify_package_capture(
    inferred_draft: &str,
    default_draft: &str,
    expected_command: &[String],
) -> Result<(), String> {
    let commands = inferred_commands(inferred_draft, default_draft)?;
    if commands != [expected_command] {
        return Err("inferred command differs from independently verified package launcher".into());
    }
    Ok(())
}

/// Only this single-window fixture accepts a launcher intermediary. Its expected path
/// comes from fixture creation, independently of both captured YAML and process metadata.
pub fn verify_desktop_launcher_capture(
    inferred_draft: &str,
    default_draft: &str,
    fixture: &crate::scenarios::DesktopLauncher,
) -> Result<(), String> {
    let commands = inferred_commands(inferred_draft, default_draft)?;
    let path = fixture.path.to_str().ok_or("non-UTF-8 fixture path")?;
    if commands != [vec!["gio".to_owned(), "launch".to_owned(), path.to_owned()]] {
        return Err("inferred command does not identify the private desktop fixture".into());
    }
    Ok(())
}

/// The already verified default draft proves every structural field, including a single
/// null command. Successful stderr is checked separately and never included in failures.
pub fn verify_unresolved_capture(
    output: &crate::process::CompletedProcess,
    default_draft: &str,
) -> Result<(), String> {
    if !output.status.success() {
        return Err("unresolved capture did not exit successfully".into());
    }
    if capture_lines(&output.stdout) != capture_lines(default_draft) {
        return Err("unresolved capture changed the draft or substituted a command".into());
    }
    let expected_stderr = "sway-stage: capture completed. Commands inferred: 0; unresolved: 1. Manual editing required.\n";
    if output.stderr != expected_stderr {
        return Err("unresolved capture reporting differs".into());
    }
    Ok(())
}

/// gio on GLib 2.84 supplies only PID metadata, so production conservatively leaves the
/// replay's command unresolved. Newer GLib preserves provenance and infers gio again.
/// Neither outcome relaxes the initial fixture's exact launcher inference assertion.
pub fn verify_desktop_replay_capture(
    inferred_draft: &str,
    default_draft: &str,
    fixture: &crate::scenarios::DesktopLauncher,
) -> Result<(), String> {
    if capture_lines(inferred_draft) == capture_lines(default_draft) {
        return Ok(());
    }
    verify_desktop_launcher_capture(inferred_draft, default_draft, fixture)
}

/// Parse command arrays and prove that normalizing only those arrays reproduces the
/// verified ordinary draft. No recovered arguments enter diagnostics.
fn inferred_commands(
    inferred_draft: &str,
    default_draft: &str,
) -> Result<Vec<Vec<String>>, String> {
    let lines = capture_lines(inferred_draft);
    let mut normalized_lines = Vec::new();
    let mut commands: Vec<Vec<String>> = Vec::new();
    let mut line_index = 0;
    while line_index < lines.len() {
        let line = lines[line_index];
        let field = line.trim_start();
        if !field.starts_with("command:") {
            normalized_lines.push(line.to_owned());
            line_index += 1;
            continue;
        }

        if field != "command:" {
            return Err("inferred capture lacks a command array".into());
        }
        let indent = " ".repeat(line.len() - field.len());
        normalized_lines.push(format!("{indent}command: null"));
        line_index += 1;

        let argument_prefix = format!("{indent}  - ");
        let mut arguments = Vec::new();
        while let Some(scalar) = lines
            .get(line_index)
            .and_then(|next_line| next_line.strip_prefix(argument_prefix.as_str()))
        {
            let argument: String = serde_saphyr::from_str(scalar)
                .map_err(|_| "inferred capture has a malformed command argument")?;
            arguments.push(argument);
            line_index += 1;
        }
        if arguments
            .first()
            .is_none_or(|executable| executable.is_empty())
        {
            return Err("inferred capture has an empty executable".into());
        }
        commands.push(arguments);
    }

    if normalized_lines != capture_lines(default_draft) {
        return Err(
            "inference changed capture hierarchy, order, sizing, matchers, IDs, or focus".into(),
        );
    }

    Ok(commands)
}

/// Compare complete option arguments, accepting Foot's combined and paired forms.
/// Repeated options are retained so duplicates cannot masquerade as a correct value.
fn option_values<'a>(arguments: &'a [String], option: &str) -> Vec<&'a str> {
    let prefix = format!("{option}=");
    let mut values = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        if let Some(value) = argument.strip_prefix(&prefix) {
            values.push(value);
        } else if argument == option {
            values.push(arguments.get(index + 1).map(String::as_str).unwrap_or(""));
        }
    }
    values
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
