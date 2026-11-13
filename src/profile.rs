use crate::diagnostics::Diagnostics;
use crate::observation::{self, Observer, WorkspaceSnapshot};
use crate::{config, discovery, launch};
use serde::Deserialize;
use serde_saphyr::Spanned;
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Instant;
use swayipc::{Event, NodeType, WorkspaceChange};

// Match the layout node limit: profiles are bounded plans for interactive construction.
const MAX_WORKSPACES: usize = 256;
const MAX_WORKSPACE_NUMBER: u64 = 999_999_999;

// A single quoted Sway argument preserves spaces. This allowlist deliberately excludes
// quoting, separators, escapes, and variable syntax; it is not a general command encoder.
const MAX_WORKSPACE_NAME_BYTES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(
    untagged,
    expecting = "a positive YAML integer or an exact workspace name"
)]
pub enum WorkspaceTarget {
    Number(u64),
    Name(String),
}

impl fmt::Display for WorkspaceTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(number) => write!(formatter, "{number}"),
            Self::Name(name) => write!(formatter, "{name:?}"),
        }
    }
}

impl WorkspaceTarget {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Number(number) => {
                if !(1..=MAX_WORKSPACE_NUMBER).contains(number) {
                    return Err(format!(
                        "expected a workspace number from 1 to {MAX_WORKSPACE_NUMBER}"
                    ));
                }
            }
            Self::Name(name) => {
                if name.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                    return Err("numeric strings and numeric-prefixed workspace names are ambiguous; use an unquoted integer for a numbered workspace".into());
                }

