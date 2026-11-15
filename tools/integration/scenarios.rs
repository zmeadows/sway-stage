//! Scenario definitions and the YAML the harness expects sway-stage to accept and
//! `capture` to print. Every scenario window is a Foot terminal.
//!
//! Configured layouts are the harness's own description of what sway-stage should build,
//! independent of the production `LayoutNode` and configuration parser. Definitions are
//! pure; `configure_process_execution` later adds the run's private paths.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::ipc::{InitialWorkspaceLayout, Layout};

/// sway-stage's default wait for each window.
pub const WINDOW_TIMEOUT_MS: u64 = 20_000;
/// How long each test terminal's `sleep` runs, bounding what an aborted run leaves behind.
const TERMINAL_LIFETIME_SECONDS: u32 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupLayout {
    /// Left to right.
    Horizontal,
    /// Top to bottom.
    Vertical,
    Tabbed,
    Stacked,
}

impl GroupLayout {
    /// The native Sway layout expected for this configured group.
    pub fn sway_layout(self) -> Layout {
        match self {
            GroupLayout::Horizontal => Layout::SplitH,
            GroupLayout::Vertical => Layout::SplitV,
            GroupLayout::Tabbed => Layout::Tabbed,
            GroupLayout::Stacked => Layout::Stacked,
        }
    }

    pub fn is_split(self) -> bool {
        matches!(self, Self::Horizontal | Self::Vertical)
    }

    fn yaml_value(self) -> &'static str {
        match self {
            GroupLayout::Horizontal => "splith",
            GroupLayout::Vertical => "splitv",
            GroupLayout::Tabbed => "tabbed",
            GroupLayout::Stacked => "stacked",
        }
    }
}

/// A configured group or window: what a scenario asks sway-stage to build. Distinct from
/// `ipc::TreeNode`, an observed Sway container.
#[derive(Clone, Debug)]
pub enum ConfiguredNode {
    Window(ConfiguredWindow),
    Group {
        layout: GroupLayout,
        /// One-based selection for tabbed/stacked groups; splits leave this at one.
        active: usize,
        /// Used only when this group is a child of a split.
        weight: u32,
        children: Vec<ConfiguredNode>,
    },
}

#[derive(Clone, Debug)]
pub struct ConfiguredWindow {
    /// `window1`, `window2`, ... in depth-first order, so it is also the ID `capture`
    /// assigns to this window.
    pub layout_id: String,
    /// Used only when this window is a child of a split.
    pub weight: u32,
    /// The executable, then literal arguments.
    pub command: Vec<String>,
    /// The exact native Wayland app ID that `match` selects this window by.
    pub app_id: String,
    /// Environment overrides for this window's process only, in configuration order.
    pub env: Vec<(String, String)>,
    /// Optional absolute YAML working directory.
    pub cwd: Option<String>,
    /// The app ID alone cannot attribute the window to this launch, so ownership also
    /// requires Sway's PID to show the expected application's literal arguments.
    pub requires_process_evidence: bool,
}

impl ConfiguredNode {
    /// Global focus takes precedence only in groups containing that configured window.
    pub fn selected_child_index(&self, focus: Option<&str>) -> usize {
        let Self::Group {
            active, children, ..
        } = self
        else {
            return 0;
        };
        children
            .iter()
            .position(|child| {
                child
                    .leaves()
                    .iter()
                    .any(|leaf| Some(leaf.layout_id.as_str()) == focus)
            })
            .unwrap_or(*active - 1)
    }

    pub fn weight(&self) -> u32 {
        match self {
            ConfiguredNode::Window(window) => window.weight,
            ConfiguredNode::Group { weight, .. } => *weight,
        }
    }

    /// Windows in depth-first order, which is both visual order and launch order.
    pub fn leaves(&self) -> Vec<&ConfiguredWindow> {
        match self {
            ConfiguredNode::Window(window) => vec![window],
            ConfiguredNode::Group { children, .. } => {
                children.iter().flat_map(ConfiguredNode::leaves).collect()
            }
        }
    }

    fn leaves_mut(&mut self) -> Vec<&mut ConfiguredWindow> {
        match self {
            ConfiguredNode::Window(window) => vec![window],
            ConfiguredNode::Group { children, .. } => children
                .iter_mut()
                .flat_map(ConfiguredNode::leaves_mut)
                .collect(),
        }
    }

