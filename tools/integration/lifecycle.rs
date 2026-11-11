//! Explicit executable lifecycle: preflight, scenarios, verified closes, and restoration.
//! A failure leaves windows alone. Only the success path enters cleanup; restoration is
//! a separate, guarded operation. Files and the IPC helper may be dropped, never windows.

use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{self, Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;

use crate::capture::{verify_capture, verify_inferred_capture};
use crate::ipc::{IPC_TIMEOUT, Swaymsg, TreeNode, validate_restoration_name, workspace_number};
use crate::layout::{
    Shape, Signature, class, is_window, observed_identifier, outline, remove_from_shape, shape,
    signature, verify_layout,
};
use crate::observation::{
    Events, ExpectedSwitch, Failure, Snapshot, WorkspaceIdentity, snapshot, walk,
};
use crate::ownership::{
    AcceptedWindows, ExpectedClose, bind_launches, require_quiet, verify_ownership,
};
use crate::process;
use crate::scenarios::{
    ConfiguredNode, OptionalApplications, Scenario, configuration_yaml,
    configure_process_execution, extended_scenarios, prepare_extended, standard_scenarios,
};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);
const CLOSE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const RESTORED: &str = "Original workspace restored.";
const LOST_CONTROL: &str =
    "Original workspace not restored: focus history is uncertain or user took control.";
const ORIGINAL_GONE: &str =
    "Original workspace not restored: its recorded identity no longer exists.";

#[derive(Debug, Parser)]
#[command(about = "Manual load/capture checks; run only in an isolated disposable Sway session")]
pub struct Arguments {
    #[arg(long, required = true, value_parser = workspace_number)]
    workspace: String,
    #[arg(
        long,
        help = "Also test Firefox, Xterm/Xwayland, and duplicate Foot app IDs"
    )]
    extended: bool,
    #[arg(
        long,
        help = "Exact native Firefox app ID, overriding application.ini RemotingName"
    )]
    firefox_app_id: Option<String>,
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/target/release/sway-stage"))]
    binary: String,
}

/// Last accepted evidence is retained for diagnostics even if a later verification fails.
struct ScenarioProgress {
    scenario: Scenario,
    preexisting_ids: Option<HashSet<i64>>,
    accepted: AcceptedWindows,
}

struct Harness {
    arguments: Arguments,
    binary: PathBuf,
    foot: String,
    sleep: String,
    sway: Swaymsg,
    events: Events,
    run_id: String,
    original: Option<WorkspaceIdentity>,
    disposable: Option<WorkspaceIdentity>,
    switch_attempted: bool,
    current_scenario: Option<ScenarioProgress>,
    stage: String,
    passed: usize,
    skipped: usize,
    retained_profiles: Vec<PathBuf>,
    restoration: Option<String>,
}

pub fn run(arguments: Arguments) -> bool {
    let mut harness = match Harness::new(arguments) {
        Ok(harness) => harness,
        Err(error) => {
            eprintln!("FAIL  preflight: {error}");
            println!("\n0 passed, 1 failed, 0 skipped");
            return false;
        }
    };

    let result = harness.execute().and_then(|()| interruption_check());
    let mut failed = result.is_err();
    if let Err(failure) = result {
        // Process helpers return strings. A SIGINT recorded during one of them must
        // invalidate history even when that helper's error is otherwise ordinary.
        if process::interruption_requested() {
            harness.events.distrust_focus_history("interrupted");
        }
        let message = failure.report(&mut harness.events);
        eprintln!("FAIL  {}: {message}", harness.stage);

        if harness.switch_attempted {
            report_remaining(
                &harness.sway,
                &harness.run_id,
                harness.current_scenario.as_ref(),
            );
            if harness.stage != "final restoration" {
                harness.restoration = Some(match harness.restore() {
                    Ok(message) => message.to_owned(),
                    Err(failure) => format!(
                        "Original workspace not restored: {}",
                        failure.report(&mut harness.events)
                    ),
                });
            }
        }
    }

    // Explicit bounded helper shutdown on every outcome. This terminates only our IPC
    // helper, and never requests desktop cleanup.
    if let Err(error) = harness.events.close() {
        failed = true;
        eprintln!("FAIL  event helper shutdown: {error}");
    }
    if process::interruption_requested() && !failed {
        failed = true;
        eprintln!("FAIL  shutdown: interrupted");
    }
    println!(
        "\n{} passed, {} failed, {} skipped",
        harness.passed,
        usize::from(failed),
        harness.skipped
    );
    for profile in harness.retained_profiles {
        println!(
            "Retained Firefox files: {}; remove only after all associated processes exit.",
            profile.display()
        );
    }
    if let Some(restoration) = harness.restoration {
        println!("{restoration}");
    }
    !failed
}

