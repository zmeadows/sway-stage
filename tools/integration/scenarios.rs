//! Scenario definitions, preparation of the optional extended scenarios, and the YAML the
//! harness expects sway-stage to accept and `capture` to print.
//!
//! Configured layouts are the harness's own description of what sway-stage should build,
//! independent of the production `LayoutNode` and configuration parser. Definitions are
//! pure; only `prepare_extended` inspects the environment, creates files, or runs a probe.

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, DirBuilder, File};
use std::os::unix::fs::DirBuilderExt;
use std::path::{self, Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::ipc::{IPC_TIMEOUT, Layout};
use crate::process;

/// sway-stage's default wait for each window.
pub const WINDOW_TIMEOUT_MS: u64 = 20_000;
/// A layout has one timeout for every leaf, so Foot windows beside Firefox share this one.
pub const FIREFOX_TIMEOUT_MS: u64 = 60_000;
/// How long each test terminal's `sleep` runs, bounding what an aborted run leaves behind.
const TERMINAL_LIFETIME_SECONDS: u32 = 300;

/// Keeps a fresh Firefox profile from opening first-run and default-browser prompts.
const FIREFOX_PREFERENCES: &str = r#"user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("startup.homepage_welcome_url", "");
user_pref("startup.homepage_welcome_url.additional", "");
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitAxis {
    /// Left to right.
    Horizontal,
    /// Top to bottom.
    Vertical,
}

impl SplitAxis {
    /// The Sway layout of a container that splits along this axis.
    pub fn sway_layout(self) -> Layout {
        match self {
            SplitAxis::Horizontal => Layout::SplitH,
            SplitAxis::Vertical => Layout::SplitV,
        }
    }

    fn yaml_value(self) -> &'static str {
        match self {
            SplitAxis::Horizontal => "horizontal",
            SplitAxis::Vertical => "vertical",
        }
    }
}

/// An exact window selector: a native Wayland app ID or an Xwayland `WM_CLASS` class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selector {
    AppId(String),
    Class(String),
}

impl Selector {
    /// The selector's YAML key, which also names it in diagnostics.
    pub fn kind(&self) -> &'static str {
        match self {
            Selector::AppId(_) => "app_id",
            Selector::Class(_) => "class",
        }
    }

    pub fn value(&self) -> &str {
        match self {
            Selector::AppId(value) | Selector::Class(value) => value,
        }
    }
}

/// A configured split or window: what a scenario asks sway-stage to build. Distinct from
/// `ipc::TreeNode`, an observed Sway container.
#[derive(Debug)]
pub enum ConfiguredNode {
    Window(ConfiguredWindow),
    Split {
        axis: SplitAxis,
        weight: u32,
        children: Vec<ConfiguredNode>,
    },
}

#[derive(Debug)]
pub struct ConfiguredWindow {
    /// `window1`, `window2`, ... in depth-first order, so it is also the ID `capture`
    /// assigns to this window.
    pub layout_id: String,
    pub weight: u32,
    /// The executable, then literal arguments.
    pub command: Vec<String>,
    pub selector: Selector,
    /// Environment overrides for this window's process only, in configuration order.
    pub env: Vec<(String, String)>,
    /// The selector alone cannot attribute the window to this launch, so ownership also
    /// requires Sway's PID to show this command's literal arguments.
    pub requires_process_evidence: bool,
}

impl ConfiguredNode {
    pub fn weight(&self) -> u32 {
        match self {
            ConfiguredNode::Window(window) => window.weight,
            ConfiguredNode::Split { weight, .. } => *weight,
        }
    }

    /// Windows in depth-first order, which is both visual order and launch order.
    pub fn leaves(&self) -> Vec<&ConfiguredWindow> {
        match self {
            ConfiguredNode::Window(window) => vec![window],
            ConfiguredNode::Split { children, .. } => {
                children.iter().flat_map(ConfiguredNode::leaves).collect()
            }
        }
    }

    fn leaves_mut(&mut self) -> Vec<&mut ConfiguredWindow> {
        match self {
            ConfiguredNode::Window(window) => vec![window],
            ConfiguredNode::Split { children, .. } => children
                .iter_mut()
                .flat_map(ConfiguredNode::leaves_mut)
                .collect(),
        }
    }

