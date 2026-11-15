//! The scenario runner: a foreign fixture, then the selected scenarios of one output setup
//! in suite order, each with verified success-path cleanup. It runs only as the launcher's child inside the
//! private headless session's environment and refuses any compositor that is not a fresh
//! headless one. A failure stops the run and leaves every window to the compositor's
//! shutdown; files and the IPC helper may be dropped, never windows.

use std::collections::{BTreeMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{self, Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::capture::{executable_capture, verify_capture, verify_inferred_capture};
use crate::headless::{
    OUTPUT_NAME, OutputSetup, SECOND_OUTPUT_ASSIGNED_WORKSPACE, SECOND_OUTPUT_INITIAL_WORKSPACE,
    SECOND_OUTPUT_NAME, is_session_ipc_socket, verify_virtual_outputs,
};
use crate::ipc::{IPC_TIMEOUT, InitialWorkspaceLayout, Output, Swaymsg, TreeNode, Workspace};
use crate::layout::{
    Shape, Signature, is_window, outline, remove_from_shape, shape, signature, verify_layout,
};
use crate::observation::{
    Events, ExpectedSwitch, Snapshot, WorkspaceIdentity, snapshot, snapshot_with_switch, walk,
};
use crate::ownership::{
    AcceptedWindows, ExpectedClose, bind_launches, require_quiet, verify_ownership,
};
use crate::preservation::{ForeignWindows, output_ancestry};
use crate::scenarios::{
    ConfiguredNode, Scenario, configuration_yaml, configure_process_execution,
    foreign_fixture_scenario, standard_scenarios,
};
use crate::{process, profile};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);
const CLOSE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A fresh compositor's only workspace. The foreign fixture loads here without a switch and
/// stays for the whole run, so it is also the occupied target of the profile refusal.
const FIXTURE_WORKSPACE: &str = "1";
/// The target of every standard scenario and the first target of every profile.
const PRIMARY_WORKSPACE: &str = "2";
/// Each profile's second target. Every profile leaves it empty and returns to the primary,
/// so Sway removes it and the next profile load creates it again.
const PROFILE_WORKSPACE: &str = "3";

/// Every scenario in suite order: the standard scenarios, then the profile cases, then the
/// two-output session's cross-output profile.
pub fn suite_scenario_names() -> Vec<&'static str> {
    // Scenario definitions are pure, so placeholder executables suffice for their names.
    let standard = standard_scenarios("names-only", "foot", "sleep");

    let mut names: Vec<&'static str> = standard.iter().map(|scenario| scenario.name).collect();
    names.extend(profile::CASES.iter().map(|case| case.name()));
    names.push(profile::CROSS_OUTPUT_SCENARIO);
    names
}

/// The session a scenario runs in. Only the cross-output profile needs a second output.
pub fn output_setup(scenario_name: &str) -> OutputSetup {
    if scenario_name == profile::CROSS_OUTPUT_SCENARIO {
        OutputSetup::TwoOutputs
    } else {
        OutputSetup::OneOutput
    }
}

/// The scenarios whose names contain `filter`, in suite order; every scenario without one.
pub fn select_scenarios(filter: Option<&str>) -> Result<Vec<&'static str>, String> {
    let selected: Vec<&'static str> = suite_scenario_names()
        .into_iter()
        .filter(|name| filter.is_none_or(|text| name.contains(text)))
        .collect();

    if selected.is_empty() {
        return Err(format!(
            "--filter {:?} matches no scenario; scenarios are: {}",
            filter.unwrap_or_default(),
            suite_scenario_names().join(", ")
        ));
    }
    Ok(selected)
}

/// The scenario being run and the ownership evidence of its current load cycle.
struct ScenarioProgress {
    scenario: Scenario,
    preexisting_ids: Option<HashSet<i64>>,
    accepted: AcceptedWindows,
}

impl ScenarioProgress {
    /// Called only after guarded cleanup succeeds, never from a failure handler.
    fn prepare_replay(
        &mut self,
        cleaned: &Snapshot,
        foreign: &ForeignWindows,
        replay: Scenario,
        retired_ids: &mut HashSet<i64>,
    ) -> Result<(), String> {
        cleaned.validate()?;
        foreign.verify(cleaned)?;
        require_quiet(&cleaned.window_events, None)?;
        require_empty(cleaned.focused_workspace())?;

        retired_ids.extend(self.accepted.keys().copied());
        self.accepted.clear();
        self.preexisting_ids = None;
        self.scenario = replay;
        Ok(())
    }

    /// Record a synchronized empty baseline for each load. Never carry accepted IDs
    /// into another load, even when its selectors and launch commands agree.
    fn begin_cycle(&mut self, before: &Snapshot, retired_ids: &HashSet<i64>) -> Result<(), String> {
        require_quiet(&before.window_events, None)?;
        require_empty(before.focused_workspace())?;
        self.accepted.clear();
        let mut preexisting_ids: HashSet<i64> = walk(&before.tree)
            .iter()
            .map(|placed| placed.node.id)
            .collect();
        // A replay must create new containers; retired IDs cannot prove a new launch.
        preexisting_ids.extend(retired_ids);
        self.preexisting_ids = Some(preexisting_ids);
        Ok(())
    }
}

struct ForeignFixture {
    workspace: WorkspaceIdentity,
    progress: ScenarioProgress,
    foreign: ForeignWindows,
    loaded_signature: Option<Signature>,
}

struct Harness {
    binary: PathBuf,
    foot: String,
    sleep: String,
    sway: Swaymsg,
    events: Events,
    run_id: String,
    /// Recorded by the switch to `PRIMARY_WORKSPACE`, before any scenario.
    primary: Option<WorkspaceIdentity>,
    current_scenario: Option<ScenarioProgress>,
    fixture: Option<ForeignFixture>,
    stage: String,
    passed: usize,
}

/// Runs the selected scenarios of `setup`'s session in suite order, stopping at the first
/// failure. Returns whether every one of them passed.
pub fn run(setup: OutputSetup, filter: Option<&str>) -> bool {
    let selected: Vec<&'static str> = match select_scenarios(filter) {
        Ok(selected) => selected
            .into_iter()
            .filter(|name| output_setup(name) == setup)
            .collect(),
        Err(error) => {
            eprintln!("FAIL  selection: {error}");
            return false;
        }
    };
    if selected.is_empty() {
        eprintln!("FAIL  selection: no selected scenario runs in the {setup:?} session");
        return false;
    }

    let mut harness = match Harness::new(setup) {
        Ok(harness) => harness,
        Err(error) => {
            eprintln!("FAIL  setup: {error}");
            println!("\n0 of {} selected scenarios passed.", selected.len());
            return false;
        }
    };

    let execution = match setup {
        OutputSetup::OneOutput => harness.execute(&selected),
        OutputSetup::TwoOutputs => harness.execute_two_outputs(),
    };
    let result = execution.and_then(|()| process::check_interruption());
    let mut failed = result.is_err();
    if let Err(error) = result {
        // Every failure ends the run, whatever its cause, including an interrupt seen only
        // by a process helper. Distrusted history refuses any later fence or cleanup.
        let message = harness.events.distrust_focus_history(error);
        eprintln!("FAIL  {}: {message}", harness.stage);
    }

    // Explicit bounded helper shutdown on every outcome. This stops only our IPC helper;
    // remaining windows end with the compositor.
    if let Err(error) = harness.events.close() {
        failed = true;
        eprintln!("FAIL  event helper shutdown: {error}");
    }
    if process::interruption_requested() && !failed {
        failed = true;
        eprintln!("FAIL  shutdown: interrupted");
    }

    println!(
        "\n{} of {} selected scenarios passed.",
        harness.passed,
        selected.len()
    );
    if failed {
        println!("The run stopped at the failure above.");
    }
    !failed
}