impl Harness {
    fn new(arguments: Arguments) -> Result<Self, String> {
        process::install_sigint_handler()?;
        process::check_interruption()?;
        let sway = Swaymsg::from_environment()?;
        let binary = process::find_executable(&arguments.binary)?;
        let search_path = env::var_os("PATH").unwrap_or_default();
        let foot = unresolved_executable_text("foot", &search_path)?;
        let sleep = unresolved_executable_text("sleep", &search_path)?;
        let run_id = random_run_id()?;
        if sway.seat_names()?.len() != 1 {
            return Err("this harness requires exactly one Sway seat".into());
        }
        // Establish subscription readiness before the first workspace observation.
        let events = Events::subscribe(&sway)?;

        Ok(Self {
            arguments,
            binary,
            foot,
            sleep,
            sway,
            events,
            run_id,
            original: None,
            disposable: None,
            switch_attempted: false,
            current_scenario: None,
            stage: "preflight".into(),
            passed: 0,
            skipped: 0,
            retained_profiles: Vec::new(),
            restoration: None,
        })
    }

    fn execute(&mut self) -> Result<(), Failure> {
        let initial = preflight(&self.sway, &mut self.events, &self.arguments.workspace)?;
        self.original = Some(initial.original);

        let directory = TemporaryConfigurations::new(&env::temp_dir(), &self.run_id)?;
        let mut scenarios = standard_scenarios(&self.run_id, &self.foot, &self.sleep);
        let helper =
            env::current_exe().map_err(|error| format!("locate process probe: {error}"))?;
        directory.create_process_cwd()?;
        for scenario in &mut scenarios {
            configure_process_execution(scenario, &helper, &self.run_id)?;
            self.stage = format!("{}: initial check", scenario.name);
            directory.write(scenario.name, &configuration_yaml(scenario))?;
            check_configuration(&self.binary, &directory.path, scenario.name, &self.stage)?;
        }
        // Discovery only searches PATH. Profiles, probes, and launches wait for preparation
        // immediately before each extended scenario.
        let optional_applications = if self.arguments.extended {
            scenarios.extend(extended_scenarios(&self.run_id, &self.foot, &self.sleep));
            Some(OptionalApplications::find(
                self.arguments.firefox_app_id.clone(),
            ))
        } else {
            None
        };
        self.stage = "preflight".into();
        let ready = preflight(&self.sway, &mut self.events, &self.arguments.workspace)?;
        self.original = Some(ready.original.clone());
        self.stage = "workspace switch".into();
        interruption_check()?;
        self.switch_attempted = true;
        self.sway.switch_disposable(&self.arguments.workspace)?;
        let transition = ExpectedSwitch {
            from_id: ready.original.id,
            destination_name: self.arguments.workspace.clone(),
        };
        let switched = snapshot(&self.sway, &mut self.events, Some(&transition))?;
        require_quiet(&switched.window_events, None)?;
        if ready
            .designated_id
            .is_some_and(|id| id != switched.focused_workspace_id)
        {
            return Err(Failure::Uncertain(
                "disposable workspace identity changed during the switch".into(),
            ));
        }
        self.disposable = Some(WorkspaceIdentity {
            id: switched.focused_workspace_id,
            name: self.arguments.workspace.clone(),
        });
        require_empty(switched.focused_workspace())?;

        for scenario in scenarios {
            interruption_check()?;
            self.current_scenario = Some(ScenarioProgress {
                scenario,
                preexisting_ids: None,
                accepted: AcceptedWindows::new(),
            });
            self.run_scenario(&directory, optional_applications.as_ref())?;
        }

        self.stage = "final restoration".into();
        let restoration = self.restore()?;
        self.restoration = Some(restoration.to_owned());
        if restoration != RESTORED {
            return Err(Failure::Check(restoration.to_owned()));
        }
        Ok(())
    }

