//! Verification of `sway-stage capture` drafts against the verified arrangement.
//!
//! This is a narrow structural comparison with capture's one known serializer format:
//! only command argument scalars use YAML decoding. A format change fails visibly, and
//! `check` on the filled draft supplies authoritative syntax and semantic validation.
//! Recovered command arguments never appear in messages.

use crate::layout::LayoutMeasurements;
use crate::scenarios::{ConfiguredNode, Scenario, expected_draft, json_sequence};

/// A captured weight is the measured share rounded to hundredths.
const WEIGHT_TOLERANCE: f64 = 0.02;

/// Fields whose generated values contain no YAML punctuation, so a serializer that quotes
/// them changes nothing a quote-stripping comparison would miss.
const IDENTIFIER_KEYS: [&str; 4] = ["id", "focus", "app_id", "group"];

/// The lines of a draft that carry content: blank and comment lines are dropped and
/// trailing whitespace and the generated version reminder are removed.
fn capture_lines(draft: &str) -> Vec<&str> {
    draft
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty() && !line.trim_start().starts_with('#'))
        .map(|line| match line {
            "version: 1  # ← Required; do not remove" => "version: 1",
            _ => line,
        })
        .collect()
}

/// Compares an inference-disabled draft with the expected one, line by line: indentation and
/// sequence arrangement, keys, identifiers, focus, `command: null` placeholders, unchanged
/// selectors, and weights within 0.02 of the measured shares. Returns the draft with each
/// placeholder filled with a harmless command, ready for `sway-stage check`. Never echo
/// captured lines: even an ordinary capture could unexpectedly contain recovered arguments.
pub fn verify_capture(
    draft: &str,
    scenario: &Scenario,
    measurements: &LayoutMeasurements,
) -> Result<String, String> {
    let observed_lines = capture_lines(draft);
    let expected_draft_yaml = expected_draft(scenario, &measurements.shares)?;
    let expected_lines: Vec<&str> = expected_draft_yaml.lines().collect();
    if observed_lines.len() != expected_lines.len() {
        return Err("capture draft has unexpected fields/structure".into());
    }

    let mut placeholder_count = 0;
    let mut filled_lines = Vec::new();
    for (observed_line, expected_line) in observed_lines.into_iter().zip(expected_lines) {
        let observed_field = observed_line.trim_start_matches([' ', '-']);
        let expected_field = expected_line.trim_start_matches([' ', '-']);
        let observed_arrangement = &observed_line[..observed_line.len() - observed_field.len()];
        let expected_arrangement = &expected_line[..expected_line.len() - expected_field.len()];
        if observed_arrangement != expected_arrangement {
            return Err("capture indentation/arrangement".into());
        }

        let Some((key, observed_scalar)) = observed_field.split_once(':') else {
            return Err("capture field".into());
        };
        let (expected_key, expected_scalar) = expected_field
            .split_once(':')
            .unwrap_or((expected_field, ""));
        if key != expected_key {
            return Err("capture field".into());
        }
        let observed_scalar = observed_scalar.trim();
        let expected_scalar = expected_scalar.trim();

        if key == "weight" {
            let weight: f64 = observed_scalar
                .parse()
                .map_err(|_| "capture weight is not numeric".to_owned())?;
            let expected_weight: f64 = expected_scalar
                .parse()
                .map_err(|_| "expected weight is not numeric".to_owned())?;
            if !weight.is_finite() || (weight - expected_weight).abs() > WEIGHT_TOLERANCE {
                return Err("capture sizing".into());
            }
        } else {
            let observed_value = if IDENTIFIER_KEYS.contains(&key) {
                strip_quotes(observed_scalar)
            } else {
                observed_scalar
            };
            if observed_value != expected_scalar {
                return Err("capture value".into());
            }
        }

        if key == "command" {
            placeholder_count += 1;
            filled_lines.push(observed_line.replace("command: null", r#"command: ["true"]"#));
        } else {
            filled_lines.push(observed_line.to_owned());
        }
    }

    if placeholder_count != scenario.layout.leaves().len() {
        return Err("capture command placeholder count differs".into());
    }
    Ok(filled_lines.join("\n") + "\n")
}

/// Builds replay YAML only by editing the fully verified capture's null commands.
/// The independent model keeps the proven shape, selectors and focus, but takes every
/// split-child weight from the captured text. Integer hundredths preserve capture's precision
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
    apply_captured_weights(&mut replay.layout, false, &mut weights)?;
    if weights.next().is_some() {
        return Err("extra captured weight".into());
    }
    replay.configured_focus = Some(scenario.expected_focus);
    Ok((replay_lines.join("\n") + "\n", replay))
}

fn apply_captured_weights(
    configured: &mut ConfiguredNode,
    weighted: bool,
    weights: &mut std::vec::IntoIter<u32>,
) -> Result<(), String> {
    let weight = if weighted {
        weights.next().ok_or("missing captured weight")?
    } else {
        1
    };
    match configured {
        ConfiguredNode::Window(window) => window.weight = weight,
        ConfiguredNode::Group {
            weight: group_weight,
            children,
            layout,
            ..
        } => {
            *group_weight = weight;
            for child in children {
                apply_captured_weights(child, layout.is_split(), weights)?;
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

/// Compares an inferred draft with the verified inference-disabled draft. Only the
/// serializer's flow and block command sequences are recognized; with each replaced by
/// `command: null`, the drafts must agree exactly, so inference changed no hierarchy, order,
/// sizing, selector, ID, or focus. Each recovered Foot command must carry its leaf's app ID,
/// as `--app-id=ID` or as `--app-id ID`. Shared app IDs also require the corresponding
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
        if option_values(arguments, "--app-id") != [leaf.app_id.as_str()] {
            return Err(format!(
                "inferred command does not identify the correct Foot window: {}",
                leaf.layout_id
            ));
        }
        let shared_app_id = leaves
            .iter()
            .filter(|other| other.app_id == leaf.app_id)
            .count()
            > 1;
        if shared_app_id {
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

/// Compares an inferred draft with the verified inference-disabled draft, as
/// `verify_inferred_capture` does, for windows that keep an application's default app ID and
/// so carry no `--app-id` to check. Each recovered command must instead be its own leaf's
/// launch: exactly the configured literal arguments after a nonempty executable, the same
/// rule that `/proc/PID/cmdline` ownership evidence applies, so a distinct per-leaf title
/// identifies the window. Such a draft can be loaded unchanged.
pub fn verify_recovered_launch_commands(
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
        let same_arguments =
            arguments.len() == leaf.command.len() && arguments.get(1..) == leaf.command.get(1..);
        if !same_arguments {
            return Err(format!(
                "inferred command does not reproduce the window's own launch: {}",
                leaf.layout_id
            ));
        }
    }
    Ok(())
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

        let command_value = field.strip_prefix("command:").unwrap().trim();
        if !command_value.is_empty() && !command_value.starts_with('[') {
            return Err("inferred capture lacks a command array".into());
        }
        let indent = " ".repeat(line.len() - field.len());
        normalized_lines.push(format!("{indent}command: null"));
        line_index += 1;

        let arguments = if command_value.is_empty() {
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
            arguments
        } else {
            serde_saphyr::from_str::<Vec<String>>(command_value)
                .map_err(|_| "inferred capture has a malformed command array")?
        };
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