impl Harness {
    fn new(setup: OutputSetup) -> Result<Self, String> {
        process::install_sigint_handler()?;
        process::check_interruption()?;

        // The headless session sets SWAYSOCK to its private socket. A desktop session's
        // socket is refused here, before any connection; the private socket must then
        // prove that it reaches the virtual output before any subscription or command.
        let socket = PathBuf::from(env::var_os("SWAYSOCK").unwrap_or_default());
        if !is_session_ipc_socket(&socket) {
            return Err(format!(
                "not started by tools/integration/run.sh: SWAYSOCK {} is not a private \
                 headless session socket",
                socket.display()
            ));
        }
        let sway = Swaymsg::at_socket(socket)?;
        verify_virtual_outputs(&sway.get_outputs()?, setup)
            .map_err(|error| format!("not the private headless compositor: {error}"))?;
        if sway.seat_names()?.len() != 1 {
            return Err("this harness requires exactly one Sway seat".into());
        }

        let harness_executable =
            env::current_exe().map_err(|error| format!("locate the harness: {error}"))?;
        let sway_stage_path = sibling_sway_stage(&harness_executable)?;
        let sway_stage_text = sway_stage_path
            .to_str()
            .ok_or("sway-stage path is not UTF-8")?;
        let binary = process::find_executable(sway_stage_text)?;
        let search_path = env::var_os("PATH").unwrap_or_default();
        let foot = unresolved_executable_text("foot", &search_path)?;
        let sleep = unresolved_executable_text("sleep", &search_path)?;
        let run_id = random_run_id()?;

        // Establish subscription readiness before the first workspace observation.
        let events = Events::subscribe(&sway)?;

        Ok(Self {
            binary,
            foot,
            sleep,
            sway,
            events,
            run_id,
            primary: None,
            current_scenario: None,
            fixture: None,
            stage: "setup".into(),
            passed: 0,
        })
    }

    fn execute(&mut self, selected: &[&'static str]) -> Result<(), String> {
        self.stage = "fresh compositor".into();
        let initial = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&initial.window_events, None)?;
        let initial_workspace = verify_fresh_compositor(&initial)?;
        // From here on, every fence rejects a workspace focus change we did not request.
        self.events.set_current_workspace(initial_workspace);

        // Every standard layout is checked, selected or not: the occupied refusal loads the
        // `single-window` layout as its second, refused layout.
        self.stage = "configuration preparation".into();
        let directory = TemporaryConfigurations::new(&env::temp_dir(), &self.run_id)?;
        let mut scenarios = standard_scenarios(&self.run_id, &self.foot, &self.sleep);
        let helper =
            env::current_exe().map_err(|error| format!("locate process probe: {error}"))?;
        directory.create_process_cwd()?;
        for scenario in &mut scenarios {
            configure_process_execution(
                scenario,
                &helper,
                &directory.path.join("process cwd"),
                &self.run_id,
            )?;
            self.stage = format!("{}: initial check", scenario.name);
            directory.write(scenario.name, &configuration_yaml(scenario))?;
            check_configuration(&self.binary, &directory.path, scenario.name, &self.stage)?;
        }

        self.prepare_fixture(&directory)?;

        self.stage = "primary workspace switch".into();
        let fixture_workspace_id = self
            .fixture
            .as_ref()
            .expect("fixture prepared before the primary switch")
            .workspace
            .id;
        process::check_interruption()?;
        self.sway.switch_workspace(PRIMARY_WORKSPACE)?;
        let transition = ExpectedSwitch {
            from_id: fixture_workspace_id,
            destination_name: PRIMARY_WORKSPACE.into(),
        };
        let switched = snapshot(&self.sway, &mut self.events, Some(&transition))?;
        require_quiet(&switched.window_events, None)?;
        require_empty(switched.focused_workspace())?;
        self.primary = Some(WorkspaceIdentity {
            id: switched.focused_workspace_id,
            name: PRIMARY_WORKSPACE.into(),
        });
        let target_baseline = ForeignWindows::record(&switched, switched.focused_workspace_id)?;

        for scenario in scenarios {
            if !selected.contains(&scenario.name) {
                continue;
            }
            process::check_interruption()?;
            self.current_scenario = Some(ScenarioProgress {
                scenario,
                preexisting_ids: None,
                accepted: AcceptedWindows::new(),
            });
            self.run_scenario(&directory, &target_baseline)?;
        }
        self.current_scenario = None;

        for case in profile::CASES {
            if !selected.contains(&case.name()) {
                continue;
            }
            if case == profile::Case::NamedSuccess {
                self.run_named_profile(&directory)?;
            } else {
                self.run_profile(case, &directory)?;
            }
        }

        self.verify_fixture_survived()
    }

    /// The two-output session: the same foreign fixture on workspace "1" of the first
    /// output, then the cross-output profile, which is its only scenario.
    fn execute_two_outputs(&mut self) -> Result<(), String> {
        self.stage = "fresh two-output compositor".into();
        let initial = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&initial.window_events, None)?;
        let initial_workspace =
            verify_fresh_two_output_compositor(&initial, &self.sway.get_outputs()?)?;
        // From here on, every fence rejects a workspace focus change we did not request.
        self.events.set_current_workspace(initial_workspace);