    fn run_scenario(
        &mut self,
        directory: &TemporaryConfigurations,
        optional_applications: Option<&OptionalApplications>,
    ) -> Result<(), Failure> {
        let progress = self
            .current_scenario
            .as_mut()
            .expect("scenario loop set progress");
        let scenario = &mut progress.scenario;
        let name = scenario.name;
        self.stage = format!("{name}: empty-workspace precondition");
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        require_empty(before.focused_workspace())?;

        self.stage = format!("{name}: scenario preparation");
        interruption_check()?;
        if scenario.extension.is_some() {
            let applications = optional_applications
                .expect("extended scenarios run only after optional application discovery");
            prepare_extended(
                scenario,
                &self.run_id,
                &self.sleep,
                applications,
                &mut self.retained_profiles,
            )?;
        }
        interruption_check()?;
        if let Some(reason) = &scenario.skip {
            self.skipped += 1;
            println!("SKIP  {name:19} {reason}");
            return Ok(());
        }

        self.stage = format!("{name}: check");
        directory.write(name, &configuration_yaml(scenario))?;
        check_configuration(&self.binary, &directory.path, name, &self.stage)?;

        // Preparation and check can take time. Refresh history and all preexisting IDs
        // immediately before load, refusing activity during preparation too.
        let before_load = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before_load.window_events, None)?;
        require_empty(before_load.focused_workspace())?;
        progress.preexisting_ids = Some(
            walk(&before_load.tree)
                .iter()
                .map(|placed| placed.node.id)
                .collect(),
        );
        let preexisting_ids = progress
            .preexisting_ids
            .as_ref()
            .expect("just recorded IDs");
        let target = self
            .disposable
            .as_ref()
            .expect("successful switch established target");

        self.stage = format!("{name}: load");
        interruption_check()?;
        let mut load = configured_command(&self.binary, &directory.path, "load", name);
        self.sway.select_socket(&mut load);
        let output = process::run(load, &self.stage, load_timeout(scenario))?;
        if !output.trim().is_empty() {
            return Err(Failure::Check("load produced unexpected stdout".into()));
        }

        self.stage = format!("{name}: raw-tree verification");
        let loaded = snapshot(&self.sway, &mut self.events, None)?;
        progress.accepted =
            bind_launches(&loaded.window_events, &scenario.layout, preexisting_ids)?;
        verify_ownership(
            &loaded.tree,
            target.id,
            &scenario.layout,
            preexisting_ids,
            &progress.accepted,
        )?;
        let measurements = verify_layout(loaded.focused_workspace(), scenario, &progress.accepted)?;
        let probe_pid = if name == "process-execution" {
            self.stage = format!("{name}: child process verification");
            let leaf = scenario.layout.leaves()[0];
            let window_id = *progress
                .accepted
                .keys()
                .next()
                .expect("verified single window");
            let terminal = walk(&loaded.tree)
                .into_iter()
                .find(|placed| placed.node.id == window_id)
                .expect("ownership verified window")
                .node;
            let terminal_pid = terminal
                .pid
                .ok_or_else(|| Failure::Uncertain("process scenario has no terminal PID".into()))?;
            Some(
                process::verify_probe(Path::new("/proc"), terminal_pid, leaf, &directory.path)
                    .map_err(Failure::Uncertain)?,
            )
        } else {
            None
        };
        let before_capture = signature(loaded.focused_workspace());

        self.stage = format!("{name}: capture");
        let mut capture = Command::new(&self.binary);
        capture.arg("capture");
        self.sway.select_socket(&mut capture);
        let draft = process::run(capture, &self.stage, CAPTURE_TIMEOUT)?;
        let after_capture = snapshot(&self.sway, &mut self.events, None)?;
        verify_unchanged(&after_capture, target.id, progress, &before_capture)?;
        let filled = verify_capture(&draft, &progress.scenario, &measurements)?;
        directory.write("captured", &filled)?;
        check_configuration(
            &self.binary,
            &directory.path,
            "captured",
            "check filled capture",
        )?;

