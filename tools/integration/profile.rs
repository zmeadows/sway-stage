//! Independent profile fixtures and observation decisions. No production parser or
//! construction code is used here; lifecycle owns execution and guarded cleanup.

use std::collections::HashSet;

use crate::headless::{
    OUTPUT_NAME, SECOND_OUTPUT_ASSIGNED_WORKSPACE, SECOND_OUTPUT_INITIAL_WORKSPACE,
    SECOND_OUTPUT_NAME,
};
use crate::ipc::{
    EventWorkspace, NodeType, Output, TreeNode, WindowChange, WindowEvent, WorkspaceChange,
    WorkspaceEvent,
};
use crate::observation::{Snapshot, WorkspaceIdentity, walk};
use crate::ownership::{AcceptedWindows, bind_launches, require_quiet};
use crate::preservation::output_ancestry;
use crate::scenarios::{Scenario, foreign_fixture_scenario, standard_scenarios};

pub const CASES: [Case; 5] = [
    Case::Success,
    Case::Occupied,
    Case::Partial,
    Case::NamedSuccess,
    Case::EventBurst,
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Case {
    Success,
    NamedSuccess,
    Occupied,
    Partial,
    /// A successful profile whose first construction carries more events than the production
    /// coordinator's queue can hold while it waits for the layout loader.
    EventBurst,
}

impl Case {
    pub fn name(self) -> &'static str {
        match self {
            Self::Success => "profile-success",
            Self::NamedSuccess => "profile-named-success",
            Self::Occupied => "profile-occupied-refusal",
            Self::Partial => "profile-partial-failure",
            Self::EventBurst => "profile-event-burst",
        }
    }
}

/// The two-output session's only scenario.
pub const CROSS_OUTPUT_SCENARIO: &str = "profile-cross-output";

/// The wrapper records *launches*, including launches that never create a window. It
/// preserves the configured executable and every argument through a literal exec.
pub const LAUNCH_SCRIPT: &str =
    "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$2\" || exit 1\nshift 2\nexec \"$@\"\n";

/// Ticks sent during the event-burst case's first construction: well over the 256 events
/// that the production profile coordinator's queue holds.
pub const TICK_BURST_COUNT: u32 = 400;

