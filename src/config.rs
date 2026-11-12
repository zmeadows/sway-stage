use crate::discovery::valid_name;
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_saphyr::Spanned;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_INPUT_BYTES: usize = 1024 * 1024;
// Capture enforces the same structural limits, so a filled-in draft remains loadable.
pub const MAX_LAYOUT_DEPTH: usize = 32;
pub const MAX_LAYOUT_NODES: usize = 256;
// Each wait is interactive: the user must keep the target workspace focused throughout.
// Five minutes covers slow cold starts; larger values are not interactive waits.
const MAX_WINDOW_TIMEOUT_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(deserialize_with = "unquoted_integer")]
    version: u64,
    pub layout: Spanned<LayoutNode>,
    #[serde(default, deserialize_with = "non_null")]
    pub focus: Option<String>,
    #[serde(
        default = "default_window_timeout_ms",
        deserialize_with = "unquoted_integer"
    )]
    pub window_timeout_ms: u64,
}

/// A configured group or window. Distinct from `swayipc::Node`, an observed Sway container.
#[derive(Debug, Deserialize)]
#[serde(try_from = "NodeFields")]
pub struct LayoutNode {
    // Retain omission so tabbed/stacked parents can reject every explicit weight.
    pub weight: Option<f64>,
    pub kind: NodeKind,
}

#[derive(Debug)]
pub enum NodeKind {
    Group {
        layout: GroupLayout,
        children: Vec<Spanned<LayoutNode>>,
    },
    Window {
        id: String,
        command: Vec<String>,
        matcher: Matcher,
        cwd: Option<PathBuf>,
        env: BTreeMap<String, String>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupLayout {
    SplitH,
    SplitV,
    Tabbed,
    Stacked,
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "MatchFields")]
pub enum Matcher {
    AppId(String),
    Class(String),
}

// A single map preserves useful field errors, unlike an untagged group/window enum
// which discards each variant's deserialization error while trying the next variant.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeFields {
    #[serde(default, deserialize_with = "optional_weight")]
    weight: Option<f64>,
    #[serde(default, deserialize_with = "non_null")]
    group: Option<GroupLayout>,
    #[serde(default, deserialize_with = "non_null")]
    children: Option<Vec<Spanned<LayoutNode>>>,
    #[serde(default, deserialize_with = "non_null")]
    id: Option<String>,
    #[serde(default, deserialize_with = "non_null_command")]
    command: Option<Vec<String>>,
    #[serde(default, rename = "match", deserialize_with = "non_null_match")]
    matcher: Option<Matcher>,
    #[serde(default, deserialize_with = "non_null")]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "non_null")]
    env: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MatchFields {
    #[serde(default, deserialize_with = "non_null")]
    app_id: Option<String>,
    #[serde(default, deserialize_with = "non_null")]
    class: Option<String>,
}

impl TryFrom<MatchFields> for Matcher {
    type Error = String;

    fn try_from(fields: MatchFields) -> Result<Self, String> {
        match (fields.app_id, fields.class) {
            (Some(app_id), None) => Ok(Self::AppId(app_id)),
            (None, Some(class)) => Ok(Self::Class(class)),
            _ => Err("match must contain exactly one of app_id or class".into()),
        }
    }
}

impl TryFrom<NodeFields> for LayoutNode {
    type Error = String;

    fn try_from(fields: NodeFields) -> Result<Self, String> {
        let is_group = fields.group.is_some() || fields.children.is_some();
        let has_window_fields = fields.id.is_some()
            || fields.command.is_some()
            || fields.matcher.is_some()
            || fields.cwd.is_some()
            || fields.env.is_some();

        let kind = if is_group {
            if has_window_fields {
                return Err("node mixes group fields with window fields".into());
            }
            NodeKind::Group {
                layout: fields.group.ok_or("group node is missing group")?,
                children: fields.children.ok_or("group node is missing children")?,
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

impl LayoutNode {
    /// The effective split share weight, including the default for an omitted field.
    pub fn weight(&self) -> f64 {
        self.weight.unwrap_or(1.0)
    }
}

fn optional_weight<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<f64>, D::Error> {
    unquoted_number(deserializer).map(Some)
}

fn default_window_timeout_ms() -> u64 {
    20_000
}

// Missing optional fields get defaults; explicit nulls must still be type errors.
fn non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

// Like `non_null`, but explains that a null command is an unfilled capture placeholder.
fn non_null_command<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error> {
    let command = Option::<Vec<String>>::deserialize(deserializer)?;
    if command.is_none() {
        return Err(de::Error::custom(
            "command: null is a capture placeholder; fill in an executable and literal arguments",
        ));
    }
    Ok(command)
}

// Like `non_null`, but explains that a null match is an unfilled capture placeholder.
fn non_null_match<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Matcher>, D::Error> {
    let matcher = Option::<Matcher>::deserialize(deserializer)?;
    if matcher.is_none() {
        return Err(de::Error::custom(
            "match: null is a capture placeholder; fill in exactly one app_id or class",
        ));
    }
    Ok(matcher)
}

// Direct numeric deserialization accepts quoted numbers in this parser. Serde's
// untagged buffering uses YAML scalar types and rejects those strings instead.
fn unquoted_integer<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged, expecting = "a nonnegative YAML integer (not a string)")]
    enum Integer {
        Value(u64),
    }
    let Integer::Value(value) = Integer::deserialize(deserializer)?;
    Ok(value)
}

fn unquoted_number<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
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
    let mut bytes = Vec::new();
    file.take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{}: read: {error}", path.display()))?;
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(format!("{}: input exceeds the 1 MiB limit", path.display()));
    }