                if name.len() > MAX_WORKSPACE_NAME_BYTES
                    || !name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                    || name.ends_with(' ')
                    || !name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b" _-.:+".contains(&byte))
                {
                    return Err(format!(
                        "expected a workspace name of 1 to {MAX_WORKSPACE_NAME_BYTES} ASCII bytes, beginning with a letter, with no trailing space; use letters, digits, spaces, _ - . : +"
                    ));
                }
                // Sway strips quotes before cmd_workspace interprets keywords. output and
                // gaps are recognized even before the --no-auto-back-and-forth flag.
                // See sway/commands.c and sway/commands/workspace.c in Sway 1.11.
                if [
                    "number",
                    "next",
                    "prev",
                    "next_on_output",
                    "prev_on_output",
                    "current",
                    "back_and_forth",
                    "output",
                    "gaps",
                ]
                .iter()
                .any(|keyword| name.eq_ignore_ascii_case(keyword))
                {
                    return Err(format!("reserved workspace command keyword {name:?}"));
                }
            }
        }
        Ok(())
    }

    fn selection_command(&self) -> Result<String, String> {
        self.validate()?;
        Ok(match self {
            Self::Number(number) => format!("workspace --no-auto-back-and-forth number {number}"),
            Self::Name(name) => format!("workspace --no-auto-back-and-forth \"{name}\""),
        })
    }

    fn matches(&self, number: i32, name: &str) -> bool {
        match self {
            Self::Number(target) => u64::try_from(number) == Ok(*target),
            Self::Name(target) => number == -1 && name == target,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(deserialize_with = "config::unquoted_integer")]
    version: u64,
    pub workspaces: Vec<Spanned<WorkspaceLayout>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLayout {
    pub workspace: WorkspaceTarget,
    pub layout: String,
}

#[derive(Debug)]
pub struct PreparedProfile {
    pub workspaces: Vec<PreparedWorkspace>,
}

#[derive(Debug)]
pub struct PreparedWorkspace {
    pub workspace: WorkspaceTarget,
    pub layout_name: String,
    pub layout_path: PathBuf,
    pub config: config::Config,
}

/// Resolve and validate the profile and every referenced layout, entirely offline.
pub fn read_named(
    name: &str,
    profiles_dir: Option<PathBuf>,
    layouts_dir: Option<PathBuf>,
) -> Result<PreparedProfile, String> {
    let path = find_profile(name, profiles_dir)?;
    let input = config::read_yaml(&path, "profile")?;
    let profile = parse(&input).map_err(|error| format!("{}: {error}", path.display()))?;

    let layout_directory = discovery::LayoutDirectory::resolve(layouts_dir)?;
    let layouts = layout_directory.discover()?;
    let mut workspaces = Vec::with_capacity(profile.workspaces.len());
    for (entry_index, entry) in profile.workspaces.iter().enumerate() {
        let layout_name = &entry.value.layout;
        let location = entry.referenced;
        let context = format!(
            "{}: workspaces[{entry_index}].layout (workspace {}, line {}, column {})",
            path.display(),
            entry.value.workspace,
            location.line(),
            location.column()
        );
        if let Some(error) = layouts.ambiguities.get(layout_name) {
            return Err(format!("{context}: {error}"));
        }
        let Some(layout_path) = layouts.layouts.get(layout_name) else {
            return Err(format!(
                "{context}: layout {layout_name:?} not found in {}; use list to see available layouts",
                layout_directory.path.display()
            ));
        };

        let config = config::read(layout_path)
            .map_err(|error| format!("{context}: invalid layout {layout_name:?}: {error}"))?;
        workspaces.push(PreparedWorkspace {
            workspace: entry.value.workspace.clone(),
            layout_name: layout_name.clone(),
            layout_path: layout_path.clone(),
            config,
        });
    }

    Ok(PreparedProfile { workspaces })
}

/// All files have already been validated. Only this coordinator selects workspaces;
/// each construction gets the ordinary loader's fresh, frozen observer.
pub fn load(prepared: &PreparedProfile, diagnostics: &mut Diagnostics) -> Result<(), String> {
    diagnostics.stage("workspace preflight");
    let socket_path = observation::sway_socket()?;
    let mut observer = Observer::subscribe(
        observation::connect(&socket_path)?,
        Instant::now() + launch::IPC_TIMEOUT,
    )?;
    let initial = observer.workspace_snapshot(
        observation::connect(&socket_path)?,
        Instant::now() + launch::IPC_TIMEOUT,
    )?;
    let expected_focus = validate_workspace_snapshot(&initial)?;
    validate_workspace_history(&initial, &expected_focus, None)?;
    require_quiet_windows(&initial)?;

    // Examine every target before selecting even the first one. Missing workspaces
    // remain absent until their own entry is selected.
    for entry in &prepared.workspaces {
        diagnostics.profile_workspace(&entry.workspace, &entry.layout_name, &[]);
        empty_target(&initial, &entry.workspace)?;
    }

    let mut previous_snapshot = initial;
    let mut completed = Vec::new();
    for entry in &prepared.workspaces {
        diagnostics.profile_workspace(&entry.workspace, &entry.layout_name, &completed);
        diagnostics.stage("workspace selection");
        let selected = select_workspace(
            &mut observer,
            &socket_path,
            &entry.workspace,
            &previous_snapshot,
            diagnostics,
        )
        .map_err(|error| format!("workspace selection: {error}"))?;

        diagnostics.record(&format!(
            "using validated layout {:?} from {} on workspace {} id={}",
            entry.layout_name,
            entry.layout_path.display(),
            entry.workspace,
            selected.id
        ));
        launch::load(&entry.config, Some(selected.id), diagnostics)
            .map_err(|error| format!("layout construction: {error}"))?;
        completed.push((entry.workspace.clone(), selected.id));
        diagnostics.profile_workspace(&entry.workspace, &entry.layout_name, &completed);

        // The coordinator stayed subscribed during construction. Check that history,
        // including the gap before the loader subscribed, before another switch.
        diagnostics.stage("workspace completion verification");
        let finished = observer.workspace_snapshot(
            observation::connect(&socket_path)?,
            Instant::now() + launch::IPC_TIMEOUT,
        )?;
        let observed_focus = validate_workspace_snapshot(&finished)?;
        validate_workspace_history(&finished, &selected, None)?;
        if observed_focus != selected {
            return Err(
                "workspace completion: focused workspace identity or output changed".into(),
            );
        }
        previous_snapshot = finished;
    }

    Ok(())
}

/// Revalidate the old focus and empty target before the sole authorized command,
/// then require observed identity, emptiness, and history to agree with its result.
fn select_workspace(
    observer: &mut Observer,
    socket_path: &Path,
    target: &WorkspaceTarget,
    previous_snapshot: &WorkspaceSnapshot,
    diagnostics: &mut Diagnostics,
) -> Result<WorkspaceIdentity, String> {
    let expected_focus = validate_workspace_snapshot(previous_snapshot)?;
    let expected_target = empty_target(previous_snapshot, target)?;
    let before = observer.workspace_snapshot(
        observation::connect(socket_path)?,
        Instant::now() + launch::IPC_TIMEOUT,
    )?;
    let observed_focus = validate_workspace_snapshot(&before)?;
    validate_workspace_history(&before, &expected_focus, None)?;
    require_quiet_windows(&before)?;
    if observed_focus != expected_focus {
        return Err("unexpected focused workspace or containing output before selection".into());
    }

    let target_before = empty_target(&before, target)?;
    if target_before != expected_target {
        return Err(format!(
            "workspace {target} identity or output changed since previous observation"
        ));
    }

    let command = target.selection_command()?;
    diagnostics.record(&command);
    let replies = launch::run_command(
        observation::connect(socket_path)?,
        command,
        Instant::now() + launch::IPC_TIMEOUT,
    )?;
    match replies.as_slice() {
        [Ok(())] => {}
        [Err(error)] => return Err(format!("workspace command rejected: {error}")),
        _ => return Err("workspace command: expected exactly one successful reply".into()),
    }

    let after = observer.workspace_snapshot(
        observation::connect(socket_path)?,
        Instant::now() + launch::IPC_TIMEOUT,
    )?;
    let selected = validate_workspace_snapshot(&after)?;
    if !target.matches(selected.number, &selected.name) {
        return Err(format!(
            "selected workspace number {} name {:?} instead of {}",
            selected.number, selected.name, target
        ));
    }
    if empty_target(&after, target)?.as_ref() != Some(&selected)
        || target_before
            .as_ref()
            .is_some_and(|target| target != &selected)
    {
        return Err(
            "selected workspace identity or output differs from the verified target".into(),
        );
    }
    let previous_workspace = before
        .tree
        .find_as_ref(|node| node.id == expected_focus.id)
        .ok_or("previous workspace missing before selection")?;
    let transition = WorkspaceTransition {
        selected: &selected,
        allow_creation: target_before.is_none(),
        allow_previous_destruction: previous_workspace.nodes.is_empty()
            && previous_workspace.floating_nodes.is_empty(),
    };
    validate_workspace_history(&after, &expected_focus, Some(transition))?;
    require_quiet_windows(&after)?;
    Ok(selected)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceIdentity {
    id: i64,
    number: i32,
    name: String,
    output_id: i64,
}

/// GET_WORKSPACES and GET_TREE are separate queries: disagreement is an error,
/// never a reason to infer a missing workspace or choose one by its display name.
fn validate_workspace_snapshot(snapshot: &WorkspaceSnapshot) -> Result<WorkspaceIdentity, String> {
    let tree = &snapshot.tree;
    let mut container_ids = HashSet::new();
    if tree.node_type != NodeType::Root || tree.iter().any(|node| !container_ids.insert(node.id)) {
        return Err("workspace snapshot: invalid root or duplicate container identities".into());
    }

    let mut workspace_ids = HashSet::new();
    let mut workspace_numbers = HashSet::new();
    let mut workspace_names = HashSet::new();
    let mut focused = None;
    for workspace in &snapshot.workspaces {
        if !workspace_ids.insert(workspace.id)
            || !workspace_names.insert(&workspace.name)
            || workspace.num < -1
            || (workspace.num >= 0 && !workspace_numbers.insert(workspace.num))
        {
            return Err(
                "workspace snapshot: ambiguous workspace identities or numbers/names".into(),
            );
        }
        let node = tree
            .find_as_ref(|node| node.id == workspace.id)
            .ok_or("workspace snapshot: listed workspace missing from tree")?;
        let output = tree
            .nodes
            .iter()
            .find(|output| {
                output.node_type == NodeType::Output
                    && output.find_as_ref(|node| node.id == workspace.id).is_some()
            })
            .ok_or("workspace snapshot: workspace has no containing output")?;
        if node.node_type != NodeType::Workspace
            || node.num != Some(workspace.num)
            || node.name.as_deref() != Some(workspace.name.as_str())
            || output.name.as_deref() != Some(workspace.output.as_str())
            || (node.iter().filter(|node| node.focused).count() == 1) != workspace.focused
        {
            return Err(
                "workspace snapshot: workspace/tree identity, output, or focus is inconsistent"
                    .into(),
            );
        }
        if workspace.focused {
            if focused.is_some() {
                return Err("workspace snapshot: expected exactly one focused workspace".into());
            }
            focused = Some(WorkspaceIdentity {
                id: workspace.id,
                number: workspace.num,
                name: workspace.name.clone(),
                output_id: output.id,
            });
        }
    }
    if tree.iter().any(|node| {
        node.node_type == NodeType::Workspace
            && node.name.as_deref() != Some("__i3_scratch")
            && !workspace_ids.contains(&node.id)
    }) || tree.iter().filter(|node| node.focused).count() != 1
    {
        return Err("workspace snapshot: unlisted workspace or inconsistent tree focus".into());
    }
    focused.ok_or_else(|| "workspace snapshot: expected exactly one focused workspace".into())
}

fn empty_target(
    snapshot: &WorkspaceSnapshot,
    target: &WorkspaceTarget,
) -> Result<Option<WorkspaceIdentity>, String> {
    let Some(workspace) = snapshot.workspaces.iter().find(|workspace| match target {
        WorkspaceTarget::Number(number) => u64::try_from(workspace.num) == Ok(*number),
        WorkspaceTarget::Name(name) => workspace.name == *name,
    }) else {
        return Ok(None);
    };
    if !target.matches(workspace.num, &workspace.name) {
        return Err(format!(
            "workspace {target} has an inconsistent number/name identity"
        ));
    }
    let node = snapshot
        .tree
        .find_as_ref(|node| node.id == workspace.id)
        .ok_or("target workspace missing from tree")?;
    if !node.nodes.is_empty() || !node.floating_nodes.is_empty() {
        return Err(format!(
            "workspace {target} id={} must be completely empty, including tiled/floating containers",
            node.id
        ));
    }
    let output = snapshot
        .tree
        .nodes
        .iter()
        .find(|output| output.find_as_ref(|node| node.id == workspace.id).is_some())
        .ok_or("target workspace has no containing output")?;
    Ok(Some(WorkspaceIdentity {
        id: workspace.id,
        number: workspace.num,
        name: workspace.name.clone(),
        output_id: output.id,
    }))
}

fn require_quiet_windows(snapshot: &WorkspaceSnapshot) -> Result<(), String> {
    for event in &snapshot.events {
        if let Event::Window(event) = event
            && matches!(
                event.change,
                swayipc::WindowChange::New
                    | swayipc::WindowChange::Close
                    | swayipc::WindowChange::Move
                    | swayipc::WindowChange::Floating
                    | swayipc::WindowChange::FullscreenMode
            )
        {
            return Err("window activity during workspace observation; safety is uncertain".into());
        }
    }
    Ok(())
}

struct WorkspaceTransition<'a> {
    selected: &'a WorkspaceIdentity,
    allow_creation: bool,
    allow_previous_destruction: bool,
}

/// Permit only the one requested transition. Away-and-back focus, reload, migration,
/// rename, and unexpected workspace creation/destruction invalidate coordination.
fn validate_workspace_history(
    snapshot: &WorkspaceSnapshot,
    expected: &WorkspaceIdentity,
    transition: Option<WorkspaceTransition<'_>>,
) -> Result<(), String> {
    let mut observed_focus_change = false;
    let mut observed_creation = false;
    let mut observed_previous_destruction = false;
    for event in &snapshot.events {
        if let Event::Window(event) = event
            && !matches!(
                event.container.node_type,
                NodeType::Con | NodeType::FloatingCon
            )
        {
            return Err("window event has an invalid container node".into());
        }
        let Event::Workspace(event) = event else {
            continue;
        };
        if event.change == WorkspaceChange::Reload {
            return Err("workspace reload: revalidation required".into());
        }
        let current = event
            .current
            .as_ref()
            .ok_or("workspace event has no current workspace")?;
        if current.node_type != NodeType::Workspace
            || event
                .old
                .as_ref()
                .is_some_and(|old| old.node_type != NodeType::Workspace)
        {
            return Err("workspace event has an invalid workspace node".into());
        }
        if event.change == WorkspaceChange::Urgent {
            continue;
        }
        let Some(transition) = transition.as_ref() else {
            return Err(format!(
                "unexpected workspace change {:?} id={}",
                event.change, current.id
            ));
        };
        let target = transition.selected;

        match event.change {
            WorkspaceChange::Focus
                if !observed_focus_change
                    && target.id != expected.id
                    && event_matches_workspace(current, target)
                    && event
                        .old
                        .as_ref()
                        .is_some_and(|old| event_matches_workspace(old, expected)) =>
            {
                observed_focus_change = true;
            }
            WorkspaceChange::Init
                if transition.allow_creation
                    && !observed_creation
                    && event.old.is_none()
                    && !observed_focus_change
                    && event_matches_workspace(current, target) =>
            {
                observed_creation = true;
            }
            WorkspaceChange::Empty
                if transition.allow_previous_destruction
                    && !observed_previous_destruction
                    && event.old.is_none()
                    && target.id != expected.id
                    && event_matches_workspace(current, expected) =>
            {
                observed_previous_destruction = true;
            }
            _ => {
                return Err(format!(
                    "unexpected workspace change {:?} id={}",
                    event.change, current.id
                ));
            }
        }
    }

    if let Some(transition) = transition {
        if transition.selected.id != expected.id && !observed_focus_change {
            return Err("workspace selection: expected focus event was not observed".into());
        }
        if transition.allow_creation && !observed_creation {
            return Err(
                "workspace selection: expected workspace creation event was not observed".into(),
            );
        }
    }
    Ok(())
}

fn event_matches_workspace(node: &swayipc::Node, workspace: &WorkspaceIdentity) -> bool {
    node.id == workspace.id
        && node.num == Some(workspace.number)
        && node.name.as_deref() == Some(workspace.name.as_str())
}

fn find_profile(name: &str, profiles_dir: Option<PathBuf>) -> Result<PathBuf, String> {
    if !discovery::valid_name(name) {
        return Err(format!(
            "invalid profile name {name:?}; use [A-Za-z0-9][A-Za-z0-9_-]*"
        ));
    }

    let is_explicit = profiles_dir.is_some();
    let directory = match profiles_dir {
        Some(path) => {
            if path.as_os_str().is_empty() {
                return Err("--profiles-dir must not be empty".into());
            }
            path
        }
        None => discovery::config_home()?.join("sway-stage/profiles"),
    };

    let mut matching_paths = Vec::new();
    for path in discovery::yaml_paths(&directory, !is_explicit)? {
        if path.file_stem().and_then(|stem| stem.to_str()) == Some(name) {
            matching_paths.push(path);
        }
    }

    match matching_paths.as_slice() {
        [] => Err(format!(
            "profile {name:?} not found in {}",
            directory.display()
        )),
        [path] => Ok(path.clone()),
        _ => {
            let filenames = matching_paths
                .iter()
                .map(|path| format!("{:?}", path.file_name().expect("entry has a filename")))
                .collect::<Vec<_>>()
                .join(" and ");
            Err(format!(
                "ambiguous profile name {name:?}: {filenames}; rename one file"
            ))
        }
    }
}

fn parse(input: &str) -> Result<Profile, String> {
    if input.len() > config::MAX_INPUT_BYTES {
        return Err("input exceeds the 1 MiB limit".into());
    }

    let mut field_path = String::new();
    let profile: Profile = serde_saphyr::with_deserializer_from_str_with_options(
        input,
        config::yaml_options(),
        |deserializer| {
            serde_path_to_error::deserialize(deserializer).map_err(|error| {
                // Spanned contributes a synthetic `value` field to Serde's path.
                field_path = error.path().to_string().replace("].value", "]");
                error.into_inner()
            })
        },
    )
    .map_err(|error| {
        if field_path.is_empty() {
            field_path = "document".into();
        }
        format!("{field_path}: {error}")
    })?;

    if profile.version != 1 {
        return Err("version: expected format version 1".into());
    }
    if profile.workspaces.is_empty() || profile.workspaces.len() > MAX_WORKSPACES {
        return Err(format!(
            "workspaces: expected from 1 to {MAX_WORKSPACES} entries"
        ));
    }

    let mut workspace_targets = HashSet::new();
    for (entry_index, entry) in profile.workspaces.iter().enumerate() {
        let location = entry.referenced;
        let fail = |field: &str, message: &str| {
            format!(
                "workspaces[{entry_index}].{field} (line {}, column {}): {message}",
                location.line(),
                location.column()
            )
        };

        let target = &entry.value.workspace;
        target
            .validate()
            .map_err(|error| fail("workspace", &error))?;
        if !workspace_targets.insert(target) {
            let kind = match target {
                WorkspaceTarget::Number(_) => "number",
                WorkspaceTarget::Name(_) => "name",
            };
            return Err(fail(
                "workspace",
                &format!("duplicate workspace {kind} {target}"),
            ));
        }
        if !discovery::valid_name(&entry.value.layout) {
            return Err(fail(
                "layout",
                "expected a layout name matching [A-Za-z0-9][A-Za-z0-9_-]*",
            ));
        }
    }

    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PROFILE: &str = "version: 1\nworkspaces:\n  - workspace: 3\n    layout: development\n  - workspace: 1\n    layout: browser\n";

    fn rejects(input: &str, expected: &str) {
        let error = parse(input).expect_err(input);
        assert!(
            error.contains(expected),
            "expected {expected:?}, got {error:?}"
        );
    }

    #[test]
    fn preserves_workspace_order_and_allows_repeated_layouts() {
        let profile = parse(PROFILE).unwrap();
        let entries: Vec<_> = profile
            .workspaces
            .iter()
            .map(|entry| (entry.value.workspace.clone(), entry.value.layout.as_str()))
            .collect();
        assert_eq!(
            entries,
            [
                (WorkspaceTarget::Number(3), "development"),
                (WorkspaceTarget::Number(1), "browser")
            ]
        );
        parse(&PROFILE.replace("browser", "development")).unwrap();
        parse("version: 1\nworkspaces: [{workspace: 999999999, layout: '123'}]").unwrap();
    }

    #[test]
    fn names_preserve_types_order_and_repeated_layouts() {
        let input = PROFILE
            .replace("workspace: 3", "workspace: 'music player'")
            .replace("workspace: 1", "workspace: music")
            .replace("browser", "development");
        let profile = parse(&input).unwrap();
        assert_eq!(
            profile.workspaces[0].value.workspace,
            WorkspaceTarget::Name("music player".into())
        );
        assert_eq!(
            profile.workspaces[1].value.workspace,
            WorkspaceTarget::Name("music".into())
        );
        parse(&PROFILE.replace("workspace: 1", "workspace: music")).unwrap();
        for name in [
            "a",
            "Music_1-foo.bar:baz+qux",
            &"a".repeat(MAX_WORKSPACE_NAME_BYTES),
        ] {
            parse(&PROFILE.replace("workspace: 3", &format!("workspace: '{name}'"))).unwrap();
        }
        let error =
            parse(&input.replace("workspace: music", "workspace: 'music player'")).unwrap_err();
        for expected in [
            "duplicate workspace name",
            "workspaces[1].workspace",
            "line 5",
            "column 5",
        ] {
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn rejects_ambiguous_reserved_and_unsafe_names_with_locations() {
        for name in [
            "1",
            "1:code",
            "",
            " music",
            "music ",
            "music;exit",
            "music,exit",
            "music\\escape",
            "music\"quote",
            "music'quote",
            "$music",
            "music\nexit",
            "music\texit",
            "music[foo]",
            "é",
            "_music",
            "--next",
            "music/other",
            "number",
            "NEXT",
            "prev",
            "Next_On_Output",
            "prev_on_output",
            "current",
            "back_and_forth",
            "output",
            "GAPS",
            &"a".repeat(MAX_WORKSPACE_NAME_BYTES + 1),
        ] {
            // JSON strings are also YAML double-quoted scalars, including escaped controls.
            let scalar = serde_json::to_string(name).unwrap();
            let error = parse(&PROFILE.replace("workspace: 3", &format!("workspace: {scalar}")))
                .unwrap_err();
            for expected in ["workspaces[0].workspace", "line", "column"] {
                assert!(error.contains(expected), "{name:?}: {error}");
            }
        }
        for scalar in [
            "false",
            "~",
            "[]",
            "{}",
            "[music]",
            "{name: music}",
            ".inf",
            ".nan",
        ] {
            rejects(
                &PROFILE.replace("workspace: 3", &format!("workspace: {scalar}")),
                "workspaces[0].workspace",
            );
        }
    }

    #[test]
    fn selection_commands_are_literal_and_revalidate_the_target() {
        assert_eq!(
            WorkspaceTarget::Number(3).selection_command().unwrap(),
            "workspace --no-auto-back-and-forth number 3"
        );
        assert_eq!(
            WorkspaceTarget::Name("music player".into())
                .selection_command()
                .unwrap(),
            "workspace --no-auto-back-and-forth \"music player\""
        );
        assert!(
            WorkspaceTarget::Name("music; exit".into())
                .selection_command()
                .is_err()
        );
    }

    #[test]
    fn rejects_invalid_versions_and_workspace_scalars() {
        for version in ["0", "2", "999999999", "'1'", "1.0", "true", "null", "-1"] {
            rejects(
                &PROFILE.replace("version: 1", &format!("version: {version}")),
                "version",
            );
        }
        for workspace in [
            "0",
            "1000000000",
            "-1",
            "'3'",
            "3.0",
            "true",
            "null",
            "18446744073709551616",
        ] {
            rejects(
                &PROFILE.replace("workspace: 3", &format!("workspace: {workspace}")),
                "workspaces[0].workspace",
            );
        }
    }

    #[test]
    fn rejects_duplicate_workspaces_with_the_second_entry_location() {
        let error = parse(&PROFILE.replace("workspace: 1", "workspace: 3")).unwrap_err();
        for expected in [
            "workspaces[1].workspace",
            "duplicate workspace number 3",
            "line 5",
            "column 5",
        ] {
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn rejects_missing_fields_wrong_shapes_and_bad_layout_names() {
        for input in [
            "",
            "version: 1",
            "workspaces: []",
            "version: 1\nworkspaces: []",
            "version: 1\nworkspaces: null",
            "version: 1\nworkspaces: {}",
            "version: 1\nworkspaces: [{workspace: 1}]",
            "version: 1\nworkspaces: [{layout: development}]",
            "version: 1\nworkspaces: [null]",
        ] {
            assert!(parse(input).is_err(), "accepted {input}");
        }
        for layout in [
            "''",
            "null",
            "123",
            "true",
            "'../escape'",
            "'bad name'",
            "'layout.yaml'",
            "'_bad'",
            "'é'",
        ] {
            rejects(
                &PROFILE.replace("layout: development", &format!("layout: {layout}")),
                "workspaces[0].layout",
            );
        }
    }

    #[test]
    fn rejects_unknown_fields_and_duplicate_keys_at_both_levels() {
        for (input, expected) in [
            (format!("{PROFILE}extra: true\n"), "unknown field"),
            (
                PROFILE.replace("workspace: 3", "extra: true\n    workspace: 3"),
                "unknown field",
            ),
            (format!("version: 1\n{PROFILE}"), "duplicate"),
            (format!("{PROFILE}workspaces: []\n"), "duplicate"),
            (
                PROFILE.replace("workspace: 3", "workspace: 3\n    workspace: 3"),
                "duplicate",
            ),
            (
                PROFILE.replace(
                    "layout: development",
                    "layout: development\n    layout: browser",
                ),
                "duplicate",
            ),
        ] {
            rejects(&input, expected);
        }
    }

    #[test]
    fn rejects_malformed_yaml_and_unsupported_constructs() {
        for input in [
            "version: 1\nworkspaces: [unterminated".into(),
            format!("{PROFILE}---\n{PROFILE}"),
            format!("{PROFILE}---\n"),
            format!("!custom\n{PROFILE}"),
            PROFILE.replace("workspace: 3", "workspace: !custom 3"),
            PROFILE.replace("layout: development", "!custom layout: development"),
            PROFILE
                .replace("layout: development", "layout: &name development")
                .replace("layout: browser", "layout: *name"),
            "version: 1\nworkspaces: [{<<: {workspace: 1}, layout: development}]".into(),
        ] {
            assert!(parse(&input).is_err(), "accepted {input}");
        }
        let error =
            parse("version: 1\nworkspaces:\n  - workspace: 1\n    layout: null\n").unwrap_err();
        for expected in ["workspaces[0].layout", "line", "column"] {
            assert!(error.contains(expected), "{error}");
        }
        assert!(!error.contains(".value"), "{error}");
    }

    #[test]
    fn bounds_input_and_entry_count() {
        rejects(&" ".repeat(config::MAX_INPUT_BYTES + 1), "1 MiB");
        let mut input = String::from("version: 1\nworkspaces:\n");
        for workspace in 1..=MAX_WORKSPACES {
            input.push_str(&format!(
                "  - {{workspace: {workspace}, layout: development}}\n"
            ));
        }
        assert_eq!(parse(&input).unwrap().workspaces.len(), MAX_WORKSPACES);
        input.push_str("  - {workspace: 257, layout: development}\n");
        rejects(&input, "from 1 to 256 entries");
    }

    #[test]
    fn reads_profile_and_validates_every_referenced_layout() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "sway-stage-profile-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let layouts_dir = directory.join("layouts");
        fs::create_dir(&layouts_dir).unwrap();
        let profile_path = directory.join("work.yaml");
        fs::write(&profile_path, PROFILE).unwrap();
        fs::write(
            layouts_dir.join("development.yaml"),
            "version: 1\nlayout: {id: t, command: [not-installed], match: {app_id: t}}\n",
        )
        .unwrap();

        let read = || read_named("work", Some(directory.clone()), Some(layouts_dir.clone()));
        let error = read().unwrap_err();
        assert!(error.contains("workspaces[1]"), "{error}");
        assert!(error.contains("layout \"browser\" not found"), "{error}");

        fs::write(
            layouts_dir.join("browser.yml"),
            "version: 1\nlayout: {id: t, command: [], match: {app_id: t}}\n",
        )
        .unwrap();
        let error = read().unwrap_err();
        for expected in [
            "work.yaml",
            "workspaces[1]",
            "browser.yml",
            "command",
            "line",
            "column",
        ] {
            assert!(error.contains(expected), "{error}");
        }

        fs::copy(
            layouts_dir.join("development.yaml"),
            layouts_dir.join("browser.yml"),
        )
        .unwrap();
        let prepared = read().unwrap();
        assert_eq!(prepared.workspaces[0].workspace, WorkspaceTarget::Number(3));
        assert_eq!(prepared.workspaces[1].workspace, WorkspaceTarget::Number(1));
        assert_eq!(prepared.workspaces[1].layout_name, "browser");
        assert_eq!(
            prepared.workspaces[1].layout_path,
            layouts_dir.join("browser.yml")
        );
        // The prepared configurations survive replacement/removal of their source files.
        fs::remove_file(layouts_dir.join("browser.yml")).unwrap();
        assert_eq!(prepared.workspaces[1].config.window_timeout_ms, 20000);
        fs::copy(
            layouts_dir.join("development.yaml"),
            layouts_dir.join("browser.yml"),
        )
        .unwrap();
        fs::write(layouts_dir.join("browser.yaml"), "").unwrap();
        assert!(
            read()
                .unwrap_err()
                .contains("ambiguous layout name \"browser\"")
        );

        fs::write(&profile_path, " ".repeat(config::MAX_INPUT_BYTES + 1)).unwrap();
        assert!(read().unwrap_err().contains("1 MiB"));
        fs::write(&profile_path, [0xff]).unwrap();
        assert!(read().unwrap_err().contains("UTF-8"));
        fs::remove_dir_all(directory).unwrap();
    }
}