        if matches!(name, "single-window" | "horizontal-pair") {
            self.stage = format!("{name}: inferred capture");
            let mut capture = Command::new(&self.binary);
            capture.args(["capture", "--infer-commands"]);
            self.sway.select_socket(&mut capture);
            let inferred = process::run(capture, &self.stage, CAPTURE_TIMEOUT)?;
            let after_inferred = snapshot(&self.sway, &mut self.events, None)?;
            verify_unchanged(&after_inferred, target.id, progress, &before_capture)?;
            verify_inferred_capture(&inferred, &draft, &progress.scenario.layout)?;
        }

        self.stage = format!("{name}: cleanup");
        let context = CleanupContext {
            target,
            layout: &progress.scenario.layout,
            preexisting_ids: progress
                .preexisting_ids
                .as_ref()
                .expect("recorded before load"),
        };
        cleanup(
            &self.sway,
            &mut self.events,
            &context,
            &progress.accepted,
            before_capture,
        )?;
        let cleaned = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&cleaned.window_events, None)?;
        require_empty(cleaned.focused_workspace())?;
        if let Some(pid) = probe_pid {
            process::wait_probe_exit(pid)?;
        }
        interruption_check()?;

        self.passed += 1;
        println!("PASS  {name:19} hierarchy, sizing, focus, identity, capture");
        for note in measurements.sizing_notes {
            println!("      above 1-point sizing target: {note}");
        }
        self.stage = "scenario preparation".into();
        Ok(())
    }

    fn restore(&mut self) -> Result<&'static str, Failure> {
        interruption_check()?;
        let (Some(original), Some(disposable)) = (&self.original, &self.disposable) else {
            return Ok(LOST_CONTROL);
        };
        if !controls_target(
            self.events.focus_history_trusted(),
            self.events.current_workspace(),
            disposable,
        ) {
            return Ok(LOST_CONTROL);
        }

        let observed = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&observed.window_events, None)?;
        if let Some(reason) = restoration_refusal(
            &observed,
            original,
            disposable,
            self.events.focus_history_trusted(),
        ) {
            return Ok(reason);
        }
        interruption_check()?;
        self.sway.restore_workspace(&original.name)?;
        let transition = ExpectedSwitch {
            from_id: disposable.id,
            destination_name: original.name.clone(),
        };
        let restored = snapshot(&self.sway, &mut self.events, Some(&transition))?;
        if restored.focused_workspace_id != original.id {
            return Err(Failure::Uncertain(
                "restored workspace has a different identity".into(),
            ));
        }
        Ok(RESTORED)
    }
}

fn interruption_check() -> Result<(), Failure> {
    process::check_interruption().map_err(Failure::Uncertain)
}

struct Preflight {
    original: WorkspaceIdentity,
    designated_id: Option<i64>,
}

fn preflight(sway: &Swaymsg, events: &mut Events, number: &str) -> Result<Preflight, Failure> {
    interruption_check()?;
    let observed = snapshot(sway, events, None)?;
    require_quiet(&observed.window_events, None)?;
    let eligibility = preflight_eligibility(&observed, number)?;
    let original = observed
        .workspaces
        .iter()
        .find(|workspace| workspace.id == eligibility.original.id)
        .expect("validated snapshot contains the focused workspace");
    events.set_current_workspace(original);
    Ok(eligibility)
}