    /// Every scenario's splits have at least two children.
    pub fn first_leaf_mut(&mut self) -> &mut ConfiguredWindow {
        match self {
            ConfiguredNode::Window(window) => window,
            ConfiguredNode::Split { children, .. } => children[0].first_leaf_mut(),
        }
    }
}

/// One integration scenario: a layout for sway-stage to build, and what to verify.
pub struct Scenario {
    pub name: &'static str,
    pub layout: ConfiguredNode,
    /// The layout ID of the window sway-stage must focus last, always written explicitly.
    pub focus: &'static str,
    pub window_timeout_ms: u64,
    /// What an extended scenario tests beyond plain Foot windows; `None` for standard ones.
    pub extension: Option<Extension>,
    /// Set by `prepare_extended` when an optional application or subsystem is unavailable.
    pub skip: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Extension {
    /// The first window is a native Wayland Firefox with a private, retained profile.
    Firefox,
    /// The first window is an Xterm under Xwayland, matched by class.
    Xterm,
    /// Foot windows share one app ID, distinguished only by launch arguments.
    DuplicateFootIds,
}

/// The six Foot-only scenarios. `foot` and `sleep` are resolved absolute executables.
pub fn standard_scenarios(run_id: &str, foot: &str, sleep: &str) -> Vec<Scenario> {
    use SplitAxis::{Horizontal, Vertical};

    // deep-nesting includes a meaningful horizontal group inside another horizontal group.
    let deep_column = split(Vertical, 3, vec![window(3, 3), window(4, 2)]);
    let deep_inner_row = split(Horizontal, 3, vec![window(2, 2), deep_column]);
    let deep_outer_row = split(Horizontal, 3, vec![window(1, 2), deep_inner_row]);
    let mixed_column = split(Vertical, 3, vec![window(2, 3), window(3, 2)]);

    let layouts = [
        ("single-window", window(1, 1), "window1"),
        (
            "horizontal-pair",
            split(Horizontal, 1, vec![window(1, 3), window(2, 2)]),
            "window2",
        ),
        (
            "vertical-pair",
            split(Vertical, 1, vec![window(1, 2), window(2, 3)]),
            "window1",
        ),
        (
            "uneven-siblings",
            split(
                Horizontal,
                1,
                vec![window(1, 5), window(2, 3), window(3, 2)],
            ),
            "window2",
        ),
        (
            "mixed-nesting",
            split(
                Horizontal,
                1,
                vec![window(1, 4), mixed_column, window(4, 3)],
            ),
            "window3",
        ),
        (
            "deep-nesting",
            split(Vertical, 1, vec![deep_outer_row, window(5, 2)]),
            "window4",
        ),
    ];

    let mut scenarios = Vec::new();
    for (name, layout, focus) in layouts {
        let mut scenario = Scenario {
            name,
            layout,
            focus,
            window_timeout_ms: WINDOW_TIMEOUT_MS,
            extension: None,
            skip: None,
        };
        use_unique_foot_windows(&mut scenario, run_id, foot, sleep);
        scenarios.push(scenario);
    }
    scenarios
}

/// The four optional scenarios, each to be passed to `prepare_extended` immediately before
/// it runs. Until then, Firefox and Xterm leaves are ordinary Foot windows.
pub fn extended_scenarios(run_id: &str, foot: &str, sleep: &str) -> Vec<Scenario> {
    use SplitAxis::{Horizontal, Vertical};

    let firefox_column = split(Vertical, 24, vec![window(3, 77), window(4, 23)]);

    let layouts = [
        (
            "firefox-two-foot",
            split(
                Horizontal,
                1,
                vec![window(1, 44), window(2, 32), window(3, 24)],
            ),
            "window2",
            Extension::Firefox,
        ),
        (
            "xterm-foot",
            split(Horizontal, 1, vec![window(1, 3), window(2, 2)]),
            "window2",
            Extension::Xterm,
        ),
        (
            "nested-firefox-foot",
            split(
                Horizontal,
                1,
                vec![window(1, 44), window(2, 32), firefox_column],
            ),
            "window4",
            Extension::Firefox,
        ),
        (
            "duplicate-foot-ids",
            split(Horizontal, 1, vec![window(1, 3), window(2, 2)]),
            "window2",
            Extension::DuplicateFootIds,
        ),
    ];

    let mut scenarios = Vec::new();
    for (name, layout, focus, extension) in layouts {
        let window_timeout_ms = match extension {
            Extension::Firefox => FIREFOX_TIMEOUT_MS,
            Extension::Xterm | Extension::DuplicateFootIds => WINDOW_TIMEOUT_MS,
        };
        let mut scenario = Scenario {
            name,
            layout,
            focus,
            window_timeout_ms,
            extension: Some(extension),
            skip: None,
        };
        match extension {
            Extension::DuplicateFootIds => {
                use_shared_foot_app_id(&mut scenario, run_id, foot, sleep)
            }
            Extension::Firefox | Extension::Xterm => {
                use_unique_foot_windows(&mut scenario, run_id, foot, sleep)
            }
        }
        scenarios.push(scenario);
    }
    scenarios
}

/// A leaf named `window{number}`. Its command and selector are placeholders that the
/// scenario definition replaces before returning the scenario.
fn window(number: u32, weight: u32) -> ConfiguredNode {
    ConfiguredNode::Window(ConfiguredWindow {
        layout_id: format!("window{number}"),
        weight,
        command: Vec::new(),
        selector: Selector::AppId(String::new()),
        env: Vec::new(),
        requires_process_evidence: false,
    })
}

fn split(axis: SplitAxis, weight: u32, children: Vec<ConfiguredNode>) -> ConfiguredNode {
    ConfiguredNode::Split {
        axis,
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
        leaf.command = foot_command(foot, &app_id, None, sleep);
        leaf.selector = Selector::AppId(app_id);
    }
}

/// Gives every Foot window one app ID. A distinct literal `--title` argument per leaf is
/// independent launch evidence even though the selectors are identical; the title Sway
/// reports is never an ownership key.
fn use_shared_foot_app_id(scenario: &mut Scenario, run_id: &str, foot: &str, sleep: &str) {
    let shared_app_id = format!("sway-stage-it-{run_id}-duplicate-foot");
    for leaf in scenario.layout.leaves_mut() {
        let title = format!("{shared_app_id}-{}", leaf.layout_id);
        leaf.command = foot_command(foot, &shared_app_id, Some(&title), sleep);
        leaf.selector = Selector::AppId(shared_app_id.clone());
        leaf.requires_process_evidence = true;
    }
}

/// Foot's documented syntax is `foot [OPTIONS] command [ARGUMENTS]`. A bounded `sleep`
/// child rather than a shell leaves nothing interactive behind after an aborted run.
fn foot_command(foot: &str, app_id: &str, title: Option<&str>, sleep: &str) -> Vec<String> {
    let mut command = vec![foot.to_owned(), format!("--app-id={app_id}")];
    if let Some(title) = title {
        command.push(format!("--title={title}"));
    }
    command.push(sleep.to_owned());
    command.push(TERMINAL_LIFETIME_SECONDS.to_string());
    command
}

/// Optional applications for the extended scenarios, looked up once per run. Paths are
/// kept as found on `PATH`, unresolved, so a wrapper or multi-call link still works.
pub struct OptionalApplications {
    /// `firefox-esr` if installed, otherwise `firefox`.
    firefox: Option<PathBuf>,
    /// An exact native app ID to expect instead of the installation's `RemotingName`.
    firefox_app_id_override: Option<String>,
    xterm: Option<PathBuf>,
}

impl OptionalApplications {
    pub fn find(firefox_app_id_override: Option<String>) -> Self {
        let firefox =
            process::find_on_path("firefox-esr").or_else(|| process::find_on_path("firefox"));
        Self {
            firefox,
            firefox_app_id_override,
            xterm: process::find_on_path("xterm"),
        }
    }
}

/// Decides whether an extended scenario can run and configures its optional application.
/// Call it immediately before the scenario runs. An unavailable application or subsystem
/// sets `scenario.skip`; an error means a failure with an available application.
///
/// Firefox preparation creates a private profile directory, pushes it onto
/// `retained_profiles` as soon as it exists, and prints its path; it is never removed. The
/// Xterm check runs a bounded, read-only X query.
pub fn prepare_extended(
    scenario: &mut Scenario,
    run_id: &str,
    sleep: &str,
    applications: &OptionalApplications,
    retained_profiles: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let Some(extension) = scenario.extension else {
        return Ok(());
    };

    // Firefox and repeated Foot app IDs need process arguments as ownership evidence.
    let needs_process_evidence = extension != Extension::Xterm;
    if needs_process_evidence && File::open("/proc/self/cmdline").is_err() {
        scenario.skip =
            Some("Linux /proc process arguments unavailable for ownership checks".into());
        return Ok(());
    }

    match extension {
        Extension::Firefox => {
            let Some(firefox) = &applications.firefox else {
                scenario.skip = Some("neither firefox-esr nor firefox is installed".into());
                return Ok(());
            };
            if !environment_is_set("WAYLAND_DISPLAY") {
                scenario.skip = Some("native Wayland display unavailable".into());
                return Ok(());
            }

            let temporary_directory = path::absolute(env::temp_dir())
                .map_err(|error| format!("locate the temporary directory: {error}"))?;
            // The run ID makes the name unique; creation still refuses any existing path.
            let profile_root =
                temporary_directory.join(format!("sway-stage-firefox-{run_id}-{}", scenario.name));
            configure_firefox(
                scenario.layout.first_leaf_mut(),
                firefox,
                applications.firefox_app_id_override.as_deref(),
                &profile_root,
                retained_profiles,
            )
        }
        Extension::Xterm => {
            let Some(xterm) = &applications.xterm else {
                scenario.skip = Some("xterm is not installed".into());
                return Ok(());
            };
            if let Some(reason) = xwayland_unavailable() {
                scenario.skip = Some(reason);
                return Ok(());
            }

            let window_class = format!("sway-stage-it-{run_id}-xterm");
            let leaf = scenario.layout.first_leaf_mut();
            // Xterm's -class sets WM_CLASS's class, not its instance. -e must come last: it
            // takes every remaining argument as the command.
            leaf.command = vec![
                utf8(xterm)?.to_owned(),
                "-class".into(),
                window_class.clone(),
                "+hold".into(),
                "-e".into(),
                sleep.to_owned(),
                TERMINAL_LIFETIME_SECONDS.to_string(),
            ];
            leaf.selector = Selector::Class(window_class);
            Ok(())
        }
        // Configured with the scenario: it needs nothing beyond Foot and /proc.
        Extension::DuplicateFootIds => Ok(()),
    }
}

/// Makes `leaf` a native Wayland Firefox window whose profile, HOME, and XDG directories
/// live under `profile_root`, a new private directory. The directory is retained: closing
/// a window cannot prove that Firefox's other processes have finished using it.
fn configure_firefox(
    leaf: &mut ConfiguredWindow,
    firefox: &Path,
    app_id_override: Option<&str>,
    profile_root: &Path,
    retained_profiles: &mut Vec<PathBuf>,
) -> Result<(), String> {
    // Paths go into a UTF-8 configuration file; check them before creating anything.
    let executable = utf8(firefox)?;
    let root = utf8(profile_root)?;

    // Fails if anything, including a symlink, already exists at the path.
    DirBuilder::new()
        .mode(0o700)
        .create(profile_root)
        .map_err(|error| format!("create Firefox directory {root}: {error}"))?;
    retained_profiles.push(profile_root.to_owned());
    println!(
        "      Firefox files retained at {root} (remove manually after all its processes exit)"
    );

    let profile = format!("{root}/profile");
    fs::create_dir(&profile).map_err(|error| format!("create {profile}: {error}"))?;
    fs::write(format!("{profile}/user.js"), FIREFOX_PREFERENCES)
        .map_err(|error| format!("write {profile}/user.js: {error}"))?;

    let app_id = match app_id_override {
        Some(app_id) => app_id.to_owned(),
        None => installed_remoting_name(firefox)?,
    };
    if !is_simple_identifier(&app_id) {
        return Err("Firefox app ID must be a simple literal identifier".into());
    }
    println!("      Firefox executable={executable} expected app_id={app_id}");

    let env = [
        ("HOME", root.to_owned()),
        ("XDG_CONFIG_HOME", format!("{root}/config")),
        ("XDG_CACHE_HOME", format!("{root}/cache")),
        ("XDG_DATA_HOME", format!("{root}/data")),
        ("XDG_STATE_HOME", format!("{root}/state")),
        ("MOZ_ENABLE_WAYLAND", "1".into()),
        ("GDK_BACKEND", "wayland".into()),
        ("XDG_ACTIVATION_TOKEN", String::new()),
        ("DESKTOP_STARTUP_ID", String::new()),
        // Inherited Firefox restart paths must not override the explicit private profile.
        ("XRE_PROFILE_PATH", String::new()),
        ("XRE_PROFILE_LOCAL_PATH", String::new()),
        ("XRE_PROFILE_NAME", String::new()),
    ];
    leaf.env = env
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect();

    // --new-instance is needed too: newer Firefox versions ignore --no-remote.
    leaf.command = [
        executable,
        "--no-remote",
        "--new-instance",
        "--profile",
        &profile,
        "--new-window",
        "about:blank",
    ]
    .map(str::to_owned)
    .to_vec();
    leaf.selector = Selector::AppId(app_id);
    leaf.requires_process_evidence = true;
    Ok(())
}

/// The native Wayland app ID of a Firefox installation, including `firefox-esr`: the
/// `[App]` `RemotingName` in the `application.ini` beside the resolved executable.
fn installed_remoting_name(firefox: &Path) -> Result<String, String> {
    let resolved = fs::canonicalize(firefox)
        .map_err(|error| format!("resolve {}: {error}", firefox.display()))?;
    let metadata = resolved
        .parent()
        .unwrap_or(Path::new("/"))
        .join("application.ini");
    let undetermined = |detail: &str| {
        format!(
            "cannot determine Firefox Wayland app ID from {}{detail}; \
             supply --firefox-app-id for this installation",
            metadata.display()
        )
    };

    let text =
        fs::read_to_string(&metadata).map_err(|error| undetermined(&format!(": {error}")))?;
    remoting_name(&text)
        .map_err(|error| undetermined(&format!(": {error}")))?
        .ok_or_else(|| undetermined(""))
}

/// Reads `RemotingName` from the `[App]` section of `application.ini` text. As in Python's
/// configparser, keys are case-insensitive and full-line `#` or `;` comments are ignored.
/// A malformed line or repeated key in that section is an error rather than a guess.
fn remoting_name(application_ini: &str) -> Result<Option<String>, String> {
    let mut in_app_section = false;
    let mut remoting_name = None;
    for line in application_ini.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(section) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            in_app_section = section == "App";
            continue;
        }
        if !in_app_section {
            continue;
        }

