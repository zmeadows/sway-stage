//! The scenario runner: a foreign fixture, then the selected scenarios of one output setup
//! in suite order, each with verified success-path cleanup. It runs only as the launcher's child inside the
//! private headless session's environment and refuses any compositor that is not a fresh
//! headless one. A failure stops the run and leaves every window to the compositor's
//! shutdown; files and the IPC helper may be dropped, never windows.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{self, Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::capture::{
    executable_capture, verify_capture, verify_inferred_capture, verify_recovered_launch_commands,
};
use crate::headless::{
    OUTPUT_NAME, OutputSetup, SECOND_OUTPUT_ASSIGNED_WORKSPACE, SECOND_OUTPUT_INITIAL_WORKSPACE,
    SECOND_OUTPUT_NAME, is_session_ipc_socket, verify_virtual_outputs,
};
use crate::ipc::{
    IPC_TIMEOUT, InitialWorkspaceLayout, Output, Swaymsg, TreeNode, WindowEvent, Workspace,
};
use crate::layout::{
    LayoutMeasurements, Shape, Signature, is_window, outline, remove_from_shape, shape, signature,
    verify_layout,
};
use crate::observation::{
    Events, ExpectedSwitch, ExpectedSwitches, Snapshot, WorkspaceIdentity, snapshot,
    snapshot_with_switch, snapshot_with_switches, walk,
};
use crate::ownership::{
    AcceptedWindows, ExpectedClose, bind_launches, require_quiet, verify_ownership,
};
use crate::preservation::{ForeignWindows, output_ancestry};
use crate::scenarios::{
    ConfiguredNode, ConfiguredWindow, Scenario, configuration_yaml, configure_process_execution,
    foreign_fixture_scenario, session_roundtrip_layouts, standard_scenarios,
};
use crate::session_roundtrip::{self, SessionWorkspace};
use crate::{floating_refusal, interference, launch_context, load_failure, process, profile};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);
const CLOSE_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How often the session round-trip looks for a window that Sway's `exec` is opening.
const WINDOW_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A fresh compositor's only workspace. The foreign fixture loads here without a switch and
/// stays for the whole run, so it is also the occupied target of the profile refusal.
const FIXTURE_WORKSPACE: &str = "1";
/// The target of every standard scenario and the first target of every profile.
const PRIMARY_WORKSPACE: &str = "2";
/// Each profile's second target. Every profile leaves it empty and returns to the primary,
/// so Sway removes it and the next profile load creates it again.
const PROFILE_WORKSPACE: &str = "3";
/// The second target of the workspace-zero profile instead, created and removed the same way.
const ZERO_WORKSPACE: &str = "0";
/// The scenario whose window a Sway `assign` rule sends to `ASSIGNED_WORKSPACE`.
const ASSIGNED_WORKSPACE_SCENARIO: &str = "assigned-workspace-refusal";
/// Created by Sway for that window alone, and removed after its cleanup.
const ASSIGNED_WORKSPACE: &str = "4";

/// Every scenario in suite order: the standard scenarios, the failing standalone loads, the
/// floating-occupancy refusals, the interfered loads, the profile cases, the sequentially verified profiles, the Sway exec entry points, the session
/// round-trip, the assigned-workspace refusal, then the two-output session's cross-output
/// profile.
pub fn suite_scenario_names() -> Vec<&'static str> {
    // Scenario definitions are pure, so placeholder executables suffice for their names.
    let standard = standard_scenarios("names-only", "foot", "sleep");

    let mut names: Vec<&'static str> = standard.iter().map(|scenario| scenario.name).collect();
    names.extend(load_failure::CASES.iter().map(|case| case.name()));
    names.extend(floating_refusal::CASES.iter().map(|case| case.name()));
    names.extend(interference::CASES.iter().map(|case| case.name()));
    names.extend(profile::CASES.iter().map(|case| case.name()));
    names.extend(profile::SEQUENTIAL_CASES.iter().map(|case| case.name()));
    names.extend(launch_context::CASES.iter().map(|case| case.name()));
    names.push(session_roundtrip::SCENARIO);
    names.push(ASSIGNED_WORKSPACE_SCENARIO);
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

/// The private directories that one `profile capture` wrote its layouts and profile to.
struct CaptureDirectories {
    layouts: TemporaryConfigurations,
    profiles: TemporaryConfigurations,
}

/// One sway-stage run through Sway's `exec`: what it loads and where its evidence goes.
struct ExecEntry {
    layout: Scenario,
    entry_script_path: PathBuf,
    evidence_directory: PathBuf,
    launch_record_path: PathBuf,
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

        for case in load_failure::CASES {
            if selected.contains(&case.name()) {
                self.run_load_failure(case, &directory)?;
            }
        }
        // These leave the primary empty and focused with its identity, like the cases above.
        for case in floating_refusal::CASES {
            if selected.contains(&case.name()) {
                self.run_floating_refusal(case, &directory)?;
            }
        }
        for case in interference::CASES {
            if selected.contains(&case.name()) {
                self.run_interference(case, &directory)?;
            }
        }

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
        // The recreated-origin case ends on a replacement primary, like the cases below.
        for case in profile::SEQUENTIAL_CASES {
            if selected.contains(&case.name()) {
                self.run_sequential_profile(case, &directory)?;
            }
        }