    /// Every configured group has at least one child.
    pub fn first_leaf_mut(&mut self) -> &mut ConfiguredWindow {
        match self {
            ConfiguredNode::Window(window) => window,
            ConfiguredNode::Group { children, .. } => children[0].first_leaf_mut(),
        }
    }
}

/// One integration scenario: a layout for sway-stage to build, and what to verify.
#[derive(Clone)]
pub struct Scenario {
    pub name: &'static str,
    pub initial_workspace_layout: Option<InitialWorkspaceLayout>,
    pub layout: ConfiguredNode,
    /// Independent expected final focus, including when YAML omits focus.
    pub expected_focus: &'static str,
    /// Optional top-level focus field to write into the input YAML.
    pub configured_focus: Option<&'static str>,
    pub window_timeout_ms: u64,
}

impl Scenario {
    pub fn captures_and_reloads(&self) -> bool {
        matches!(
            self.name,
            "capture-reload-roundtrip"
                | "mixed-capture-reload"
                | "independent-native-groups"
                | "outside-focus-capture-reload"
        )
    }
}

/// The integration scenarios. `foot` and `sleep` are absolute executables found on `PATH`.
pub fn standard_scenarios(run_id: &str, foot: &str, sleep: &str) -> Vec<Scenario> {
    use GroupLayout::{Horizontal, Stacked, Tabbed, Vertical};

    // deep-nesting includes a meaningful horizontal group inside another horizontal group.
    let deep_column = group(Vertical, 3, vec![window(3, 3), window(4, 2)]);
    let deep_inner_row = group(Horizontal, 3, vec![window(2, 2), deep_column]);
    let deep_outer_row = group(Horizontal, 3, vec![window(1, 2), deep_inner_row]);
    let mixed_column = group(Vertical, 3, vec![window(2, 3), window(3, 2)]);

    let layouts = [
        ("single-window", window(1, 1), "window1"),
        (
            "horizontal-pair",
            group(Horizontal, 1, vec![window(1, 3), window(2, 2)]),
            "window2",
        ),
        (
            "vertical-pair",
            group(Vertical, 1, vec![window(1, 2), window(2, 3)]),
            "window1",
        ),
        (
            "uneven-siblings",
            group(
                Horizontal,
                1,
                vec![window(1, 5), window(2, 3), window(3, 2)],
            ),
            "window2",
        ),
        (
            "mixed-nesting",
            group(
                Horizontal,
                1,
                vec![window(1, 4), mixed_column, window(4, 3)],
            ),
            "window3",
        ),
        (
            "deep-nesting",
            group(Vertical, 1, vec![deep_outer_row, window(5, 2)]),
            "window4",
        ),
        (
            "sibling-nested-groups",
            group(
                Horizontal,
                1,
                vec![
                    group(Vertical, 3, vec![window(1, 2), window(2, 3)]),
                    group(Vertical, 2, vec![window(3, 3), window(4, 1)]),
                ],
            ),
            "window3",
        ),
        ("process-execution", window(1, 1), "window1"),
        (
            "default-focus",
            group(
                Horizontal,
                1,
                vec![
                    group(Vertical, 3, vec![window(1, 2), window(2, 3)]),
                    window(3, 2),
                ],
            ),
            "window1",
        ),
        (
            "occupied-workspace-refusal",
            group(Horizontal, 1, vec![window(1, 3), window(2, 2)]),
            "window2",
        ),
        (
            "capture-reload-roundtrip",
            group(
                Horizontal,
                1,
                vec![
                    window(1, 4),
                    group(Vertical, 3, vec![window(2, 3), window(3, 2)]),
                    window(4, 3),
                ],
            ),
            "window3",
        ),
        (
            "root-tabbed",
            group(Tabbed, 1, vec![window(1, 1), window(2, 1), window(3, 1)]),
            "window1",
        ),
        (
            "root-stacked",
            group(Stacked, 1, vec![window(1, 1), window(2, 1), window(3, 1)]),
            "window2",
        ),
        (
            "singleton-groups",
            group(
                Stacked,
                1,
                vec![group(Tabbed, 1, vec![window(1, 1)]), window(2, 1)],
            ),
            "window1",
        ),
        (
            "mixed-native-nesting",
            group(
                Horizontal,
                1,
                vec![
                    group(
                        Tabbed,
                        3,
                        vec![
                            window(1, 1),
                            group(Vertical, 1, vec![window(2, 3), window(3, 2)]),
                        ],
                    ),
                    window(4, 2),
                ],
            ),
            "window3",
        ),
        (
            "independent-native-groups",
            group(
                Horizontal,
                1,
                vec![
                    group_with_active(
                        Tabbed,
                        3,
                        2,
                        vec![
                            window(1, 1),
                            group_with_active(Stacked, 1, 2, vec![window(2, 1), window(3, 1)]),
                        ],
                    ),
                    group_with_active(
                        Stacked,
                        2,
                        2,
                        vec![
                            group_with_active(Tabbed, 1, 2, vec![window(4, 1), window(5, 1)]),
                            window(6, 1),
                        ],
                    ),
                ],
            ),
            "window3",
        ),
        (
            "mixed-capture-reload",
            group(
                Vertical,
                1,
                vec![
                    group(
                        Tabbed,
                        3,
                        vec![
                            group(Stacked, 1, vec![window(1, 1)]),
                            group(Horizontal, 1, vec![window(2, 3), window(3, 2)]),
                        ],
                    ),
                    window(4, 2),
                ],
            ),
            "window3",
        ),
    ];

    let mut scenarios = Vec::new();
    for (name, layout, focus) in layouts {
        let mut scenario = Scenario {
            name,
            initial_workspace_layout: None,
            layout,
            expected_focus: focus,
            configured_focus: Some(focus),
            window_timeout_ms: WINDOW_TIMEOUT_MS,
        };
        use_unique_foot_windows(&mut scenario, run_id, foot, sleep);
        if matches!(name, "default-focus" | "root-tabbed" | "singleton-groups") {
            scenario.configured_focus = None;
        }
        scenarios.push(scenario);
    }

    // Keep the original scenarios in order. Both additions build the same split-root
    // layout, so successful construction/cleanup leaves no special initial state to reset.
    let mixed = scenarios
        .iter()
        .find(|scenario| scenario.name == "mixed-native-nesting")
        .expect("standard scenarios include mixed native nesting")
        .clone();
    for (name, initial_layout) in [
        ("initially-tabbed", InitialWorkspaceLayout::Tabbed),
        ("initially-stacked", InitialWorkspaceLayout::Stacked),
    ] {
        let mut scenario = mixed.clone();
        scenario.name = name;
        scenario.initial_workspace_layout = Some(initial_layout);
        use_unique_foot_windows(&mut scenario, run_id, foot, sleep);
        scenarios.push(scenario);
    }

    let mut outside_focus = scenarios
        .iter()
        .find(|scenario| scenario.name == "mixed-capture-reload")
        .expect("standard scenarios include mixed capture/reload")
        .clone();
    outside_focus.name = "outside-focus-capture-reload";
    outside_focus.expected_focus = "window4";
    outside_focus.configured_focus = Some("window4");
    let ConfiguredNode::Group { children, .. } = &mut outside_focus.layout else {
        unreachable!("mixed capture has a split root")
    };
    let ConfiguredNode::Group { active, .. } = &mut children[0] else {
        unreachable!("mixed capture starts with a tabbed group")
    };
    *active = 2;
    use_unique_foot_windows(&mut outside_focus, run_id, foot, sleep);
    scenarios.push(outside_focus);

    // Both windows share one app ID, so only their distinct literal launch arguments tell
    // them apart, both as ownership evidence and in inferred commands.
    let mut duplicate_ids = scenarios
        .iter()
        .find(|scenario| scenario.name == "horizontal-pair")
        .expect("standard scenarios include horizontal pair")
        .clone();
    duplicate_ids.name = "duplicate-foot-ids";
    use_shared_foot_app_id(&mut duplicate_ids, run_id, foot, sleep);
    scenarios.push(duplicate_ids);
    scenarios
}