        let Some(delimiter_index) = line.find(['=', ':']) else {
            return Err("malformed line in [App]".into());
        };
        let key = line[..delimiter_index].trim();
        let value = line[delimiter_index + 1..].trim();
        if key.eq_ignore_ascii_case("RemotingName")
            && remoting_name.replace(value.to_owned()).is_some()
        {
            return Err("RemotingName is repeated in [App]".into());
        }
    }
    Ok(remoting_name)
}

/// `[A-Za-z][A-Za-z0-9_.-]*`: safe to write unquoted into YAML and to compare exactly.
fn is_simple_identifier(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
        })
}

/// Why Xterm cannot run under the session's Xwayland, if it cannot. The read-only X query
/// distinguishes a disabled or unreachable server from an Xterm launch failure, which must
/// still fail rather than become a skip.
fn xwayland_unavailable() -> Option<String> {
    if process::find_on_path("Xwayland").is_none() || !environment_is_set("DISPLAY") {
        return Some("Xwayland executable or DISPLAY unavailable".into());
    }
    let Some(xdpyinfo) = process::find_on_path("xdpyinfo") else {
        return Some("xdpyinfo unavailable (needed for the read-only Xwayland probe)".into());
    };

    let mut query = Command::new(xdpyinfo);
    query.arg("-queryExtensions");
    match process::run(query, "Xwayland availability probe", IPC_TIMEOUT) {
        Err(error) => Some(error),
        Ok(extensions) if !extensions.contains("XWAYLAND") => {
            Some("DISPLAY does not advertise the XWAYLAND extension".into())
        }
        Ok(_) => None,
    }
}