        self.stage = "configuration preparation".into();
        let directory = TemporaryConfigurations::new(&env::temp_dir(), &self.run_id)?;
        self.prepare_fixture(&directory)?;
        self.run_cross_output_profile(&directory)?;
        self.verify_fixture_survived()
    }

    /// Selects a workspace that Sway's rules create on the second output, from the fixture
    /// workspace on the first. That hides the second output's empty initial workspace, and
    /// Sway removes it: the regression is demonstrated only if that removal is observed.
    fn run_cross_output_profile(
        &mut self,
        directory: &TemporaryConfigurations,
    ) -> Result<(), String> {
        let name = profile::CROSS_OUTPUT_SCENARIO;
        let target_name = SECOND_OUTPUT_ASSIGNED_WORKSPACE;
        self.stage = format!("{name}: preparation");
        let files = directory.subdirectory(name)?;
        let layouts_directory = files.subdirectory("layouts")?;
        let profiles_directory = files.subdirectory("profiles")?;
        let script_path = files.path.join("record-launch.sh");
        let record_path = files.path.join("launches");
        fs::write(&script_path, profile::LAUNCH_SCRIPT)
            .map_err(|error| format!("write profile launch recorder: {error}"))?;
        let layout = profile::cross_output_layout(
            &self.run_id,
            &self.foot,
            &self.sleep,
            script_path
                .to_str()
                .ok_or_else(|| "profile script path is not UTF-8".to_owned())?,
            record_path
                .to_str()
                .ok_or_else(|| "profile record path is not UTF-8".to_owned())?,
        );
        layouts_directory.write(layout.name, &configuration_yaml(&layout))?;
        profiles_directory.write(name, &profile::cross_output_configuration())?;
        let profile_command = |operation: &str| {
            let mut command = Command::new(&self.binary);
            command
                .arg("--layouts-dir")
                .arg(&layouts_directory.path)
                .arg("--profiles-dir")
                .arg(&profiles_directory.path)
                .args(["profile", operation, name]);
            self.sway.select_socket(&mut command);
            command
        };
        let checked = process::run(profile_command("check"), &self.stage, IPC_TIMEOUT)?;
        if checked != format!("{name}: valid\n") {
            return Err("unexpected profile check output".into());
        }

        self.stage = format!("{name}: fresh workspace baselines");
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        let fixture = self.fixture.as_ref().expect("foreign fixture prepared");
        let fixture_signature = fixture.loaded_signature.as_ref().expect("fixture verified");
        verify_fixture_state(&before, fixture, fixture_signature)?;
        let baseline = profile::CrossOutputBaseline::record(
            &before,
            &self.sway.get_outputs()?,
            &fixture.workspace,
        )?;
        let preexisting: HashSet<i64> = walk(&before.tree)
            .iter()
            .map(|placed| placed.node.id)
            .collect();

        self.stage = format!("{name}: profile load");
        process::check_interruption()?;
        let completed =
            process::run_with_status(profile_command("load"), &self.stage, load_timeout(&layout))?;
        process::check_interruption()?;
        if !completed.status.success() || !completed.stdout.trim().is_empty() {
            return Err(format!(
                "profile success failed: {}: {}",
                completed.status, completed.stderr
            ));
        }
        let transition = ExpectedSwitch {
            from_id: fixture.workspace.id,
            destination_name: target_name.into(),
        };
        let (loaded, switch_index) =
            snapshot_with_switch(&self.sway, &mut self.events, &transition)?;

        self.stage = format!("{name}: independent verification");
        let target =
            profile::Target::record(&loaded, target_name)?.ok_or("cross-output target missing")?;
        if preexisting.contains(&target.workspace.id) {
            return Err("created workspace reused a preexisting ID".into());
        }
        let expected_events = [
            profile::ExpectedWorkspaceEvent::Init(&target.workspace),
            profile::ExpectedWorkspaceEvent::Focus {
                from_id: fixture.workspace.id,
                to: &target.workspace,
            },
            profile::ExpectedWorkspaceEvent::Empty(&baseline.hidden_workspace),
        ];
        profile::verify_workspace_events(&loaded.workspace_events, &expected_events).map_err(
            |error| {
                format!(
                    "{error}; Sway's removal of the hidden empty workspace was not demonstrated"
                )
            },
        )?;
        baseline.verify_result(&loaded, &target.workspace, fixture.workspace.id)?;
        verify_fixture_state(&loaded, fixture, fixture_signature)?;

        // Nothing may launch before the switch; afterwards, only this layout's windows.
        let (before_switch, after_switch) = loaded.window_events.split_at(switch_index);
        require_quiet(before_switch, None)?;
        let accepted = bind_launches(after_switch, &layout.layout, &preexisting)?;
        verify_ownership(
            &loaded.tree,
            target.workspace.id,
            &layout.layout,
            &preexisting,
            &accepted,
        )?;
        let measurements = verify_layout(target.verify(&loaded)?, &layout, &accepted)?;
        for note in &measurements.sizing_notes {
            println!("      {note}");
        }
        let expected_record: String = layout
            .layout
            .leaves()
            .iter()
            .map(|leaf| format!("{}-{}\n", layout.name, leaf.layout_id))
            .collect();
        let observed_record = fs::read_to_string(&record_path)
            .map_err(|error| format!("read launch record: {error}"))?;
        if observed_record != expected_record {
            return Err(format!(
                "profile launch order/count differs: expected {expected_record:?}, observed {observed_record:?}"
            ));
        }
        println!(
            "      workspace {target_name} id={} created on {SECOND_OUTPUT_NAME}; Sway removed the hidden empty workspace {} id={}; fixture workspace stayed on {OUTPUT_NAME}",
            target.workspace.id, baseline.hidden_workspace.name, baseline.hidden_workspace.id
        );

        // Only an independently verified expected outcome enters cleanup.
        self.stage = format!("{name}: verified cleanup");
        let foreign = ForeignWindows::record(&loaded, target.workspace.id)?;
        let context = CleanupContext {
            target: &target.workspace,
            layout: &layout.layout,
            preexisting_ids: &preexisting,
            foreign: &foreign,
        };
        cleanup(
            &self.sway,
            &mut self.events,
            &context,
            &accepted,
            signature(loaded.focused_workspace()),
        )?;
        let cleaned = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&cleaned.window_events, None)?;
        foreign.verify(&cleaned)?;
        target.verify(&cleaned)?;
        require_empty(cleaned.focused_workspace())?;

        self.passed += 1;
        println!("PASS  {name} (independent verification; success-path cleanup only)");
        Ok(())
    }

    fn run_named_profile(&mut self, directory: &TemporaryConfigurations) -> Result<(), String> {
        let original_name = self
            .primary
            .as_ref()
            .expect("primary selected")
            .name
            .clone();
        let named = format!("stage-{} music", self.run_id);
        self.rename_empty_primary(&named)?;
        self.run_profile(profile::Case::NamedSuccess, directory)?;
        // Name restoration is an explicit verified success phase, never a failure handler.
        self.rename_empty_primary(&original_name)?;
        self.passed += 1;
        println!(
            "PASS  profile-named-success (independent verification; cleanup and original name verified)"
        );
        Ok(())
    }

    fn rename_empty_primary(&mut self, destination: &str) -> Result<(), String> {
        self.stage = "profile named target: guarded primary rename".into();
        let before = snapshot(&self.sway, &mut self.events, None)?;
        let primary = self.primary.as_ref().expect("primary selected");
        profile::verify_rename_preparation(&before, primary, destination)?;
        let foreign = ForeignWindows::record(&before, primary.id)?;
        let renamed = WorkspaceIdentity {
            id: primary.id,
            name: destination.into(),
        };
        println!(
            "      renaming primary id={} {:?} to {:?}",
            primary.id, primary.name, destination
        );
        process::check_interruption()?;
        self.sway.rename_workspace(&primary.name, destination)?;
        let after =
            crate::observation::snapshot_after_rename(&self.sway, &mut self.events, &renamed)?;
        profile::verify_rename_result(&before, &after, &renamed)?;
        foreign.verify(&after)?;
        self.primary = Some(renamed);
        Ok(())
    }

    fn run_profile(
        &mut self,
        case: profile::Case,
        directory: &TemporaryConfigurations,
    ) -> Result<(), String> {
        let name = case.name();
        let second_number = PROFILE_WORKSPACE;
        // The refusal names the still-occupied fixture workspace as its second target.
        let destination = if case == profile::Case::Occupied {
            FIXTURE_WORKSPACE
        } else {
            second_number
        };
        self.stage = format!("{name}: preparation");
        let files = directory.subdirectory(name)?;
        let layouts_directory = files.subdirectory("layouts")?;
        let profiles_directory = files.subdirectory("profiles")?;
        let script_path = files.path.join("record-launch.sh");
        let record_path = files.path.join("launches");
        fs::write(&script_path, profile::LAUNCH_SCRIPT)
            .map_err(|error| format!("write profile launch recorder: {error}"))?;
        let tick_burst = if case == profile::Case::EventBurst {
            let burst_script_path = files.path.join("tick-burst.sh");
            fs::write(&burst_script_path, profile::TICK_BURST_SCRIPT)
                .map_err(|error| format!("write profile tick burst: {error}"))?;
            let utf8 = |path: &Path| {
                path.to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("tick burst path {} is not UTF-8", path.display()))
            };
            profile::tick_burst_command(
                &utf8(&burst_script_path)?,
                &utf8(self.sway.executable())?,
                &utf8(self.sway.socket())?,
                &self.sleep,
            )
        } else {
            Vec::new()
        };
        let layouts = profile::layouts(
            case,
            &self.run_id,
            &self.foot,
            &self.sleep,
            script_path
                .to_str()
                .ok_or_else(|| "profile script path is not UTF-8".to_owned())?,
            record_path
                .to_str()
                .ok_or_else(|| "profile record path is not UTF-8".to_owned())?,
            &tick_burst,
        );
        for layout in &layouts {
            layouts_directory.write(layout.name, &configuration_yaml(layout))?;
        }
        profiles_directory.write(
            name,
            &profile::configuration(
                &self.primary.as_ref().expect("primary selected").name,
                destination,
            ),
        )?;
        let profile_command = |operation: &str| {
            let mut command = Command::new(&self.binary);
            command
                .arg("--layouts-dir")
                .arg(&layouts_directory.path)
                .arg("--profiles-dir")
                .arg(&profiles_directory.path)
                .args(["profile", operation, name]);
            self.sway.select_socket(&mut command);
            command
        };
        let checked = process::run(profile_command("check"), &self.stage, IPC_TIMEOUT)?;
        if checked != format!("{name}: valid\n") {
            return Err("unexpected profile check output".into());
        }
        let command = profile_command("load");

        self.stage = format!("{name}: fresh workspace baselines");
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        let primary = self
            .primary
            .as_ref()
            .expect("primary selected before scenarios");
        let primary_target = profile::Target::record(&before, &primary.name)?
            .ok_or("primary profile workspace disappeared")?;
        if before.focused_workspace_id != primary.id || primary_target.workspace != *primary {
            return Err("profile requires the original primary target focused".into());
        }
        require_empty(before.focused_workspace())?;
        let fixture = self.fixture.as_ref().expect("foreign fixture prepared");
        verify_fixture_state(
            &before,
            fixture,
            fixture.loaded_signature.as_ref().expect("fixture verified"),
        )?;
        fixture.foreign.verify(&before)?;
        let foreign = ForeignWindows::record(&before, primary.id)?;
        let second_before = if case == profile::Case::Occupied {
            None
        } else {
            // Normally absent, removed after the previous profile; if present, it must be
            // empty and keep its identity and output.
            let existing = profile::Target::record(&before, second_number)?;
            if let Some(existing) = &existing {
                require_empty(existing.verify(&before)?)?;
            }
            existing
        };
        let preexisting: HashSet<i64> = walk(&before.tree)
            .iter()
            .map(|placed| placed.node.id)
            .collect();
        let workspace_revision = self.events.workspace_revision();

        self.stage = format!("{name}: profile load");
        process::check_interruption()?;
        let timeout = load_timeout(&layouts[0]) + load_timeout(&layouts[1]);
        let completed = process::run_with_status(command, &self.stage, timeout)?;
        process::check_interruption()?;
        // Report an unexpected failure with sway-stage's own diagnostic, before requiring a
        // workspace switch that it may never have made.
        let expects_success = matches!(
            case,
            profile::Case::Success | profile::Case::NamedSuccess | profile::Case::EventBurst
        );
        if expects_success && !completed.status.success() {
            return Err(format!(
                "profile success failed: {}: {}",
                completed.status, completed.stderr
            ));
        }
        let (loaded, switch_index) = if case == profile::Case::Occupied {
            (snapshot(&self.sway, &mut self.events, None)?, None)
        } else {
            let transition = ExpectedSwitch {
                from_id: primary.id,
                destination_name: second_number.into(),
            };
            let (observed, switch_index) = crate::observation::snapshot_with_switch(
                &self.sway,
                &mut self.events,
                &transition,
            )?;
            (observed, Some(switch_index))
        };
        self.stage = format!("{name}: independent verification");
        foreign.verify(&loaded)?;
        verify_fixture_state(
            &loaded,
            fixture,
            fixture.loaded_signature.as_ref().expect("fixture verified"),
        )?;
        primary_target.verify(&loaded)?;
        verify_profile_launch_record(&record_path, &layouts, case)?;
        if case == profile::Case::Occupied {
            verify_profile_refusal(
                &completed,
                &before,
                &loaded,
                destination,
                fixture.workspace.id,
                workspace_revision != self.events.workspace_revision(),
            )?;
        } else {
            let partial = case == profile::Case::Partial;
            let accepted = profile::bind_ordered_launches(
                &loaded.window_events,
                switch_index.expect("profile switch observed"),
                &layouts,
                &preexisting,
                partial,
            )?;
            let second = profile::Target::record(&loaded, second_number)?
                .ok_or("second profile target missing")?;
            if let Some(expected) = &second_before {
                expected.verify(&loaded)?;
            } else if preexisting.contains(&second.workspace.id) {
                return Err("created workspace reused a preexisting ID".into());
            }
            if case == profile::Case::EventBurst {
                // However many ticks arrived, the only workspace changes are the one selection
                // and, when the target was absent, its creation.
                let switch = profile::ExpectedWorkspaceEvent::Focus {
                    from_id: primary.id,
                    to: &second.workspace,
                };
                let expected_events = if second_before.is_some() {
                    vec![switch]
                } else {
                    vec![
                        profile::ExpectedWorkspaceEvent::Init(&second.workspace),
                        switch,
                    ]
                };
                profile::verify_workspace_events(&loaded.workspace_events, &expected_events)?;
            }
            // A created target must belong to an output that existed before loading.
            let output = crate::preservation::output_ancestry(&loaded.tree, second.workspace.id)?;
            for (output_id, output_name) in &output {
                if !walk(&before.tree).iter().any(|placed| {
                    placed.node.node_type == crate::ipc::NodeType::Output
                        && placed.node.id == *output_id
                        && placed.node.name == *output_name
                }) {
                    return Err("profile target has an unexpected containing output".into());
                }
            }
            if loaded.focused_workspace_id != second.workspace.id {
                return Err("profile did not leave the second workspace focused".into());
            }
            for (layout, (target, owned)) in layouts
                .iter()
                .zip([(&primary_target, &accepted[0]), (&second, &accepted[1])])
            {
                verify_ownership(
                    &loaded.tree,
                    target.workspace.id,
                    &layout.layout,
                    &preexisting,
                    owned,
                )?;
            }
            let measurements = crate::layout::verify_inactive_layout(
                primary_target.verify(&loaded)?,
                &layouts[0],
                &accepted[0],
            )?;
            for note in measurements.sizing_notes {
                println!("      {note}");
            }
            if partial {
                require_empty(second.verify(&loaded)?)?;
                verify_profile_partial_failure(&completed, primary, second_number)?;
            } else {
                if !completed.status.success() || !completed.stdout.trim().is_empty() {
                    return Err(format!(
                        "profile success failed: {}: {}",
                        completed.status, completed.stderr
                    ));
                }
                let measurements =
                    verify_layout(second.verify(&loaded)?, &layouts[1], &accepted[1])?;
                for note in &measurements.sizing_notes {
                    println!("      {note}");
                }

                self.stage = format!("{name}: read-only profile capture");
                let captured_layouts = files.subdirectory("captured-layouts")?;
                let captured_profiles = files.subdirectory("captured-profiles")?;
                let before_capture = snapshot(&self.sway, &mut self.events, None)?;
                require_quiet(&before_capture.window_events, None)?;
                foreign.verify(&before_capture)?;
                if signature(&before_capture.tree) != signature(&loaded.tree) {
                    return Err("desktop changed before profile capture".into());
                }
                let capture_revision = self.events.workspace_revision();
                let mut command = Command::new(&self.binary);
                command
                    .arg("--layouts-dir")
                    .arg(&captured_layouts.path)
                    .arg("--profiles-dir")
                    .arg(&captured_profiles.path)
                    .args([
                        "profile",
                        "capture",
                        "captured",
                        &primary.name,
                        second_number,
                        "--no-infer-commands",
                    ]);
                self.sway.select_socket(&mut command);
                let output = process::run(command, &self.stage, CAPTURE_TIMEOUT)?;
                let after_capture = snapshot(&self.sway, &mut self.events, None)?;
                require_quiet(&after_capture.window_events, None)?;
                foreign.verify(&after_capture)?;
                if !output.is_empty()
                    || capture_revision != self.events.workspace_revision()
                    || signature(&before_capture.tree) != signature(&after_capture.tree)
                {
                    return Err(
                        "profile capture changed the observed desktop or emitted unexpected stdout"
                            .into(),
                    );
                }
                let profile_yaml = fs::read_to_string(captured_profiles.path.join("captured.yaml"))
                    .map_err(|error| format!("read captured profile: {error}"))?;
                profile::verify_captured_references(&profile_yaml, &primary.name, second_number)?;
                let first_measurements = crate::layout::verify_inactive_layout(
                    primary_target.verify(&after_capture)?,
                    &layouts[0],
                    &accepted[0],
                )?;
                let second_measurements =
                    verify_layout(second.verify(&after_capture)?, &layouts[1], &accepted[1])?;
                for (index, measurements) in
                    [first_measurements, second_measurements].iter().enumerate()
                {
                    let draft = fs::read_to_string(
                        captured_layouts
                            .path
                            .join(format!("captured-{:03}.yaml", index + 1)),
                    )
                    .map_err(|error| format!("read captured layout: {error}"))?;
                    verify_capture(&draft, &layouts[index], measurements)?;
                }
                println!(
                    "      profile capture: inactive focus, hierarchy, sizing, selections, references, unchanged desktop"
                );
            }
            println!(
                "      workspaces {} id={} and {} id={} outputs={output:?}; second target {}",
                primary.name,
                primary.id,
                second_number,
                second.workspace.id,
                if second_before.is_some() {
                    "identity preserved"
                } else {
                    "created"
                }
            );

            // Only an independently verified expected outcome enters cleanup. Freeze the
            // first layout as foreign state while cleaning the focused second target.
            self.stage = format!("{name}: verified second-target cleanup");
            let second_foreign = ForeignWindows::record(&loaded, second.workspace.id)?;
            if !partial {
                let context = CleanupContext {
                    target: &second.workspace,
                    layout: &layouts[1].layout,
                    preexisting_ids: &preexisting,
                    foreign: &second_foreign,
                };
                cleanup(
                    &self.sway,
                    &mut self.events,
                    &context,
                    &accepted[1],
                    signature(loaded.focused_workspace()),
                )?;
            }
            let cleaned = snapshot(&self.sway, &mut self.events, None)?;
            require_quiet(&cleaned.window_events, None)?;
            second_foreign.verify(&cleaned)?;
            foreign.verify(&cleaned)?;
            second.verify(&cleaned)?;
            require_empty(cleaned.focused_workspace())?;
            crate::layout::verify_inactive_layout(
                primary_target.verify(&cleaned)?,
                &layouts[0],
                &accepted[0],
            )?;

            self.stage = format!("{name}: return to primary and refresh cleanup evidence");
            process::check_interruption()?;
            self.sway.switch_workspace(&primary.name)?;
            let transition = ExpectedSwitch {
                from_id: second.workspace.id,
                destination_name: primary.name.clone(),
            };
            let returned = snapshot(&self.sway, &mut self.events, Some(&transition))?;
            require_return_focus(&returned.window_events, &accepted[0])?;
            profile::require_preserved_layout(
                primary_target.verify(&cleaned)?,
                primary_target.verify(&returned)?,
            )?;
            foreign.verify(&returned)?;
            verify_ownership(
                &returned.tree,
                primary.id,
                &layouts[0].layout,
                &preexisting,
                &accepted[0],
            )?;
            verify_layout(returned.focused_workspace(), &layouts[0], &accepted[0])?;
            // The empty second workspace may already be gone. Never require its stale ID
            // or reuse the second workspace's cleanup baseline for the primary workspace.
            let primary_foreign = ForeignWindows::record(&returned, primary.id)?;
            let context = CleanupContext {
                target: primary,
                layout: &layouts[0].layout,
                preexisting_ids: &preexisting,
                foreign: &primary_foreign,
            };
            self.stage = format!("{name}: verified primary cleanup");
            cleanup(
                &self.sway,
                &mut self.events,
                &context,
                &accepted[0],
                signature(returned.focused_workspace()),
            )?;
            let cleaned = snapshot(&self.sway, &mut self.events, None)?;
            primary_target.verify(&cleaned)?;
            foreign.verify(&cleaned)?;
            require_quiet(&cleaned.window_events, None)?;
            require_empty(cleaned.focused_workspace())?;
        }
        if case != profile::Case::NamedSuccess {
            self.passed += 1;
            println!("PASS  {name} (independent verification; success-path cleanup only)");
        }
        Ok(())
    }

    fn run_scenario(
        &mut self,
        directory: &TemporaryConfigurations,
        target_baseline: &ForeignWindows,
    ) -> Result<(), String> {
        let progress = self
            .current_scenario
            .as_mut()
            .expect("scenario loop set progress");
        let scenario = &progress.scenario;
        let name = scenario.name;
        self.stage = format!("{name}: empty-workspace precondition");
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        require_empty(before.focused_workspace())?;

        self.stage = format!("{name}: check");
        process::check_interruption()?;
        directory.write(name, &configuration_yaml(scenario))?;
        check_configuration(&self.binary, &directory.path, name, &self.stage)?;

        let mut replay_cycle = false;
        let mut retired_ids = HashSet::new();
        let mut previous_foreign: Option<ForeignWindows> = None;
        loop {
            let cycle_name = if replay_cycle {
                format!("{name}: replay")
            } else {
                format!("{name}: initial")
            };
            self.stage = format!("{cycle_name}: fresh baseline");
            // Preparation and check can take time. Refresh history and all preexisting IDs
            // immediately before load, refusing activity during preparation too.
            let mut before_load = snapshot(&self.sway, &mut self.events, None)?;
            require_quiet(&before_load.window_events, None)?;
            require_empty(before_load.focused_workspace())?;
            let fixture = self
                .fixture
                .as_ref()
                .expect("fixture prepared before scenarios");
            verify_fixture_state(
                &before_load,
                fixture,
                fixture
                    .loaded_signature
                    .as_ref()
                    .expect("fixture load verified"),
            )?;
            if let Some(foreign) = &previous_foreign {
                foreign.verify(&before_load)?;
            }
            let foreign = ForeignWindows::record(&before_load, before_load.focused_workspace_id)?;

            if let Some(initial_layout) = progress.scenario.initial_workspace_layout {
                self.stage = format!("{cycle_name}: initial workspace layout");
                let target = self
                    .primary
                    .as_ref()
                    .expect("target selected before scenarios");
                let workspace_revision = self.events.workspace_revision();
                before_load = prepare_initial_workspace_layout(
                    &before_load,
                    target,
                    target_baseline,
                    initial_layout,
                    workspace_revision,
                    || {
                        process::check_interruption()?;
                        self.sway.set_empty_workspace_layout(initial_layout)?;

                        // Sway changes workspace->layout directly on an empty workspace:
                        // no window::move/layout or workspace event is expected. Retain
                        // history rather than resetting it after our command.
                        let prepared = snapshot(&self.sway, &mut self.events, None)?;
                        Ok((prepared, self.events.workspace_revision()))
                    },
                )?;
            }

            progress.begin_cycle(&before_load, &retired_ids)?;
            let scenario = &progress.scenario;
            let preexisting_ids = progress
                .preexisting_ids
                .as_ref()
                .expect("just recorded IDs");
            let target = self
                .primary
                .as_ref()
                .expect("successful switch established target");

            self.stage = format!("{cycle_name}: load");
            process::check_interruption()?;
            let mut load = configured_command(
                &self.binary,
                &directory.path,
                "load",
                if replay_cycle { "replay" } else { name },
            );
            self.sway.select_socket(&mut load);
            let output = process::run(load, &self.stage, load_timeout(scenario))?;
            if !output.trim().is_empty() {
                return Err("load produced unexpected stdout".into());
            }

            self.stage = format!("{cycle_name}: raw-tree verification");
            let loaded = snapshot(&self.sway, &mut self.events, None)?;
            foreign.verify(&loaded)?;
            progress.accepted =
                bind_launches(&loaded.window_events, &scenario.layout, preexisting_ids)?;
            verify_ownership(
                &loaded.tree,
                target.id,
                &scenario.layout,
                preexisting_ids,
                &progress.accepted,
            )?;
            let measurements =
                verify_layout(loaded.focused_workspace(), scenario, &progress.accepted)?;
            let probe_pid = if name == "process-execution" {
                self.stage = format!("{cycle_name}: child process verification");
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
                let terminal_pid = terminal.pid.ok_or("process scenario has no terminal PID")?;
                let probe_pid = process::verify_probe(Path::new("/proc"), terminal_pid, leaf)?;
                Some(probe_pid)
            } else {
                None
            };
            let before_capture = signature(loaded.focused_workspace());

            if name == "occupied-workspace-refusal" {
                self.stage = format!("{cycle_name}: expected refusal");
                // A different, already checked layout has distinct selectors. A broken load
                // cannot silently substitute its new windows for the first layout's windows.
                let mut refused =
                    configured_command(&self.binary, &directory.path, "load", "single-window");
                self.sway.select_socket(&mut refused);

                let before_refusal = snapshot(&self.sway, &mut self.events, None)?;
                foreign.verify(&before_refusal)?;
                verify_unchanged(&before_refusal, target.id, progress, &before_capture)?;
                let revision = self.events.workspace_revision();
                let result = process::run_with_status(refused, &self.stage, CAPTURE_TIMEOUT);
                let output = refusal_completion(result, process::interruption_requested())?;
                let after_refusal = snapshot(&self.sway, &mut self.events, None)?;
                verify_refusal(
                    &output,
                    &before_refusal,
                    &after_refusal,
                    revision != self.events.workspace_revision(),
                )?;
                foreign.verify(&after_refusal)?;
                verify_unchanged(&after_refusal, target.id, progress, &before_capture)?;
            }

            self.stage = format!("{cycle_name}: capture");
            let mut capture = Command::new(&self.binary);
            capture.args(["capture", "--no-infer-commands"]);
            self.sway.select_socket(&mut capture);
            let draft = process::run(capture, &self.stage, CAPTURE_TIMEOUT)?;
            let after_capture = snapshot(&self.sway, &mut self.events, None)?;
            foreign.verify(&after_capture)?;
            verify_unchanged(&after_capture, target.id, progress, &before_capture)?;
            let filled = verify_capture(&draft, &progress.scenario, &measurements)?;
            directory.write("captured", &filled)?;
            check_configuration(
                &self.binary,
                &directory.path,
                "captured",
                "check filled capture",
            )?;

            if matches!(
                name,
                "single-window" | "horizontal-pair" | "duplicate-foot-ids"
            ) {
                self.stage = format!("{cycle_name}: inferred capture");
                let mut capture = Command::new(&self.binary);
                capture.arg("capture");
                self.sway.select_socket(&mut capture);
                let inferred = process::run(capture, &self.stage, CAPTURE_TIMEOUT)?;
                let after_inferred = snapshot(&self.sway, &mut self.events, None)?;
                foreign.verify(&after_inferred)?;
                verify_unchanged(&after_inferred, target.id, progress, &before_capture)?;
                verify_inferred_capture(&inferred, &draft, &progress.scenario.layout)?;
            }

            let replay = if progress.scenario.captures_and_reloads() && !replay_cycle {
                self.stage = format!("{cycle_name}: executable capture check");
                let (yaml, replay) = executable_capture(&draft, &progress.scenario, &measurements)?;
                directory.write("replay", &yaml)?;
                check_configuration(&self.binary, &directory.path, "replay", &self.stage)?;
                Some(replay)
            } else {
                None
            };

            self.stage = format!("{cycle_name}: cleanup");
            let context = CleanupContext {
                foreign: &foreign,
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
            foreign.verify(&cleaned)?;
            require_quiet(&cleaned.window_events, None)?;
            require_empty(cleaned.focused_workspace())?;
            if let Some(pid) = probe_pid {
                process::wait_probe_exit(pid)?;
            }
            process::check_interruption()?;

            for note in measurements.sizing_notes {
                println!("      {cycle_name}: above 1-point sizing target: {note}");
            }
            if let Some(replay) = replay {
                // Reached only after exact-ID cleanup, a synchronized empty tree, and
                // foreign preservation. Any earlier error returns without another launch.
                self.stage = format!("{cycle_name}: prepare replay");
                progress.prepare_replay(&cleaned, &foreign, replay, &mut retired_ids)?;
                previous_foreign = Some(foreign);
                replay_cycle = true;
                continue;
            }
            break;
        }

        self.passed += 1;
        let coverage = if name == "occupied-workspace-refusal" {
            "hierarchy, sizing, focus, identity, capture, occupied refusal"
        } else if progress.scenario.captures_and_reloads() {
            "hierarchy, sizing, group selections, focus, identity, capture/reload round-trip"
        } else if matches!(
            name,
            "single-window" | "horizontal-pair" | "duplicate-foot-ids"
        ) {
            "hierarchy, sizing, focus, identity, capture, inferred commands"
        } else {
            "hierarchy, sizing, focus, identity, capture"
        };
        println!("PASS  {name:19} {coverage}");
        self.stage = "scenario preparation".into();
        Ok(())
    }

    /// Loads one independently owned Foot window on the fresh compositor's focused
    /// workspace. It stays outside every scenario's target for the whole run, so each
    /// scenario can prove it left a window it does not own untouched.
    fn prepare_fixture(&mut self, directory: &TemporaryConfigurations) -> Result<(), String> {
        self.stage = "foreign fixture: check".into();
        let scenario = foreign_fixture_scenario(&self.run_id, &self.foot, &self.sleep);
        directory.write(scenario.name, &configuration_yaml(&scenario))?;
        check_configuration(&self.binary, &directory.path, scenario.name, &self.stage)?;

        self.stage = "foreign fixture: empty-workspace precondition".into();
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        require_empty(before.focused_workspace())?;
        // Every snapshot requires focus on the workspace the initial observation recorded.
        let workspace = WorkspaceIdentity {
            id: before.focused_workspace_id,
            name: FIXTURE_WORKSPACE.into(),
        };
        let foreign = ForeignWindows::record(&before, workspace.id)?;
        let preexisting_ids = walk(&before.tree)
            .iter()
            .map(|placed| placed.node.id)
            .collect();
        self.fixture = Some(ForeignFixture {
            workspace,
            progress: ScenarioProgress {
                scenario,
                preexisting_ids: Some(preexisting_ids),
                accepted: AcceptedWindows::new(),
            },
            foreign,
            loaded_signature: None,
        });
        let fixture = self.fixture.as_mut().expect("just recorded fixture");
        self.stage = "foreign fixture: load and independent verification".into();
        let mut command = configured_command(
            &self.binary,
            &directory.path,
            "load",
            fixture.progress.scenario.name,
        );
        self.sway.select_socket(&mut command);
        let output = process::run(
            command,
            &self.stage,
            load_timeout(&fixture.progress.scenario),
        )?;
        if !output.trim().is_empty() {
            return Err("fixture load produced unexpected stdout".into());
        }
        let loaded = snapshot(&self.sway, &mut self.events, None)?;
        fixture.foreign.verify(&loaded)?;
        let preexisting = fixture
            .progress
            .preexisting_ids
            .as_ref()
            .expect("recorded before fixture load");
        fixture.progress.accepted = bind_launches(
            &loaded.window_events,
            &fixture.progress.scenario.layout,
            preexisting,
        )?;
        verify_ownership(
            &loaded.tree,
            fixture.workspace.id,
            &fixture.progress.scenario.layout,
            preexisting,
            &fixture.progress.accepted,
        )?;
        verify_layout(
            loaded.focused_workspace(),
            &fixture.progress.scenario,
            &fixture.progress.accepted,
        )?;
        fixture.loaded_signature = Some(signature(loaded.focused_workspace()));
        println!(
            "Foreign fixture: workspace {:?}, container IDs {:?} (verified).",
            fixture.workspace.name,
            fixture.progress.accepted.keys().collect::<Vec<_>>()
        );
        Ok(())
    }

    /// The fixture must survive the whole run unchanged. It is never closed: compositor
    /// shutdown ends it with every other client.
    fn verify_fixture_survived(&mut self) -> Result<(), String> {
        self.stage = "foreign fixture: final verification".into();
        let fixture = self
            .fixture
            .as_ref()
            .expect("fixture prepared before scenarios");

        let observed = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&observed.window_events, None)?;
        require_empty(observed.focused_workspace())?;
        fixture.foreign.verify(&observed)?;
        verify_fixture_state(
            &observed,
            fixture,
            fixture
                .loaded_signature
                .as_ref()
                .expect("fixture load verified"),
        )?;

        println!("Foreign fixture preserved through the whole run.");
        Ok(())
    }
}