/// Foot's own app ID when no `--app-id` is given. Every window of the session round-trip
/// keeps it, as ordinary terminals do.
pub const FOOT_DEFAULT_APP_ID: &str = "foot";

/// The session round-trip's four layouts in profile order: three Foot terminals each in a
/// horizontal split, a vertical split, a tabbed group, and a stacked group, with the third
/// focused and, in the tabbed and stacked groups, selected. Its windows are first opened by
/// Sway itself on an empty workspace given `initial_workspace_layout`, so every split child
/// has an equal share; the captured profile then reloads them through sway-stage.
pub fn session_roundtrip_layouts(run_id: &str, foot: &str, sleep: &str) -> [Scenario; 4] {
    use GroupLayout::{Horizontal, Stacked, Tabbed, Vertical};

    let three_windows = || vec![window(1, 1), window(2, 1), window(3, 1)];
    let workspaces = [
        (
            "session-splith",
            InitialWorkspaceLayout::SplitH,
            group(Horizontal, 1, three_windows()),
        ),
        (
            "session-splitv",
            InitialWorkspaceLayout::SplitV,
            group(Vertical, 1, three_windows()),
        ),
        (
            "session-tabbed",
            InitialWorkspaceLayout::Tabbed,
            group_with_active(Tabbed, 1, 3, three_windows()),
        ),
        (
            "session-stacked",
            InitialWorkspaceLayout::Stacked,
            group_with_active(Stacked, 1, 3, three_windows()),
        ),
    ];

    workspaces.map(|(name, initial_layout, layout)| {
        let mut scenario = Scenario {
            name,
            initial_workspace_layout: Some(initial_layout),
            layout,
            expected_focus: "window3",
            configured_focus: Some("window3"),
            window_timeout_ms: WINDOW_TIMEOUT_MS,
        };
        use_default_foot_app_id(&mut scenario, run_id, foot, sleep);
        scenario
    })
}