/// Arguments: swaymsg, the private IPC socket, sleep, and the tick count, then the window's
/// own command. Each tick goes through the explicitly named socket and must succeed; a
/// failure exits before the window starts, so the load and the scenario fail rather than
/// silently sending fewer ticks. Pausing after every ten ticks (at most 100 per second)
/// lets sway-stage's own loader and the harness subscription keep draining their events.
pub const TICK_BURST_SCRIPT: &str = "\
#!/bin/sh
swaymsg=$1 socket=$2 sleep=$3 remaining=$4
shift 4
while [ \"$remaining\" -gt 0 ]; do
    \"$swaymsg\" -s \"$socket\" -t send_tick sway-stage-integration-burst >/dev/null || exit 1
    remaining=$((remaining - 1))
    if [ $((remaining % 10)) -eq 0 ]; then
        \"$sleep\" 0.1 || exit 1
    fi
done
exec \"$@\"
";

/// The command prefix that runs `TICK_BURST_SCRIPT` before a window's own command.
pub fn tick_burst_command(script: &str, swaymsg: &str, socket: &str, sleep: &str) -> Vec<String> {
    vec![
        "/bin/sh".into(),
        script.into(),
        swaymsg.into(),
        socket.into(),
        sleep.into(),
        TICK_BURST_COUNT.to_string(),
    ]
}

/// `tick_burst` is the `tick_burst_command` prefix, used only by the event-burst case.
pub fn layouts(
    case: Case,
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
    tick_burst: &[String],
) -> [Scenario; 2] {
    let unique_id = format!("{run_id}-{}", case.name());
    let scenarios = standard_scenarios(&unique_id, foot, sleep);
    let first_name = if case == Case::Success {
        "mixed-capture-reload"
    } else {
        "horizontal-pair"
    };
    let mut first = scenarios
        .iter()
        .find(|scenario| scenario.name == first_name)
        .cloned()
        .expect("standard first profile scenario");
    let mut second = scenarios
        .into_iter()
        .find(|scenario| scenario.name == "vertical-pair")
        .expect("standard vertical-pair scenario");
    first.name = "profile-first";
    second.name = "profile-second";
    if case == Case::Partial {
        second = foreign_fixture_scenario(&unique_id, foot, sleep);
        second.name = "profile-second";
        second.window_timeout_ms = 500;
        second.layout.first_leaf_mut().command = vec![sleep.into(), "0".into()];
    }
    if case == Case::EventBurst {
        // sway-stage spawns this first window only after its loader has subscribed, and the
        // coordinator subscribed before preflight. Foot starts only after the last tick, so
        // the whole burst falls inside the first construction.
        let first_leaf = first.layout.first_leaf_mut();
        let mut command = tick_burst.to_vec();
        command.append(&mut first_leaf.command);
        first_leaf.command = command;
    }

    for scenario in [&mut first, &mut second] {
        wrap_launches(
            &mut scenario.layout,
            scenario.name,
            launch_script,
            launch_record,
        );
    }
    [first, second]
}

fn wrap_launches(
    node: &mut crate::scenarios::ConfiguredNode,
    name: &str,
    script: &str,
    record: &str,
) {
    match node {
        crate::scenarios::ConfiguredNode::Window(window) => {
            let mut command = vec![
                "/bin/sh".into(),
                script.into(),
                format!("{name}-{}", window.layout_id),
                record.into(),
            ];
            command.append(&mut window.command);
            window.command = command;
        }
        crate::scenarios::ConfiguredNode::Group { children, .. } => {
            for child in children {
                wrap_launches(child, name, script, record);
            }
        }
    }
}

/// `horizontal-pair` with recorded launches, for the cross-output profile's one target.
pub fn cross_output_layout(
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
) -> Scenario {
    let unique_id = format!("{run_id}-{CROSS_OUTPUT_SCENARIO}");
    let mut layout = standard_scenarios(&unique_id, foot, sleep)
        .into_iter()
        .find(|scenario| scenario.name == "horizontal-pair")
        .expect("standard horizontal-pair scenario");
    layout.name = CROSS_OUTPUT_SCENARIO;
    wrap_launches(
        &mut layout.layout,
        layout.name,
        launch_script,
        launch_record,
    );
    layout
}

/// One entry: the workspace Sway's rules create on the second output, with the profile's
/// own expectation that it is there.
pub fn cross_output_configuration() -> String {
    format!(
        "version: 1\nworkspaces:\n  - workspace: {SECOND_OUTPUT_ASSIGNED_WORKSPACE}\n    layout: {CROSS_OUTPUT_SCENARIO}\n    output: {SECOND_OUTPUT_NAME}\n"
    )
}

pub fn configuration(primary: &str, second: &str) -> String {
    let primary = if primary.bytes().all(|byte| byte.is_ascii_digit()) {
        primary.to_owned()
    } else {
        serde_json::to_string(primary).expect("string serialization")
    };
    format!(
        "version: 1\nworkspaces:\n  - workspace: {primary}\n    layout: profile-first\n  - workspace: {second}\n    layout: profile-second\n"
    )
}

/// Independent decoding of the two known profile references. Layout verification is
/// performed separately against measured Sway state, never through production parsing.
pub fn verify_captured_references(yaml: &str, primary: &str, second: &str) -> Result<(), String> {
    let observed: serde_json::Value =
        serde_saphyr::from_str(yaml).map_err(|_| "invalid captured profile YAML")?;
    let target = |name: &str| -> serde_json::Value {
        match name.parse::<u64>() {
            Ok(number) => serde_json::json!(number),
            Err(_) => serde_json::json!(name),
        }
    };
    let expected = serde_json::json!({
        "version": 1,
        "workspaces": [
            {"workspace": target(primary), "layout": "captured-001"},
            {"workspace": target(second), "layout": "captured-002"},
        ]
    });
    if observed != expected {
        return Err("captured profile targets/order/references differ".into());
    }
    Ok(())
}

/// Existing targets keep their identity/output; missing ones may be created on any
/// existing output according to Sway's own rules. Move events are rejected separately.
pub struct Target {
    pub workspace: WorkspaceIdentity,
    output: Vec<(i64, Option<String>)>,
    number: i32,
}

impl Target {
    pub fn record(observed: &Snapshot, target_name: &str) -> Result<Option<Self>, String> {
        let candidates: Vec<_> = observed
            .workspaces
            .iter()
            .filter(|workspace| {
                workspace.num.to_string() == target_name || workspace.name == target_name
            })
            .collect();
        match candidates.as_slice() {
            [] => Ok(None),
            [workspace]
                if workspace.name == target_name
                    && workspace.num == target_name.parse::<i32>().unwrap_or(-1) =>
            {
                Ok(Some(Self {
                    workspace: WorkspaceIdentity {
                        id: workspace.id,
                        name: workspace.name.clone(),
                    },
                    output: output_ancestry(&observed.tree, workspace.id)?,
                    number: workspace.num,
                }))
            }
            _ => Err("ambiguous or renamed profile workspace".into()),
        }
    }

    pub fn verify<'a>(&self, observed: &'a Snapshot) -> Result<&'a TreeNode, String> {
        let workspace = walk(&observed.tree)
            .into_iter()
            .map(|placed| placed.node)
            .find(|node| {
                node.id == self.workspace.id
                    && node.name.as_ref() == Some(&self.workspace.name)
                    && node.num == Some(self.number)
            })
            .ok_or("profile target identity changed/disappeared")?;
        if output_ancestry(&observed.tree, workspace.id)? != self.output {
            return Err("profile target output ancestry changed".into());
        }
        Ok(workspace)
    }
}