        // Each profile case removes the primary and leaves an empty, focused replacement with
        // the same name, so they run after every scenario that relies on the primary's ID.
        for case in launch_context::CASES {
            if !selected.contains(&case.name()) {
                continue;
            }
            if case.loads_profile() {
                self.run_exec_profile(case, &directory)?;
            } else {
                self.run_exec_layout(case, &directory)?;
            }
        }
        // The round-trip and the assigned refusal also end on a replacement primary.
        if selected.contains(&session_roundtrip::SCENARIO) {
            self.run_session_roundtrip(&directory)?;
        }
        if selected.contains(&ASSIGNED_WORKSPACE_SCENARIO) {
            self.run_assigned_workspace_refusal(&directory)?;
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

    /// Writes the case's layout, its one-entry profile, a menu case's Fuzzel substitute,
    /// and the script that Sway's `exec` runs. The configuration is checked directly, as in
    /// every other scenario; only the load goes through Sway's exec.
    fn prepare_exec_entry(
        &mut self,
        case: launch_context::Case,
        directory: &TemporaryConfigurations,
    ) -> Result<ExecEntry, String> {
        let name = case.name();
        self.stage = format!("{name}: preparation");
        let files = directory.subdirectory(name)?;
        let layouts_directory = files.subdirectory("layouts")?;
        let profiles_directory = files.subdirectory("profiles")?;
        let evidence_directory = files.subdirectory("evidence")?;
        let launch_script_path = files.path.join("record-launch.sh");
        let launch_record_path = files.path.join("launches");
        let entry_script_path = files.path.join("exec-entry.sh");
        fs::write(&launch_script_path, profile::LAUNCH_SCRIPT)
            .map_err(|error| format!("write profile launch recorder: {error}"))?;

        let layout = launch_context::layout(
            case,
            &self.run_id,
            &self.foot,
            &self.sleep,
            utf8_path(&launch_script_path)?,
            utf8_path(&launch_record_path)?,
        );
        layouts_directory.write(layout.name, &configuration_yaml(&layout))?;
        profiles_directory.write(
            name,
            &launch_context::profile_configuration(case, PROFILE_WORKSPACE),
        )?;

        // The profile check also validates the layout that the profile references.
        let mut check = Command::new(&self.binary);
        check
            .arg("--layouts-dir")
            .arg(&layouts_directory.path)
            .arg("--profiles-dir")
            .arg(&profiles_directory.path)
            .args(["profile", "check", name]);
        self.sway.select_socket(&mut check);
        if process::run(check, &self.stage, IPC_TIMEOUT)? != format!("{name}: valid\n") {
            return Err("unexpected profile check output".into());
        }

        // Only the menu runs `fuzzel`; the entry script puts this substitute first on PATH.
        let fuzzel_directory = match case.menu_selection() {
            Some(selection) => {
                let substitute_directory = files.subdirectory("fuzzel-substitute")?;
                let substitute = launch_context::fuzzel_substitute_script(
                    &selection,
                    utf8_path(&evidence_directory.path)?,
                );
                substitute_directory.create_executable("fuzzel", &substitute)?;
                Some(utf8_path(&substitute_directory.path)?.to_owned())
            }
            None => None,
        };
        let sway_stage_command = launch_context::sway_stage_arguments(
            case,
            utf8_path(&self.binary)?,
            utf8_path(&layouts_directory.path)?,
            utf8_path(&profiles_directory.path)?,
        );
        let entry_script = launch_context::entry_script(
            case.entry_mode(),
            utf8_path(&evidence_directory.path)?,
            fuzzel_directory.as_deref(),
            &sway_stage_command,
        );
        fs::write(&entry_script_path, entry_script)
            .map_err(|error| format!("write exec entry script: {error}"))?;

        Ok(ExecEntry {
            layout,
            entry_script_path,
            evidence_directory: evidence_directory.path,
            launch_record_path,
        })
    }

    /// Starts sway-stage through Sway's own `exec`, as a keybinding would, to load a
    /// one-entry profile for the absent `PROFILE_WORKSPACE`, directly or through the menu.
    /// The exec starts from the empty, focused primary, or for one case from the fixture's
    /// occupied workspace. Each case requires the window on that target and a successful
    /// load, whichever launch-context paths Sway was left. The case ends on an empty,
    /// focused workspace with the primary's name, which becomes the primary.
    fn run_exec_profile(
        &mut self,
        case: launch_context::Case,
        directory: &TemporaryConfigurations,
    ) -> Result<(), String> {
        let name = case.name();
        let target_name = PROFILE_WORKSPACE;
        let entry = self.prepare_exec_entry(case, directory)?;
        let primary = self
            .primary
            .clone()
            .expect("primary selected before scenarios");
        let origin = if case.starts_on_fixture_workspace() {
            self.switch_to_fixture_workspace(name, &primary)?
        } else {
            primary
        };

        self.stage = format!("{name}: fresh workspace baselines");
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        let recorded_origin = profile::Target::record(&before, &origin.name)?
            .ok_or("origin workspace disappeared")?;
        if before.focused_workspace_id != origin.id || recorded_origin.workspace != origin {
            return Err("the exec'd profile case must start on its focused origin".into());
        }
        // The fixture's workspace holds only the fixture, which `verify_fixture_state` checks.
        if !case.starts_on_fixture_workspace() {
            require_empty(before.focused_workspace())?;
        }
        if profile::Target::record(&before, target_name)?.is_some() {
            return Err(format!(
                "workspace {target_name} must not exist before the load creates it"
            ));
        }
        let fixture = self.fixture.as_ref().expect("foreign fixture prepared");
        let fixture_signature = fixture.loaded_signature.as_ref().expect("fixture verified");
        verify_fixture_state(&before, fixture, fixture_signature)?;
        let preexisting: HashSet<i64> = walk(&before.tree)
            .iter()
            .map(|placed| placed.node.id)
            .collect();

        // Sway records the focused origin in this exec's launch context.
        self.stage = format!("{name}: profile load through Sway's exec");
        exec_sway_stage(&self.sway, &entry)?;
        let transition = ExpectedSwitch {
            from_id: origin.id,
            destination_name: target_name.into(),
        };
        let (loaded, switch_index) =
            snapshot_with_switch(&self.sway, &mut self.events, &transition)?;

        self.stage = format!("{name}: independent verification");
        let target =
            profile::Target::record(&loaded, target_name)?.ok_or("profile target missing")?;
        if preexisting.contains(&target.workspace.id) {
            return Err("created workspace reused a preexisting ID".into());
        }
        if loaded.focused_workspace_id != target.workspace.id {
            return Err("the profile did not leave its target focused".into());
        }
        // From an occupied origin, this also proves that no window opened there.
        verify_fixture_state(&loaded, fixture, fixture_signature)?;

        // Nothing may launch before the target is selected; afterwards, only this window.
        let (before_switch, after_switch) = loaded.window_events.split_at(switch_index);
        require_quiet(before_switch, None)?;
        let accepted = bind_launches(after_switch, &entry.layout.layout, &preexisting)?;
        let window_id = *accepted
            .keys()
            .next()
            .expect("a one-window layout binds exactly one launch");
        verify_exec_records(case, &entry)?;

        println!("      launch context: {}", case.description());

        // The window is on the selected target, and nothing recreated the origin.
        let expected_events =
            launch_context::expected_workspace_events(case, &origin, &target.workspace);
        profile::verify_workspace_events(&loaded.workspace_events, &expected_events)?;
        verify_ownership(
            &loaded.tree,
            target.workspace.id,
            &entry.layout.layout,
            &preexisting,
            &accepted,
        )?;
        let measurements = verify_layout(target.verify(&loaded)?, &entry.layout, &accepted)?;
        for note in &measurements.sizing_notes {
            println!("      {note}");
        }
        let origin_outcome = if case.starts_on_fixture_workspace() {
            "kept only the unchanged fixture"
        } else {
            "was removed"
        };
        println!(
            "      origin {} id={} was focused at exec and {origin_outcome} when target \
             {target_name} id={} was selected; window id={window_id} appeared on the target",
            origin.name, origin.id, target.workspace.id
        );

        // Only an independently verified expected outcome enters cleanup.
        self.stage = format!("{name}: verified cleanup");
        let foreign = ForeignWindows::record(&loaded, target.workspace.id)?;
        let context = CleanupContext {
            target: &target.workspace,
            layout: &entry.layout.layout,
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
        require_empty(cleaned.focused_workspace())?;

        self.return_to_primary_name(name, &target.workspace)?;

        self.passed += 1;
        println!("PASS  {name} (independent verification; success-path cleanup only)");
        Ok(())
    }

    /// Starts sway-stage through Sway's own `exec`, as a keybinding would, from the empty,
    /// focused primary, to load the case's layout there, directly or through the menu. A
    /// standalone load selects no workspace, so the primary keeps its identity.
    fn run_exec_layout(
        &mut self,
        case: launch_context::Case,
        directory: &TemporaryConfigurations,
    ) -> Result<(), String> {
        let name = case.name();
        let entry = self.prepare_exec_entry(case, directory)?;

        self.stage = format!("{name}: fresh workspace baseline");
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        let primary = self
            .primary
            .clone()
            .expect("primary selected before scenarios");
        if before.focused_workspace_id != primary.id {
            return Err("the layout case must start on the focused primary".into());
        }
        require_empty(before.focused_workspace())?;
        let fixture = self.fixture.as_ref().expect("foreign fixture prepared");
        let fixture_signature = fixture.loaded_signature.as_ref().expect("fixture verified");
        verify_fixture_state(&before, fixture, fixture_signature)?;
        let foreign = ForeignWindows::record(&before, primary.id)?;
        let preexisting: HashSet<i64> = walk(&before.tree)
            .iter()
            .map(|placed| placed.node.id)
            .collect();

        self.stage = format!("{name}: layout load through Sway's exec");
        exec_sway_stage(&self.sway, &entry)?;
        let loaded = snapshot(&self.sway, &mut self.events, None)?;

        self.stage = format!("{name}: independent verification");
        // Every snapshot already requires the primary to stay focused. Nothing may create,
        // select, or remove a workspace either.
        profile::verify_workspace_events(&loaded.workspace_events, &[])?;
        foreign.verify(&loaded)?;
        verify_fixture_state(&loaded, fixture, fixture_signature)?;
        let accepted = bind_launches(&loaded.window_events, &entry.layout.layout, &preexisting)?;
        verify_exec_records(case, &entry)?;
        verify_ownership(
            &loaded.tree,
            primary.id,
            &entry.layout.layout,
            &preexisting,
            &accepted,
        )?;
        let measurements = verify_layout(loaded.focused_workspace(), &entry.layout, &accepted)?;
        for note in &measurements.sizing_notes {
            println!("      {note}");
        }
        println!("      launch context: {}", case.description());

        // Only an independently verified expected outcome enters cleanup.
        self.stage = format!("{name}: verified cleanup");
        let context = CleanupContext {
            target: &primary,
            layout: &entry.layout.layout,
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
        require_empty(cleaned.focused_workspace())?;

        self.passed += 1;
        println!("PASS  {name} (independent verification; success-path cleanup only)");
        Ok(())
    }

    /// The four-workspace session round-trip. Sway itself opens three Foot terminals on each
    /// of `session_roundtrip::WORKSPACES`, which `profile capture` saves into a profile,
    /// once without and once with command inference. Verified cleanup closes only those
    /// windows and returns to the fixture's occupied workspace, from which Sway's `exec`
    /// starts the menu to load the inferred profile unchanged. Every reloaded window must be
    /// on its own workspace with the captured hierarchy, sizing, selections, and focus.
    fn run_session_roundtrip(&mut self, directory: &TemporaryConfigurations) -> Result<(), String> {
        let name = session_roundtrip::SCENARIO;
        self.stage = format!("{name}: preparation");
        let files = directory.subdirectory(name)?;
        let layouts = session_roundtrip_layouts(&self.run_id, &self.foot, &self.sleep);
        let launch_scripts = write_window_launch_scripts(&files, &layouts)?;

        let (original, original_preexisting) =
            self.open_session_workspaces(&layouts, &launch_scripts)?;
        let inferred_capture =
            self.capture_session_profiles(&files, &original, &original_preexisting)?;

        // Only verified capture reaches cleanup; the reload must rebuild the same desktop.
        let first_original =
            self.clean_session_workspaces(name, &original, &original_preexisting)?;
        self.switch_to_fixture_workspace(name, &first_original)?;

        let retired_ids: HashSet<i64> = original
            .iter()
            .flat_map(|workspace| workspace.accepted.keys().copied())
            .collect();
        let (reloaded, reload_preexisting) =
            self.reload_session_profile(&files, &inferred_capture, &layouts, &retired_ids)?;
        let first_reloaded = self.clean_session_workspaces(name, &reloaded, &reload_preexisting)?;
        self.return_to_primary_name(name, &first_reloaded)?;

        self.passed += 1;
        println!("PASS  {name} (independent verification; success-path cleanup only)");
        Ok(())
    }

    /// Opens the original windows through Sway alone. From the empty, focused primary, it
    /// selects each round-trip workspace in turn, sets the empty workspace's layout, then
    /// starts each window's script through Sway's own `exec`, waiting for that window before
    /// the next. Sway arranges the windows itself. Leaves the last workspace focused, and
    /// returns the workspaces with the IDs that existed before the first one.
    fn open_session_workspaces(
        &mut self,
        layouts: &[Scenario; 4],
        launch_scripts: &[Vec<PathBuf>],
    ) -> Result<(Vec<SessionWorkspace>, HashSet<i64>), String> {
        let name = session_roundtrip::SCENARIO;
        self.stage = format!("{name}: original desktop baseline");
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        let primary = self
            .primary
            .clone()
            .expect("primary selected before scenarios");
        if before.focused_workspace_id != primary.id {
            return Err("the session round-trip must start on the focused primary".into());
        }
        require_empty(before.focused_workspace())?;
        session_roundtrip::require_workspace_names(
            &before,
            &[FIXTURE_WORKSPACE, PRIMARY_WORKSPACE],
        )?;
        let fixture = self.fixture.as_ref().expect("foreign fixture prepared");
        let fixture_signature = fixture.loaded_signature.as_ref().expect("fixture verified");
        verify_fixture_state(&before, fixture, fixture_signature)?;
        let preexisting: HashSet<i64> = walk(&before.tree)
            .iter()
            .map(|placed| placed.node.id)
            .collect();

        let mut opened_workspaces: Vec<SessionWorkspace> = Vec::new();
        let mut origin = primary.clone();
        let workspace_plans = layouts
            .iter()
            .zip(launch_scripts)
            .zip(session_roundtrip::WORKSPACES);
        for ((layout, scripts), workspace_name) in workspace_plans {
            self.stage = format!("{name}: select original workspace {workspace_name}");
            process::check_interruption()?;
            self.sway.switch_workspace(workspace_name)?;
            let transition = ExpectedSwitch {
                from_id: origin.id,
                destination_name: workspace_name.into(),
            };
            let selected = snapshot(&self.sway, &mut self.events, Some(&transition))?;
            require_quiet(&selected.window_events, None)?;
            let target = profile::Target::record(&selected, workspace_name)?
                .ok_or_else(|| format!("workspace {workspace_name} missing after selection"))?;
            if preexisting.contains(&target.workspace.id) {
                return Err("created workspace reused a preexisting ID".into());
            }
            // Only the primary is empty when focus leaves it; earlier round-trip workspaces
            // hold their windows and stay.
            let origin_removed = origin == primary;
            let expected_events =
                session_roundtrip::selection_events(&origin, &target.workspace, origin_removed);
            profile::verify_workspace_events(&selected.workspace_events, &expected_events)?;

            self.stage = format!("{name}: original workspace {workspace_name} layout");
            let initial_layout = layout
                .initial_workspace_layout
                .expect("every round-trip layout starts from an empty-workspace layout");
            let foreign = ForeignWindows::record(&selected, target.workspace.id)?;
            let workspace_revision = self.events.workspace_revision();
            let prepared = prepare_initial_workspace_layout(
                &selected,
                &target.workspace,
                &foreign,
                initial_layout,
                workspace_revision,
                || {
                    process::check_interruption()?;
                    self.sway.set_empty_workspace_layout(initial_layout)?;
                    let prepared = snapshot(&self.sway, &mut self.events, None)?;
                    Ok((prepared, self.events.workspace_revision()))
                },
            )?;

            self.stage =
                format!("{name}: original workspace {workspace_name} windows through Sway's exec");
            let workspace_preexisting: HashSet<i64> = walk(&prepared.tree)
                .iter()
                .map(|placed| placed.node.id)
                .collect();
            let window_timeout = Duration::from_millis(layout.window_timeout_ms);
            let mut window_events = Vec::new();
            for (window_index, script) in scripts.iter().enumerate() {
                let events_while_opening = exec_window_and_wait(
                    &self.sway,
                    &mut self.events,
                    script,
                    &workspace_preexisting,
                    window_index + 1,
                    window_timeout,
                )?;
                window_events.extend(events_while_opening);
            }

            self.stage = format!("{name}: original workspace {workspace_name} verification");
            let mut opened = snapshot(&self.sway, &mut self.events, None)?;
            profile::verify_workspace_events(&opened.workspace_events, &[])?;
            window_events.append(&mut opened.window_events);
            let accepted = bind_launches(&window_events, &layout.layout, &workspace_preexisting)?;
            verify_ownership(
                &opened.tree,
                target.workspace.id,
                &layout.layout,
                &workspace_preexisting,
                &accepted,
            )?;
            verify_layout(target.verify(&opened)?, layout, &accepted)?;
            foreign.verify(&opened)?;
            verify_fixture_state(&opened, fixture, fixture_signature)?;
            println!(
                "      Sway opened workspace {workspace_name} id={} as {:?} with windows {:?}",
                target.workspace.id,
                initial_layout.observed_layout(),
                accepted.keys().collect::<Vec<_>>()
            );

            origin = target.workspace.clone();
            opened_workspaces.push(SessionWorkspace {
                target,
                layout: layout.clone(),
                accepted,
            });
        }
        Ok((opened_workspaces, preexisting))
    }

    /// Runs `profile capture` for the round-trip's workspaces, in order, into new private
    /// directories: first with command inference disabled, whose drafts are compared with
    /// the independently measured desktop, then with inference, whose drafts must differ
    /// only by recovering each window's own launch. Neither may change the observed desktop.
    /// Returns the inferred capture's directories, ready to load.
    fn capture_session_profiles(
        &mut self,
        files: &TemporaryConfigurations,
        opened: &[SessionWorkspace],
        preexisting: &HashSet<i64>,
    ) -> Result<CaptureDirectories, String> {
        let name = session_roundtrip::SCENARIO;
        let profile_name = session_roundtrip::PROFILE_NAME;
        self.stage = format!("{name}: read-only profile captures");
        let before_capture = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before_capture.window_events, None)?;
        session_roundtrip::verify_session_ownership(&before_capture.tree, opened, preexisting)?;
        let foreign = ForeignWindows::record(&before_capture, before_capture.focused_workspace_id)?;
        let capture_revision = self.events.workspace_revision();

        let plain = self.run_session_capture(files, "plain-capture", false)?;
        let inferred = self.run_session_capture(files, "inferred-capture", true)?;

        let after_capture = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&after_capture.window_events, None)?;
        foreign.verify(&after_capture)?;
        if capture_revision != self.events.workspace_revision()
            || signature(&before_capture.tree) != signature(&after_capture.tree)
        {
            return Err("profile capture changed the observed desktop".into());
        }

        self.stage = format!("{name}: captured profile verification");
        let profile_file_name = format!("{profile_name}.yaml");
        let layout_file_names: Vec<String> = (0..opened.len())
            .map(|index| {
                format!(
                    "{}.yaml",
                    profile::captured_layout_name(profile_name, index)
                )
            })
            .collect();
        for capture in [&plain, &inferred] {
            session_roundtrip::require_directory_files(&capture.layouts.path, &layout_file_names)?;
            session_roundtrip::require_directory_files(
                &capture.profiles.path,
                std::slice::from_ref(&profile_file_name),
            )?;
        }
        let read_capture = |path: PathBuf| {
            fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))
        };