/// Ownership is re-established on the fixture's own workspace, never by selecting all
/// preexisting foreign windows. Focus is checked separately around the expected switch.
fn verify_fixture_state(
    observed: &Snapshot,
    fixture: &ForeignFixture,
    baseline: &Signature,
) -> Result<(), String> {
    verify_ownership(
        &observed.tree,
        fixture.workspace.id,
        &fixture.progress.scenario.layout,
        fixture
            .progress
            .preexisting_ids
            .as_ref()
            .expect("recorded before fixture load"),
        &fixture.progress.accepted,
    )?;
    let workspace = walk(&observed.tree)
        .into_iter()
        .map(|placed| placed.node)
        .find(|node| node.id == fixture.workspace.id)
        .ok_or("foreign fixture workspace disappeared")?;
    let mut current = signature(workspace);
    current.ignore_focus();
    // The stored signature has focus from creation; compare a fresh signature with the
    // same policy without changing the authoritative baseline.
    let mut baseline_without_focus = baseline.clone();
    baseline_without_focus.ignore_focus();
    if current != baseline_without_focus {
        return Err("foreign fixture hierarchy/identity/geometry/state changed".into());
    }
    Ok(())
}

/// A timeout, failed observation stream, or interrupt cannot count as an expected
/// refusal, even if the child printed the occupied diagnostic before it stopped.
fn refusal_completion(
    result: Result<process::CompletedProcess, String>,
    interrupted: bool,
) -> Result<process::CompletedProcess, String> {
    if interrupted {
        return Err("interrupted during occupied refusal".into());
    }
    result
}