/// Pure eligibility rules over an already validated, synchronized snapshot.
fn preflight_eligibility(observed: &Snapshot, number: &str) -> Result<Preflight, String> {
    let number = workspace_number(number)?;
    let numeric_workspace: i32 = number
        .parse()
        .expect("validated nine-digit workspace number");
    let original = observed.focused_workspace();
    let original_name = original
        .name
        .as_deref()
        .ok_or("starting workspace has no name")?;
    if original_name == number
        || observed
            .workspaces
            .iter()
            .any(|workspace| workspace.id == original.id && workspace.num == numeric_workspace)
    {
        return Err("disposable workspace is the starting workspace".into());
    }
    validate_restoration_name(original_name)?;

    let candidates: Vec<_> = observed
        .workspaces
        .iter()
        .filter(|workspace| workspace.name == number || workspace.num == numeric_workspace)
        .collect();
    if candidates.len() > 1 {
        return Err("ambiguous disposable workspace number".into());
    }
    let designated_id = if let Some(designated) = candidates.first() {
        if designated.name != number {
            return Err(
                "workspace number belongs to a differently named workspace; refusing".into(),
            );
        }
        let workspace = walk(&observed.tree)
            .into_iter()
            .map(|placed| placed.node)
            .find(|node| node.id == designated.id)
            .ok_or("designated workspace missing from tree")?;
        require_empty(workspace)?;
        Some(designated.id)
    } else {
        None
    };
    Ok(Preflight {
        original: WorkspaceIdentity {
            id: original.id,
            name: original_name.to_owned(),
        },
        designated_id,
    })
}

fn require_empty(workspace: &TreeNode) -> Result<(), String> {
    if !workspace.nodes.is_empty() || !workspace.floating_nodes.is_empty() {
        return Err(format!(
            "workspace {:?} is occupied: {}",
            workspace.name,
            outline(workspace)
        ));
    }
    Ok(())
}

fn controls_target(
    trusted: bool,
    current: Option<&WorkspaceIdentity>,
    target: &WorkspaceIdentity,
) -> bool {
    trusted && current == Some(target)
}

fn restoration_refusal(
    observed: &Snapshot,
    original: &WorkspaceIdentity,
    disposable: &WorkspaceIdentity,
    trusted: bool,
) -> Option<&'static str> {
    let focused = observed.focused_workspace();
    if !trusted || focused.id != disposable.id || focused.name.as_deref() != Some(&disposable.name)
    {
        return Some(LOST_CONTROL);
    }
    let matches = observed
        .workspaces
        .iter()
        .filter(|workspace| workspace.id == original.id && workspace.name == original.name)
        .count();
    if matches != 1 {
        return Some(ORIGINAL_GONE);
    }
    None
}

fn verify_unchanged(
    observed: &Snapshot,
    target_id: i64,
    progress: &ScenarioProgress,
    baseline: &Signature,
) -> Result<(), Failure> {
    require_quiet(&observed.window_events, None)?;
    verify_ownership(
        &observed.tree,
        target_id,
        &progress.scenario.layout,
        progress
            .preexisting_ids
            .as_ref()
            .expect("recorded before load"),
        &progress.accepted,
    )?;
    require_signature(
        observed.focused_workspace(),
        baseline,
        "capture changed the workspace",
    )
}

fn require_signature(
    workspace: &TreeNode,
    baseline: &Signature,
    message: &str,
) -> Result<(), Failure> {
    if signature(workspace) != *baseline {
        return Err(Failure::Uncertain(message.to_owned()));
    }
    Ok(())
}

struct CleanupContext<'a> {
    target: &'a WorkspaceIdentity,
    layout: &'a ConfiguredNode,
    preexisting_ids: &'a HashSet<i64>,
}

#[derive(Debug)]
struct ClosePlan {
    remaining: AcceptedWindows,
    hierarchy: Option<Shape>,
}

/// Every destructive decision is computed from fresh evidence, before issuing a close.
fn prepare_close(
    observed: &Snapshot,
    context: &CleanupContext,
    remaining: &AcceptedWindows,
    baseline: &Signature,
    closing_id: i64,
    trusted: bool,
) -> Result<ClosePlan, Failure> {
    let workspace = observed.focused_workspace();
    if !trusted
        || workspace.id != context.target.id
        || workspace.name.as_deref() != Some(&context.target.name)
    {
        return Err(Failure::Uncertain(
            "cleanup requires trusted target workspace focus".into(),
        ));
    }
    require_quiet(&observed.window_events, None)?;
    verify_ownership(
        &observed.tree,
        context.target.id,
        context.layout,
        context.preexisting_ids,
        remaining,
    )?;
    require_signature(
        workspace,
        baseline,
        "workspace changed before cleanup; ownership/state uncertain",
    )?;
    if !remaining.contains_key(&closing_id) || closing_id <= 0 {
        return Err(Failure::Uncertain(
            "close requires an individually verified owned window".into(),
        ));
    }
    let hierarchy = remove_from_shape(shape(workspace)?, closing_id);
    let mut expected_remaining = remaining.clone();
    expected_remaining.remove(&closing_id);
    Ok(ClosePlan {
        remaining: expected_remaining,
        hierarchy,
    })
}

