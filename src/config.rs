use crate::discovery::valid_name;
use serde::{Deserialize, Deserializer, de};
use serde_saphyr::Spanned;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_INPUT_BYTES: usize = 1024 * 1024;
const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 256;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(deserialize_with = "integer")]
    version: u64,
    pub layout: Spanned<Node>,
    #[serde(default, deserialize_with = "present")]
    pub focus: Option<String>,
    #[serde(default = "default_timeout", deserialize_with = "integer")]
    pub window_timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "NodeFields")]
pub struct Node {
    pub weight: f64,
    pub kind: NodeKind,
}

#[derive(Debug)]
pub enum NodeKind {
    Split {
        split: Orientation,
        children: Vec<Spanned<Node>>,
    },
    Window {
        id: String,
        command: Vec<String>,
        matcher: Matcher,
        cwd: Option<PathBuf>,
        env: BTreeMap<String, String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Orientation {
    Horizontal,
    Vertical,
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "MatchFields")]
pub enum Matcher {
    AppId(String),
    Class(String),
}

// A single map preserves useful field errors, unlike an untagged split/window enum
// which discards each variant's deserialization error while trying the next variant.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeFields {
    #[serde(default = "default_weight", deserialize_with = "number")]
    weight: f64,
    #[serde(default, deserialize_with = "present")]
    split: Option<Orientation>,
    #[serde(default, deserialize_with = "present")]
    children: Option<Vec<Spanned<Node>>>,
    #[serde(default, deserialize_with = "present")]
    id: Option<String>,
    #[serde(default, deserialize_with = "command_field")]
    command: Option<Vec<String>>,
    #[serde(default, rename = "match", deserialize_with = "match_field")]
    matcher: Option<Matcher>,
    #[serde(default, deserialize_with = "present")]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "present")]
    env: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MatchFields {
    #[serde(default, deserialize_with = "present")]
    app_id: Option<String>,
    #[serde(default, deserialize_with = "present")]
    class: Option<String>,
}

impl TryFrom<MatchFields> for Matcher {
    type Error = String;

    fn try_from(fields: MatchFields) -> Result<Self, String> {
        match (fields.app_id, fields.class) {
            (Some(value), None) => Ok(Self::AppId(value)),
            (None, Some(value)) => Ok(Self::Class(value)),
            _ => Err("match must contain exactly one of app_id or class".into()),
        }
    }
}

impl TryFrom<NodeFields> for Node {
    type Error = String;

    fn try_from(fields: NodeFields) -> Result<Self, String> {
        let kind = if fields.split.is_some() || fields.children.is_some() {
            if fields.id.is_some()
                || fields.command.is_some()
                || fields.matcher.is_some()
                || fields.cwd.is_some()
                || fields.env.is_some()
            {
                return Err("node mixes split fields with window fields".into());
            }
            NodeKind::Split {
                split: fields.split.ok_or("split node is missing split")?,
                children: fields.children.ok_or("split node is missing children")?,
            }
        } else {
            NodeKind::Window {
                id: fields.id.ok_or("window node is missing id")?,
                command: fields.command.ok_or("window node is missing command")?,
                matcher: fields.matcher.ok_or("window node is missing match")?,
                cwd: fields.cwd.map(PathBuf::from),
                env: fields.env.unwrap_or_default(),
            }
        };
        Ok(Self {
            weight: fields.weight,
            kind,
        })
    }
}

fn default_weight() -> f64 {
    1.0
}

fn default_timeout() -> u64 {
    20_000
}

// Missing optional fields get defaults; explicit nulls must still be type errors.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn command_field<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error> {
    let value = Option::<Vec<String>>::deserialize(deserializer)?;
    if value.is_none() {
        return Err(de::Error::custom(
            "command: null is a capture placeholder; fill in an executable and literal arguments",
        ));
    }
    Ok(value)
}

fn match_field<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Matcher>, D::Error> {
    let value = Option::<Matcher>::deserialize(deserializer)?;
    if value.is_none() {
        return Err(de::Error::custom(
            "match: null is a capture placeholder; fill in exactly one app_id or class",
        ));
    }
    Ok(value)
}