fn verify_refusal(
    output: &process::CompletedProcess,
    before: &Snapshot,
    after: &Snapshot,
    workspace_activity: bool,
) -> Result<(), String> {
    before.validate()?;
    after.validate()?;
    if before.focused_workspace().nodes.is_empty()
        && before.focused_workspace().floating_nodes.is_empty()
    {
        return Err("occupied refusal has no occupied baseline".into());
    }
    verify_unchanged_desktop(before, after, workspace_activity)?;
    let occupied_diagnostic = output.stderr.lines().any(|line| {
        line.starts_with("sway-stage: prelaunch workspace=")
            && line.contains(&format!(
                "workspace={}: layout ",
                before.focused_workspace_id
            ))
            && line.contains(
                ": target workspace must be completely empty, including tiled/floating containers;",
            )
    });
    if output.status.code() != Some(1) || !occupied_diagnostic || !output.stdout.trim().is_empty() {
        return Err(format!(
            "expected occupied-workspace prelaunch refusal (exit 1), got {}: {}",
            output.status,
            output.stderr.trim()
        ));
    }
    Ok(())
}

fn verify_unchanged_desktop(
    before: &Snapshot,
    after: &Snapshot,
    workspace_activity: bool,
) -> Result<(), String> {
    before.validate()?;
    after.validate()?;
    require_quiet(&after.window_events, None)?;
    let before_workspaces: BTreeMap<_, _> = before
        .workspaces
        .iter()
        .map(|workspace| {
            (
                workspace.id,
                (&workspace.name, workspace.num, workspace.focused),
            )
        })
        .collect();
    let after_workspaces: BTreeMap<_, _> = after
        .workspaces
        .iter()
        .map(|workspace| {
            (
                workspace.id,
                (&workspace.name, workspace.num, workspace.focused),
            )
        })
        .collect();

    // Compare the entire modeled desktop, not merely the loaded workspace: every
    // container ID, actual wrapper, child order, view identity, geometry, and focus.
    if workspace_activity
        || before_workspaces != after_workspaces
        || signature(&before.tree) != signature(&after.tree)
        || before.focused_workspace_id != after.focused_workspace_id
    {
        return Err("occupied refusal changed desktop state or workspace event history".into());
    }
    Ok(())
}