        let plain_profile = read_capture(plain.profiles.path.join(&profile_file_name))?;
        profile::verify_captured_references(
            &plain_profile,
            profile_name,
            &session_roundtrip::WORKSPACES,
        )?;
        if read_capture(inferred.profiles.path.join(&profile_file_name))? != plain_profile {
            return Err("command inference changed the captured profile".into());
        }

        let last_index = opened.len() - 1;
        for (index, (workspace, layout_file_name)) in
            opened.iter().zip(&layout_file_names).enumerate()
        {
            let measurements =
                verify_session_layout(&after_capture, workspace, index == last_index)?;
            let plain_draft = read_capture(plain.layouts.path.join(layout_file_name))?;
            verify_capture(&plain_draft, &workspace.layout, &measurements).map_err(|error| {
                format!("workspace {}: {error}", workspace.target.workspace.name)
            })?;
            let inferred_draft = read_capture(inferred.layouts.path.join(layout_file_name))?;
            verify_recovered_launch_commands(
                &inferred_draft,
                &plain_draft,
                &workspace.layout.layout,
            )
            .map_err(|error| format!("workspace {}: {error}", workspace.target.workspace.name))?;
        }

        let mut check = Command::new(&self.binary);
        check
            .arg("--layouts-dir")
            .arg(&inferred.layouts.path)
            .arg("--profiles-dir")
            .arg(&inferred.profiles.path)
            .args(["profile", "check", profile_name]);
        self.sway.select_socket(&mut check);
        if process::run(check, &self.stage, IPC_TIMEOUT)? != format!("{profile_name}: valid\n") {
            return Err("the inferred capture is not a valid profile".into());
        }
        println!(
            "      profile capture: {} workspaces in order; hierarchy, sizing, selections, focus, \
             and selectors match; inference recovered every launch and changed nothing else",
            opened.len()
        );
        Ok(inferred)
    }

    /// One `profile capture` of the round-trip's workspaces into new private directories
    /// under `subdirectory`, which it returns. Prints nothing on stdout.
    fn run_session_capture(
        &self,
        files: &TemporaryConfigurations,
        subdirectory: &str,
        infer_commands: bool,
    ) -> Result<CaptureDirectories, String> {
        let capture_files = files.subdirectory(subdirectory)?;
        let capture = CaptureDirectories {
            layouts: capture_files.subdirectory("layouts")?,
            profiles: capture_files.subdirectory("profiles")?,
        };

        let mut command = Command::new(&self.binary);
        command
            .arg("--layouts-dir")
            .arg(&capture.layouts.path)
            .arg("--profiles-dir")
            .arg(&capture.profiles.path)
            .args(["profile", "capture", session_roundtrip::PROFILE_NAME])
            .args(session_roundtrip::WORKSPACES);
        if !infer_commands {
            command.arg("--no-infer-commands");
        }
        self.sway.select_socket(&mut command);
        process::check_interruption()?;
        let output = process::run(command, &self.stage, CAPTURE_TIMEOUT)?;
        if !output.is_empty() {
            return Err("profile capture emitted unexpected stdout".into());
        }
        Ok(capture)
    }

    /// Closes only one cycle's verified windows, workspace by workspace from the focused
    /// last one back to the first. As focus returns to each earlier workspace, Sway removes
    /// the emptied one. Returns the first workspace, empty and focused.
    fn clean_session_workspaces(
        &mut self,
        name: &str,
        workspaces: &[SessionWorkspace],
        preexisting: &HashSet<i64>,
    ) -> Result<WorkspaceIdentity, String> {
        for index in (0..workspaces.len()).rev() {
            let workspace = &workspaces[index];
            let identity = &workspace.target.workspace;

            let before_cleanup = if let Some(emptied) = workspaces.get(index + 1) {
                self.stage = format!("{name}: return to workspace {} for cleanup", identity.name);
                process::check_interruption()?;
                self.sway.switch_workspace(&identity.name)?;
                let transition = ExpectedSwitch {
                    from_id: emptied.target.workspace.id,
                    destination_name: identity.name.clone(),
                };
                let returned = snapshot(&self.sway, &mut self.events, Some(&transition))?;
                require_return_focus(&returned.window_events, &workspace.accepted)?;
                let expected_events =
                    session_roundtrip::return_events(&emptied.target.workspace, identity);
                profile::verify_workspace_events(&returned.workspace_events, &expected_events)?;
                returned
            } else {
                let observed = snapshot(&self.sway, &mut self.events, None)?;
                require_quiet(&observed.window_events, None)?;
                observed
            };

            self.stage = format!("{name}: verified cleanup of workspace {}", identity.name);
            if before_cleanup.focused_workspace_id != identity.id {
                return Err(format!(
                    "cleanup requires workspace {} focused",
                    identity.name
                ));
            }
            let remaining_workspaces = &workspaces[..=index];
            session_roundtrip::verify_session_ownership(
                &before_cleanup.tree,
                remaining_workspaces,
                preexisting,
            )?;
            verify_session_layout(&before_cleanup, workspace, true)?;
            let fixture = self.fixture.as_ref().expect("foreign fixture prepared");
            let fixture_signature = fixture.loaded_signature.as_ref().expect("fixture verified");
            verify_fixture_state(&before_cleanup, fixture, fixture_signature)?;

            let known_ids =
                session_roundtrip::known_ids_besides(remaining_workspaces, index, preexisting);
            let foreign = ForeignWindows::record(&before_cleanup, identity.id)?;
            let context = CleanupContext {
                target: identity,
                layout: &workspace.layout.layout,
                preexisting_ids: &known_ids,
                foreign: &foreign,
            };
            cleanup(
                &self.sway,
                &mut self.events,
                &context,
                &workspace.accepted,
                signature(before_cleanup.focused_workspace()),
            )?;
            let cleaned = snapshot(&self.sway, &mut self.events, None)?;
            require_quiet(&cleaned.window_events, None)?;
            foreign.verify(&cleaned)?;
            workspace.target.verify(&cleaned)?;
            require_empty(cleaned.focused_workspace())?;
        }
        Ok(workspaces[0].target.workspace.clone())
    }

    /// Starts sway-stage's menu through Sway's own `exec` on the fixture's occupied
    /// workspace, as a keybinding would, with a Fuzzel substitute that selects the inferred
    /// profile. Requires each target created and selected in profile order, exactly its own
    /// layout's new windows on it, and nothing on the fixture's workspace, which the
    /// inherited launch context names. Returns the reloaded workspaces with the IDs that
    /// may not belong to the reload.
    fn reload_session_profile(
        &mut self,
        files: &TemporaryConfigurations,
        inferred_capture: &CaptureDirectories,
        layouts: &[Scenario; 4],
        retired_ids: &HashSet<i64>,
    ) -> Result<(Vec<SessionWorkspace>, HashSet<i64>), String> {
        let name = session_roundtrip::SCENARIO;
        self.stage = format!("{name}: menu entry preparation");
        let evidence_directory = files.subdirectory("evidence")?;
        let substitute_directory = files.subdirectory("fuzzel-substitute")?;
        let selection = format!("Profile: {}", session_roundtrip::PROFILE_NAME);
        let substitute = launch_context::fuzzel_substitute_script(
            &selection,
            utf8_path(&evidence_directory.path)?,
        );
        substitute_directory.create_executable("fuzzel", &substitute)?;
        let sway_stage_command = vec![
            utf8_path(&self.binary)?.to_owned(),
            "--layouts-dir".to_owned(),
            utf8_path(&inferred_capture.layouts.path)?.to_owned(),
            "--profiles-dir".to_owned(),
            utf8_path(&inferred_capture.profiles.path)?.to_owned(),
            "menu".to_owned(),
        ];
        let entry_script = launch_context::entry_script(
            launch_context::EntryMode::Inherited,
            utf8_path(&evidence_directory.path)?,
            Some(utf8_path(&substitute_directory.path)?),
            &sway_stage_command,
        );
        let entry_script_path = files.path.join("exec-entry.sh");
        fs::write(&entry_script_path, entry_script)
            .map_err(|error| format!("write exec entry script: {error}"))?;

        self.stage = format!("{name}: reload baseline");
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        let fixture = self.fixture.as_ref().expect("foreign fixture prepared");
        let fixture_signature = fixture.loaded_signature.as_ref().expect("fixture verified");
        if before.focused_workspace_id != fixture.workspace.id {
            return Err("the reload must start on the fixture's focused workspace".into());
        }
        session_roundtrip::require_workspace_names(&before, &[FIXTURE_WORKSPACE])?;
        verify_fixture_state(&before, fixture, fixture_signature)?;
        let mut preexisting: HashSet<i64> = walk(&before.tree)
            .iter()
            .map(|placed| placed.node.id)
            .collect();
        // Closed windows cannot prove a new launch, even if Sway reused their IDs.
        preexisting.extend(retired_ids);

        // Sway records the fixture's workspace in this exec's launch context.
        self.stage = format!("{name}: profile load through the menu and Sway's exec");
        process::check_interruption()?;
        self.sway.exec_script(&entry_script_path)?;
        let load_timeout: Duration = layouts.iter().map(load_timeout).sum();
        let outcome = launch_context::ExecOutcome::wait(&evidence_directory.path, load_timeout)?;
        process::check_interruption()?;
        outcome.require_launch_context()?;
        outcome.require_success()?;
        if !outcome.stderr.is_empty() {
            return Err(format!(
                "the profile load reported unexpected diagnostics: {}",
                outcome.stderr.trim()
            ));
        }
        let expected_switches = ExpectedSwitches {
            from_id: fixture.workspace.id,
            destination_names: session_roundtrip::WORKSPACES.map(str::to_owned).to_vec(),
        };
        let (loaded, switch_indexes) =
            snapshot_with_switches(&self.sway, &mut self.events, &expected_switches)?;

        self.stage = format!("{name}: independent reload verification");
        launch_context::MenuEvidence::read(&evidence_directory.path)?
            .verify_rows(&session_roundtrip::menu_rows(), &selection)?;
        let mut targets = Vec::new();
        for workspace_name in session_roundtrip::WORKSPACES {
            let target = profile::Target::record(&loaded, workspace_name)?
                .ok_or_else(|| format!("profile target {workspace_name} missing"))?;
            if preexisting.contains(&target.workspace.id) {
                return Err("created workspace reused a preexisting ID".into());
            }
            if output_ancestry(&loaded.tree, target.workspace.id)?
                != output_ancestry(&loaded.tree, fixture.workspace.id)?
            {
                return Err(format!(
                    "profile target {workspace_name} is not on {OUTPUT_NAME}"
                ));
            }
            targets.push(target);
        }
        let target_identities: Vec<WorkspaceIdentity> = targets
            .iter()
            .map(|target| target.workspace.clone())
            .collect();
        let expected_events =
            session_roundtrip::reload_events(&fixture.workspace, &target_identities);
        profile::verify_workspace_events(&loaded.workspace_events, &expected_events)?;
        let mut expected_workspace_names = vec![FIXTURE_WORKSPACE];
        expected_workspace_names.extend(session_roundtrip::WORKSPACES);
        session_roundtrip::require_workspace_names(&loaded, &expected_workspace_names)?;
        let last_target = target_identities.last().expect("the profile has targets");
        if loaded.focused_workspace_id != last_target.id {
            return Err("the profile did not leave its last target focused".into());
        }
        // The launch context names the fixture's workspace: it must hold only the fixture.
        verify_fixture_state(&loaded, fixture, fixture_signature)?;

        let accepted_per_layout = profile::bind_sequential_launches(
            &loaded.window_events,
            &switch_indexes,
            layouts,
            &preexisting,
        )?;
        let reloaded: Vec<SessionWorkspace> = targets
            .into_iter()
            .zip(layouts.iter().cloned())
            .zip(accepted_per_layout)
            .map(|((target, layout), accepted)| SessionWorkspace {
                target,
                layout,
                accepted,
            })
            .collect();
        session_roundtrip::verify_session_ownership(&loaded.tree, &reloaded, &preexisting)?;

        let last_index = reloaded.len() - 1;
        for (index, workspace) in reloaded.iter().enumerate() {
            let measurements = verify_session_layout(&loaded, workspace, index == last_index)?;
            for note in &measurements.sizing_notes {
                println!("      {note}");
            }
            println!(
                "      reloaded workspace {} id={} with windows {:?}",
                workspace.target.workspace.name,
                workspace.target.workspace.id,
                workspace.accepted.keys().collect::<Vec<_>>()
            );
        }
        println!(
            "      menu loaded profile {:?} from the fixture's workspace id={}, which kept only \
             the fixture",
            session_roundtrip::PROFILE_NAME,
            fixture.workspace.id
        );
        Ok((reloaded, preexisting))
    }

    /// Loads a one-window layout on the empty, focused primary after a Sway `assign` rule has
    /// sent its app ID to `ASSIGNED_WORKSPACE`. sway-stage must refuse the window there after
    /// its one launch, leaving it open where Sway put it. Verified cleanup then selects that
    /// workspace, closes only the window, and returns to a new primary.
    fn run_assigned_workspace_refusal(
        &mut self,
        directory: &TemporaryConfigurations,
    ) -> Result<(), String> {
        let name = ASSIGNED_WORKSPACE_SCENARIO;
        self.stage = format!("{name}: preparation");
        let files = directory.subdirectory(name)?;
        let launch_script_path = files.path.join("record-launch.sh");
        let launch_record_path = files.path.join("launches");
        fs::write(&launch_script_path, profile::LAUNCH_SCRIPT)
            .map_err(|error| format!("write launch recorder: {error}"))?;
        let layout = assigned_workspace_layout(
            &self.run_id,
            &self.foot,
            &self.sleep,
            utf8_path(&launch_script_path)?,
            utf8_path(&launch_record_path)?,
        );
        directory.write(layout.name, &configuration_yaml(&layout))?;
        check_configuration(&self.binary, &directory.path, layout.name, &self.stage)?;

        // Sway keeps the rule until the private compositor stops. It names only this
        // scenario's run-unique app ID, so no other window can match it.
        self.stage = format!("{name}: assign rule");
        let assigned_app_id = layout.layout.leaves()[0].app_id.clone();
        process::check_interruption()?;
        self.sway
            .assign_app_id_to_workspace(&assigned_app_id, ASSIGNED_WORKSPACE)?;

        self.stage = format!("{name}: fresh workspace baselines");
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        let primary = self
            .primary
            .clone()
            .expect("primary selected before scenarios");
        if before.focused_workspace_id != primary.id {
            return Err("the assigned-workspace case must start on the focused primary".into());
        }
        require_empty(before.focused_workspace())?;
        if profile::Target::record(&before, ASSIGNED_WORKSPACE)?.is_some() {
            return Err(format!(
                "workspace {ASSIGNED_WORKSPACE} must not exist before Sway assigns the window to it"
            ));
        }
        let fixture = self.fixture.as_ref().expect("foreign fixture prepared");
        let fixture_signature = fixture.loaded_signature.as_ref().expect("fixture verified");
        verify_fixture_state(&before, fixture, fixture_signature)?;
        let foreign = ForeignWindows::record(&before, primary.id)?;
        let preexisting: HashSet<i64> = walk(&before.tree)
            .iter()
            .map(|placed| placed.node.id)
            .collect();

        self.stage = format!("{name}: load refused by placement");
        process::check_interruption()?;
        let mut load = configured_command(&self.binary, &directory.path, "load", name);
        self.sway.select_socket(&mut load);
        let completed = process::run_with_status(load, &self.stage, load_timeout(&layout))?;
        process::check_interruption()?;
        let loaded = snapshot(&self.sway, &mut self.events, None)?;

        self.stage = format!("{name}: independent verification");
        let assigned = profile::Target::record(&loaded, ASSIGNED_WORKSPACE)?.ok_or_else(|| {
            format!(
                "Sway created no workspace {ASSIGNED_WORKSPACE} for the window; sway-stage {}: {}",
                completed.status,
                completed.stderr.trim()
            )
        })?;
        if preexisting.contains(&assigned.workspace.id) {
            return Err("created workspace reused a preexisting ID".into());
        }
        // Sway created the assigned workspace without selecting it; the primary stays
        // focused and empty.
        profile::verify_workspace_events(
            &loaded.workspace_events,
            &[profile::ExpectedWorkspaceEvent::Init(&assigned.workspace)],
        )?;
        require_empty(loaded.focused_workspace())?;
        foreign.verify(&loaded)?;
        verify_fixture_state(&loaded, fixture, fixture_signature)?;

        // The one launch's window is still where Sway put it: neither moved, closed,
        // adopted, nor launched again.
        let accepted = bind_launches(&loaded.window_events, &layout.layout, &preexisting)?;
        let window_id = *accepted
            .keys()
            .next()
            .expect("a one-window layout binds exactly one launch");
        verify_ownership(
            &loaded.tree,
            assigned.workspace.id,
            &layout.layout,
            &preexisting,
            &accepted,
        )?;
        let expected_record = format!("{name}-window1\n");
        let observed_record = fs::read_to_string(&launch_record_path)
            .map_err(|error| format!("read launch record: {error}"))?;
        if observed_record != expected_record {
            return Err(format!(
                "launch count differs: expected {expected_record:?}, observed {observed_record:?}"
            ));
        }
        verify_assigned_workspace_refusal(
            &completed,
            window_id,
            assigned.workspace.id,
            primary.id,
        )?;
        println!(
            "      Sway assigned window id={window_id} to workspace {ASSIGNED_WORKSPACE} id={}; \
             sway-stage refused it there and left it open",
            assigned.workspace.id
        );

        // Only an independently verified refusal enters cleanup. The window is on an
        // unfocused workspace: select it before closing only that window there. Sway removes
        // the empty primary as focus leaves it.
        self.stage = format!("{name}: select the assigned workspace");
        process::check_interruption()?;
        self.sway.switch_workspace(ASSIGNED_WORKSPACE)?;
        let transition = ExpectedSwitch {
            from_id: primary.id,
            destination_name: ASSIGNED_WORKSPACE.into(),
        };
        let selected = snapshot(&self.sway, &mut self.events, Some(&transition))?;
        require_return_focus(&selected.window_events, &accepted)?;
        let expected_events = [
            profile::ExpectedWorkspaceEvent::Focus {
                from_id: primary.id,
                to: &assigned.workspace,
            },
            profile::ExpectedWorkspaceEvent::Empty(&primary),
        ];
        profile::verify_workspace_events(&selected.workspace_events, &expected_events)?;
        verify_fixture_state(&selected, fixture, fixture_signature)?;

        self.stage = format!("{name}: verified cleanup");
        let assigned_foreign = ForeignWindows::record(&selected, assigned.workspace.id)?;
        let context = CleanupContext {
            target: &assigned.workspace,
            layout: &layout.layout,
            preexisting_ids: &preexisting,
            foreign: &assigned_foreign,
        };
        cleanup(
            &self.sway,
            &mut self.events,
            &context,
            &accepted,
            signature(selected.focused_workspace()),
        )?;
        let cleaned = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&cleaned.window_events, None)?;
        assigned_foreign.verify(&cleaned)?;
        require_empty(cleaned.focused_workspace())?;

        self.return_to_primary_name(name, &assigned.workspace)?;

        self.passed += 1;
        println!("PASS  {name} (independent verification; success-path cleanup only)");
        Ok(())
    }

    /// Selects the fixture's workspace from `from`, which is focused and empty, so that the
    /// next exec starts there. Sway removes `from` as focus leaves it; focus returning to the
    /// fixture's window is the only window activity allowed.
    fn switch_to_fixture_workspace(
        &mut self,
        scenario_name: &str,
        from: &WorkspaceIdentity,
    ) -> Result<WorkspaceIdentity, String> {
        self.stage = format!("{scenario_name}: switch to the fixture's workspace");
        let fixture = self.fixture.as_ref().expect("foreign fixture prepared");

        process::check_interruption()?;
        self.sway.switch_workspace(&fixture.workspace.name)?;
        let transition = ExpectedSwitch {
            from_id: from.id,
            destination_name: fixture.workspace.name.clone(),
        };
        let switched = snapshot(&self.sway, &mut self.events, Some(&transition))?;

        require_return_focus(&switched.window_events, &fixture.progress.accepted)?;
        let expected_events = [
            profile::ExpectedWorkspaceEvent::Focus {
                from_id: from.id,
                to: &fixture.workspace,
            },
            profile::ExpectedWorkspaceEvent::Empty(from),
        ];
        profile::verify_workspace_events(&switched.workspace_events, &expected_events)?;
        verify_fixture_state(
            &switched,
            fixture,
            fixture.loaded_signature.as_ref().expect("fixture verified"),
        )?;
        Ok(fixture.workspace.clone())
    }

    /// Selects `PRIMARY_WORKSPACE` from `from`, which is focused and empty after verified
    /// cleanup, once Sway has removed the primary. Sway creates a new workspace with that
    /// name, which becomes the primary; nothing but it and the fixture's workspace remains.
    fn return_to_primary_name(
        &mut self,
        scenario_name: &str,
        from: &WorkspaceIdentity,
    ) -> Result<(), String> {
        self.stage = format!("{scenario_name}: return to the primary's name");
        process::check_interruption()?;
        self.sway.switch_workspace(PRIMARY_WORKSPACE)?;
        let transition = ExpectedSwitch {
            from_id: from.id,
            destination_name: PRIMARY_WORKSPACE.into(),
        };
        let returned = snapshot(&self.sway, &mut self.events, Some(&transition))?;
        require_quiet(&returned.window_events, None)?;
        require_empty(returned.focused_workspace())?;
        let new_primary = WorkspaceIdentity {
            id: returned.focused_workspace_id,
            name: PRIMARY_WORKSPACE.into(),
        };

        let settled = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&settled.window_events, None)?;
        let fixture = self.fixture.as_ref().expect("foreign fixture prepared");
        verify_fixture_state(
            &settled,
            fixture,
            fixture.loaded_signature.as_ref().expect("fixture verified"),
        )?;
        let only_fixture_and_primary = settled.workspaces.len() == 2
            && settled.workspaces.iter().all(|workspace| {
                workspace.id == fixture.workspace.id || workspace.id == new_primary.id
            });
        if settled.focused_workspace_id != new_primary.id || !only_fixture_and_primary {
            return Err(format!(
                "expected only the fixture's workspace and an empty, focused {PRIMARY_WORKSPACE}"
            ));
        }
        self.primary = Some(new_primary);
        Ok(())
    }

    /// Loads a layout on the empty, focused primary that sway-stage must fail to construct.
    /// The failure counts only after sway-stage's report, the launch record, and the desktop
    /// are verified independently: any window it accepted stays where it opened, and nothing
    /// else changed. Verified cleanup then closes only that window.
    fn run_load_failure(
        &mut self,
        case: load_failure::Case,
        directory: &TemporaryConfigurations,
    ) -> Result<(), String> {
        let name = case.name();
        self.stage = format!("{name}: preparation");
        let files = directory.subdirectory(name)?;
        let launch_script_path = files.path.join("record-launch.sh");
        let launch_record_path = files.path.join("launches");
        fs::write(&launch_script_path, profile::LAUNCH_SCRIPT)
            .map_err(|error| format!("write launch recorder: {error}"))?;
        // Inside this case's new private directory, so no installed program can be found
        // there, whatever the host provides.
        let missing_executable_path = files.path.join(load_failure::MISSING_EXECUTABLE_FILE_NAME);
        match fs::symlink_metadata(&missing_executable_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err("the missing executable's private path exists".into()),
        }
        let failing = load_failure::failing_load(
            case,
            &self.run_id,
            &self.foot,
            &self.sleep,
            utf8_path(&launch_script_path)?,
            utf8_path(&launch_record_path)?,
            utf8_path(&missing_executable_path)?,
        );
        // `check` validates the configuration only; it never looks for the executable.
        directory.write(name, &configuration_yaml(&failing.layout))?;
        check_configuration(&self.binary, &directory.path, name, &self.stage)?;

        self.stage = format!("{name}: fresh workspace baseline");
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        let primary = self
            .primary
            .clone()
            .expect("primary selected before scenarios");
        if before.focused_workspace_id != primary.id {
            return Err("the failing load must start on the focused primary".into());
        }
        require_empty(before.focused_workspace())?;
        let fixture = self.fixture.as_ref().expect("foreign fixture prepared");
        let fixture_signature = fixture.loaded_signature.as_ref().expect("fixture verified");
        verify_fixture_state(&before, fixture, fixture_signature)?;
        let foreign = ForeignWindows::record(&before, primary.id)?;
        let preexisting: HashSet<i64> = walk(&before.tree)
            .iter()
            .map(|placed| placed.node.id)
            .collect();
        let workspace_revision = self.events.workspace_revision();

        self.stage = format!("{name}: load expected to fail");
        process::check_interruption()?;
        let mut load = configured_command(&self.binary, &directory.path, "load", name);
        self.sway.select_socket(&mut load);
        let completed = process::run_with_status(load, &self.stage, load_timeout(&failing.layout))?;
        process::check_interruption()?;
        let loaded = snapshot(&self.sway, &mut self.events, None)?;

        self.stage = format!("{name}: independent verification");
        load_failure::verify_failure_report(case, &completed)?;
        // Every snapshot already requires the primary to stay focused. Nothing may create,
        // select, or remove a workspace either.
        profile::verify_workspace_events(&loaded.workspace_events, &[])?;
        foreign.verify(&loaded)?;
        verify_fixture_state(&loaded, fixture, fixture_signature)?;
        let observed_record = match fs::read_to_string(&launch_record_path) {
            Ok(record) => record,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(format!("read launch record: {error}")),
        };
        if observed_record != failing.expected_launch_record {
            return Err(format!(
                "launch order/count differs: expected {:?}, observed {observed_record:?}",
                failing.expected_launch_record
            ));
        }

        let Some(accepted_part) = &failing.accepted_part else {
            // Nothing started, so nothing anywhere may differ from the baseline.
            verify_unchanged_desktop(
                &before,
                &loaded,
                workspace_revision != self.events.workspace_revision(),
            )?;
            println!("      the spawn failed before any launch; the desktop is unchanged");
            self.passed += 1;
            println!("PASS  {name} (independent verification; nothing to clean up)");
            return Ok(());
        };

        // The accepted window is the only new window: neither moved, closed, replaced, nor
        // launched again, and still tiled and focused where it opened.
        let accepted = bind_launches(&loaded.window_events, &accepted_part.layout, &preexisting)?;
        verify_ownership(
            &loaded.tree,
            primary.id,
            &accepted_part.layout,
            &preexisting,
            &accepted,
        )?;
        verify_layout(loaded.focused_workspace(), accepted_part, &accepted)?;
        let first_window_id = *accepted
            .keys()
            .next()
            .expect("a one-window accepted part binds exactly one launch");
        println!(
            "      window1 id={first_window_id} stayed open on the primary; window2 opened no \
             window; window3 never launched"
        );

        // Only an independently verified failure enters cleanup, which the harness performs;
        // sway-stage left the window in place.
        self.stage = format!("{name}: verified cleanup");
        let context = CleanupContext {
            target: &primary,
            layout: &accepted_part.layout,
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
        require_empty(cleaned.focused_workspace())?;

        self.passed += 1;
        println!("PASS  {name} (independent verification; success-path cleanup only)");
        Ok(())
    }

    /// Opens this run's own window on the empty, focused primary and makes it floating, then
    /// asks sway-stage to load onto that floating-only workspace: directly, or as a profile's
    /// second target behind the empty, focused workspace `3`. sway-stage must refuse before
    /// launching or selecting anything. Verified cleanup then closes only the floating
    /// window. The primary is occupied whenever focus leaves it, so it keeps its identity.
    fn run_floating_refusal(
        &mut self,
        case: floating_refusal::Case,
        directory: &TemporaryConfigurations,
    ) -> Result<(), String> {
        let name = case.name();
        self.stage = format!("{name}: preparation");
        let files = directory.subdirectory(name)?;
        let layouts_directory = files.subdirectory("layouts")?;
        let profiles_directory = files.subdirectory("profiles")?;
        let launch_script_path = files.path.join("record-launch.sh");
        let launch_record_path = files.path.join("launches");
        let window_script_path = files.path.join("floating-window.sh");
        fs::write(&launch_script_path, profile::LAUNCH_SCRIPT)
            .map_err(|error| format!("write launch recorder: {error}"))?;
        let primary = self
            .primary
            .clone()
            .expect("primary selected before scenarios");

        let floating_layout =
            floating_refusal::floating_window(case, &self.run_id, &self.foot, &self.sleep);
        let floating_leaf = floating_layout.layout.leaves()[0].clone();
        fs::write(
            &window_script_path,
            session_roundtrip::window_launch_script(&floating_leaf.command),
        )
        .map_err(|error| format!("write floating window launch script: {error}"))?;

        let refused_layouts = floating_refusal::refused_layouts(
            case,
            &self.run_id,
            &self.foot,
            &self.sleep,
            utf8_path(&launch_script_path)?,
            utf8_path(&launch_record_path)?,
        );
        for layout in &refused_layouts {
            layouts_directory.write(layout.name, &configuration_yaml(layout))?;
        }
        let mut load = Command::new(&self.binary);
        load.arg("--layouts-dir").arg(&layouts_directory.path);
        match case {
            floating_refusal::Case::Layout => {
                check_configuration(&self.binary, &layouts_directory.path, name, &self.stage)?;
                load.args(["load", name]);
            }
            floating_refusal::Case::Profile => {
                // The empty first target is selected before the load; the floating-only
                // primary is the second.
                profiles_directory.write(
                    name,
                    &profile::configuration(PROFILE_WORKSPACE, &primary.name),
                )?;
                let mut check = Command::new(&self.binary);
                check
                    .arg("--layouts-dir")
                    .arg(&layouts_directory.path)
                    .arg("--profiles-dir")
                    .arg(&profiles_directory.path)
                    .args(["profile", "check", name]);
                self.sway.select_socket(&mut check);
                if process::run(check, &self.stage, IPC_TIMEOUT)? != format!("{name}: valid\n") {
                    return Err("unexpected profile check output".into());
                }
                load.arg("--profiles-dir")
                    .arg(&profiles_directory.path)
                    .args(["profile", "load", name]);
            }
        }
        self.sway.select_socket(&mut load);

        let floating_window_id =
            self.open_floating_window(name, &primary, &floating_layout, &window_script_path)?;
        let floating_window =
            AcceptedWindows::from([(floating_window_id, floating_leaf.layout_id.clone())]);

        let first_target = match case {
            floating_refusal::Case::Layout => None,
            floating_refusal::Case::Profile => {
                self.stage = format!("{name}: select the empty first target");
                process::check_interruption()?;
                self.sway.switch_workspace(PROFILE_WORKSPACE)?;
                let transition = ExpectedSwitch {
                    from_id: primary.id,
                    destination_name: PROFILE_WORKSPACE.into(),
                };
                let selected = snapshot(&self.sway, &mut self.events, Some(&transition))?;
                require_quiet(&selected.window_events, None)?;
                let first = profile::Target::record(&selected, PROFILE_WORKSPACE)?
                    .ok_or("the empty first target is missing after its selection")?;
                // The primary holds the floating window, so Sway keeps it.
                let expected_events = [
                    profile::ExpectedWorkspaceEvent::Init(&first.workspace),
                    profile::ExpectedWorkspaceEvent::Focus {
                        from_id: primary.id,
                        to: &first.workspace,
                    },
                ];
                profile::verify_workspace_events(&selected.workspace_events, &expected_events)?;
                require_empty(first.verify(&selected)?)?;
                Some(first)
            }
        };

        self.stage = format!("{name}: fresh desktop baseline");
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        floating_refusal::verify_floating_only(
            &before,
            primary.id,
            floating_window_id,
            &floating_leaf,
        )?;
        let fixture = self.fixture.as_ref().expect("foreign fixture prepared");
        let fixture_signature = fixture.loaded_signature.as_ref().expect("fixture verified");
        verify_fixture_state(&before, fixture, fixture_signature)?;
        let foreign = ForeignWindows::record(&before, primary.id)?;
        if let Some(first) = &first_target {
            let first_workspace = first.verify(&before)?;
            require_empty(first_workspace)?;
            if before.focused_workspace_id != first.workspace.id {
                return Err("the profile must start on its empty, focused first target".into());
            }
        } else if before.focused_workspace_id != primary.id {
            return Err("the load must start on the focused, floating-only primary".into());
        }
        let workspace_revision = self.events.workspace_revision();

        self.stage = format!("{name}: load expected to be refused");
        process::check_interruption()?;
        let result = process::run_with_status(load, &self.stage, CAPTURE_TIMEOUT);
        let completed = refusal_completion(result, process::interruption_requested())?;
        let after = snapshot(&self.sway, &mut self.events, None)?;

        self.stage = format!("{name}: independent verification");
        // Both also require the complete observed desktop and its event history unchanged.
        let workspace_activity = workspace_revision != self.events.workspace_revision();
        match case {
            floating_refusal::Case::Layout => {
                verify_refusal(&completed, &before, &after, workspace_activity)?;
            }
            floating_refusal::Case::Profile => verify_profile_refusal(
                &completed,
                &before,
                &after,
                &primary.name,
                primary.id,
                workspace_activity,
            )?,
        }
        floating_refusal::verify_floating_only(
            &after,
            primary.id,
            floating_window_id,
            &floating_leaf,
        )?;
        foreign.verify(&after)?;
        verify_fixture_state(&after, fixture, fixture_signature)?;
        let observed_record = match fs::read_to_string(&launch_record_path) {
            Ok(record) => record,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(format!("read launch record: {error}")),
        };
        if !observed_record.is_empty() {
            return Err(format!(
                "the refused load launched applications: {observed_record:?}"
            ));
        }
        println!(
            "      workspace {} id={} held only floating window id={floating_window_id}; \
             sway-stage refused it before any launch or selection",
            primary.name, primary.id
        );

        // Only an independently verified refusal enters cleanup, which the harness performs.
        if let Some(first) = &first_target {
            self.stage = format!("{name}: return to the primary");
            process::check_interruption()?;
            self.sway.switch_workspace(&primary.name)?;
            let transition = ExpectedSwitch {
                from_id: first.workspace.id,
                destination_name: primary.name.clone(),
            };
            let returned = snapshot(&self.sway, &mut self.events, Some(&transition))?;
            require_return_focus(&returned.window_events, &floating_window)?;
            let expected_events = [
                profile::ExpectedWorkspaceEvent::Focus {
                    from_id: first.workspace.id,
                    to: &primary,
                },
                profile::ExpectedWorkspaceEvent::Empty(&first.workspace),
            ];
            profile::verify_workspace_events(&returned.workspace_events, &expected_events)?;
            floating_refusal::verify_floating_only(
                &returned,
                primary.id,
                floating_window_id,
                &floating_leaf,
            )?;
            foreign.verify(&returned)?;
        }

        self.stage = format!("{name}: verified cleanup");
        close_floating_window(
            &self.sway,
            &mut self.events,
            &primary,
            floating_window_id,
            &floating_leaf,
            &foreign,
        )?;

        self.passed += 1;
        println!("PASS  {name} (independent verification; success-path cleanup only)");
        Ok(())
    }

    /// Opens `layout`'s one window on the empty, focused primary through Sway's own `exec` of
    /// `window_script`, verifies that this run owns it, and makes exactly that window
    /// floating. Returns its container ID once Sway reports it floating alone on the primary.
    fn open_floating_window(
        &mut self,
        scenario_name: &str,
        primary: &WorkspaceIdentity,
        layout: &Scenario,
        window_script: &Path,
    ) -> Result<i64, String> {
        self.stage = format!("{scenario_name}: open this run's window through Sway's exec");
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        if before.focused_workspace_id != primary.id {
            return Err("the floating window must open on the focused primary".into());
        }
        require_empty(before.focused_workspace())?;
        let fixture = self.fixture.as_ref().expect("foreign fixture prepared");
        let fixture_signature = fixture.loaded_signature.as_ref().expect("fixture verified");
        verify_fixture_state(&before, fixture, fixture_signature)?;
        let preexisting: HashSet<i64> = walk(&before.tree)
            .iter()
            .map(|placed| placed.node.id)
            .collect();

        let mut window_events = exec_window_and_wait(
            &self.sway,
            &mut self.events,
            window_script,
            &preexisting,
            1,
            Duration::from_millis(layout.window_timeout_ms),
        )?;
        let mut opened = snapshot(&self.sway, &mut self.events, None)?;
        profile::verify_workspace_events(&opened.workspace_events, &[])?;
        window_events.append(&mut opened.window_events);
        let accepted = bind_launches(&window_events, &layout.layout, &preexisting)?;
        verify_ownership(
            &opened.tree,
            primary.id,
            &layout.layout,
            &preexisting,
            &accepted,
        )?;
        verify_fixture_state(&opened, fixture, fixture_signature)?;
        let window_id = *accepted
            .keys()
            .next()
            .expect("a one-window layout binds exactly one launch");

        self.stage = format!("{scenario_name}: make this run's window floating");
        process::check_interruption()?;
        self.sway.float_owned_window(window_id)?;
        let floated = snapshot(&self.sway, &mut self.events, None)?;
        floating_refusal::require_float_events(&floated.window_events, window_id)?;
        profile::verify_workspace_events(&floated.workspace_events, &[])?;
        floating_refusal::verify_floating_only(
            &floated,
            primary.id,
            window_id,
            layout.layout.leaves()[0],
        )?;
        verify_fixture_state(&floated, fixture, fixture_signature)?;
        Ok(window_id)
    }

    /// Loads a three-window layout on the empty, focused primary whose second window is the
    /// injector, which interferes once sway-stage has accepted the first window. The failure
    /// counts only after sway-stage's report, the injector's and launch records, and the
    /// desktop, including the ordered events of each injected change, are verified
    /// independently. Only then does the harness return to the primary if the injector left
    /// it, and close the windows this run opened.
    fn run_interference(
        &mut self,
        case: interference::Case,
        directory: &TemporaryConfigurations,
    ) -> Result<(), String> {
        let name = case.name();
        self.stage = format!("{name}: preparation");
        let files = directory.subdirectory(name)?;
        let launch_script_path = files.path.join("record-launch.sh");
        let launch_record_path = files.path.join("launches");
        let injector_script_path = files.path.join("inject.sh");
        let injector_record_path = files.path.join("injected-commands");
        let injected_window_script_path = files.path.join("injected-window.sh");
        fs::write(&launch_script_path, profile::LAUNCH_SCRIPT)
            .map_err(|error| format!("write launch recorder: {error}"))?;
        fs::write(&injector_script_path, interference::INJECTOR_SCRIPT)
            .map_err(|error| format!("write interference injector: {error}"))?;
        let primary = self
            .primary
            .clone()
            .expect("primary selected before scenarios");
        let fixture = self.fixture.as_ref().expect("foreign fixture prepared");
        let fixture_signature = fixture.loaded_signature.as_ref().expect("fixture verified");

        let sway_commands = interference::sway_commands(
            case,
            &fixture.workspace.name,
            &primary.name,
            &injected_window_script_path,
        )?;
        let injector = interference::injector_command(
            utf8_path(&injector_script_path)?,
            utf8_path(self.sway.executable())?,
            utf8_path(self.sway.socket())?,
            utf8_path(&injector_record_path)?,
            &sway_commands,
        );
        let interfering = interference::interfering_load(
            case,
            &self.run_id,
            &self.foot,
            &self.sleep,
            utf8_path(&launch_script_path)?,
            utf8_path(&launch_record_path)?,
            &injector,
        );
        if let Some(observed_target) = &interfering.observed_target {
            let injected_window = observed_target.layout.leaves()[1];
            fs::write(
                &injected_window_script_path,
                session_roundtrip::window_launch_script(&injected_window.command),
            )
            .map_err(|error| format!("write injected window launch script: {error}"))?;
        }
        directory.write(name, &configuration_yaml(&interfering.layout))?;
        check_configuration(&self.binary, &directory.path, name, &self.stage)?;

        self.stage = format!("{name}: fresh workspace baseline");
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        if before.focused_workspace_id != primary.id {
            return Err("the interfered load must start on the focused primary".into());
        }
        require_empty(before.focused_workspace())?;
        verify_fixture_state(&before, fixture, fixture_signature)?;
        let foreign = ForeignWindows::record(&before, primary.id)?;
        let preexisting: HashSet<i64> = walk(&before.tree)
            .iter()
            .map(|placed| placed.node.id)
            .collect();

        self.stage = format!("{name}: load stopped by interference");
        process::check_interruption()?;
        let mut load = configured_command(&self.binary, &directory.path, "load", name);
        self.sway.select_socket(&mut load);
        let completed =
            process::run_with_status(load, &self.stage, load_timeout(&interfering.layout))?;
        process::check_interruption()?;
        // The fence accepts the injected switches as the harness's own expected events, in
        // order; any other focus change still fails it and distrusts focus history.
        let destinations = case.switch_destinations(&fixture.workspace.name, &primary.name);
        let (loaded, switch_window_indexes) = if destinations.is_empty() {
            (snapshot(&self.sway, &mut self.events, None)?, Vec::new())
        } else {
            let expected_switches = ExpectedSwitches {
                from_id: primary.id,
                destination_names: destinations.iter().map(|&name| name.to_owned()).collect(),
            };
            snapshot_with_switches(&self.sway, &mut self.events, &expected_switches).map_err(
                |error| {
                    format!(
                        "{error}; sway-stage {}: {}",
                        completed.status,
                        completed.stderr.trim()
                    )
                },
            )?
        };

        self.stage = format!("{name}: independent verification");
        let injector_record = match fs::read_to_string(&injector_record_path) {
            Ok(record) => record,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(format!("read injector record: {error}")),
        };
        if injector_record != interference::expected_injector_record(&sway_commands) {
            return Err(format!(
                "Sway did not accept every injected command: {injector_record:?}"
            ));
        }
        let observed_launch_record = fs::read_to_string(&launch_record_path)
            .map_err(|error| format!("read launch record: {error}"))?;
        if observed_launch_record != interfering.expected_launch_record {
            return Err(format!(
                "launch order/count differs: expected {:?}, observed {observed_launch_record:?}",
                interfering.expected_launch_record
            ));
        }

        // Exactly the injected selections, and nothing from sway-stage: in particular, it
        // never selects the target again after the injector left it.
        let expected_events = match case {
            interference::Case::SwitchAway => vec![profile::ExpectedWorkspaceEvent::Focus {
                from_id: primary.id,
                to: &fixture.workspace,
            }],
            interference::Case::SwitchAndReturn => vec![
                profile::ExpectedWorkspaceEvent::Focus {
                    from_id: primary.id,
                    to: &fixture.workspace,
                },
                profile::ExpectedWorkspaceEvent::Focus {
                    from_id: fixture.workspace.id,
                    to: &primary,
                },
            ],
            interference::Case::UnexpectedWindow => Vec::new(),
        };
        profile::verify_workspace_events(&loaded.workspace_events, &expected_events)?;
        foreign.verify(&loaded)?;
        verify_fixture_state(&loaded, fixture, fixture_signature)?;

        // Ordered new-window events bind this run's windows: the first window's launch must
        // precede any switch, after which selecting a workspace may only focus the window it
        // shows, and the injected window must open after the first window.
        let (expected_target, owned) = match &interfering.observed_target {
            Some(observed_target) => {
                let owned =
                    bind_launches(&loaded.window_events, &observed_target.layout, &preexisting)?;
                (observed_target, owned)
            }
            None => {
                let (before_switch, after_switch) =
                    loaded.window_events.split_at(switch_window_indexes[0]);
                let owned = bind_launches(
                    before_switch,
                    &interfering.accepted_part.layout,
                    &preexisting,
                )?;
                let mut focusable = owned.clone();
                focusable.extend(fixture.progress.accepted.clone());
                require_return_focus(after_switch, &focusable)?;
                (&interfering.accepted_part, owned)
            }
        };
        verify_ownership(
            &loaded.tree,
            primary.id,
            &expected_target.layout,
            &preexisting,
            &owned,
        )?;
        let primary_workspace = walk(&loaded.tree)
            .into_iter()
            .map(|placed| placed.node)
            .find(|node| node.id == primary.id && node.name.as_deref() == Some(&primary.name))
            .ok_or("the primary disappeared or changed identity")?;
        if loaded.focused_workspace_id == primary.id {
            verify_layout(primary_workspace, expected_target, &owned)?;
        } else {
            crate::layout::verify_inactive_layout(primary_workspace, expected_target, &owned)?;
        }
        let injected_window_id = owned
            .iter()
            .find(|(_, layout_id)| layout_id.as_str() == interference::INJECTED_LAYOUT_ID)
            .map(|(&container_id, _)| container_id);
        interference::verify_failure_report(case, &completed, primary.id, injected_window_id)?;
        println!(
            "      sway-stage stopped during window2's wait after {:?}; windows {:?} stayed on \
             the primary; window3 never launched",
            sway_commands,
            owned.keys().collect::<Vec<_>>()
        );

        // Only an independently verified failure enters cleanup, which the harness performs.
        let cleanup_baseline = if loaded.focused_workspace_id == primary.id {
            signature(loaded.focused_workspace())
        } else {
            self.stage = format!("{name}: return to the primary");
            process::check_interruption()?;
            self.sway.switch_workspace(&primary.name)?;
            let transition = ExpectedSwitch {
                from_id: fixture.workspace.id,
                destination_name: primary.name.clone(),
            };
            let returned = snapshot(&self.sway, &mut self.events, Some(&transition))?;
            require_return_focus(&returned.window_events, &owned)?;
            profile::verify_workspace_events(
                &returned.workspace_events,
                &[profile::ExpectedWorkspaceEvent::Focus {
                    from_id: fixture.workspace.id,
                    to: &primary,
                }],
            )?;
            verify_fixture_state(&returned, fixture, fixture_signature)?;
            foreign.verify(&returned)?;
            verify_ownership(
                &returned.tree,
                primary.id,
                &expected_target.layout,
                &preexisting,
                &owned,
            )?;
            verify_layout(returned.focused_workspace(), expected_target, &owned)?;
            signature(returned.focused_workspace())
        };

        self.stage = format!("{name}: verified cleanup");
        let context = CleanupContext {
            target: &primary,
            layout: &expected_target.layout,
            preexisting_ids: &preexisting,
            foreign: &foreign,
        };
        cleanup(
            &self.sway,
            &mut self.events,
            &context,
            &owned,
            cleanup_baseline,
        )?;
        let cleaned = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&cleaned.window_events, None)?;
        foreign.verify(&cleaned)?;
        require_empty(cleaned.focused_workspace())?;

        self.passed += 1;
        println!("PASS  {name} (independent verification; success-path cleanup only)");
        Ok(())
    }

    /// Loads a two-entry profile from the empty, focused primary and verifies it workspace by
    /// workspace, as the session round-trip's reload is: each target selected in profile
    /// order, exactly its own layout's new windows between its selection and the next, and
    /// Sway's exact workspace events, including any removal and recreation. Verified cleanup
    /// then closes only those windows, from the last target back to the first, and ends on
    /// an empty, focused primary, which may be a replacement.
    fn run_sequential_profile(
        &mut self,
        case: profile::SequentialCase,
        directory: &TemporaryConfigurations,
    ) -> Result<(), String> {
        let name = case.name();
        self.stage = format!("{name}: preparation");
        let files = directory.subdirectory(name)?;
        let layouts_directory = files.subdirectory("layouts")?;
        let profiles_directory = files.subdirectory("profiles")?;
        let launch_script_path = files.path.join("record-launch.sh");
        let launch_record_path = files.path.join("launches");
        fs::write(&launch_script_path, profile::LAUNCH_SCRIPT)
            .map_err(|error| format!("write profile launch recorder: {error}"))?;

        let primary = self
            .primary
            .clone()
            .expect("primary selected before scenarios");
        let mut entries = profile::sequential_entries(
            case,
            &primary.name,
            PROFILE_WORKSPACE,
            &self.run_id,
            &self.foot,
            &self.sleep,
        );
        for entry in &mut entries {
            profile::wrap_launches(
                &mut entry.layout.layout,
                entry.layout.name,
                utf8_path(&launch_script_path)?,
                utf8_path(&launch_record_path)?,
            );
        }

        // Entries that share a layout reference its one file.
        let mut written_layouts = BTreeSet::new();
        for entry in &entries {
            if written_layouts.insert(entry.layout.name) {
                layouts_directory.write(entry.layout.name, &configuration_yaml(&entry.layout))?;
            }
        }
        profiles_directory.write(name, &profile::sequential_configuration(&entries))?;
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
        let recorded_primary = profile::Target::record(&before, &primary.name)?
            .ok_or("primary profile workspace disappeared")?;
        if before.focused_workspace_id != primary.id || recorded_primary.workspace != primary {
            return Err("the profile must start on the focused primary".into());
        }
        require_empty(before.focused_workspace())?;
        session_roundtrip::require_workspace_names(&before, &[FIXTURE_WORKSPACE, &primary.name])?;
        let fixture = self.fixture.as_ref().expect("foreign fixture prepared");
        let fixture_signature = fixture.loaded_signature.as_ref().expect("fixture verified");
        verify_fixture_state(&before, fixture, fixture_signature)?;
        let preexisting: HashSet<i64> = walk(&before.tree)
            .iter()
            .map(|placed| placed.node.id)
            .collect();

        self.stage = format!("{name}: profile load");
        process::check_interruption()?;
        let timeout = entries
            .iter()
            .map(|entry| load_timeout(&entry.layout))
            .sum();
        let completed = process::run_with_status(profile_command("load"), &self.stage, timeout)?;
        process::check_interruption()?;
        if !completed.status.success() || !completed.stdout.trim().is_empty() {
            return Err(format!(
                "profile success failed: {}: {}",
                completed.status, completed.stderr
            ));
        }
        // Selecting the already focused primary is no switch. Every other target is one.
        let first_target_focused = entries[0].workspace == primary.name;
        let expected_switches = ExpectedSwitches {
            from_id: primary.id,
            destination_names: entries
                .iter()
                .skip(usize::from(first_target_focused))
                .map(|entry| entry.workspace.clone())
                .collect(),
        };
        let (loaded, switch_indexes) =
            snapshot_with_switches(&self.sway, &mut self.events, &expected_switches)?;

        self.stage = format!("{name}: independent verification");
        let fixture_output = output_ancestry(&loaded.tree, fixture.workspace.id)?;
        let mut targets = Vec::new();
        for entry in &entries {
            let target = profile::Target::record(&loaded, &entry.workspace)?
                .ok_or_else(|| format!("profile target {} missing", entry.workspace))?;
            if target.workspace != primary && preexisting.contains(&target.workspace.id) {
                return Err("created workspace reused a preexisting ID".into());
            }
            if output_ancestry(&loaded.tree, target.workspace.id)? != fixture_output {
                return Err(format!(
                    "profile target {} is not on {OUTPUT_NAME}",
                    entry.workspace
                ));
            }
            targets.push(target);
        }
        let target_identities: Vec<WorkspaceIdentity> = targets
            .iter()
            .map(|target| target.workspace.clone())
            .collect();
        let expected_events = profile::sequential_selection_events(&primary, &target_identities);
        profile::verify_workspace_events(&loaded.workspace_events, &expected_events)?;
        session_roundtrip::require_workspace_names(
            &loaded,
            &[FIXTURE_WORKSPACE, &primary.name, PROFILE_WORKSPACE],
        )?;
        let last_target = target_identities.last().expect("the profile has entries");
        if loaded.focused_workspace_id != last_target.id {
            return Err("the profile did not leave its last target focused".into());
        }
        verify_fixture_state(&loaded, fixture, fixture_signature)?;

        let expected_record: String = entries
            .iter()
            .flat_map(|entry| {
                entry
                    .layout
                    .layout
                    .leaves()
                    .into_iter()
                    .map(|leaf| format!("{}-{}\n", entry.layout.name, leaf.layout_id))
            })
            .collect();
        let observed_record = fs::read_to_string(&launch_record_path)
            .map_err(|error| format!("read launch record: {error}"))?;
        if observed_record != expected_record {
            return Err(format!(
                "profile launch order/count differs: expected {expected_record:?}, observed {observed_record:?}"
            ));
        }

        // A first target that was already focused received its windows before the first
        // switch, so its segment of window events starts at the beginning.
        let mut segment_starts = Vec::new();
        if first_target_focused {
            segment_starts.push(0);
        }
        segment_starts.extend(switch_indexes);
        let layouts: Vec<Scenario> = entries.iter().map(|entry| entry.layout.clone()).collect();
        let accepted_per_layout = profile::bind_sequential_launches(
            &loaded.window_events,
            &segment_starts,
            &layouts,
            &preexisting,
        )?;
        let workspaces: Vec<SessionWorkspace> = targets
            .into_iter()
            .zip(layouts)
            .zip(accepted_per_layout)
            .map(|((target, layout), accepted)| SessionWorkspace {
                target,
                layout,
                accepted,
            })
            .collect();
        session_roundtrip::verify_session_ownership(&loaded.tree, &workspaces, &preexisting)?;
        let observed_targets = workspaces
            .iter()
            .map(|workspace| workspace.target.verify(&loaded))
            .collect::<Result<Vec<_>, _>>()?;
        profile::require_distinct_new_containers(&observed_targets, &preexisting)?;

        let last_index = workspaces.len() - 1;
        for (index, workspace) in workspaces.iter().enumerate() {
            let measurements = verify_session_layout(&loaded, workspace, index == last_index)?;
            for note in &measurements.sizing_notes {
                println!("      {note}");
            }
            println!(
                "      workspace {} id={} received layout {:?} with new windows {:?}",
                workspace.target.workspace.name,
                workspace.target.workspace.id,
                workspace.layout.name,
                workspace.accepted.keys().collect::<Vec<_>>()
            );
        }
        let recreated_primary = target_identities
            .iter()
            .find(|target| target.name == primary.name && target.id != primary.id);
        if let Some(recreated_primary) = recreated_primary {
            println!(
                "      Sway removed the empty primary {} id={} when focus left it; selecting {} \
                 again created id={}",
                primary.name, primary.id, primary.name, recreated_primary.id
            );
        }

        // Only an independently verified expected outcome enters cleanup.
        let first_target = self.clean_session_workspaces(name, &workspaces, &preexisting)?;
        if first_target != primary {
            self.return_to_primary_name(name, &first_target)?;
        }

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
        let second_number = if case == profile::Case::WorkspaceZero {
            ZERO_WORKSPACE
        } else {
            PROFILE_WORKSPACE
        };
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
            profile::Case::Success
                | profile::Case::NamedSuccess
                | profile::Case::EventBurst
                | profile::Case::WorkspaceZero
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
                profile::verify_captured_references(
                    &profile_yaml,
                    "captured",
                    &[&primary.name, second_number],
                )?;
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

/// Runs the entry script through Sway's `exec`, which records the focused workspace in a
/// launch context, and waits for sway-stage to exit. A failure reports sway-stage's own
/// diagnostic before any observation that it would make misleading.
fn exec_sway_stage(sway: &Swaymsg, entry: &ExecEntry) -> Result<(), String> {
    process::check_interruption()?;
    sway.exec_script(&entry.entry_script_path)?;
    let outcome =
        launch_context::ExecOutcome::wait(&entry.evidence_directory, load_timeout(&entry.layout))?;
    process::check_interruption()?;

    outcome.require_launch_context()?;
    outcome.require_success()
}

/// Verifies one round-trip workspace's arrangement. Only the workspace that is
/// `globally_focused` holds keyboard focus; the others keep their selected windows in Sway's
/// child-focus histories alone.
fn verify_session_layout(
    observed: &Snapshot,
    workspace: &SessionWorkspace,
    globally_focused: bool,
) -> Result<LayoutMeasurements, String> {
    let observed_workspace = workspace.target.verify(observed)?;
    let measurements = if globally_focused {
        verify_layout(observed_workspace, &workspace.layout, &workspace.accepted)
    } else {
        crate::layout::verify_inactive_layout(
            observed_workspace,
            &workspace.layout,
            &workspace.accepted,
        )
    };
    measurements.map_err(|error| format!("workspace {}: {error}", workspace.target.workspace.name))
}

/// Writes the script that Sway's `exec` runs for each original window of the round-trip, one
/// list per workspace in leaf order.
fn write_window_launch_scripts(
    files: &TemporaryConfigurations,
    layouts: &[Scenario; 4],
) -> Result<Vec<Vec<PathBuf>>, String> {
    let scripts_directory = files.subdirectory("window-launches")?;
    let mut launch_scripts = Vec::new();
    for layout in layouts {
        let mut workspace_scripts = Vec::new();
        for leaf in layout.layout.leaves() {
            let script_path = scripts_directory
                .path
                .join(format!("{}-{}.sh", layout.name, leaf.layout_id));
            fs::write(
                &script_path,
                session_roundtrip::window_launch_script(&leaf.command),
            )
            .map_err(|error| format!("write window launch script: {error}"))?;
            workspace_scripts.push(script_path);
        }
        launch_scripts.push(workspace_scripts);
    }
    Ok(launch_scripts)
}

/// Starts one original window's script through Sway's own `exec` and polls until
/// `expected_new_windows` windows absent from `preexisting` have appeared and reported their
/// app IDs, the last of them just opened. No workspace may change meanwhile. Returns every
/// window event read, so the caller binds launches from the complete ordered history.
fn exec_window_and_wait(
    sway: &Swaymsg,
    events: &mut Events,
    script: &Path,
    preexisting: &HashSet<i64>,
    expected_new_windows: usize,
    window_timeout: Duration,
) -> Result<Vec<WindowEvent>, String> {
    process::check_interruption()?;
    sway.exec_script(script)?;
    let deadline = Instant::now() + window_timeout;

    let mut window_events = Vec::new();
    loop {
        process::check_interruption()?;
        let mut observed = snapshot(sway, events, None)?;
        profile::verify_workspace_events(&observed.workspace_events, &[])?;
        window_events.append(&mut observed.window_events);

        let new_windows: Vec<&TreeNode> = walk(&observed.tree)
            .into_iter()
            .map(|placed| placed.node)
            .filter(|node| is_window(node) && !preexisting.contains(&node.id))
            .collect();
        if new_windows.len() > expected_new_windows {
            return Err(format!(
                "more windows appeared than were started: {}",
                outline(observed.focused_workspace())
            ));
        }
        let all_identified = new_windows.iter().all(|window| window.app_id.is_some());
        if new_windows.len() == expected_new_windows && all_identified {
            return Ok(window_events);
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "no identified window within {window_timeout:?} of exec {}",
                script.display()
            ));
        }
        thread::sleep(WINDOW_POLL_INTERVAL);
    }
}