fn cleanup(
    sway: &Swaymsg,
    events: &mut Events,
    context: &CleanupContext,
    accepted: &AcceptedWindows,
    mut baseline: Signature,
) -> Result<(), Failure> {
    let mut remaining = accepted.clone();
    // Accepted windows are sorted by container ID, which need not follow launch order.
    // Derive close order from configured leaves so windows always close in launch order.
    for leaf in context.layout.leaves() {
        let closing_id = accepted
            .iter()
            .find(|(_, layout_id)| **layout_id == leaf.layout_id)
            .map(|(&id, _)| id)
            .ok_or_else(|| Failure::Uncertain("missing accepted cleanup window".into()))?;
        interruption_check()?;
        let before = snapshot(sway, events, None)?;
        let plan = prepare_close(
            &before,
            context,
            &remaining,
            &baseline,
            closing_id,
            events.focus_history_trusted(),
        )?;

        // The sole destructive desktop command. IPC cannot make the guard/command atomic.
        interruption_check()?;
        sway.close_owned_window(closing_id)?;
        let deadline = Instant::now() + IPC_TIMEOUT;
        let after = loop {
            interruption_check()?;
            let observed = snapshot(sway, events, None)?;
            if verify_close_observation(
                &observed, context, &remaining, &plan, &baseline, closing_id,
            )? {
                break observed;
            }
            if Instant::now() >= deadline {
                return Err(Failure::Check(format!(
                    "cleanup timeout: container {closing_id} did not disappear"
                )));
            }
            thread::sleep(CLOSE_POLL_INTERVAL);
        };

        // Advance evidence only after disappearance, ownership, and hierarchy agree.
        remaining = plan.remaining;
        baseline = signature(after.focused_workspace());
    }
    Ok(())
}

/// Accept disappearance only with the expected close/focus events, exact ownership of
/// survivors, and the predicted hierarchy. A pending close must leave the baseline intact.
fn verify_close_observation(
    observed: &Snapshot,
    context: &CleanupContext,
    remaining: &AcceptedWindows,
    plan: &ClosePlan,
    baseline: &Signature,
    closing_id: i64,
) -> Result<bool, Failure> {
    let expected_close = ExpectedClose {
        closing_id,
        remaining: &plan.remaining,
    };
    require_quiet(&observed.window_events, Some(&expected_close))?;
    let disappeared = !walk(&observed.tree)
        .iter()
        .any(|placed| placed.node.id == closing_id);
    let expected_owned = if disappeared {
        &plan.remaining
    } else {
        remaining
    };
    verify_ownership(
        &observed.tree,
        context.target.id,
        context.layout,
        context.preexisting_ids,
        expected_owned,
    )?;

    let workspace = observed.focused_workspace();
    if !disappeared {
        require_signature(
            workspace,
            baseline,
            "workspace changed while waiting for close",
        )?;
        return Ok(false);
    }
    let observed_after = shape(workspace)?;
    if plan.remaining.is_empty() {
        require_empty(workspace)?;
    } else if Some(&observed_after) != plan.hierarchy.as_ref() {
        return Err(Failure::Uncertain(format!(
            "cleanup hierarchy changed: expected {:?}, observed {observed_after:?}",
            plan.hierarchy
        )));
    }
    Ok(true)
}