fn environment_is_set(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| !value.is_empty())
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
/// `measured_shares` maps each non-root node's child-index path to its percentage.
pub fn expected_draft(
    scenario: &Scenario,
    measured_shares: &BTreeMap<Vec<usize>, f64>,
) -> Result<String, String> {
    render_yaml(scenario, &Rendering::Draft(measured_shares))
}

enum Rendering<'a> {
    Configuration,
    Diagnostic,
    Draft(&'a BTreeMap<Vec<usize>, f64>),
}

/// One fixed layout of sway-stage's format; capture's serializer uses the same field
/// order and indentation, so a draft can be compared with it line by line.
fn render_yaml(scenario: &Scenario, rendering: &Rendering) -> Result<String, String> {
    let mut lines = vec![
        "version: 1".to_owned(),
        format!("focus: {}", scenario.focus),
    ];
    if !matches!(rendering, Rendering::Draft(_)) {
        lines.push(format!("window_timeout_ms: {}", scenario.window_timeout_ms));
    }
    lines.push("layout:".to_owned());

    // The root is the `layout` mapping itself, indented under its key.
    emit_node(
        &scenario.layout,
        2,
        &mut Vec::new(),
        false,
        rendering,
        &mut lines,
    )?;
    Ok(lines.join("\n") + "\n")
}