/// One independently owned window kept outside every scenario's target workspace.
pub fn foreign_fixture_scenario(run_id: &str, foot: &str, sleep: &str) -> Scenario {
    let mut scenario = Scenario {
        name: "foreign-fixture",
        initial_workspace_layout: None,
        layout: window(1, 1),
        expected_focus: "window1",
        configured_focus: Some("window1"),
        window_timeout_ms: WINDOW_TIMEOUT_MS,
    };
    use_unique_foot_windows(&mut scenario, run_id, foot, sleep);
    // The whole suite can outlast ordinary scenario terminals. Still bound what an aborted
    // run leaves behind; disappearance at any point fails preservation.
    let leaf = scenario.layout.first_leaf_mut();
    *leaf
        .command
        .last_mut()
        .expect("Foot command ends with sleep duration") = "3600".into();
    scenario
}

/// A leaf named `window{number}`. Its command and app ID are placeholders that the
/// scenario definition replaces before returning the scenario.
fn window(number: u32, weight: u32) -> ConfiguredNode {
    ConfiguredNode::Window(ConfiguredWindow {
        layout_id: format!("window{number}"),
        weight,
        command: Vec::new(),
        app_id: String::new(),
        env: Vec::new(),
        cwd: None,
        requires_process_evidence: false,
    })
}

fn group(layout: GroupLayout, weight: u32, children: Vec<ConfiguredNode>) -> ConfiguredNode {
    group_with_active(layout, weight, 1, children)
}

fn group_with_active(
    layout: GroupLayout,
    weight: u32,
    active: usize,
    children: Vec<ConfiguredNode>,
) -> ConfiguredNode {
    ConfiguredNode::Group {
        layout,
        active,
        weight,
        children,
    }
}

/// Makes every leaf a Foot window with a run-, scenario-, and leaf-specific app ID.
fn use_unique_foot_windows(scenario: &mut Scenario, run_id: &str, foot: &str, sleep: &str) {
    for leaf in scenario.layout.leaves_mut() {
        let app_id = format!(
            "sway-stage-it-{run_id}-{}-{}",
            scenario.name, leaf.layout_id
        );
        leaf.command = foot_command(foot, Some(&app_id), None, sleep);
        leaf.app_id = app_id;
    }
}

/// Gives every Foot window one app ID. A distinct literal `--title` argument per leaf is
/// independent launch evidence even though the app IDs are identical; the title Sway
/// reports is never an ownership key.
fn use_shared_foot_app_id(scenario: &mut Scenario, run_id: &str, foot: &str, sleep: &str) {
    let shared_app_id = format!("sway-stage-it-{run_id}-duplicate-foot");
    for leaf in scenario.layout.leaves_mut() {
        let title = format!("{shared_app_id}-{}", leaf.layout_id);
        leaf.command = foot_command(foot, Some(&shared_app_id), Some(&title), sleep);
        leaf.app_id = shared_app_id.clone();
        leaf.requires_process_evidence = true;
    }
}