// Direct numeric deserialization accepts quoted numbers in this parser. Serde's
// untagged buffering uses YAML scalar types and rejects those strings instead.
fn integer<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged, expecting = "a nonnegative YAML integer (not a string)")]
    enum Integer {
        Value(u64),
    }
    let Integer::Value(value) = Integer::deserialize(deserializer)?;
    Ok(value)
}

fn number<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged, expecting = "a finite positive YAML number")]
    enum Number {
        Value(f64),
    }
    let Number::Value(value) = Number::deserialize(deserializer)?;
    Ok(value)
}

pub fn read(path: &Path) -> Result<Config, String> {
    let file = File::open(path).map_err(|error| format!("{}: open: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("{}: inspect: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "{}: expected a regular layout file",
            path.display()
        ));
    }

    // Cap the read itself, including if the file grows after discovery/metadata.
    let mut input = Vec::new();
    file.take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut input)
        .map_err(|error| format!("{}: read: {error}", path.display()))?;
    if input.len() > MAX_INPUT_BYTES {
        return Err(format!("{}: input exceeds the 1 MiB limit", path.display()));
    }
    let input = std::str::from_utf8(&input)
        .map_err(|error| format!("{}: expected UTF-8 YAML: {error}", path.display()))?;
    parse(input, path).map_err(|error| format!("{}: {error}", path.display()))
}

fn parse(input: &str, path: &Path) -> Result<Config, String> {
    if input.len() > MAX_INPUT_BYTES {
        return Err("input exceeds the 1 MiB limit".into());
    }

    let options = serde_saphyr::options! {
        duplicate_keys: serde_saphyr::DuplicateKeyPolicy::Error,
        merge_keys: serde_saphyr::MergeKeyPolicy::Error,
        reject_unsupported_tags: true,
        reject_tagged_field_names: true,
        strict_booleans: true,
        no_schema: true,
        // Source snippets could expose command arguments or environment values.
        with_snippet: false,
        budget: serde_saphyr::budget! {
            max_aliases: 0,
            max_documents: 1,
            // YAML includes children sequences and window field collections in
            // addition to layout nodes. Allow the full 32-node layout depth.
            max_depth: MAX_DEPTH * 2 + 4,
            flow_nesting_limit: MAX_DEPTH * 2 + 4,
            max_events: MAX_INPUT_BYTES,
            max_nodes: MAX_INPUT_BYTES,
            max_total_scalar_bytes: MAX_INPUT_BYTES,
        },
    };
    let mut field_path = String::new();
    let mut config: Config =
        serde_saphyr::with_deserializer_from_str_with_options(input, options, |deserializer| {
            serde_path_to_error::deserialize(deserializer).map_err(|error| {
                let path = error.path().to_string();
                // Spanned contributes synthetic `value` fields to Serde's path.
                field_path = match path.strip_prefix("layout.value") {
                    Some(rest) => format!("layout{}", rest.replace("].value", "]")),
                    None => path,
                };
                error.into_inner()
            })
        })
        .map_err(|error| {
            if field_path.is_empty() {
                field_path = "document".into();
            }
            format!("{field_path}: {error}")
        })?;

    if config.version != 1 {
        return Err("version: expected format version 1".into());
    }
    if config.window_timeout_ms == 0 {
        return Err("window_timeout_ms: expected a positive integer in milliseconds".into());
    }

    let parent = path.parent().unwrap_or(Path::new("."));
    let mut ids = HashSet::new();
    let mut node_count = 0;
    validate_node(
        &mut config.layout,
        "layout",
        1,
        &mut node_count,
        &mut ids,
        parent,
    )?;
    if let Some(focus) = &config.focus
        && !ids.contains(focus)
    {
        return Err(format!(
            "focus: {focus:?} does not identify a window id in this layout"
        ));
    }
    Ok(config)
}