fn verify_profile_refusal(
    output: &process::CompletedProcess,
    before: &Snapshot,
    after: &Snapshot,
    occupied_number: &str,
    occupied_id: i64,
    workspace_activity: bool,
) -> Result<(), String> {
    verify_unchanged_desktop(before, after, workspace_activity)?;
    let diagnostic =
        format!("workspace {occupied_number} id={occupied_id} must be completely empty");
    if output.status.code() != Some(1)
        || !output.stdout.trim().is_empty()
        || !output.stderr.contains("workspace preflight")
        || !output.stderr.contains(&diagnostic)
        || !output.stderr.contains("completed 0 workspaces []")
        || !output.stderr.contains("no applications launched")
    {
        return Err(format!(
            "expected occupied profile refusal: {}: {}",
            output.status, output.stderr
        ));
    }
    Ok(())
}

fn verify_profile_partial_failure(
    output: &process::CompletedProcess,
    primary: &WorkspaceIdentity,
    second_number: &str,
) -> Result<(), String> {
    if output.status.code() != Some(1)
        || !output.stdout.trim().is_empty()
        || !output.stderr.contains(&format!(
            "workspace {second_number}, layout \"profile-second\""
        ))
        || !output.stderr.contains(&format!(
            "completed 1 workspaces [({}, {})]",
            primary.name, primary.id
        ))
        || !output.stderr.contains("window timeout:")
        || !output
            .stderr
            .contains("stopped without cleanup or relaunch")
    {
        return Err(format!(
            "expected second-workspace timeout with completed primary: {}: {}",
            output.status, output.stderr
        ));
    }
    Ok(())
}