    let input = std::str::from_utf8(&bytes)
        .map_err(|error| format!("{}: expected UTF-8 YAML: {error}", path.display()))?;
    parse(input, path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Parses and validates one layout document. `path` locates the file only for resolving
/// relative `cwd` values; it is not read here.
pub(crate) fn parse(input: &str, path: &Path) -> Result<Config, String> {
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
            max_depth: MAX_LAYOUT_DEPTH * 2 + 4,
            flow_nesting_limit: MAX_LAYOUT_DEPTH * 2 + 4,
            max_events: MAX_INPUT_BYTES,
            max_nodes: MAX_INPUT_BYTES,
            max_total_scalar_bytes: MAX_INPUT_BYTES,
        },
    };
    let mut field_path = String::new();
    let mut config: Config =
        serde_saphyr::with_deserializer_from_str_with_options(input, options, |deserializer| {
            serde_path_to_error::deserialize(deserializer).map_err(|error| {
                let serde_path = error.path().to_string();
                // Spanned contributes synthetic `value` fields to Serde's path.
                field_path = match serde_path.strip_prefix("layout.value") {
                    Some(rest) => format!("layout{}", rest.replace("].value", "]")),
                    None => serde_path,
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
    if !(1..=MAX_WINDOW_TIMEOUT_MS).contains(&config.window_timeout_ms) {
        return Err(format!(
            "window_timeout_ms: expected milliseconds from 1 to {MAX_WINDOW_TIMEOUT_MS} (five minutes)"
        ));
    }

    let layout_dir = path.parent().unwrap_or(Path::new("."));
    let mut window_ids = HashSet::new();
    let mut node_count = 0;
    validate_node(
        &mut config.layout,
        "layout",
        1,
        &mut node_count,
        &mut window_ids,
        layout_dir,
    )?;

    if let Some(focus) = &config.focus
        && !window_ids.contains(focus)
    {
        return Err(format!(
            "focus: {focus:?} does not identify a window id in this layout"
        ));
    }
    Ok(config)
}

/// Validates a subtree and resolves each window's relative `cwd` against `layout_dir`.
/// `field_path` names the node in errors, such as `layout.children[1]`.
fn validate_node(
    node: &mut Spanned<LayoutNode>,
    field_path: &str,
    depth: usize,
    node_count: &mut usize,
    window_ids: &mut HashSet<String>,
    layout_dir: &Path,
) -> Result<(), String> {
    let location = node.referenced;
    let fail = |field: &str, message: &str| {
        format!(
            "{field_path}{field} (line {}, column {}): {message}",
            location.line(),
            location.column()
        )
    };

    *node_count += 1;
    if depth > MAX_LAYOUT_DEPTH {
        return Err(fail("", "layout depth exceeds 32 nodes along a path"));
    }
    if *node_count > MAX_LAYOUT_NODES {
        return Err(fail("", "layout exceeds 256 nodes"));
    }
    if !node.value.weight().is_finite() || node.value.weight() <= 0.0 {
        return Err(fail(".weight", "expected a finite positive number"));
    }

    match &mut node.value.kind {
        NodeKind::Group { layout, children } => {
            let is_split = matches!(layout, GroupLayout::SplitH | GroupLayout::SplitV);
            if is_split && children.len() < 2 {
                return Err(fail(
                    ".children",
                    "a split group requires at least two children",
                ));
            }
            if children.is_empty() {
                return Err(fail(
                    ".children",
                    "a tabbed or stacked group requires at least one child",
                ));
            }

            let mut weight_sum = 0.0;
            for (child_index, child) in children.iter_mut().enumerate() {
                let child_path = format!("{field_path}.children[{child_index}]");
                if !is_split && child.value.weight.is_some() {
                    let location = child.referenced;
                    return Err(format!(
                        "{child_path}.weight (line {}, column {}): children of tabbed or stacked groups must omit weight",
                        location.line(),
                        location.column()
                    ));
                }
                validate_node(
                    child,
                    &child_path,
                    depth + 1,
                    node_count,
                    window_ids,
                    layout_dir,
                )?;
                weight_sum += child.value.weight();
            }
            if !weight_sum.is_finite() {
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
            if !window_ids.insert(id.clone()) {
                return Err(fail(".id", &format!("duplicate window id {id:?}")));
            }

            if command.is_empty() || command[0].is_empty() {
                return Err(fail(
                    ".command",
                    "provide a nonempty executable followed by literal arguments",
                ));
            }
            for (argument_index, argument) in command.iter().enumerate() {
                if argument.contains('\0') {
                    return Err(fail(
                        &format!(".command[{argument_index}]"),
                        "arguments cannot contain NUL",
                    ));
                }
            }

            let (field, selector) = match matcher {
                Matcher::AppId(app_id) => (".match.app_id", app_id),
                Matcher::Class(class) => (".match.class", class),
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
                let resolved = layout_dir.join(&directory);
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
        let NodeKind::Group {
            layout: root_layout,
            children: root_children,
        } = config.layout.value.kind
        else {
            panic!("expected root split");
        };
        assert!(matches!(root_layout, GroupLayout::SplitH));
        assert_eq!(root_children.len(), 3);
        assert_eq!(root_children[0].value.weight(), 44.0);
        assert_eq!(root_children[1].value.weight(), 32.0);
        assert_eq!(root_children[2].value.weight(), 24.0);

        let NodeKind::Window { id, matcher, .. } = &root_children[0].value.kind else {
            panic!("expected browser window");
        };
        assert_eq!(id, "browser");
        assert!(matches!(matcher, Matcher::AppId(value) if value == "firefox-esr"));

        let NodeKind::Group {
            layout: column_layout,
            children: column_children,
        } = &root_children[2].value.kind
        else {
            panic!("expected nested right column split");
        };
        assert!(matches!(column_layout, GroupLayout::SplitV));
        let NodeKind::Window { id, matcher, .. } = &column_children[0].value.kind else {
            panic!("expected upper-right window");
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
        assert_eq!(config.layout.value.weight(), 1.0);
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
    }

    #[test]
    fn unknown_and_duplicate_keys_at_every_mapping_boundary() {
        for (input, expected) in [
            (format!("version: 1\nlayout: {WINDOW}\nextra: true"), "unknown field"),
            ("version: 1\nlayout: {id: t, command: [foot], match: {app_id: foot}, extra: 1}".into(), "unknown field"),
            ("version: 1\nlayout: {group: splith, children: [], extra: 1}".into(), "unknown field"),
            ("version: 1\nlayout: {id: t, command: [foot], match: {title: foot}}".into(), "unknown field"),
            (format!("version: 1\nversion: 1\nlayout: {WINDOW}"), "duplicate"),
            ("version: 1\nlayout: {id: t, id: x, command: [foot], match: {app_id: foot}}".into(), "duplicate"),
            ("version: 1\nlayout: {id: t, command: [foot], match: {app_id: foot, app_id: foot}}".into(), "duplicate"),
            ("version: 1\nlayout: {id: t, command: [foot], match: {app_id: foot}, env: {A: x, 'A': y}}".into(), "duplicate"),
            ("version: 1\nlayout: {group: splith, children: [], children: []}".into(), "duplicate"),
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
        for field in ["weight", "cwd", "env", "id", "group", "children"] {
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
    fn group_layouts_child_counts_and_mixed_nesting() {
        let second = WINDOW.replace("terminal", "second");
        for layout in ["splith", "splitv", "tabbed", "stacked"] {
            let input =
                format!("version: 1\nlayout: {{group: {layout}, children: [{WINDOW}, {second}]}}");
            assert!(parse_test(&input).is_ok(), "{input}");
            rejects(
                &format!("version: 1\nlayout: {{group: {layout}, children: []}}"),
                "at least",
            );
            let singleton =
                format!("version: 1\nlayout: {{group: {layout}, children: [{WINDOW}]}}");
            assert_eq!(
                parse_test(&singleton).is_ok(),
                matches!(layout, "tabbed" | "stacked")
            );
        }

        let input = format!(
            "version: 1\nfocus: second\nlayout: {{group: splith, weight: 7, children: [{{group: tabbed, weight: 3, children: [{{group: stacked, children: [{{group: splitv, children: [{WINDOW}, {second}]}}]}}]}}, {{id: third, command: [foot], match: {{app_id: foot}}}}]}}"
        );
        let config = parse_test(&input).unwrap();
        assert_eq!(config.layout.value.weight(), 7.0);
        assert_eq!(config.focus.as_deref(), Some("second"));
        let NodeKind::Group { children, .. } = &config.layout.value.kind else {
            panic!("expected group");
        };
        assert_eq!(children[0].value.weight, Some(3.0));
        assert_eq!(children[1].value.weight, None);
    }

    #[test]
    fn tabbed_and_stacked_children_must_omit_weight() {
        for layout in ["tabbed", "stacked"] {
            for child in [
                "{id: terminal, command: [foot], match: {app_id: foot}, weight: 1}".to_owned(),
                format!("{{group: tabbed, weight: 1, children: [{WINDOW}]}}"),
                format!("{{group: stacked, weight: 2, children: [{WINDOW}]}}"),
                format!(
                    "{{group: splith, weight: 3, children: [{WINDOW}, {{id: second, command: [foot], match: {{app_id: foot}}}}]}}"
                ),
            ] {
                let input = format!(
                    "version: 1\nlayout: {{group: tabbed, children: [{{group: {layout}, children: [{child}]}}]}}"
                );
                let error = parse_test(&input).unwrap_err();
                for context in [
                    "layout.children[0].children[0].weight",
                    "must omit weight",
                    "line",
                    "column",
                ] {
                    assert!(error.contains(context), "{error}");
                }
            }
        }
    }

    #[test]
    fn rejects_legacy_and_invalid_group_fields_with_context() {
        for child in [
            "{split: horizontal, children: []}",
            "{split: vertical, children: []}",
            "{group: horizontal, children: []}",
            "{group: vertical, children: []}",
            "{group: diagonal, children: []}",
            "{group: null, children: []}",
            "{group: tabbed}",
            "{group: tabbed, children: [], id: terminal}",
            "{group: stacked, children: [], command: [foot]}",
        ] {
            let input = format!("version: 1\nlayout: {{group: tabbed, children: [{child}]}}");
            let error = parse_test(&input).unwrap_err();
            for context in ["layout.children[0]", "line", "column"] {
                assert!(error.contains(context), "{error}");
            }
        }
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
            "{group: splith, children: []}",
            "{group: splitv}",
            "{children: []}",
            "{group: diagonal, children: []}",
            "{group: splith, children: [], id: t}",
            "{group: splith, children: [], env: {}}",
        ] {
            assert!(
                parse_test(&format!("version: 1\nlayout: {input}")).is_err(),
                "accepted {input}"
            );
        }
        rejects(
            &format!("version: 1\nlayout: {{group: splith, children: [{WINDOW}]}}"),
            "at least two",
        );
        rejects(
            &format!("version: 1\nlayout: {{group: splith, children: [{WINDOW}, {WINDOW}]}}"),
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
            "version: 1\nlayout: {group: splith, children: [{id: a, command: [foot], match: {app_id: foot}, weight: 1e308}, {id: b, command: [foot], match: {app_id: foot}, weight: 1e308}]}",
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
        for timeout in ["0", "300001", "18446744073709551615"] {
            rejects(
                &format!("version: 1\nlayout: {WINDOW}\nwindow_timeout_ms: {timeout}"),
                "from 1 to 300000",
            );
        }
        for timeout in [1, MAX_WINDOW_TIMEOUT_MS] {
            let input = format!("version: 1\nlayout: {WINDOW}\nwindow_timeout_ms: {timeout}");
            assert_eq!(parse_test(&input).unwrap().window_timeout_ms, timeout);
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
                "{{group: splith, children: [{node}, {{id: w{index}, command: [foot], match: {{app_id: foot}}}}]}}"
            );
        }
        format!("version: 1\nlayout: {node}")
    }

    #[test]
    fn bounds_layout_depth_and_parser_nesting() {
        assert!(parse_test(&nested_layout(MAX_LAYOUT_DEPTH)).is_ok());
        rejects(&nested_layout(MAX_LAYOUT_DEPTH + 1), "depth exceeds 32");
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
                "version: 1\nlayout: {{group: splith, children: [{}]}}",
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
            "version: 1\nlayout:\n  group: splith\n  children:\n    - {WINDOW}\n    - id: second\n      command: [foot]\n      match: {{app_id: foot}}\n      weight: 0\n"
        );
        let error = parse_test(&input).unwrap_err();
        assert!(error.contains("layout.children[1].weight"), "{error}");
        assert!(error.contains("line 6, column"), "{error}");
        let error = parse_test(&input.replace("weight: 0", "command: []")).unwrap_err();
        assert!(error.contains("layout.children[1]"), "{error}");
        assert!(error.contains("line"), "{error}");
    }
}
