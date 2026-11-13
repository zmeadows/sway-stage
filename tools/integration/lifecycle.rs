//! Explicit executable lifecycle: preflight, scenarios, verified closes, and restoration.
//! A failure leaves windows alone. Only the success path enters cleanup; restoration is
//! a separate, guarded operation. Files and the IPC helper may be dropped, never windows.

use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{self, Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;

use crate::capture::{
    executable_capture, verify_capture, verify_desktop_launcher_capture,
    verify_desktop_replay_capture, verify_inferred_capture, verify_package_capture,
    verify_package_default_capture, verify_unresolved_capture,
};
use crate::ipc::{
    IPC_TIMEOUT, InitialWorkspaceLayout, Swaymsg, TreeNode, validate_restoration_name,
    workspace_number,
};
use crate::layout::{
    Shape, Signature, class, is_window, observed_identifier, outline, remove_from_shape, shape,
    signature, verify_layout,
};
use crate::observation::{
    Events, ExpectedSwitch, Failure, Snapshot, WorkspaceIdentity, snapshot, walk,
};
use crate::ownership::{
    AcceptedWindows, ExpectedClose, bind_launches, require_quiet, verify_ownership,
    verify_process_evidence,
};
use crate::packages::{self, Launch, Ownership, Package};
use crate::preservation::ForeignWindows;
use crate::scenarios::{
    ConfiguredNode, DesktopLauncher, Extension, OptionalApplications, Scenario, configuration_yaml,
    configure_desktop_launcher, configure_process_execution, desktop_launcher_replay,
    extended_scenarios, foreign_fixture_scenario, prepare_extended, standard_scenarios,
};
use crate::{process, profile};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);
const CLOSE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const RESTORED: &str = "Original workspace restored.";
const LOST_CONTROL: &str =
    "Original workspace not restored: focus history is uncertain or user took control.";
const ORIGINAL_GONE: &str =
    "Original workspace not restored: its recorded identity no longer exists.";

#[derive(Debug, Parser)]
#[command(
    about = "Manual layout/profile checks on two or three disposable workspaces in the existing Sway session"
)]
pub struct Arguments {
    #[arg(long, required = true, value_parser = workspace_number)]
    workspace: String,
    #[arg(long, required = true, value_parser = workspace_number,
        help = "Second empty disposable workspace for the owned foreign-window fixture")]
    foreign_workspace: String,
    #[arg(long, value_parser = workspace_number,
        help = "Third empty disposable workspace; enables four multi-workspace profile scenarios")]
    profile_workspace: Option<String>,
    #[arg(
        long,
        help = "Also test Firefox, Xterm/Xwayland, duplicate Foot IDs, desktop launcher replay, unresolved capture, and optional package inference"
    )]
    extended: bool,
    #[arg(
        long,
        help = "Exact native Firefox app ID, overriding application.ini RemotingName"
    )]
    firefox_app_id: Option<String>,
    #[arg(
        long,
        requires = "extended",
        value_name = "PATH",
        help = "Existing executable AppImage for optional command-inference coverage"
    )]
    appimage: Option<PathBuf>,
    #[arg(long, requires = "extended", value_name = "APP_ID", value_parser = parse_flatpak_id,
        help = "Installed Flatpak application for optional command-inference coverage")]
    flatpak: Option<String>,
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/target/release/sway-stage"))]
    binary: String,
}