fn verify_profile_launch_record(
    path: &Path,
    layouts: &[Scenario; 2],
    case: profile::Case,
) -> Result<(), String> {
    let observed = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("read launch record: {error}")),
    };
    let expected = if case == profile::Case::Occupied {
        String::new()
    } else {
        layouts
            .iter()
            .flat_map(|scenario| {
                scenario
                    .layout
                    .leaves()
                    .into_iter()
                    .map(|leaf| format!("{}-{}\n", scenario.name, leaf.layout_id))
            })
            .collect::<String>()
    };
    if observed != expected {
        return Err(format!(
            "profile launch order/count differs: expected {expected:?}, observed {observed:?}"
        ));
    }
    Ok(())
}

fn require_return_focus(
    events: &[crate::ipc::WindowEvent],
    accepted: &AcceptedWindows,
) -> Result<(), String> {
    for event in events {
        if event.change == crate::ipc::WindowChange::Focus
            && accepted.contains_key(&event.container.id)
        {
            continue;
        }
        require_quiet(std::slice::from_ref(event), None)?;
    }
    Ok(())
}

/// A compositor nothing has used yet: exactly the fixture workspace, focused and empty, and
/// no window anywhere, including the scratchpad. Anything else means a reused or wrong
/// compositor, so the runner stops before its first command.
fn verify_fresh_compositor(observed: &Snapshot) -> Result<&Workspace, String> {
    observed.validate()?;

    let [workspace] = observed.workspaces.as_slice() else {
        let names: Vec<&str> = observed
            .workspaces
            .iter()
            .map(|workspace| workspace.name.as_str())
            .collect();
        return Err(format!(
            "a fresh compositor has only workspace {FIXTURE_WORKSPACE:?}, found {names:?}"
        ));
    };
    // Validation already proved that the only workspace is the focused one.
    if workspace.name != FIXTURE_WORKSPACE {
        return Err(format!(
            "a fresh compositor starts on workspace {FIXTURE_WORKSPACE:?}, found {:?}",
            workspace.name
        ));
    }

    require_empty(observed.focused_workspace())?;
    if let Some(window) = walk(&observed.tree)
        .into_iter()
        .find(|placed| is_window(placed.node))
    {
        return Err(format!(
            "a fresh compositor has no windows, found container {}",
            window.node.id
        ));
    }
    Ok(workspace)
}