/// Every configured window launched exactly once, in layout order, and a menu case's
/// Fuzzel substitute received the menu and selected the case's row.
fn verify_exec_records(case: launch_context::Case, entry: &ExecEntry) -> Result<(), String> {
    let expected_record: String = entry
        .layout
        .layout
        .leaves()
        .iter()
        .map(|leaf| format!("{}-{}\n", entry.layout.name, leaf.layout_id))
        .collect();
    let observed_record = fs::read_to_string(&entry.launch_record_path)
        .map_err(|error| format!("read launch record: {error}"))?;
    if observed_record != expected_record {
        return Err(format!(
            "launch order/count differs: expected {expected_record:?}, observed {observed_record:?}"
        ));
    }

    if case.menu_selection().is_some() {
        launch_context::MenuEvidence::read(&entry.evidence_directory)?.verify(case)?;
    }
    Ok(())
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

/// `single-window` with a recorded launch and an app ID unique to this scenario and run,
/// which its `assign` rule names.
fn assigned_workspace_layout(
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
) -> Scenario {
    let unique_id = format!("{run_id}-{ASSIGNED_WORKSPACE_SCENARIO}");
    let mut layout = standard_scenarios(&unique_id, foot, sleep)
        .into_iter()
        .find(|scenario| scenario.name == "single-window")
        .expect("standard single-window scenario");
    layout.name = ASSIGNED_WORKSPACE_SCENARIO;
    profile::wrap_launches(
        &mut layout.layout,
        layout.name,
        launch_script,
        launch_record,
    );
    layout
}

/// sway-stage's refusal of the window that Sway assigned elsewhere: exit 1, naming the
/// window, the workspace it was found on, and the focused target, with no cleanup or
/// relaunch.
fn verify_assigned_workspace_refusal(
    output: &process::CompletedProcess,
    window_id: i64,
    assigned_workspace_id: i64,
    target_workspace_id: i64,
) -> Result<(), String> {
    let placement_failure = format!(
        "placement failure: wrong workspace: matching window id={window_id} \
         (workspace=Some({assigned_workspace_id}) "
    );
    let expected_target = format!("expected workspace={target_workspace_id};");
    let refused = output.status.code() == Some(1)
        && output.stdout.trim().is_empty()
        && output.stderr.contains(&placement_failure)
        && output.stderr.contains(&expected_target)
        && output
            .stderr
            .contains("stopped without cleanup or relaunch");
    if !refused {
        return Err(format!(
            "expected sway-stage to refuse window id={window_id} on assigned workspace \
             id={assigned_workspace_id} (exit 1), got {}: {}",
            output.status,
            output.stderr.trim()
        ));
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
        return Err("the refused load changed desktop state or workspace event history".into());
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

/// Closes this run's one floating window on the focused `target`, after fresh evidence that
/// it is still the window this run opened, alone and floating there. The ordinary cleanup
/// accepts only tiled windows, so this is its floating counterpart: the same trusted focus,
/// event, preservation, and identity checks before the one exact-ID close, then a bounded
/// wait for the window to disappear and leave `target` empty.
fn close_floating_window(
    sway: &Swaymsg,
    events: &mut Events,
    target: &WorkspaceIdentity,
    window_id: i64,
    leaf: &ConfiguredWindow,
    foreign: &ForeignWindows,
) -> Result<(), String> {
    process::check_interruption()?;
    let before = snapshot(sway, events, None)?;
    if !events.focus_history_trusted() || before.focused_workspace_id != target.id {
        return Err("cleanup requires trusted target workspace focus".into());
    }
    require_quiet(&before.window_events, None)?;
    foreign.verify(&before)?;
    floating_refusal::verify_floating_only(&before, target.id, window_id, leaf)?;

    // The sole destructive desktop command. IPC cannot make the guard/command atomic.
    process::check_interruption()?;
    sway.close_owned_window(window_id)?;
    let no_remaining_windows = AcceptedWindows::new();
    let expected_close = ExpectedClose {
        closing_window_id: window_id,
        remaining: &no_remaining_windows,
    };
    let deadline = Instant::now() + IPC_TIMEOUT;
    loop {
        process::check_interruption()?;
        let observed = snapshot(sway, events, None)?;
        require_quiet(&observed.window_events, Some(&expected_close))?;
        foreign.verify(&observed)?;

        let disappeared = !walk(&observed.tree)
            .iter()
            .any(|placed| placed.node.id == window_id);
        if disappeared {
            return require_empty(observed.focused_workspace());
        }
        floating_refusal::verify_floating_only(&observed, target.id, window_id, leaf)?;

        if Instant::now() >= deadline {
            return Err(format!(
                "cleanup timeout: container {window_id} did not disappear"
            ));
        }
        thread::sleep(CLOSE_POLL_INTERVAL);
    }
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

/// Paths written into scripts and configuration files must be UTF-8.
fn utf8_path(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("{} is not UTF-8", path.display()))
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

    /// A new owner-only executable, such as a substitute that a child finds on `PATH`.
    fn create_executable(&self, file_name: &str, contents: &str) -> Result<(), String> {
        let path = self.path.join(file_name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&path)
            .map_err(|error| format!("create executable {}: {error}", path.display()))?;
        file.write_all(contents.as_bytes())
            .map_err(|error| format!("write executable {}: {error}", path.display()))
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