fn report_remaining(sway: &Swaymsg, run_id: &str, progress: Option<&ScenarioProgress>) {
    eprintln!(
        "No further cleanup attempted. Test terminals expire after 300 seconds; Firefox has no lifetime limit."
    );
    let empty = AcceptedWindows::new();
    let known = progress.map_or(&empty, |progress| &progress.accepted);
    eprintln!("  Last associated container IDs -> layout IDs: {known:?}");
    let tree = match sway.remaining_tree() {
        Ok(tree) => tree,
        Err(error) => {
            eprintln!("  Remaining windows could not be queried: {error}; last accepted={known:?}");
            return;
        }
    };
    let leaves = progress
        .map(|progress| progress.scenario.layout.leaves())
        .unwrap_or_default();
    let prefix = format!("sway-stage-it-{run_id}-");
    for placed in walk(&tree) {
        let window = placed.node;
        let matches = leaves
            .iter()
            .any(|leaf| observed_identifier(window, &leaf.selector) == Some(leaf.selector.value()));
        let newly_observed = progress
            .and_then(|progress| progress.preexisting_ids.as_ref())
            .is_some_and(|preexisting| !preexisting.contains(&window.id) && is_window(window));
        let run_identifier = [window.app_id.as_deref(), class(window)]
            .into_iter()
            .flatten()
            .any(|identifier| identifier.starts_with(&prefix));
        if known.contains_key(&window.id) || matches || newly_observed || run_identifier {
            let workspace_name = placed
                .workspace
                .and_then(|workspace| workspace.name.as_deref());
            eprintln!(
                "  observed id={} app_id={:?} class={:?} pid={:?} workspace={workspace_name:?} unsafe_state={} (candidate only; ownership not implied)",
                window.id,
                window.app_id,
                class(window),
                window.pid,
                placed.unsafe_state
            );
        }
    }
}

fn load_timeout(scenario: &Scenario) -> Duration {
    Duration::from_millis(scenario.layout.leaves().len() as u64 * scenario.window_timeout_ms)
        + Duration::from_secs(30)
}

fn configured_command(binary: &Path, directory: &Path, operation: &str, name: &str) -> Command {
    let mut command = Command::new(binary);
    command
        .arg("--layouts-dir")
        .arg(directory)
        .arg(operation)
        .arg(name);
    command
}

fn check_configuration(
    binary: &Path,
    directory: &Path,
    name: &str,
    stage: &str,
) -> Result<(), String> {
    process::run(
        configured_command(binary, directory, "check", name),
        stage,
        IPC_TIMEOUT,
    )?;
    Ok(())
}

/// Foot and `sleep` keep the path found on `PATH`, unresolved, because a multi-call
/// executable such as BusyBox chooses its applet from the name it is invoked by.
fn unresolved_executable_text(name: &str, search_path: &OsStr) -> Result<String, String> {
    let path = process::find_in_search_path(name, search_path)
        .ok_or_else(|| format!("required executable not found or not executable: {name}"))?;
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("executable path is not UTF-8: {}", path.display()))
}

fn random_run_id() -> Result<String, String> {
    let mut bytes = [0; 12];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("read random run ID: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Owns private YAML files and the process test cwd, never profiles or desktop windows.
struct TemporaryConfigurations {
    path: PathBuf,
}

impl TemporaryConfigurations {
    fn new(parent: &Path, run_id: &str) -> Result<Self, String> {
        let parent =
            path::absolute(parent).map_err(|error| format!("temporary directory: {error}"))?;
        let path = parent.join(format!("sway-stage-integration-{run_id}"));
        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|error| {
                format!("create private YAML directory {}: {error}", path.display())
            })?;
        Ok(Self { path })
    }

    fn create_process_cwd(&self) -> Result<(), String> {
        DirBuilder::new()
            .mode(0o700)
            .create(self.path.join("process cwd"))
            .map_err(|error| format!("create private process cwd: {error}"))
    }

    fn write(&self, name: &str, contents: &str) -> Result<(), String> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        {
            return Err("invalid temporary configuration name".into());
        }
        let path = self.path.join(format!("{name}.yaml"));
        // All writes stay in our exclusively created private directory. Refuse unexpected
        // entries, including symlinks, before replacing a standard or captured draft.
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_file() => {
                return Err("unexpected temporary configuration entry".into());
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("inspect temporary YAML: {error}"));
            }
            _ => {}
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("open temporary YAML {}: {error}", path.display()))?;
        file.write_all(contents.as_bytes())
            .map_err(|error| format!("write temporary YAML {}: {error}", path.display()))
    }
}

impl Drop for TemporaryConfigurations {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            eprintln!(
                "Could not remove temporary YAML directory {}: {error}",
                self.path.display()
            );
        }
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