/// A two-output compositor nothing has used yet: workspace "1" focused on the first output
/// and the second output's initial workspace, each shown and empty, and no window anywhere.
fn verify_fresh_two_output_compositor<'a>(
    observed: &'a Snapshot,
    outputs: &[Output],
) -> Result<&'a Workspace, String> {
    observed.validate()?;

    let mut names: Vec<&str> = observed
        .workspaces
        .iter()
        .map(|workspace| workspace.name.as_str())
        .collect();
    names.sort_unstable();
    if names != [FIXTURE_WORKSPACE, SECOND_OUTPUT_INITIAL_WORKSPACE] {
        return Err(format!(
            "a fresh two-output compositor has only workspaces {FIXTURE_WORKSPACE:?} and \
             {SECOND_OUTPUT_INITIAL_WORKSPACE:?}, found {names:?}"
        ));
    }

    for (output_name, workspace_name) in [
        (OUTPUT_NAME, FIXTURE_WORKSPACE),
        (SECOND_OUTPUT_NAME, SECOND_OUTPUT_INITIAL_WORKSPACE),
    ] {
        let workspace = observed
            .workspaces
            .iter()
            .find(|workspace| workspace.name == workspace_name)
            .expect("workspace names were compared above");
        let on_output = matches!(
            output_ancestry(&observed.tree, workspace.id)?.as_slice(),
            [(_, Some(name))] if name == output_name
        );
        let shown = outputs.iter().any(|output| {
            output.name == output_name
                && output.current_workspace.as_deref() == Some(workspace_name)
        });
        if !on_output || !shown {
            return Err(format!(
                "a fresh two-output compositor shows workspace {workspace_name:?} on {output_name}"
            ));
        }
        let node = walk(&observed.tree)
            .into_iter()
            .map(|placed| placed.node)
            .find(|node| node.id == workspace.id)
            .expect("snapshot validation matched every workspace to the tree");
        require_empty(node)?;
    }

    let focused = observed
        .workspaces
        .iter()
        .find(|workspace| workspace.focused)
        .expect("snapshot validation found one focused workspace");
    if focused.name != FIXTURE_WORKSPACE {
        return Err(format!(
            "a fresh two-output compositor starts on workspace {FIXTURE_WORKSPACE:?}, found {:?}",
            focused.name
        ));
    }
    if let Some(window) = walk(&observed.tree)
        .into_iter()
        .find(|placed| is_window(placed.node))
    {
        return Err(format!(
            "a fresh compositor has no windows, found container {}",
            window.node.id
        ));
    }
    Ok(focused)
}

/// Shared pre/post guard for the one empty-workspace layout mutation. The baseline
/// freezes the target output and foreign windows at the explicitly selected target.
fn verify_empty_target(
    observed: &Snapshot,
    target: &WorkspaceIdentity,
    baseline: &ForeignWindows,
) -> Result<(), String> {
    observed.validate()?;
    let workspace = observed.focused_workspace();
    if workspace.id != target.id || workspace.name.as_deref() != Some(&target.name) {
        return Err("initial layout target identity/focus changed".into());
    }
    baseline.verify(observed)?;
    require_quiet(&observed.window_events, None)?;
    require_empty(workspace)
}

/// The operation closure keeps command + reply checking + synchronized observation in
/// order, while offline tests can prove that failed guards never dispatch a mutation.
fn prepare_initial_workspace_layout(
    before: &Snapshot,
    target: &WorkspaceIdentity,
    baseline: &ForeignWindows,
    initial_layout: InitialWorkspaceLayout,
    before_revision: u64,
    mutate_and_observe: impl FnOnce() -> Result<(Snapshot, u64), String>,
) -> Result<Snapshot, String> {
    verify_empty_target(before, target, baseline)?;

    let (prepared, after_revision) = mutate_and_observe()?;
    verify_empty_target(&prepared, target, baseline)?;
    if before_revision != after_revision {
        return Err("workspace activity during initial layout preparation".into());
    }
    let actual = prepared.focused_workspace().layout;
    if actual != initial_layout.observed_layout() {
        return Err(format!(
            "empty workspace initial layout was not established: expected {:?}, observed {actual:?}",
            initial_layout.observed_layout()
        ));
    }
    Ok(prepared)
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

fn verify_unchanged(
    observed: &Snapshot,
    target_workspace_id: i64,
    progress: &ScenarioProgress,
    baseline: &Signature,
) -> Result<(), String> {
    require_quiet(&observed.window_events, None)?;
    verify_ownership(
        &observed.tree,
        target_workspace_id,
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
) -> Result<(), String> {
    if signature(workspace) != *baseline {
        return Err(message.to_owned());
    }
    Ok(())
}

struct CleanupContext<'a> {
    target: &'a WorkspaceIdentity,
    layout: &'a ConfiguredNode,
    preexisting_ids: &'a HashSet<i64>,
    foreign: &'a ForeignWindows,
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
    closing_window_id: i64,
    trusted: bool,
) -> Result<ClosePlan, String> {
    let workspace = observed.focused_workspace();
    if !trusted
        || workspace.id != context.target.id
        || workspace.name.as_deref() != Some(&context.target.name)
    {
        return Err("cleanup requires trusted target workspace focus".into());
    }
    context.foreign.verify(observed)?;
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

    if !remaining.contains_key(&closing_window_id) || closing_window_id <= 0 {
        return Err("close requires an individually verified owned window".into());
    }

    let hierarchy = remove_from_shape(shape(workspace)?, closing_window_id);
    let mut expected_remaining = remaining.clone();
    expected_remaining.remove(&closing_window_id);
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
) -> Result<(), String> {
    let mut remaining = accepted.clone();
    // Accepted windows are sorted by container ID, which need not follow launch order.
    // Derive close order from configured leaves so windows always close in launch order.
    for leaf in context.layout.leaves() {
        let closing_window_id = accepted
            .iter()
            .find(|(_, layout_id)| **layout_id == leaf.layout_id)
            .map(|(&id, _)| id)
            .ok_or("missing accepted cleanup window")?;
        process::check_interruption()?;
        let before = snapshot(sway, events, None)?;
        let plan = prepare_close(
            &before,
            context,
            &remaining,
            &baseline,
            closing_window_id,
            events.focus_history_trusted(),
        )?;

        // The sole destructive desktop command. IPC cannot make the guard/command atomic.
        process::check_interruption()?;
        sway.close_owned_window(closing_window_id)?;
        let deadline = Instant::now() + IPC_TIMEOUT;
        let after = loop {
            process::check_interruption()?;
            let observed = snapshot(sway, events, None)?;
            if verify_close_observation(
                &observed,
                context,
                &remaining,
                &plan,
                &baseline,
                closing_window_id,
            )? {
                break observed;
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "cleanup timeout: container {closing_window_id} did not disappear"
                ));
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
    closing_window_id: i64,
) -> Result<bool, String> {
    context.foreign.verify(observed)?;
    let expected_close = ExpectedClose {
        closing_window_id,
        remaining: &plan.remaining,
    };
    require_quiet(&observed.window_events, Some(&expected_close))?;
    let disappeared = !walk(&observed.tree)
        .iter()
        .any(|placed| placed.node.id == closing_window_id);
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
        return Err(format!(
            "cleanup hierarchy changed: expected {:?}, observed {observed_after:?}",
            plan.hierarchy
        ));
    }
    Ok(true)
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

/// The sway-stage that the same `cargo build` placed beside this harness, which runs from
/// `<profile directory>/examples/`. Never a separately built or installed copy.
fn sibling_sway_stage(harness_executable: &Path) -> Result<PathBuf, String> {
    let examples_directory = harness_executable
        .parent()
        .filter(|directory| directory.file_name() == Some(OsStr::new("examples")));
    let Some(profile_directory) = examples_directory.and_then(Path::parent) else {
        return Err(format!(
            "{} is not a Cargo example build; use tools/integration/run.sh",
            harness_executable.display()
        ));
    };
    Ok(profile_directory.join("sway-stage"))
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

/// Owns private YAML files, profile-scenario directories, and the process test cwd in the
/// session's temporary directory; never desktop windows. Nothing here removes them: the
/// launcher removes the whole session directory after a successful run and keeps it, with
/// these files, as diagnostics after a failure.
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

    fn subdirectory(&self, name: &str) -> Result<Self, String> {
        let path = self.path.join(name);
        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|error| format!("create private profile directory: {error}"))?;
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

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