fn validate_node(
    node: &mut Spanned<Node>,
    path: &str,
    depth: usize,
    node_count: &mut usize,
    ids: &mut HashSet<String>,
    parent: &Path,
) -> Result<(), String> {
    let location = node.referenced;
    let fail = |field: &str, message: &str| {
        format!(
            "{path}{field} (line {}, column {}): {message}",
            location.line(),
            location.column()
        )
    };
    *node_count += 1;
    if depth > MAX_DEPTH {
        return Err(fail("", "layout depth exceeds 32 nodes along a path"));
    }
    if *node_count > MAX_NODES {
        return Err(fail("", "layout exceeds 256 nodes"));
    }
    if !node.value.weight.is_finite() || node.value.weight <= 0.0 {
        return Err(fail(".weight", "expected a finite positive number"));
    }

    match &mut node.value.kind {
        NodeKind::Split { children, .. } => {
            if children.len() < 2 {
                return Err(fail(".children", "a split requires at least two children"));
            }
            let mut sum = 0.0;
            for (index, child) in children.iter_mut().enumerate() {
                let child_path = format!("{path}.children[{index}]");
                validate_node(child, &child_path, depth + 1, node_count, ids, parent)?;
                sum += child.value.weight;
            }
            if !sum.is_finite() {
                return Err(fail(".children", "sibling weight sum must be finite"));
            }
        }
        NodeKind::Window {
            id,
            command,
            matcher,
            cwd,
            env,
        } => {
            if !valid_name(id) {
                return Err(fail(".id", "expected [A-Za-z0-9][A-Za-z0-9_-]*"));
            }
            if !ids.insert(id.clone()) {
                return Err(fail(".id", &format!("duplicate window id {id:?}")));
            }
            if command.is_empty() || command[0].is_empty() {
                return Err(fail(
                    ".command",
                    "provide a nonempty executable followed by literal arguments",
                ));
            }
            for (index, argument) in command.iter().enumerate() {
                if argument.contains('\0') {
                    return Err(fail(
                        &format!(".command[{index}]"),
                        "arguments cannot contain NUL",
                    ));
                }
            }
            let (field, selector) = match matcher {
                Matcher::AppId(value) => (".match.app_id", value),
                Matcher::Class(value) => (".match.class", value),
            };
            if selector.is_empty() {
                return Err(fail(field, "provide a nonempty exact window selector"));
            }
            if let Some(directory) = cwd {
                if directory.as_os_str().is_empty()
                    || directory.as_os_str().as_encoded_bytes().contains(&0)
                {
                    return Err(fail(
                        ".cwd",
                        "provide a nonempty directory path without NUL",
                    ));
                }
                let resolved = parent.join(&directory);
                let metadata = fs::metadata(&resolved).map_err(|error| {
                    fail(".cwd", &format!("inspect {}: {error}", resolved.display()))
                })?;
                if !metadata.is_dir() {
                    return Err(fail(
                        ".cwd",
                        &format!("{} is not a directory", resolved.display()),
                    ));
                }
                *directory = resolved;
            }
            for (key, value) in env {
                if key.is_empty() || key.contains(['=', '\0']) {
                    return Err(fail(
                        ".env",
                        "environment keys must be nonempty and contain neither '=' nor NUL",
                    ));
                }
                if value.contains('\0') {
                    return Err(fail(".env", "environment values cannot contain NUL"));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: &str = "{id: terminal, command: [foot], match: {app_id: foot}}";

    fn parse_test(input: &str) -> Result<Config, String> {
        parse(
            input,
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/test.yaml"),
        )
    }

    fn rejects(input: &str, expected: &str) {
        let error = parse_test(input).expect_err(input);
        assert!(
            error.contains(expected),
            "expected {expected:?}, got {error:?}"
        );
    }

    #[test]
    fn four_window_example_preserves_hierarchy_and_order() {
        let config = parse_test(include_str!("../examples/four-windows.yaml")).unwrap();
        assert_eq!(config.focus.as_deref(), Some("browser"));
        assert_eq!(config.window_timeout_ms, 20_000);
        let NodeKind::Split { split, children } = config.layout.value.kind else {
            panic!("expected root split");
        };
        assert!(matches!(split, Orientation::Horizontal));
        assert_eq!(children.len(), 3);
        assert_eq!(children[0].value.weight, 44.0);
        assert_eq!(children[1].value.weight, 32.0);
        assert_eq!(children[2].value.weight, 24.0);
        let NodeKind::Split { split, children } = &children[2].value.kind else {
            panic!("expected nested split");
        };
        assert!(matches!(split, Orientation::Vertical));
        let NodeKind::Window { id, matcher, .. } = &children[0].value.kind else {
            panic!("expected window");
        };
        assert_eq!(id, "upper-right");
        assert!(matches!(matcher, Matcher::AppId(value) if value == "sway-stage-top"));
    }

    #[test]
    fn defaults_and_literal_child_settings() {
        let config = parse_test(
            "version: 1\nlayout:\n  id: '1'\n  command: [./app, '', '$HOME', '~', '*.txt', 'two words']\n  match: {class: Example}\n  cwd: ..\n  env: {'1': '', PATH: 'literal:$PATH'}\n",
        ).unwrap();
        assert!(config.focus.is_none());
        assert_eq!(config.layout.value.weight, 1.0);
        let NodeKind::Window {
            command,
            matcher,
            cwd,
            env,
            ..
        } = config.layout.value.kind
        else {
            panic!("expected window");
        };
        assert_eq!(command, ["./app", "", "$HOME", "~", "*.txt", "two words"]);
        assert!(matches!(matcher, Matcher::Class(value) if value == "Example"));
        assert_eq!(
            cwd.unwrap(),
            Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/..")
        );
        assert_eq!(env["1"], "");
        assert_eq!(env["PATH"], "literal:$PATH");
        assert!(
            parse_test(&format!(
                "version: 1\nlayout: {WINDOW}\nwindow_timeout_ms: {}",
                u64::MAX
            ))
            .is_ok()
        );
    }

    #[test]
    fn unknown_and_duplicate_keys_at_every_mapping_boundary() {
        for (input, expected) in [
            (format!("version: 1\nlayout: {WINDOW}\nextra: true"), "unknown field"),
            ("version: 1\nlayout: {id: t, command: [foot], match: {app_id: foot}, extra: 1}".into(), "unknown field"),
            ("version: 1\nlayout: {split: horizontal, children: [], extra: 1}".into(), "unknown field"),
            ("version: 1\nlayout: {id: t, command: [foot], match: {title: foot}}".into(), "unknown field"),
            (format!("version: 1\nversion: 1\nlayout: {WINDOW}"), "duplicate"),
            ("version: 1\nlayout: {id: t, id: x, command: [foot], match: {app_id: foot}}".into(), "duplicate"),
            ("version: 1\nlayout: {id: t, command: [foot], match: {app_id: foot, app_id: foot}}".into(), "duplicate"),
            ("version: 1\nlayout: {id: t, command: [foot], match: {app_id: foot}, env: {A: x, 'A': y}}".into(), "duplicate"),
            ("version: 1\nlayout: {split: horizontal, children: [], children: []}".into(), "duplicate"),
        ] {
            rejects(&input, expected);
        }
    }

    #[test]
    fn rejects_unsupported_yaml_constructs_and_malformed_input() {
        for input in [
            format!("version: 1\nlayout: &window {WINDOW}\nfocus: *window"),
            "version: 1\nlayout: {id: &id t, command: [foot], match: {app_id: *id}}".into(),
            "version: 1\nlayout: {id: t, command: [&app foot, *app], match: {app_id: foot}}".into(),
            "version: 1\nlayout: {<<: {id: t}, command: [foot], match: {app_id: foot}}".into(),
            "version: 1\nlayout: {id: t, command: [foot], match: {app_id: foot}, env: {<<: {A: x}}}".into(),
            format!("!custom\nversion: 1\nlayout: {WINDOW}"),
            format!("version: 1\nlayout: !custom {WINDOW}"),
            format!("version: 1\n!custom layout: {WINDOW}"),
            "version: 1\nlayout: {id: t, command: [!custom foot], match: {app_id: foot}}".into(),
            "version: 1\nlayout: {id: t, command: [foot], match: {app_id: foot}, env: {!custom A: x}}".into(),
            format!("%TAG !e! tag:example.com,2026:\n---\nversion: 1\nlayout: !e!custom {WINDOW}"),
            format!("version: 1\nlayout: {WINDOW}\n---\nversion: 1\nlayout: {WINDOW}"),
            format!("version: 1\nlayout: {WINDOW}\n---\n"),
            format!("version: 1\nlayout: {WINDOW}\n...\ntrailing garbage"),
            "version: 1\nlayout: [unterminated".into(),
            "".into(),
        ] {
            assert!(parse_test(&input).is_err(), "accepted {input}");
        }
        // YAML punctuation inside literal strings is not a construct to reject.
        assert!(parse_test("version: 1\nlayout: {id: t, command: [foot, '*alias', '!tag', '<<:'], match: {app_id: foot}}").is_ok());
    }

    #[test]
    fn rejects_wrong_types_and_explicit_nulls() {
        for input in [
            format!("version: '1'\nlayout: {WINDOW}"),
            format!("version: 1.0\nlayout: {WINDOW}"),
            format!("version: true\nlayout: {WINDOW}"),
            format!("layout: {WINDOW}"),
            "version: 1".into(),
            "version: 1\nlayout: null".into(),
            "version: 1\nlayout: []".into(),
            "version: 1\nlayout: {id: 123, command: [foot], match: {app_id: foot}}".into(),
            "version: 1\nlayout: {id: t, command: foot, match: {app_id: foot}}".into(),
            "version: 1\nlayout: {id: t, command: [foot, 123], match: {app_id: foot}}".into(),
            "version: 1\nlayout: {id: t, command: [foot], match: {app_id: true}}".into(),
            "version: 1\nlayout: {id: t, command: [foot], match: {app_id: foot}, env: {A: 1}}"
                .into(),
            "version: 1\nlayout: {id: t, command: [foot], match: {app_id: foot}, env: {1: x}}"
                .into(),
        ] {
            assert!(parse_test(&input).is_err(), "accepted {input}");
        }
        for field in ["focus", "window_timeout_ms"] {
            rejects(
                &format!("version: 1\nlayout: {WINDOW}\n{field}: null"),
                field,
            );
        }
        for field in ["weight", "cwd", "env", "id", "split", "children"] {
            let input = format!(
                "version: 1\nlayout: {{command: [foot], match: {{app_id: foot}}, {field}: null}}"
            );
            assert!(parse_test(&input).is_err(), "accepted {input}");
        }
        rejects(
            "version: 1\nlayout: {id: t, command: null, match: {app_id: foot}}",
            "fill in an executable",
        );
        rejects(
            "version: 1\nlayout: {id: t, command: [foot], match: null}",
            "fill in exactly one",
        );
        rejects(
            "version: 1\nlayout: {id: t, command: [foot], match: {app_id: null}}",
            "match.app_id",
        );
    }

    #[test]
    fn validates_structure_ids_commands_and_selectors() {
        for input in [
            "{id: '', command: [foot], match: {app_id: foot}}",
            "{id: bad.id, command: [foot], match: {app_id: foot}}",
            "{id: t, command: [], match: {app_id: foot}}",
            "{id: t, command: ['', x], match: {app_id: foot}}",
            r#"{id: t, command: [foot, "a\0b"], match: {app_id: foot}}"#,
            "{id: t, match: {app_id: foot}}",
            "{id: t, command: [foot]}",
            "{command: [foot], match: {app_id: foot}}",
            "{id: t, command: [foot], match: {}}",
            "{id: t, command: [foot], match: {app_id: ''}}",
            "{id: t, command: [foot], match: {class: ''}}",
            "{id: t, command: [foot], match: {app_id: foot, class: Foot}}",
            "{split: horizontal, children: []}",
            "{split: vertical}",
            "{children: []}",
            "{split: diagonal, children: []}",
            "{split: horizontal, children: [], id: t}",
            "{split: horizontal, children: [], env: {}}",
        ] {
            assert!(
                parse_test(&format!("version: 1\nlayout: {input}")).is_err(),
                "accepted {input}"
            );
        }
        rejects(
            &format!("version: 1\nlayout: {{split: horizontal, children: [{WINDOW}]}}"),
            "at least two",
        );
        rejects(
            &format!("version: 1\nlayout: {{split: horizontal, children: [{WINDOW}, {WINDOW}]}}"),
            "duplicate window id",
        );
        rejects(
            &format!("version: 1\nlayout: {WINDOW}\nfocus: missing"),
            "focus",
        );
        rejects(&format!("version: 2\nlayout: {WINDOW}"), "version");
    }

    #[test]
    fn validates_weights_and_timeouts_without_coercion() {
        for weight in [
            "0", "-1", ".nan", ".inf", "-.inf", "1e999", "'1'", "true", "null",
        ] {
            let input = format!(
                "version: 1\nlayout: {{id: t, command: [foot], match: {{app_id: foot}}, weight: {weight}}}"
            );
            rejects(&input, "weight");
        }
        for weight in ["0.5", "1", "1e300", "5e-324"] {
            let input = format!(
                "version: 1\nlayout: {{id: t, command: [foot], match: {{app_id: foot}}, weight: {weight}}}"
            );
            assert!(parse_test(&input).is_ok(), "rejected {input}");
        }
        rejects(
            "version: 1\nlayout: {split: horizontal, children: [{id: a, command: [foot], match: {app_id: foot}, weight: 1e308}, {id: b, command: [foot], match: {app_id: foot}, weight: 1e308}]}",
            "sum must be finite",
        );
        for timeout in [
            "0",
            "-1",
            "1.5",
            "'10'",
            "true",
            ".inf",
            "18446744073709551616",
        ] {
            rejects(
                &format!("version: 1\nlayout: {WINDOW}\nwindow_timeout_ms: {timeout}"),
                "window_timeout_ms",
            );
        }
    }

    #[test]
    fn validates_cwd_and_environment() {
        for cwd in [
            "''",
            "missing-sway-stage-test-directory",
            "../Cargo.toml",
            r#""a\0b""#,
        ] {
            let input = format!(
                "version: 1\nlayout: {{id: t, command: [foot], match: {{app_id: foot}}, cwd: {cwd}}}"
            );
            rejects(&input, "cwd");
        }
        for env in ["{'': x}", "{'A=B': x}", r#"{"A\0B": x}"#, r#"{A: "x\0y"}"#] {
            let input = format!(
                "version: 1\nlayout: {{id: t, command: [foot], match: {{app_id: foot}}, env: {env}}}"
            );
            rejects(&input, "env");
        }
    }

    fn nested_layout(depth: usize) -> String {
        let mut node = WINDOW.to_owned();
        for index in 1..depth {
            node = format!(
                "{{split: horizontal, children: [{node}, {{id: w{index}, command: [foot], match: {{app_id: foot}}}}]}}"
            );
        }
        format!("version: 1\nlayout: {node}")
    }

    #[test]
    fn bounds_layout_depth_and_parser_nesting() {
        assert!(parse_test(&nested_layout(MAX_DEPTH)).is_ok());
        rejects(&nested_layout(MAX_DEPTH + 1), "depth exceeds 32");
        let input = format!(
            "version: 1\nlayout: &anchor {}0{}",
            "[".repeat(10_000),
            "]".repeat(10_000)
        );
        assert!(parse_test(&input).is_err());
    }

    #[test]
    fn bounds_layout_node_count() {
        for (count, valid) in [(255, true), (256, false)] {
            let children: Vec<_> = (0..count)
                .map(|index| format!("{{id: w{index}, command: [foot], match: {{app_id: foot}}}}"))
                .collect();
            let input = format!(
                "version: 1\nlayout: {{split: horizontal, children: [{}]}}",
                children.join(",")
            );
            assert_eq!(parse_test(&input).is_ok(), valid);
        }
    }

    #[test]
    fn bounds_input_bytes_including_comments() {
        let mut input = format!("version: 1\nlayout: {WINDOW}\n#");
        input.push_str(&"x".repeat(MAX_INPUT_BYTES - input.len()));
        assert!(parse_test(&input).is_ok());
        input.push('x');
        rejects(&input, "1 MiB");
    }

    #[test]
    fn nested_errors_report_field_path_and_source_location() {
        let input = format!(
            "version: 1\nlayout:\n  split: horizontal\n  children:\n    - {WINDOW}\n    - id: second\n      command: [foot]\n      match: {{app_id: foot}}\n      weight: 0\n"
        );
        let error = parse_test(&input).unwrap_err();
        assert!(error.contains("layout.children[1].weight"), "{error}");
        assert!(error.contains("line 6, column"), "{error}");
        let error = parse_test(&input.replace("weight: 0", "command: []")).unwrap_err();
        assert!(error.contains("layout.children[1]"), "{error}");
        assert!(error.contains("line"), "{error}");
    }
}
