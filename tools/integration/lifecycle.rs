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
    verify_process_evidence,
};
use crate::packages::{self, Launch, Ownership, Package};
use crate::preservation::ForeignWindows;
use crate::process;
use crate::scenarios::{
    ConfiguredNode, DesktopLauncher, Extension, OptionalApplications, Scenario, configuration_yaml,
    configure_desktop_launcher, configure_process_execution, desktop_launcher_replay,
    extended_scenarios, foreign_fixture_scenario, prepare_extended, standard_scenarios,
};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);
const CLOSE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const RESTORED: &str = "Original workspace restored.";
const LOST_CONTROL: &str =
    "Original workspace not restored: focus history is uncertain or user took control.";
const ORIGINAL_GONE: &str =
    "Original workspace not restored: its recorded identity no longer exists.";

#[derive(Debug, Parser)]
#[command(
    about = "Manual load/capture checks on two disposable workspaces in the existing Sway session"
)]
pub struct Arguments {
    #[arg(long, required = true, value_parser = workspace_number)]
    workspace: String,
    #[arg(long, required = true, value_parser = workspace_number,
        help = "Second empty disposable workspace for the owned foreign-window fixture")]
    foreign_workspace: String,
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
        harness.passed,
        usize::from(harness.refusal_verified)
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

        for scenario in scenarios {
            interruption_check()?;
            self.current_scenario = Some(ScenarioProgress {
                scenario,
                preexisting_ids: None,
                accepted: AcceptedWindows::new(),
            });
            if let Err(failure) = self.run_scenario(&directory, optional_applications.as_ref()) {
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

        self.finish_fixture()?;
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
            let before_load = snapshot(&self.sway, &mut self.events, None)?;
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
                    process::verify_probe(Path::new("/proc"), terminal_pid, leaf, &directory.path)
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
            capture.arg("capture");
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
                capture.args(["capture", "--infer-commands"]);
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
                capture.args(["capture", "--infer-commands"]);
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
            } else if matches!(name, "capture-reload-roundtrip" | "mixed-capture-reload")
                && !replay_cycle
            {
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
        } else if matches!(name, "capture-reload-roundtrip" | "mixed-capture-reload") {
            "hierarchy, sizing, focus, identity, capture/reload round-trip"
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
            if infer {
                command.arg("--infer-commands");
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
    target_id: i64,
    progress: &ScenarioProgress,
    baseline: &Snapshot,
    launch: &Launch,
    ownership: &Ownership,
) -> Result<(), Failure> {
    foreign.verify(observed)?;
    verify_unchanged(
        observed,
        target_id,
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
            .find(|placed| placed.node.id == closing_id)
            .ok_or_else(|| Failure::Uncertain("package cleanup window disappeared".into()))?
            .node;
        package
            .ownership
            .verify(package.launch, window)
            .map_err(Failure::Uncertain)?;
    }
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
        let closing_id = accepted
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
            require_package_workspace_history(context, events.workspace_revision())?;
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
    context.foreign.verify(observed)?;
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
        if let Some(package) = &context.package {
            let window = walk(&observed.tree)
                .into_iter()
                .find(|placed| placed.node.id == closing_id)
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

        let mut command = configured_command(binary, &self.path, "load", "single-window");
        command.env("PATH", child_search_path);
        Ok(command)
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