/// Leaves every window with Foot's default app ID, which this run's other windows never
/// use. As with a shared app ID, a distinct literal `--title` argument per leaf, unique to
/// the run, scenario, and leaf, is the independent launch evidence.
fn use_default_foot_app_id(scenario: &mut Scenario, run_id: &str, foot: &str, sleep: &str) {
    for leaf in scenario.layout.leaves_mut() {
        let title = format!(
            "sway-stage-it-{run_id}-{}-{}",
            scenario.name, leaf.layout_id
        );
        leaf.command = foot_command(foot, None, Some(&title), sleep);
        leaf.app_id = FOOT_DEFAULT_APP_ID.to_owned();
        leaf.requires_process_evidence = true;
    }
}

/// Foot's documented syntax is `foot [OPTIONS] command [ARGUMENTS]`. A bounded `sleep`
/// child rather than a shell leaves nothing interactive behind after an aborted run.
/// Without `app_id`, Foot uses `FOOT_DEFAULT_APP_ID`.
fn foot_command(foot: &str, app_id: Option<&str>, title: Option<&str>, sleep: &str) -> Vec<String> {
    let mut command = vec![foot.to_owned()];
    if let Some(app_id) = app_id {
        command.push(format!("--app-id={app_id}"));
    }
    if let Some(title) = title {
        command.push(format!("--title={title}"));
    }
    command.push(sleep.to_owned());
    command.push(TERMINAL_LIFETIME_SECONDS.to_string());
    command
}

/// Configuration files are UTF-8, so every path written into one must be too.
fn utf8(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))
}

/// The scenario as a sway-stage configuration file.
pub fn configuration_yaml(scenario: &Scenario) -> String {
    render_yaml(scenario, &Rendering::Configuration)
        .expect("a configuration looks up no measured shares")
}

/// Layout context for a failed verification, without launch arguments or environment.
pub fn diagnostic_yaml(scenario: &Scenario) -> String {
    render_yaml(scenario, &Rendering::Diagnostic).expect("diagnostics look up no measured shares")
}

/// What `capture` must print for the scenario once its arrangement is verified, apart from
/// comments: measured shares as weights, null commands, and no timeout or environment.
/// `measured_shares` maps children of splits to their measured percentages.
pub fn expected_draft(
    scenario: &Scenario,
    measured_shares: &BTreeMap<Vec<usize>, f64>,
) -> Result<String, String> {
    render_yaml(
        scenario,
        &Rendering::Draft {
            measured_shares,
            focus: scenario.expected_focus,
        },
    )
}

enum Rendering<'a> {
    Configuration,
    Diagnostic,
    Draft {
        measured_shares: &'a BTreeMap<Vec<usize>, f64>,
        focus: &'a str,
    },
}

/// One fixed layout of sway-stage's format; capture's serializer uses the same field
/// order and indentation, so a draft can be compared with it line by line.
fn render_yaml(scenario: &Scenario, rendering: &Rendering) -> Result<String, String> {
    let mut lines = vec!["version: 1".to_owned()];
    let focus = if matches!(rendering, Rendering::Configuration) {
        scenario.configured_focus
    } else {
        Some(scenario.expected_focus)
    };
    if let Some(focus) = focus {
        lines.push(format!("focus: {focus}"));
    }
    if !matches!(rendering, Rendering::Draft { .. }) {
        lines.push(format!("window_timeout_ms: {}", scenario.window_timeout_ms));
    }
    lines.push("layout:".to_owned());

    // The root is the `layout` mapping itself, indented under its key.
    emit_node(
        &scenario.layout,
        2,
        &mut Vec::new(),
        false,
        false,
        rendering,
        &mut lines,
    )?;
    Ok(lines.join("\n") + "\n")
}