/// Workspace switches may change global focus and focus-stack metadata. Every identity,
/// wrapper, window state, and rectangle must otherwise survive; selection is verified
/// separately with the independent layout model and the accepted focus events.
pub fn require_preserved_layout(before: &TreeNode, after: &TreeNode) -> Result<(), String> {
    let mut baseline = crate::layout::signature(before);
    let mut current = crate::layout::signature(after);
    baseline.ignore_focus();
    current.ignore_focus();
    if baseline != current {
        return Err("profile layout changed across cleanup workspace switch".into());
    }
    Ok(())
}

/// Bind each side of the sole expected switch independently. In particular, a second
/// layout cannot launch before that switch, and the first cannot receive later activity.
pub fn bind_ordered_launches(
    events: &[WindowEvent],
    switch_index: usize,
    layouts: &[Scenario; 2],
    preexisting: &HashSet<i64>,
    partial: bool,
) -> Result<[AcceptedWindows; 2], String> {
    let (first_events, second_events) = events
        .split_at_checked(switch_index)
        .ok_or("invalid profile transition boundary")?;
    let first = bind_launches(first_events, &layouts[0].layout, preexisting)?;
    let expected_focus = first
        .iter()
        .find(|(_, leaf)| leaf.as_str() == layouts[0].expected_focus)
        .map(|(&id, _)| id);
    let last_focus = first_events
        .iter()
        .rev()
        .find(|event| event.change == WindowChange::Focus)
        .map(|event| event.container.id);
    if last_focus != expected_focus {
        return Err("first profile layout did not select its final focus before switching".into());
    }

    let mut before_second = preexisting.clone();
    before_second.extend(first.keys());
    let second = if partial {
        require_quiet(second_events, None)?;
        AcceptedWindows::new()
    } else {
        bind_launches(second_events, &layouts[1].layout, &before_second)?
    };
    Ok([first, second])
}