/// Appends `node`'s fields at `indent` columns, as a sequence item when `in_sequence`, then
/// its children. `path` holds child indexes from the root, which alone has no weight.
fn emit_node(
    node: &ConfiguredNode,
    indent: usize,
    path: &mut Vec<usize>,
    in_sequence: bool,
    rendering: &Rendering,
    lines: &mut Vec<String>,
) -> Result<(), String> {
    let mut fields = Vec::new();
    match node {
        ConfiguredNode::Window(window) => fields.push(format!("id: {}", window.layout_id)),
        ConfiguredNode::Split { axis, .. } => fields.push(format!("split: {}", axis.yaml_value())),
    }

    if !path.is_empty() {
        let weight = match rendering {
            Rendering::Configuration | Rendering::Diagnostic => node.weight().to_string(),
            Rendering::Draft(measured_shares) => measured_shares
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
                Rendering::Diagnostic | Rendering::Draft(_) => "null".to_owned(),
            };
            fields.push(format!("command: {command}"));
            fields.push("match:".to_owned());
            fields.push(format!(
                "  {}: {}",
                window.selector.kind(),
                window.selector.value()
            ));
            if matches!(rendering, Rendering::Configuration) && !window.env.is_empty() {
                fields.push(format!("env: {}", json_mapping(&window.env)));
            }
        }
        ConfiguredNode::Split { .. } => fields.push("children:".to_owned()),
    }

    for (field_index, field) in fields.iter().enumerate() {
        let marker = match (in_sequence, field_index) {
            (false, _) => "",
            (true, 0) => "- ",
            (true, _) => "  ",
        };
        lines.push(format!("{}{marker}{field}", " ".repeat(indent)));
    }

    if let ConfiguredNode::Split { children, .. } = node {
        let child_indent = indent + if in_sequence { 4 } else { 2 };
        for (child_index, child) in children.iter().enumerate() {
            path.push(child_index);
            emit_node(child, child_indent, path, true, rendering, lines)?;
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

/// A JSON array, which is also a YAML flow sequence, spaced like Python's `json.dumps`.
fn json_sequence(items: &[String]) -> String {
    let items: Vec<String> = items.iter().map(|item| json_string(item)).collect();
    format!("[{}]", items.join(", "))
}

/// A JSON object, which is also a YAML flow mapping, spaced like Python's `json.dumps`.
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

#[cfg(test)]
#[path = "scenarios_tests.rs"]
mod tests;