/// Appends `node`'s fields at `indent` columns, as a sequence item when `in_sequence`, then
/// its children. Only children of split parents emit weights, including group children.
fn emit_node(
    node: &ConfiguredNode,
    indent: usize,
    path: &mut Vec<usize>,
    in_sequence: bool,
    weighted: bool,
    rendering: &Rendering,
    lines: &mut Vec<String>,
) -> Result<(), String> {
    let mut fields = Vec::new();
    match node {
        ConfiguredNode::Window(window) => fields.push(format!("id: {}", window.layout_id)),
        ConfiguredNode::Group { layout, .. } => {
            fields.push(format!("group: {}", layout.yaml_value()))
        }
    }

    if weighted {
        let weight = match rendering {
            Rendering::Configuration | Rendering::Diagnostic => node.weight().to_string(),
            Rendering::Draft {
                measured_shares, ..
            } => measured_shares
                .get(path.as_slice())
                .ok_or_else(|| format!("no measured share for {}", describe_path(path)))?
                .to_string(),
        };
        fields.push(format!("weight: {weight}"));
    }

    match node {
        ConfiguredNode::Window(window) => {
            let command = match rendering {
                Rendering::Configuration => json_sequence(&window.command),
                Rendering::Diagnostic | Rendering::Draft { .. } => "null".to_owned(),
            };
            fields.push(format!("command: {command}"));
            fields.push("match:".to_owned());
            fields.push(format!("  app_id: {}", window.app_id));
            if matches!(rendering, Rendering::Configuration)
                && let Some(cwd) = &window.cwd
            {
                fields.push(format!("cwd: {}", json_string(cwd)));
            }
            if matches!(rendering, Rendering::Configuration) && !window.env.is_empty() {
                fields.push(format!("env: {}", json_mapping(&window.env)));
            }
        }
        ConfiguredNode::Group { layout, active, .. } => {
            let selected_child = match rendering {
                Rendering::Draft { focus, .. } => node.selected_child_index(Some(focus)) + 1,
                _ => *active,
            };
            if !layout.is_split() && selected_child > 1 {
                fields.push(format!("active: {selected_child}"));
            }
            fields.push("children:".to_owned());
        }
    }

    for (field_index, field) in fields.iter().enumerate() {
        let marker = match (in_sequence, field_index) {
            (false, _) => "",
            (true, 0) => "- ",
            (true, _) => "  ",
        };
        lines.push(format!("{}{marker}{field}", " ".repeat(indent)));
    }

    if let ConfiguredNode::Group {
        layout, children, ..
    } = node
    {
        let child_indent = indent + if in_sequence { 4 } else { 2 };
        for (child_index, child) in children.iter().enumerate() {
            path.push(child_index);
            emit_node(
                child,
                child_indent,
                path,
                true,
                layout.is_split(),
                rendering,
                lines,
            )?;
            path.pop();
        }
    }
    Ok(())
}

/// Names a configured node by its child-index path in YAML field-path style:
/// `layout`, or `layout.children[2].children[0]`.
pub fn describe_path(path: &[usize]) -> String {
    let mut description = String::from("layout");
    for child_index in path {
        description.push_str(&format!(".children[{child_index}]"));
    }
    description
}

/// A JSON array, which is also a YAML flow sequence, with `", "` between items.
pub fn json_sequence(items: &[String]) -> String {
    let items: Vec<String> = items.iter().map(|item| json_string(item)).collect();
    format!("[{}]", items.join(", "))
}

/// A JSON object, which is also a YAML flow mapping, with `", "` between entries.
fn json_mapping(entries: &[(String, String)]) -> String {
    let entries: Vec<String> = entries
        .iter()
        .map(|(key, value)| format!("{}: {}", json_string(key), json_string(value)))
        .collect();
    format!("{{{}}}", entries.join(", "))
}

/// YAML double-quoted scalars accept every JSON string escape.
fn json_string(text: &str) -> String {
    Value::from(text).to_string()
}

/// Prepare the process scenario before validation. The helper is this harness executable,
/// invoked by Foot directly; no shell parses the deliberately awkward literal arguments.
pub fn configure_process_execution(
    scenario: &mut Scenario,
    helper: &Path,
    process_directory: &Path,
    run_id: &str,
) -> Result<(), String> {
    if scenario.name != "process-execution" {
        return Ok(());
    }
    let leaf = scenario.layout.first_leaf_mut();
    leaf.command.truncate(2);
    leaf.command.extend([
        utf8(helper)?.to_owned(),
        crate::process::PROBE_FLAG.into(),
        format!("{run_id} two words"),
        format!("{run_id} $HOME ; $(literal) * ' \""),
        String::new(),
    ]);
    leaf.cwd = Some(utf8(process_directory)?.to_owned());
    leaf.env = vec![(
        format!("SWAY_STAGE_IT_{run_id}"),
        format!("{run_id} override"),
    )];
    leaf.requires_process_evidence = true;
    Ok(())
}

#[cfg(test)]
#[path = "scenarios_tests.rs"]
mod tests;