/// A rename borrows the focused primary slot, never a fourth workspace. Refuse name
/// collisions (including a numbered alias) and any loss of identity or empty state.
pub fn verify_rename_preparation(
    observed: &Snapshot,
    primary: &WorkspaceIdentity,
    destination: &str,
) -> Result<(), String> {
    observed.validate()?;
    crate::ipc::validate_workspace_name(destination)?;
    require_quiet(&observed.window_events, None)?;
    let workspace = observed.focused_workspace();
    let expected_number = primary.name.parse::<i32>().unwrap_or(-1);
    let destination_number = destination.parse::<i32>().ok();
    if workspace.id != primary.id
        || workspace.name.as_ref() != Some(&primary.name)
        || workspace.num != Some(expected_number)
        || !workspace.nodes.is_empty()
        || !workspace.floating_nodes.is_empty()
        || observed.workspaces.iter().any(|workspace| {
            workspace.name == destination || Some(workspace.num) == destination_number
        })
    {
        return Err(
            "disposable rename requires the exact empty focused primary and an unused destination"
                .into(),
        );
    }
    Ok(())
}

pub fn verify_rename_result(
    before: &Snapshot,
    after: &Snapshot,
    renamed: &WorkspaceIdentity,
) -> Result<(), String> {
    after.validate()?;
    require_quiet(&after.window_events, None)?;
    let workspace = after.focused_workspace();
    if workspace.id != renamed.id
        || workspace.name.as_ref() != Some(&renamed.name)
        || workspace.num != Some(renamed.name.parse::<i32>().unwrap_or(-1))
        || crate::layout::signature(before.focused_workspace())
            != crate::layout::signature(workspace)
        || output_ancestry(&before.tree, renamed.id)? != output_ancestry(&after.tree, renamed.id)?
    {
        return Err(
            "disposable rename changed identity, number, output, focus, or empty structure".into(),
        );
    }
    // Renaming can reorder output children. Compare workspace identities independently.
    let other_workspaces = |snapshot: &Snapshot| {
        snapshot
            .workspaces
            .iter()
            .filter(|workspace| workspace.id != renamed.id)
            .map(|workspace| {
                (
                    workspace.id,
                    (workspace.name.clone(), workspace.num, workspace.focused),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    if other_workspaces(before) != other_workspaces(after) {
        return Err("another workspace changed during disposable rename".into());
    }
    Ok(())
}

/// One workspace event a profile load must produce. Each workspace is matched exactly: its
/// container ID, its name, and the number Sway derives from a purely numeric name.
#[derive(Debug)]
pub enum ExpectedWorkspaceEvent<'a> {
    Init(&'a WorkspaceIdentity),
    Focus {
        from_id: i64,
        to: &'a WorkspaceIdentity,
    },
    Empty(&'a WorkspaceIdentity),
}

/// A fence's non-urgent workspace events must be exactly `expected`, in order. A missing,
/// extra, or reordered creation, focus change, destruction, or other change fails.
pub fn verify_workspace_events(
    observed: &[WorkspaceEvent],
    expected: &[ExpectedWorkspaceEvent],
) -> Result<(), String> {
    let all_match = observed.len() == expected.len()
        && observed
            .iter()
            .zip(expected)
            .all(|(event, expected_event)| workspace_event_matches(event, expected_event));
    if !all_match {
        return Err(format!(
            "workspace events differ: expected {expected:?}, observed {observed:?}"
        ));
    }
    Ok(())
}

fn workspace_event_matches(event: &WorkspaceEvent, expected: &ExpectedWorkspaceEvent) -> bool {
    let is_workspace = |node: Option<&EventWorkspace>, workspace: &WorkspaceIdentity| {
        node.is_some_and(|node| {
            node.node_type == Some(NodeType::Workspace)
                && node.id == workspace.id
                && node.name.as_ref() == Some(&workspace.name)
                && node.num == Some(workspace.name.parse::<i32>().unwrap_or(-1))
        })
    };
    let current = event.current.as_ref();

    match expected {
        ExpectedWorkspaceEvent::Init(workspace) => {
            event.change == WorkspaceChange::Init
                && is_workspace(current, workspace)
                && event.old.is_none()
        }
        ExpectedWorkspaceEvent::Focus { from_id, to } => {
            event.change == WorkspaceChange::Focus
                && is_workspace(current, to)
                && event.old.as_ref().is_some_and(|old| old.id == *from_id)
        }
        ExpectedWorkspaceEvent::Empty(workspace) => {
            event.change == WorkspaceChange::Empty
                && is_workspace(current, workspace)
                && event.old.is_none()
        }
    }
}

/// The cross-output profile's starting state: the fixture workspace focused on the first
/// output, and the second output showing its empty initial workspace, which selecting the
/// target will hide.
pub struct CrossOutputBaseline {
    /// The only workspace whose removal the load may cause.
    pub hidden_workspace: WorkspaceIdentity,
    fixture_output: Vec<(i64, Option<String>)>,
    second_output: Vec<(i64, Option<String>)>,
}

impl CrossOutputBaseline {
    pub fn record(
        observed: &Snapshot,
        outputs: &[Output],
        fixture_workspace: &WorkspaceIdentity,
    ) -> Result<Self, String> {
        observed.validate()?;
        if observed.focused_workspace_id != fixture_workspace.id {
            return Err("the cross-output profile must start on the fixture workspace".into());
        }
        if Target::record(observed, SECOND_OUTPUT_ASSIGNED_WORKSPACE)?.is_some() {
            return Err(format!(
                "workspace {SECOND_OUTPUT_ASSIGNED_WORKSPACE} must not exist before the load creates it"
            ));
        }

        let hidden = Target::record(observed, SECOND_OUTPUT_INITIAL_WORKSPACE)?
            .ok_or("the second output's initial workspace is missing")?;
        let hidden_node = hidden.verify(observed)?;
        if !hidden_node.nodes.is_empty() || !hidden_node.floating_nodes.is_empty() {
            return Err("the second output's initial workspace must be empty".into());
        }

        let fixture_output = output_ancestry(&observed.tree, fixture_workspace.id)?;
        let second_output = output_ancestry(&observed.tree, hidden.workspace.id)?;
        let fixture_shown = output_shows(outputs, OUTPUT_NAME, &fixture_workspace.name);
        let hidden_shown =
            output_shows(outputs, SECOND_OUTPUT_NAME, SECOND_OUTPUT_INITIAL_WORKSPACE);
        if !is_output(&fixture_output, OUTPUT_NAME)
            || !is_output(&second_output, SECOND_OUTPUT_NAME)
            || !fixture_shown
            || !hidden_shown
        {
            return Err(format!(
                "expected the fixture workspace shown on {OUTPUT_NAME} and workspace \
                 {SECOND_OUTPUT_INITIAL_WORKSPACE} shown on {SECOND_OUTPUT_NAME}"
            ));
        }

        Ok(Self {
            hidden_workspace: hidden.workspace,
            fixture_output,
            second_output,
        })
    }

    /// The hidden workspace is gone, the created target is focused on the second output,
    /// and the fixture workspace is still on the first.
    pub fn verify_result(
        &self,
        loaded: &Snapshot,
        target: &WorkspaceIdentity,
        fixture_workspace_id: i64,
    ) -> Result<(), String> {
        loaded.validate()?;
        let hidden_id = self.hidden_workspace.id;
        if walk(&loaded.tree)
            .iter()
            .any(|placed| placed.node.id == hidden_id)
            || loaded
                .workspaces
                .iter()
                .any(|workspace| workspace.id == hidden_id)
        {
            return Err(format!(
                "hidden empty workspace id={hidden_id} still exists"
            ));
        }
        if loaded.focused_workspace_id != target.id {
            return Err("the profile did not leave its target focused".into());
        }
        if output_ancestry(&loaded.tree, target.id)? != self.second_output {
            return Err("the profile target is not on the second output".into());
        }
        if output_ancestry(&loaded.tree, fixture_workspace_id)? != self.fixture_output {
            return Err("the fixture workspace changed output".into());
        }
        Ok(())
    }
}

/// Whether `get_outputs` reports `workspace_name` as the workspace shown on `output_name`.
fn output_shows(outputs: &[Output], output_name: &str, workspace_name: &str) -> bool {
    outputs.iter().any(|output| {
        output.name == output_name && output.current_workspace.as_deref() == Some(workspace_name)
    })
}

fn is_output(ancestry: &[(i64, Option<String>)], output_name: &str) -> bool {
    matches!(ancestry, [(_, Some(name))] if name == output_name)
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