/// Last accepted evidence is retained for diagnostics even if a later verification fails.
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
    ) -> Result<(), Failure> {
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
    fn begin_cycle(
        &mut self,
        before: &Snapshot,
        retired_ids: &HashSet<i64>,
    ) -> Result<(), Failure> {
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
    fixture: Option<ForeignFixture>,
    refusal_verified: bool,
    profile_passed: usize,
    profile_progress: Vec<(String, AcceptedWindows)>,
    stage: String,
    passed: usize,
    skipped: usize,
    retained_profiles: Vec<PathBuf>,
    restoration: Option<String>,
    packages: Vec<(&'static str, Option<Package>)>,
    package_preexisting: Option<HashSet<i64>>,
    package_launch_pid: Option<u32>,
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

        if let Some(pid) = harness.package_launch_pid {
            eprintln!(
                "  Package launch PID={pid}; package windows/applications retained without termination or automatic lifetime limit."
            );
        }
        if harness.switch_attempted {
            for (workspace, accepted) in &harness.profile_progress {
                eprintln!(
                    "  Profile workspace {workspace}: last known owned IDs {accepted:?}; remaining windows retained on failure (some verified closes may already have completed)."
                );
            }
            report_remaining(
                &harness.sway,
                &harness.run_id,
                harness.current_scenario.as_ref(),
                harness.package_preexisting.as_ref(),
            );
            if let Some(fixture) = &harness.fixture {
                eprintln!(
                    "  Foreign fixture workspace {:?}, id={}, last accepted={:?}; terminal lifetime 3600 seconds.",
                    fixture.workspace.name, fixture.workspace.id, fixture.progress.accepted
                );
            }
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
    println!(
        "{} successful scenarios with foreign-window preservation; {} expected occupied-workspace refusal verified.",
        harness.passed - harness.profile_passed,
        usize::from(harness.refusal_verified)
    );
    println!(
        "{} profile scenarios verified (including expected failures).",
        harness.profile_passed
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
        let mut packages = Vec::new();
        if arguments.extended {
            for name in packages::SCENARIOS {
                let configured = Package::configured(
                    name,
                    arguments.appimage.as_deref(),
                    arguments.flatpak.as_deref(),
                )?;
                packages.push((name, configured));
            }
        }
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
            fixture: None,
            refusal_verified: false,
            profile_passed: 0,
            profile_progress: Vec::new(),
            stage: "preflight".into(),
            passed: 0,
            skipped: 0,
            retained_profiles: Vec::new(),
            restoration: None,
            packages,
            package_preexisting: None,
            package_launch_pid: None,
        })
    }

    fn execute(&mut self) -> Result<(), Failure> {
        let initial = preflight(&self.sway, &mut self.events, &self.arguments.workspace)?;
        fixture_eligibility(&self.arguments.workspace, &self.arguments.foreign_workspace)?;
        preflight(
            &self.sway,
            &mut self.events,
            &self.arguments.foreign_workspace,
        )?;
        if let Some(second) = &self.arguments.profile_workspace {
            profile::workspace_eligibility(
                &self.arguments.workspace,
                &self.arguments.foreign_workspace,
                second,
            )?;
            preflight(&self.sway, &mut self.events, second)?;
        }
        self.original = Some(initial.original);

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
        self.prepare_fixture(&directory)?;
        self.stage = "primary target: preflight".into();
        let ready = preflight(&self.sway, &mut self.events, &self.arguments.workspace)?;
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

        let target_baseline = ForeignWindows::record(&switched, switched.focused_workspace_id)?;

        for scenario in scenarios {
            interruption_check()?;
            self.current_scenario = Some(ScenarioProgress {
                scenario,
                preexisting_ids: None,
                accepted: AcceptedWindows::new(),
            });
            if let Err(failure) =
                self.run_scenario(&directory, optional_applications.as_ref(), &target_baseline)
            {
                if self
                    .current_scenario
                    .as_ref()
                    .is_some_and(|progress| progress.scenario.name == "unresolved-launcher")
                {
                    self.events.distrust_focus_history(
                        "unresolved-launcher failed; no restoration permitted",
                    );
                }
                return Err(failure);
            }
        }

        for (name, package) in std::mem::take(&mut self.packages) {
            self.current_scenario = None;
            let Some(package) = package else {
                self.skipped += 1;
                let flag = if name == "appimage-inference" {
                    "--appimage"
                } else {
                    "--flatpak"
                };
                println!("SKIP  {name:19} {flag} not supplied");
                continue;
            };
            // All package failures retain windows and prohibit restoration, including a
            // failed assertion after ownership was established. Never retry the launch.
            if let Err(failure) = self.run_package(name, package, &directory) {
                self.events
                    .distrust_focus_history("package scenario failed; retained windows");
                return Err(failure);
            }
        }

        self.current_scenario = None;
        if let Some(second) = self.arguments.profile_workspace.clone() {
            for case in profile::CASES {
                let result = if case == profile::Case::NamedSuccess {
                    self.run_named_profile(&second, &directory)
                } else {
                    self.run_profile(case, &second, &directory)
                };
                if let Err(failure) = result {
                    self.events
                        .distrust_focus_history("profile scenario failed; retained windows");
                    return Err(failure);
                }
            }
        } else {
            self.skipped += profile::CASES.len();
            for case in profile::CASES {
                println!("SKIP  {} --profile-workspace not supplied", case.name());
            }
        }

        self.finish_fixture()?;
        self.stage = "final restoration".into();
        let restoration = self.restore()?;
        self.restoration = Some(restoration.to_owned());
        if restoration != RESTORED {
            return Err(Failure::Check(restoration.to_owned()));
        }
        Ok(())
    }

    fn run_named_profile(
        &mut self,
        second: &str,
        directory: &TemporaryConfigurations,
    ) -> Result<(), Failure> {
        let original_name = self
            .disposable
            .as_ref()
            .expect("primary selected")
            .name
            .clone();
        let named = format!("stage-{} music", self.run_id);
        self.rename_empty_primary(&named)?;
        self.run_profile(profile::Case::NamedSuccess, second, directory)?;
        // Name restoration is an explicit verified success phase, never a failure handler.
        self.rename_empty_primary(&original_name)?;
        self.passed += 1;
        self.profile_passed += 1;
        println!(
            "PASS  profile-named-success (independent verification; cleanup and original name verified)"
        );
        Ok(())
    }

    fn rename_empty_primary(&mut self, destination: &str) -> Result<(), Failure> {
        self.stage = "profile named target: guarded disposable rename".into();
        let before = snapshot(&self.sway, &mut self.events, None)?;
        let primary = self.disposable.as_ref().expect("primary selected");
        profile::verify_rename_preparation(&before, primary, destination)?;
        let foreign = ForeignWindows::record(&before, primary.id)?;
        let renamed = WorkspaceIdentity {
            id: primary.id,
            name: destination.into(),
        };
        println!(
            "      renaming disposable id={} {:?} to {:?}; failure retains its current name",
            primary.id, primary.name, destination
        );
        interruption_check()?;
        self.sway.rename_disposable(&primary.name, destination)?;
        let after =
            crate::observation::snapshot_after_rename(&self.sway, &mut self.events, &renamed)?;
        profile::verify_rename_result(&before, &after, &renamed)?;
        foreign.verify(&after)?;
        self.disposable = Some(renamed);
        Ok(())
    }

    fn run_profile(
        &mut self,
        case: profile::Case,
        second_number: &str,
        directory: &TemporaryConfigurations,
    ) -> Result<(), Failure> {
        let name = case.name();
        let destination = if case == profile::Case::Occupied {
            self.arguments.foreign_workspace.clone()
        } else {
            second_number.to_owned()
        };
        self.profile_progress = vec![
            (
                self.disposable
                    .as_ref()
                    .expect("primary selected")
                    .name
                    .clone(),
                AcceptedWindows::new(),
            ),
            (destination.clone(), AcceptedWindows::new()),
        ];
        self.stage = format!("{name}: preparation");
        let files = directory.subdirectory(name)?;
        let layouts_directory = files.subdirectory("layouts")?;
        let profiles_directory = files.subdirectory("profiles")?;
        let script_path = files.path.join("record-launch.sh");
        let record_path = files.path.join("launches");
        fs::write(&script_path, profile::LAUNCH_SCRIPT)
            .map_err(|error| format!("write profile launch recorder: {error}"))?;
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
        );
        for layout in &layouts {
            layouts_directory.write(layout.name, &configuration_yaml(layout))?;
        }
        profiles_directory.write(
            name,
            &profile::configuration(
                &self.disposable.as_ref().expect("primary selected").name,
                &destination,
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
            return Err(Failure::Uncertain("unexpected profile check output".into()));
        }
        let mut command = profile_command("load");
        if matches!(case, profile::Case::Occupied | profile::Case::Partial) {
            files.suppress_notification(&mut command, &env::var_os("PATH").unwrap_or_default())?;
        }

        self.stage = format!("{name}: fresh workspace baselines");
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        let primary = self
            .disposable
            .as_ref()
            .expect("primary selected before scenarios");
        let primary_target = profile::Target::record(&before, &primary.name)?
            .ok_or_else(|| Failure::Uncertain("primary profile workspace disappeared".into()))?;
        if before.focused_workspace_id != primary.id || primary_target.workspace != *primary {
            return Err(Failure::Uncertain(
                "profile requires the original primary target focused".into(),
            ));
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
            preflight_eligibility(&before, second_number)?;
            profile::Target::record(&before, second_number)?
        };
        let preexisting: HashSet<i64> = walk(&before.tree)
            .iter()
            .map(|placed| placed.node.id)
            .collect();
        let workspace_revision = self.events.workspace_revision();

        self.stage = format!("{name}: profile load");
        interruption_check()?;
        let timeout = load_timeout(&layouts[0]) + load_timeout(&layouts[1]);
        let completed =
            process::run_with_status(command, &self.stage, timeout).map_err(Failure::Uncertain)?;
        interruption_check()?;
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
                &destination,
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
                .ok_or_else(|| Failure::Uncertain("second profile target missing".into()))?;
            if let Some(expected) = &second_before {
                expected.verify(&loaded)?;
            } else if preexisting.contains(&second.workspace.id) {
                return Err(Failure::Uncertain(
                    "created workspace reused a preexisting ID".into(),
                ));
            }
            // A created target must belong to an output that existed before loading.
            let output = crate::preservation::output_ancestry(&loaded.tree, second.workspace.id)?;
            for (output_id, output_name) in &output {
                if !walk(&before.tree).iter().any(|placed| {
                    placed.node.node_type == crate::ipc::NodeType::Output
                        && placed.node.id == *output_id
                        && placed.node.name == *output_name
                }) {
                    return Err(Failure::Uncertain(
                        "profile target has an unexpected containing output".into(),
                    ));
                }
            }
            if loaded.focused_workspace_id != second.workspace.id {
                return Err(Failure::Uncertain(
                    "profile did not leave the second workspace focused".into(),
                ));
            }
            for (target_index, (layout, (target, owned))) in layouts
                .iter()
                .zip([(&primary_target, &accepted[0]), (&second, &accepted[1])])
                .enumerate()
            {
                verify_ownership(
                    &loaded.tree,
                    target.workspace.id,
                    &layout.layout,
                    &preexisting,
                    owned,
                )?;
                self.profile_progress[target_index].1 = owned.clone();
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
                    return Err(Failure::Uncertain(format!(
                        "profile success failed: {}: {}",
                        completed.status, completed.stderr
                    )));
                }
                let measurements =
                    verify_layout(second.verify(&loaded)?, &layouts[1], &accepted[1])?;
                for note in measurements.sizing_notes {
                    println!("      {note}");
                }
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
                    package: None,
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
            interruption_check()?;
            self.sway.restore_workspace(&primary.name)?;
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
                package: None,
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
        self.profile_progress.clear();
        if case != profile::Case::NamedSuccess {
            self.passed += 1;
            self.profile_passed += 1;
            println!("PASS  {name} (independent verification; success-path cleanup only)");
        }
        Ok(())
    }

    fn run_scenario(
        &mut self,
        directory: &TemporaryConfigurations,
        optional_applications: Option<&OptionalApplications>,
        target_baseline: &ForeignWindows,
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
        if scenario.extension == Some(Extension::DesktopLauncher) {
            configure_desktop_launcher(scenario, &directory.path)?;
        }

        self.stage = format!("{name}: check");
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
                    .disposable
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
                .disposable
                .as_ref()
                .expect("successful switch established target");

            self.stage = format!("{cycle_name}: load");
            interruption_check()?;
            let mut load = configured_command(
                &self.binary,
                &directory.path,
                "load",
                if replay_cycle { "replay" } else { name },
            );
            self.sway.select_socket(&mut load);
            let output = process::run(load, &self.stage, load_timeout(scenario))
                .map_err(Failure::Uncertain)?;
            if !output.trim().is_empty() {
                return Err(Failure::Check("load produced unexpected stdout".into()));
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
                let terminal_pid = terminal.pid.ok_or_else(|| {
                    Failure::Uncertain("process scenario has no terminal PID".into())
                })?;
                Some(
                    process::verify_probe(Path::new("/proc"), terminal_pid, leaf)
                        .map_err(Failure::Uncertain)?,
                )
            } else {
                None
            };
            let before_capture = signature(loaded.focused_workspace());

            if name == "occupied-workspace-refusal" {
                self.stage = format!("{cycle_name}: expected refusal");
                // A different, already checked layout has distinct selectors. A broken load
                // cannot silently substitute its new windows for the first layout's windows.
                let search_path = env::var_os("PATH").unwrap_or_default();
                let mut refused = directory.refusal_command(&self.binary, &search_path)?;
                self.sway.select_socket(&mut refused);

                let before_refusal = snapshot(&self.sway, &mut self.events, None)?;
                foreign.verify(&before_refusal)?;
                verify_unchanged(&before_refusal, target.id, progress, &before_capture)?;
                let revision = self.events.workspace_revision();
                let result = process::run_with_status(refused, &self.stage, CAPTURE_TIMEOUT);
                let output = refusal_completion(result, process::interruption_requested())?;
                let after_refusal =
                    snapshot(&self.sway, &mut self.events, None).map_err(Failure::Uncertain)?;
                verify_refusal(
                    &output,
                    &before_refusal,
                    &after_refusal,
                    revision != self.events.workspace_revision(),
                )?;
                foreign.verify(&after_refusal)?;
                verify_unchanged(&after_refusal, target.id, progress, &before_capture)?;
                self.refusal_verified = true;
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

            if name == "unresolved-launcher" {
                self.stage = format!("{cycle_name}: unresolved inferred capture");
                let leaf = progress.scenario.layout.leaves()[0];
                let fixture = leaf
                    .desktop_launcher
                    .as_ref()
                    .expect("prepared desktop fixture");
                let window_id = *progress
                    .accepted
                    .keys()
                    .next()
                    .expect("verified single window");
                let pid = walk(&after_capture.tree)
                    .into_iter()
                    .find(|placed| placed.node.id == window_id)
                    .and_then(|placed| placed.node.pid)
                    .filter(|pid| *pid > 0)
                    .ok_or_else(|| Failure::Uncertain("launcher window lost its PID".into()))?;

                // Hold this proc directory across capture. Reads through the descriptor
                // cannot silently follow a reused PID to a replacement process.
                let process_directory = File::open(format!("/proc/{pid}")).map_err(|_| {
                    Failure::Uncertain("cannot retain launcher process identity".into())
                })?;
                let retained_process =
                    PathBuf::from(format!("/proc/self/fd/{}", process_directory.as_raw_fd()));
                verify_process_evidence(&retained_process, pid, leaf)
                    .map_err(|reason| Failure::Uncertain(reason.into()))?;
                let mut capture = Command::new(&self.binary);
                capture.arg("capture");
                self.sway.select_socket(&mut capture);
                let revision = self.events.workspace_revision();
                let output = directory.with_unavailable_launcher(fixture, || {
                    process::run_with_status(capture, &self.stage, CAPTURE_TIMEOUT)
                })?;

                // Restoration and independent file verification precede ALL subsequent
                // process/Sway ownership checks, even when the capture command failed.
                verify_process_evidence(&retained_process, pid, leaf)
                    .map_err(|reason| Failure::Uncertain(reason.into()))?;
                let after_unresolved = snapshot(&self.sway, &mut self.events, None)?;
                foreign.verify(&after_unresolved)?;
                verify_unchanged(&after_unresolved, target.id, progress, &before_capture)?;
                if revision != self.events.workspace_revision() {
                    return Err(Failure::Uncertain(
                        "workspace activity during unresolved capture".into(),
                    ));
                }
                verify_unresolved_capture(&output, &draft)?;
            }

            let mut inferred_replay = None;
            if matches!(
                name,
                "single-window"
                    | "horizontal-pair"
                    | "duplicate-foot-ids"
                    | "desktop-launcher-roundtrip"
            ) {
                self.stage = format!("{cycle_name}: inferred capture");
                let mut capture = Command::new(&self.binary);
                capture.arg("capture");
                self.sway.select_socket(&mut capture);
                let inferred = process::run(capture, &self.stage, CAPTURE_TIMEOUT)?;
                let after_inferred = snapshot(&self.sway, &mut self.events, None)?;
                foreign.verify(&after_inferred)?;
                verify_unchanged(&after_inferred, target.id, progress, &before_capture)?;
                if name == "desktop-launcher-roundtrip" {
                    let fixture = progress.scenario.layout.leaves()[0]
                        .desktop_launcher
                        .as_ref()
                        .expect("prepared desktop fixture");
                    if replay_cycle {
                        verify_desktop_replay_capture(&inferred, &draft, fixture)?;
                    } else {
                        verify_desktop_launcher_capture(&inferred, &draft, fixture)?;
                        inferred_replay = Some(inferred);
                    }
                } else {
                    verify_inferred_capture(&inferred, &draft, &progress.scenario.layout)?;
                }
            }

            let replay = if let Some(inferred) = inferred_replay {
                self.stage = format!("{cycle_name}: save inferred replay");
                // This fixture has one root window, so no measured weights need copying
                // into the independent model. Save the actual, fully verified YAML.
                directory.write("replay", &inferred)?;
                Some(desktop_launcher_replay(&progress.scenario))
            } else if progress.scenario.captures_and_reloads() && !replay_cycle {
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
                package: None,
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
            interruption_check()?;

            for note in measurements.sizing_notes {
                println!("      {cycle_name}: above 1-point sizing target: {note}");
            }
            if let Some(replay) = replay {
                // Reached only after exact-ID cleanup, a synchronized empty tree, and
                // foreign preservation. Any earlier error returns without another launch.
                self.stage = format!("{cycle_name}: prepare replay");
                if name == "desktop-launcher-roundtrip" {
                    check_configuration(&self.binary, &directory.path, "replay", &self.stage)?;
                }
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
        } else if name == "unresolved-launcher" {
            "GLib provenance, unresolved capture/reporting, restored fixture, unchanged desktop"
        } else if name == "desktop-launcher-roundtrip" {
            "hierarchy, focus, ownership, GLib provenance, inferred capture/reload"
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

    fn run_package(
        &mut self,
        name: &'static str,
        package: Package,
        directory: &TemporaryConfigurations,
    ) -> Result<(), Failure> {
        self.stage = format!("{name}: package availability");
        package.require_available()?;
        self.stage = format!("{name}: prelaunch baseline");
        interruption_check()?;
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        require_empty(before.focused_workspace())?;
        let target = self
            .disposable
            .as_ref()
            .expect("primary target established");
        let fixture = self.fixture.as_ref().expect("foreign fixture established");
        verify_fixture_state(
            &before,
            fixture,
            fixture.loaded_signature.as_ref().expect("fixture verified"),
        )?;
        let foreign = ForeignWindows::record(&before, target.id)?;
        let preexisting: HashSet<i64> = walk(&before.tree)
            .iter()
            .map(|placed| placed.node.id)
            .collect();
        self.package_preexisting = Some(preexisting.clone());
        let revision = self.events.workspace_revision();

        self.stage = format!("{name}: direct package launch and discovery");
        let (mut launcher, launch) = Launch::start(package).map_err(Failure::Uncertain)?;
        self.package_launch_pid = Some(launcher.id());
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut creation_events = Vec::new();
        let (loaded, container_id, ownership, scenario) = loop {
            interruption_check()?;
            let mut observed = snapshot(&self.sway, &mut self.events, None)?;
            foreign.verify(&observed)?;
            if revision != self.events.workspace_revision() {
                return Err(Failure::Uncertain(
                    "workspace activity during package scenario".into(),
                ));
            }
            creation_events.append(&mut observed.window_events);
            if creation_events.len() > 4096 {
                return Err(Failure::Uncertain(
                    "package discovery exceeded event-history limit".into(),
                ));
            }
            if let Some(window) =
                packages::discover(&observed.tree, &creation_events, &preexisting, target.id)
                    .map_err(Failure::Uncertain)?
            {
                let container_id = window.id;
                let ownership = launch.accept(window).map_err(Failure::Uncertain)?;
                let scenario = packages::captured_scenario(name, packages::selector(window)?);
                break (observed, container_id, ownership, scenario);
            }
            if Instant::now() >= deadline {
                return Err(Failure::Uncertain("package discovery timed out after 60 seconds; missing/unusable app, singleton, or unresolved window identity; leave windows untouched".into()));
            }
            thread::sleep(CLOSE_POLL_INTERVAL);
        };
        let accepted = BTreeMap::from([(container_id, "window1".to_owned())]);
        verify_ownership(
            &loaded.tree,
            target.id,
            &scenario.layout,
            &preexisting,
            &accepted,
        )?;
        verify_layout(loaded.focused_workspace(), &scenario, &accepted)?;
        let baseline = signature(loaded.focused_workspace());
        self.current_scenario = Some(ScenarioProgress {
            scenario,
            preexisting_ids: Some(preexisting),
            accepted,
        });
        let progress = self
            .current_scenario
            .as_ref()
            .expect("package window accepted");

        let mut default_draft = None;
        for infer in [false, true] {
            self.stage = format!(
                "{name}: {}capture",
                if infer { "inferred " } else { "ordinary " }
            );
            // Recheck after discovery and before each capture, then again after it. Capture
            // must cause neither desktop mutations nor new application/window launches.
            let ready = snapshot(&self.sway, &mut self.events, None)?;
            verify_package_state(
                &ready, &foreign, target.id, progress, &loaded, &launch, &ownership,
            )?;
            if revision != self.events.workspace_revision() {
                return Err(Failure::Uncertain(
                    "workspace activity before package capture".into(),
                ));
            }
            let mut command = Command::new(&self.binary);
            command.arg("capture");
            if !infer {
                command.arg("--no-infer-commands");
            }
            self.sway.select_socket(&mut command);
            let draft = process::run(command, &self.stage, CAPTURE_TIMEOUT)?;
            let after = snapshot(&self.sway, &mut self.events, None)?;
            verify_package_state(
                &after, &foreign, target.id, progress, &loaded, &launch, &ownership,
            )?;
            if revision != self.events.workspace_revision() {
                return Err(Failure::Uncertain(
                    "workspace activity during package capture".into(),
                ));
            }
            if infer {
                verify_package_capture(
                    &draft,
                    default_draft.as_deref().expect("ordinary capture verified"),
                    &ownership.expected_command,
                )?;
                directory.write("captured", &draft)?;
            } else {
                verify_package_default_capture(
                    &draft,
                    &progress.scenario.layout.leaves()[0].selector,
                )?;
                default_draft = Some(draft);
            }
        }
        check_configuration(
            &self.binary,
            &directory.path,
            "captured",
            "check inferred package capture",
        )?;

        self.stage = format!("{name}: guarded cleanup");
        let context = CleanupContext {
            target,
            layout: &progress.scenario.layout,
            preexisting_ids: progress
                .preexisting_ids
                .as_ref()
                .expect("package baseline recorded"),
            foreign: &foreign,
            package: Some(PackageCleanup {
                launch: &launch,
                ownership: &ownership,
                workspace_revision: revision,
            }),
        };
        cleanup(
            &self.sway,
            &mut self.events,
            &context,
            &progress.accepted,
            baseline,
        )?;
        let cleaned = snapshot(&self.sway, &mut self.events, None)?;
        foreign.verify(&cleaned)?;
        require_quiet(&cleaned.window_events, None)?;
        require_empty(cleaned.focused_workspace())?;
        require_package_workspace_history(&context, self.events.workspace_revision())?;
        // Reap only if already exited. Never wait for or terminate a packaged application.
        launcher
            .try_wait()
            .map_err(|error| format!("reap package launcher: {error}"))?;
        self.package_launch_pid = None;
        self.package_preexisting = None;
        self.passed += 1;
        println!(
            "PASS  {name:19} package provenance, dynamic identity, inferred capture, unchanged desktop"
        );
        Ok(())
    }

    fn prepare_fixture(&mut self, directory: &TemporaryConfigurations) -> Result<(), Failure> {
        self.stage = "foreign fixture: preflight".into();
        let scenario = foreign_fixture_scenario(&self.run_id, &self.foot, &self.sleep);
        directory.write(scenario.name, &configuration_yaml(&scenario))?;
        check_configuration(&self.binary, &directory.path, scenario.name, &self.stage)?;
        let ready = preflight(
            &self.sway,
            &mut self.events,
            &self.arguments.foreign_workspace,
        )?;
        self.stage = "foreign fixture: workspace switch".into();
        interruption_check()?;
        self.switch_attempted = true;
        self.sway
            .switch_disposable(&self.arguments.foreign_workspace)?;
        let transition = ExpectedSwitch {
            from_id: ready.original.id,
            destination_name: self.arguments.foreign_workspace.clone(),
        };
        let before = snapshot(&self.sway, &mut self.events, Some(&transition))?;
        require_quiet(&before.window_events, None)?;
        require_empty(before.focused_workspace())?;
        if ready
            .designated_id
            .is_some_and(|id| id != before.focused_workspace_id)
        {
            return Err(Failure::Uncertain(
                "foreign fixture workspace identity changed during switch".into(),
            ));
        }
        let workspace = WorkspaceIdentity {
            id: before.focused_workspace_id,
            name: self.arguments.foreign_workspace.clone(),
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
        )
        .map_err(Failure::Uncertain)?;
        if !output.trim().is_empty() {
            return Err(Failure::Uncertain(
                "fixture load produced unexpected stdout".into(),
            ));
        }
        let loaded = snapshot(&self.sway, &mut self.events, None).map_err(Failure::Uncertain)?;
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
            "Foreign fixture: workspace {:?}, container IDs {:?} (verified; lifetime 3600 seconds).",
            fixture.workspace.name,
            fixture.progress.accepted.keys().collect::<Vec<_>>()
        );
        Ok(())
    }

    fn finish_fixture(&mut self) -> Result<(), Failure> {
        self.stage = "foreign fixture: verified return".into();
        let fixture = self
            .fixture
            .as_ref()
            .expect("fixture prepared before scenarios");
        let target = self
            .disposable
            .as_ref()
            .expect("scenario target established");
        let before = snapshot(&self.sway, &mut self.events, None)?;
        require_quiet(&before.window_events, None)?;
        require_empty(before.focused_workspace())?;
        fixture.foreign.verify(&before)?;
        // Prove the fixture survived the last scenario before deliberately changing focus.
        let baseline = fixture
            .loaded_signature
            .as_ref()
            .expect("fixture load verified");
        verify_fixture_state(&before, fixture, baseline)?;
        interruption_check()?;
        self.sway.switch_disposable(&fixture.workspace.name)?;
        let transition = ExpectedSwitch {
            from_id: target.id,
            destination_name: fixture.workspace.name.clone(),
        };
        let returned = snapshot(&self.sway, &mut self.events, Some(&transition))?;
        for event in &returned.window_events {
            if event.change == crate::ipc::WindowChange::Focus
                && fixture.progress.accepted.contains_key(&event.container.id)
            {
                continue;
            }
            require_quiet(std::slice::from_ref(event), None)?;
        }
        fixture.foreign.verify(&returned)?;
        verify_fixture_state(&returned, fixture, baseline)?;
        if returned.focused_workspace_id != fixture.workspace.id {
            return Err(Failure::Uncertain(
                "foreign fixture workspace identity changed".into(),
            ));
        }
        self.stage = "foreign fixture: cleanup".into();
        let context = CleanupContext {
            package: None,
            target: &fixture.workspace,
            layout: &fixture.progress.scenario.layout,
            preexisting_ids: fixture
                .progress
                .preexisting_ids
                .as_ref()
                .expect("recorded before fixture load"),
            foreign: &fixture.foreign,
        };
        cleanup(
            &self.sway,
            &mut self.events,
            &context,
            &fixture.progress.accepted,
            signature(returned.focused_workspace()),
        )?;
        let cleaned = snapshot(&self.sway, &mut self.events, None)?;
        fixture.foreign.verify(&cleaned)?;
        require_quiet(&cleaned.window_events, None)?;
        require_empty(cleaned.focused_workspace())?;
        println!("Foreign fixture preservation and verified cleanup passed.");
        Ok(())
    }

    fn restore(&mut self) -> Result<&'static str, Failure> {
        interruption_check()?;
        let current = self.events.current_workspace();
        let disposable = self
            .disposable
            .as_ref()
            .filter(|target| Some(*target) == current)
            .or_else(|| {
                self.fixture
                    .as_ref()
                    .map(|fixture| &fixture.workspace)
                    .filter(|target| Some(*target) == current)
            });
        let (Some(original), Some(disposable)) = (&self.original, disposable) else {
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

fn parse_flatpak_id(application: &str) -> Result<String, String> {
    packages::validate_flatpak_id(application)?;
    Ok(application.into())
}

fn verify_package_state(
    observed: &Snapshot,
    foreign: &ForeignWindows,
    target_workspace_id: i64,
    progress: &ScenarioProgress,
    baseline: &Snapshot,
    launch: &Launch,
    ownership: &Ownership,
) -> Result<(), Failure> {
    foreign.verify(observed)?;
    verify_unchanged(
        observed,
        target_workspace_id,
        progress,
        &signature(baseline.focused_workspace()),
    )?;
    require_signature(
        &observed.tree,
        &signature(&baseline.tree),
        "package capture changed the modeled desktop",
    )?;
    let window_id = *progress
        .accepted
        .keys()
        .next()
        .expect("one accepted package window");
    let window = walk(&observed.tree)
        .into_iter()
        .find(|placed| placed.node.id == window_id)
        .ok_or_else(|| Failure::Uncertain("package window disappeared".into()))?
        .node;
    ownership.verify(launch, window).map_err(Failure::Uncertain)
}

fn fixture_eligibility(target_number: &str, fixture_number: &str) -> Result<(), String> {
    workspace_number(target_number)?;
    workspace_number(fixture_number)?;
    if target_number == fixture_number {
        return Err(
            "foreign fixture workspace must differ from the primary disposable target".into(),
        );
    }
    Ok(())
}

/// Ownership is re-established on the fixture's own workspace, never by selecting all
/// preexisting foreign windows. Focus is checked separately around the expected switch.
fn verify_fixture_state(
    observed: &Snapshot,
    fixture: &ForeignFixture,
    baseline: &Signature,
) -> Result<(), Failure> {
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
        .ok_or_else(|| Failure::Uncertain("foreign fixture workspace disappeared".into()))?;
    let mut current = signature(workspace);
    current.ignore_focus();
    // The stored signature has focus from creation; compare a fresh signature with the
    // same policy without changing the authoritative baseline.
    let mut baseline_without_focus = baseline.clone();
    baseline_without_focus.ignore_focus();
    if current != baseline_without_focus {
        return Err(Failure::Uncertain(
            "foreign fixture hierarchy/identity/geometry/state changed".into(),
        ));
    }
    Ok(())
}

/// A timeout, failed observation stream, or interrupt cannot count as an expected
/// refusal, even if the child printed the occupied diagnostic before it stopped.
fn refusal_completion(
    result: Result<process::CompletedProcess, String>,
    interrupted: bool,
) -> Result<process::CompletedProcess, Failure> {
    if interrupted {
        return Err(Failure::Uncertain(
            "interrupted during occupied refusal".into(),
        ));
    }
    result.map_err(Failure::Uncertain)
}

fn verify_refusal(
    output: &process::CompletedProcess,
    before: &Snapshot,
    after: &Snapshot,
    workspace_activity: bool,
) -> Result<(), Failure> {
    before.validate()?;
    after.validate()?;
    if before.focused_workspace().nodes.is_empty()
        && before.focused_workspace().floating_nodes.is_empty()
    {
        return Err(Failure::Uncertain(
            "occupied refusal has no occupied baseline".into(),
        ));
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
        return Err(Failure::Uncertain(format!(
            "expected occupied-workspace prelaunch refusal (exit 1), got {}: {}",
            output.status,
            output.stderr.trim()
        )));
    }
    Ok(())
}

fn verify_unchanged_desktop(
    before: &Snapshot,
    after: &Snapshot,
    workspace_activity: bool,
) -> Result<(), Failure> {
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
        return Err(Failure::Uncertain(
            "occupied refusal changed desktop state or workspace event history".into(),
        ));
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
) -> Result<(), Failure> {
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
        return Err(Failure::Uncertain(format!(
            "expected occupied profile refusal: {}: {}",
            output.status, output.stderr
        )));
    }
    Ok(())
}

fn verify_profile_partial_failure(
    output: &process::CompletedProcess,
    primary: &WorkspaceIdentity,
    second_number: &str,
) -> Result<(), Failure> {
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
        return Err(Failure::Uncertain(format!(
            "expected second-workspace timeout with completed primary: {}: {}",
            output.status, output.stderr
        )));
    }
    Ok(())
}

fn verify_profile_launch_record(
    path: &Path,
    layouts: &[Scenario; 2],
    case: profile::Case,
) -> Result<(), Failure> {
    let observed = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(Failure::Uncertain(format!("read launch record: {error}"))),
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
        return Err(Failure::Uncertain(format!(
            "profile launch order/count differs: expected {expected:?}, observed {observed:?}"
        )));
    }
    Ok(())
}

fn require_return_focus(
    events: &[crate::ipc::WindowEvent],
    accepted: &AcceptedWindows,
) -> Result<(), Failure> {
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

/// Shared pre/post guard for the one empty-workspace layout mutation. The baseline
/// freezes the target output and foreign windows at the explicitly selected target.
fn verify_empty_target(
    observed: &Snapshot,
    target: &WorkspaceIdentity,
    baseline: &ForeignWindows,
) -> Result<(), Failure> {
    observed.validate()?;
    let workspace = observed.focused_workspace();
    if workspace.id != target.id || workspace.name.as_deref() != Some(&target.name) {
        return Err(Failure::Uncertain(
            "initial layout target identity/focus changed".into(),
        ));
    }
    baseline.verify(observed)?;
    require_quiet(&observed.window_events, None)?;
    require_empty(workspace).map_err(Failure::Uncertain)
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
) -> Result<Snapshot, Failure> {
    verify_empty_target(before, target, baseline)?;

    let (prepared, after_revision) = mutate_and_observe().map_err(Failure::Uncertain)?;
    verify_empty_target(&prepared, target, baseline)?;
    if before_revision != after_revision {
        return Err(Failure::Uncertain(
            "workspace activity during initial layout preparation".into(),
        ));
    }
    let actual = prepared.focused_workspace().layout;
    if actual != initial_layout.observed_layout() {
        return Err(Failure::Uncertain(format!(
            "empty workspace initial layout was not established: expected {:?}, observed {actual:?}",
            initial_layout.observed_layout()
        )));
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
    target_workspace_id: i64,
    progress: &ScenarioProgress,
    baseline: &Signature,
) -> Result<(), Failure> {
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
) -> Result<(), Failure> {
    if signature(workspace) != *baseline {
        return Err(Failure::Uncertain(message.to_owned()));
    }
    Ok(())
}

struct PackageCleanup<'a> {
    launch: &'a Launch,
    ownership: &'a Ownership,
    workspace_revision: u64,
}

struct CleanupContext<'a> {
    package: Option<PackageCleanup<'a>>,
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
    if context.package.is_none()
        && context
            .layout
            .leaves()
            .iter()
            .any(|leaf| leaf.command.is_empty())
    {
        return Err(Failure::Uncertain(
            "dynamic package cleanup requires independent package provenance".into(),
        ));
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

    if let Some(package) = &context.package {
        let window = walk(&observed.tree)
            .into_iter()
            .find(|placed| placed.node.id == closing_window_id)
            .ok_or_else(|| Failure::Uncertain("package cleanup window disappeared".into()))?
            .node;
        package
            .ownership
            .verify(package.launch, window)
            .map_err(Failure::Uncertain)?;
    }

    if !remaining.contains_key(&closing_window_id) || closing_window_id <= 0 {
        return Err(Failure::Uncertain(
            "close requires an individually verified owned window".into(),
        ));
    }

    let hierarchy = remove_from_shape(shape(workspace)?, closing_window_id);
    let mut expected_remaining = remaining.clone();
    expected_remaining.remove(&closing_window_id);
    Ok(ClosePlan {
        remaining: expected_remaining,
        hierarchy,
    })
}

fn require_package_workspace_history(
    context: &CleanupContext,
    revision: u64,
) -> Result<(), Failure> {
    if let Some(package) = &context.package
        && package.workspace_revision != revision
    {
        return Err(Failure::Uncertain(
            "workspace activity during package cleanup".into(),
        ));
    }
    Ok(())
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
        let closing_window_id = accepted
            .iter()
            .find(|(_, layout_id)| **layout_id == leaf.layout_id)
            .map(|(&id, _)| id)
            .ok_or_else(|| Failure::Uncertain("missing accepted cleanup window".into()))?;
        interruption_check()?;
        let before = snapshot(sway, events, None)?;
        require_package_workspace_history(context, events.workspace_revision())?;
        let plan = prepare_close(
            &before,
            context,
            &remaining,
            &baseline,
            closing_window_id,
            events.focus_history_trusted(),
        )?;

        // The sole destructive desktop command. IPC cannot make the guard/command atomic.
        interruption_check()?;
        sway.close_owned_window(closing_window_id)?;
        let deadline = Instant::now() + IPC_TIMEOUT;
        let after = loop {
            interruption_check()?;
            let observed = snapshot(sway, events, None)?;
            require_package_workspace_history(context, events.workspace_revision())?;
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
                return Err(Failure::Check(format!(
                    "cleanup timeout: container {closing_window_id} did not disappear"
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
    closing_window_id: i64,
) -> Result<bool, Failure> {
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
        if let Some(package) = &context.package {
            let window = walk(&observed.tree)
                .into_iter()
                .find(|placed| placed.node.id == closing_window_id)
                .expect("disappearance checked above")
                .node;
            package
                .ownership
                .verify(package.launch, window)
                .map_err(Failure::Uncertain)?;
        }
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

fn report_remaining(
    sway: &Swaymsg,
    run_id: &str,
    progress: Option<&ScenarioProgress>,
    package_preexisting: Option<&HashSet<i64>>,
) {
    eprintln!(
        "No further cleanup attempted. Scenario terminals expire after 300 seconds; the foreign fixture after 3600 seconds; Firefox has no lifetime limit."
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
            .or(package_preexisting)
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

/// Owns private YAML files, the refusal notification stub, and the process test cwd;
/// never profiles or desktop windows.
struct TemporaryConfigurations {
    path: PathBuf,
    retain: Cell<bool>,
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
        Ok(Self {
            path,
            retain: Cell::new(false),
        })
    }

    fn subdirectory(&self, name: &str) -> Result<Self, String> {
        let path = self.path.join(name);
        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|error| format!("create private profile directory: {error}"))?;
        Ok(Self {
            path,
            retain: Cell::new(false),
        })
    }

    /// Only our private launcher is hidden, only while this bounded inference runs.
    /// Restoration is ordinary control flow and is attempted even on capture failure or
    /// interruption. A restoration failure retains the files and forbids desktop cleanup.
    fn with_unavailable_launcher(
        &self,
        fixture: &DesktopLauncher,
        capture: impl FnOnce() -> Result<process::CompletedProcess, String>,
    ) -> Result<process::CompletedProcess, Failure> {
        let applications = self.path.join("unresolved desktop data/applications");
        if fixture.path.parent() != Some(applications.as_path()) {
            return Err(Failure::Uncertain(
                "launcher is outside the private fixture directory".into(),
            ));
        }
        let original = fs::symlink_metadata(&fixture.path)
            .map_err(|_| Failure::Uncertain("cannot inspect private launcher".into()))?;
        if !original.is_file() || !private_launcher_matches(fixture) {
            return Err(Failure::Uncertain(
                "private launcher changed before capture".into(),
            ));
        }
        let hidden = fixture.path.with_extension("desktop.unavailable");
        match fs::symlink_metadata(&hidden) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => {
                return Err(Failure::Uncertain(
                    "private launcher backup path is occupied or inaccessible".into(),
                ));
            }
        }
        fs::rename(&fixture.path, &hidden)
            .map_err(|_| Failure::Uncertain("cannot hide private launcher".into()))?;

        let result = capture();

        // Do not overwrite an unexpected replacement, even inside our private directory.
        let path_absent = match fs::symlink_metadata(&fixture.path) {
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
            Ok(_) => false,
        };
        let restored = path_absent && fs::rename(&hidden, &fixture.path).is_ok();
        let verified = restored
            && fs::symlink_metadata(&fixture.path).is_ok_and(|metadata| {
                metadata.is_file()
                    && metadata.dev() == original.dev()
                    && metadata.ino() == original.ino()
            })
            && private_launcher_matches(fixture);
        if !verified {
            self.retain.set(true);
            return Err(Failure::Uncertain(format!(
                "private launcher restoration failed; files retained at {}; leave windows untouched",
                applications.display()
            )));
        }
        result.map_err(Failure::Uncertain)
    }

    fn create_process_cwd(&self) -> Result<(), String> {
        DirBuilder::new()
            .mode(0o700)
            .create(self.path.join("process cwd"))
            .map_err(|error| format!("create private process cwd: {error}"))
    }

    /// Only the intentionally refused load intercepts notify-send. Keep its exit status,
    /// stderr, bounded execution, and desktop verification unchanged.
    fn refusal_command(&self, binary: &Path, search_path: &OsStr) -> Result<Command, String> {
        let mut command = configured_command(binary, &self.path, "load", "single-window");
        self.suppress_notification(&mut command, search_path)?;
        Ok(command)
    }

    fn suppress_notification(
        &self,
        command: &mut Command,
        search_path: &OsStr,
    ) -> Result<(), String> {
        let notification_bin = self.path.join("refusal-notification-bin");
        let child_search_path = env::join_paths(
            std::iter::once(notification_bin.clone()).chain(env::split_paths(search_path)),
        )
        .map_err(|error| format!("prepare refusal notification PATH: {error}"))?;

        DirBuilder::new()
            .mode(0o700)
            .create(&notification_bin)
            .map_err(|error| format!("create private refusal notification directory: {error}"))?;
        let mut stub = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(notification_bin.join("notify-send"))
            .map_err(|error| format!("create refusal notify-send stub: {error}"))?;
        stub.write_all(b"#!/bin/sh\nexit 0\n")
            .map_err(|error| format!("write refusal notify-send stub: {error}"))?;

        command.env("PATH", child_search_path);
        Ok(())
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

fn private_launcher_matches(fixture: &DesktopLauncher) -> bool {
    let Ok(file) = File::open(&fixture.path) else {
        return false;
    };
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1).read_to_end(&mut bytes).is_ok() && bytes == fixture.contents.as_bytes()
}

impl Drop for TemporaryConfigurations {
    fn drop(&mut self) {
        if self.retain.get() {
            return;
        }
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
