//! Verification of `sway-stage capture` drafts against the verified arrangement.
//!
//! This is a narrow comparison with capture's one known serializer format, not a YAML
//! parser: a format change fails visibly, and `check` on the filled draft supplies the
//! authoritative syntax and semantic validation. Recovered command arguments from
//! `--infer-commands` stay in memory: they never appear in messages or files.

use crate::layout::LayoutMeasurements;
use crate::scenarios::{ConfiguredNode, Scenario, Selector, expected_draft};

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
/// placeholder filled with a harmless command, ready for `sway-stage check`.
pub fn verify_capture(
    draft: &str,
    scenario: &Scenario,
    measurements: &LayoutMeasurements,
) -> Result<String, String> {
    let actual_lines = capture_lines(draft);
    let expected = expected_draft(scenario, &measurements.shares)?;
    let expected_lines: Vec<&str> = expected.lines().collect();
    if actual_lines.len() != expected_lines.len() {
        return Err(format!(
            "capture draft has unexpected fields/structure:\n{draft}"
        ));
    }

    let mut placeholder_count = 0;
    let mut filled_lines = Vec::new();
    for (actual_line, expected_line) in actual_lines.into_iter().zip(expected_lines) {
        let mismatch =
            |problem: &str| format!("{problem}: {actual_line:?}, expected {expected_line:?}");

        let actual_field = actual_line.trim_start_matches([' ', '-']);
        let expected_field = expected_line.trim_start_matches([' ', '-']);
        let actual_arrangement = &actual_line[..actual_line.len() - actual_field.len()];
        let expected_arrangement = &expected_line[..expected_line.len() - expected_field.len()];
        if actual_arrangement != expected_arrangement {
            return Err(mismatch("capture indentation/arrangement"));
        }

        let Some((key, actual_scalar)) = actual_field.split_once(':') else {
            return Err(mismatch("capture field"));
        };
        let (expected_key, expected_scalar) = expected_field
            .split_once(':')
            .unwrap_or((expected_field, ""));
        if key != expected_key {
            return Err(mismatch("capture field"));
        }
        let actual_scalar = actual_scalar.trim();
        let expected_scalar = expected_scalar.trim();

        if key == "weight" {
            let weight: f64 = actual_scalar
                .parse()
                .map_err(|_| format!("capture weight is not numeric: {actual_line}"))?;
            let expected_weight: f64 = expected_scalar
                .parse()
                .map_err(|_| format!("expected weight is not numeric: {expected_line}"))?;
            if !weight.is_finite() || (weight - expected_weight).abs() > WEIGHT_TOLERANCE {
                return Err(mismatch("capture sizing"));
            }
        } else {
            let actual_value = if IDENTIFIER_KEYS.contains(&key) {
                strip_quotes(actual_scalar)
            } else {
                actual_scalar
            };
            if actual_value != expected_scalar {
                return Err(mismatch("capture value"));
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
/// as `--app-id=ID` or as `--app-id ID`. Other arguments stay opaque.
pub fn verify_inferred_capture(
    inferred_draft: &str,
    default_draft: &str,
    layout: &ConfiguredNode,
) -> Result<(), String> {
    let lines = capture_lines(inferred_draft);
    let mut normalized_lines = Vec::new();
    let mut commands: Vec<Vec<&str>> = Vec::new();
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
            arguments.push(strip_matching_quotes(scalar));
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

    let leaves = layout.leaves();
    if commands.len() != leaves.len() {
        return Err("inferred command count differs".into());
    }
    for (arguments, leaf) in commands.iter().zip(leaves) {
        let Selector::AppId(app_id) = &leaf.selector else {
            return Err(format!(
                "inferred command checks need an app ID selector: {}",
                leaf.layout_id
            ));
        };
        let combined = format!("--app-id={app_id}");
        let has_combined = arguments.contains(&combined.as_str());
        let has_pair = arguments
            .windows(2)
            .any(|pair| pair[0] == "--app-id" && pair[1] == app_id);
        if !has_combined && !has_pair {
            return Err(format!(
                "inferred command does not identify the correct Foot window: {}",
                leaf.layout_id
            ));
        }
    }
    Ok(())
}

/// Removes one pair of matching surrounding quotes. Only the simple, generated `--app-id`
/// tokens are interpreted; escapes are left alone.
fn strip_matching_quotes(scalar: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = scalar
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    scalar
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
