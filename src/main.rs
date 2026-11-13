mod capture;
mod command_inference;
mod config;
mod diagnostics;
mod discovery;
mod launch;
mod menu;
mod observation;
mod profile;

use clap::{CommandFactory, Parser, Subcommand};
use diagnostics::Diagnostics;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    version,
    about = "Validate, construct, and capture nested Sway layouts",
    after_help = "Load applies sibling weights and final focus on the focused empty workspace. Sizing aims within 1 percentage point and warns beyond 5, after at most three passes per split. Capture automatically infers best-effort launch commands that may contain sensitive arguments; --no-infer-commands leaves commands null. Review inferred commands before use or sharing, and fill remaining placeholders before checking or loading a draft. Menu uses one Fuzzel selector to load layouts or profiles, or save captures as NAME.yaml in the layouts directory; edit the saved draft and run check before loading it."
)]
struct Cli {
    /// Read layouts, and save menu captures, in PATH instead of the XDG configuration directory
    #[arg(long, global = true, value_name = "PATH")]
    layouts_dir: Option<PathBuf>,
    /// Read profiles in PATH instead of the XDG configuration directory
    #[arg(long, global = true, value_name = "PATH")]
    profiles_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Construct a weighted tiled layout and select final focus on the focused empty workspace
    Load {
        /// Layout name (the YAML filename without its extension)
        name: String,
        /// Override layout cwd; window cwd still wins. Relative paths use the invoking directory
        #[arg(long, value_name = "PATH")]
        cwd: Option<PathBuf>,
    },
    /// Print available layout names in lexical order without parsing their contents
    List,
    /// Validate a layout without contacting Sway or launching applications
    Check {
        /// Layout name (the YAML filename without its extension)
        name: String,
    },
    /// Validate or sequentially load named multi-workspace profiles
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Print an editable YAML draft of the focused workspace's tiled layout, read-only
    Capture {
        /// Disable automatic launch-command inference; leave commands as null
        #[arg(long)]
        no_infer_commands: bool,
    },
    /// Use one Fuzzel menu to load layouts or profiles, or capture the current workspace
    Menu {
        /// Disable automatic launch-command inference for captures; leave commands as null
        #[arg(long)]
        no_infer_commands: bool,
    },
}

#[derive(Subcommand)]
enum ProfileCommand {
    /// Construct layouts in order on the profile's explicitly targeted empty workspaces
    Load {
        /// Profile name (the YAML filename without its extension)
        name: String,
    },
    /// Validate a profile and every referenced layout without contacting Sway
    Check {
        /// Profile name (the YAML filename without its extension)
        name: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Only layout-loading attempts write the launch log. Menu and capture also notify
    // on failure; the other commands report solely to stdout/stderr.
    let command_result = match cli.command {
        None => help().map_err(|error| format!("help: stdout: {error}")),
        Some(CliCommand::List) => list(cli.layouts_dir).map_err(|error| format!("list: {error}")),
        Some(CliCommand::Check { name }) => {
            check(&name, cli.layouts_dir).map_err(|error| format!("check: {error}"))
        }
        Some(CliCommand::Profile {
            command: ProfileCommand::Check { name },
        }) => check_profile(&name, cli.profiles_dir, cli.layouts_dir)
            .map_err(|error| format!("profile check: {error}")),
        Some(CliCommand::Profile {
            command: ProfileCommand::Load { name },
        }) => load_profile(&name, cli.profiles_dir, cli.layouts_dir),
        Some(CliCommand::Load { name, cwd }) => load(&name, cli.layouts_dir, cwd),
        Some(CliCommand::Capture { no_infer_commands }) => capture(!no_infer_commands),
        Some(CliCommand::Menu { no_infer_commands }) => {
            menu(cli.layouts_dir, cli.profiles_dir, !no_infer_commands)
        }
    };

    match command_result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "sway-stage: {error}");
            ExitCode::FAILURE
        }
    }
}

fn help() -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    Cli::command().write_help(&mut stdout)?;
    writeln!(stdout)
}

fn list(layouts_dir: Option<PathBuf>) -> Result<(), String> {
    let discovery = discovery::LayoutDirectory::resolve(layouts_dir)?.discover()?;

    for warning in discovery.warnings() {
        let _ = writeln!(io::stderr().lock(), "sway-stage: warning: {warning}");
    }

    let mut stdout = io::stdout().lock();
    for name in discovery.layouts.keys() {
        writeln!(stdout, "{name}").map_err(|error| format!("stdout: {error}"))?;
    }

    Ok(())
}

fn check(name: &str, layouts_dir: Option<PathBuf>) -> Result<(), String> {
    let path = find_layout(name, layouts_dir)?;
    config::read(&path)?;
    writeln!(io::stdout().lock(), "{name}: valid").map_err(|error| format!("stdout: {error}"))
}

fn check_profile(
    name: &str,
    profiles_dir: Option<PathBuf>,
    layouts_dir: Option<PathBuf>,
) -> Result<(), String> {
    profile::read_named(name, profiles_dir, layouts_dir)?;
    writeln!(io::stdout().lock(), "{name}: valid").map_err(|error| format!("stdout: {error}"))
}

fn load_profile(
    name: &str,
    profiles_dir: Option<PathBuf>,
    layouts_dir: Option<PathBuf>,
) -> Result<(), String> {
    let mut diagnostics = Diagnostics::start("profile load");
    diagnostics.stage(format!("profile {name:?}: validation"));
    diagnostics.record(&format!("profile {name:?}; completed 0 workspaces []"));
    let result = profile::read_named(name, profiles_dir, layouts_dir)
        .and_then(|prepared| profile::load(&prepared, &mut diagnostics));
    result.map_err(|error| diagnostics.report_failure(format!("profile {name:?}: {error}")))
}

fn load(name: &str, layouts_dir: Option<PathBuf>, cwd: Option<PathBuf>) -> Result<(), String> {
    let mut diagnostics = Diagnostics::start("load");
    diagnostics.stage(format!("load {name:?}: validation"));
    load_named_layout(
        name,
        read_named_layout(name, layouts_dir),
        cwd,
        None,
        &mut diagnostics,
    )
    .map_err(|error| diagnostics.report_failure(error))
}

/// Standalone and menu loading share named-layout validation and cwd overrides.
/// Menu calls only after Fuzzel has exited with a valid selection.
fn load_named_layout(
    name: &str,
    layout_result: Result<(PathBuf, config::Config), String>,
    default_cwd_override: Option<PathBuf>,
    expected_workspace_id: Option<i64>,
    diagnostics: &mut Diagnostics,
) -> Result<(), String> {
    let (path, mut config) = layout_result?;
    if let Some(mut directory) = default_cwd_override {
        if directory.is_relative()
            && !directory.as_os_str().as_encoded_bytes().starts_with(b"~/")
            && !directory.as_os_str().is_empty()
        {
            directory = std::env::current_dir()
                .map_err(|error| format!("--cwd: read current directory: {error}"))?
                .join(directory);
        }
        config::resolve_directory(&mut directory).map_err(|error| format!("--cwd: {error}"))?;

        config.cwd = Some(directory);
    }

    diagnostics.record(&format!(
        "validated layout {name:?} from {}",
        path.display()
    ));
    launch::load(&config, expected_workspace_id, diagnostics)
        .map_err(|error| format!("layout {name:?}: {error}"))
}

/// Read once so menu selection and loading use the same validated configuration.
fn read_named_layout(
    name: &str,
    layouts_dir: Option<PathBuf>,
) -> Result<(PathBuf, config::Config), String> {
    let path = find_layout(name, layouts_dir)?;
    let config = config::read(&path)?;
    Ok((path, config))
}

/// Resolves a layout name to its discovered file. Parses no layout contents.
fn find_layout(name: &str, layouts_dir: Option<PathBuf>) -> Result<PathBuf, String> {
    if !discovery::valid_name(name) {
        return Err(format!(
            "invalid layout name {name:?}; use [A-Za-z0-9][A-Za-z0-9_-]*"
        ));
    }

    let directory = discovery::LayoutDirectory::resolve(layouts_dir)?;
    let mut discovery = directory.discover()?;
    if let Some(error) = discovery.ambiguities.get(name) {
        return Err(error.clone());
    }

    discovery.layouts.remove(name).ok_or_else(|| {
        format!(
            "layout {name:?} not found in {}; use list to see available layouts",
            directory.path.display()
        )
    })
}

/// Reports failure with a notification, like load, but never touches `last-run.log`, which
/// records layout launches only. Capture does not use layout discovery.
fn capture(infer_commands: bool) -> Result<(), String> {
    print_draft(infer_commands).map_err(|error| {
        // notify_failure reports its own failure to stderr; the capture error stays primary.
        let _ = diagnostics::notify_failure("capture", &error);
        format!("capture: {error}")
    })
}

/// Writes the draft only once it is complete. Flushing surfaces a write error before exit.
fn print_draft(infer_commands: bool) -> Result<(), String> {
    let draft = capture::capture(infer_commands)?;

    let mut stdout = io::stdout().lock();
    stdout
        .write_all(draft.yaml.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("stdout: {error}"))?;

    let _ = writeln!(
        io::stderr().lock(),
        "sway-stage: capture completed. {}",
        draft.commands.summary()
    );

    Ok(())
}

fn menu(
    layouts_dir: Option<PathBuf>,
    profiles_dir: Option<PathBuf>,
    infer_commands: bool,
) -> Result<(), String> {
    let snapshot = capture::snapshot();
    let Some(action) =
        menu::select_action(snapshot.as_ref(), layouts_dir.clone(), profiles_dir.clone())
            .map_err(report_menu_failure)?
    else {
        return Ok(());
    };

    let name = match action {
        menu::MenuAction::Profile(name) => {
            // Invalid selections are menu errors and preserve the previous launch log.
            // load_profile then owns the entire attempt, including its fresh IPC preflight.
            profile::read_named(&name, profiles_dir.clone(), layouts_dir.clone())
                .map_err(|error| report_menu_failure(menu::invalid_profile(&name, Some(error))))?;
            return load_profile(&name, profiles_dir, layouts_dir);
        }
        menu::MenuAction::Capture => {
            let snapshot = snapshot
                .as_ref()
                .expect("capture requires a valid saved snapshot");
            return menu::capture_draft(snapshot, layouts_dir, infer_commands)
                .map_err(report_menu_failure);
        }
        menu::MenuAction::Layout(name) => name,
    };

    let snapshot = snapshot.expect("layout selection requires an empty saved workspace");
    let output_name = snapshot.output_name();
    let (path, config) = read_named_layout(&name, layouts_dir)
        .map_err(|error| report_menu_failure(menu::invalid_layout(&name, Some(error))))?;

    let selected_directory = if config.cwd_select {
        let root = config
            .cwd
            .as_deref()
            .expect("validated cwd_select requires cwd");
        let display_path = config
            .cwd_display_path
            .as_deref()
            .expect("validated cwd_select retains the configured cwd");
        let Some(selection) =
            menu::select_directory(root, display_path, output_name).map_err(report_menu_failure)?
        else {
            return Ok(());
        };
        match selection {
            menu::DirectorySelection::Base => None,
            menu::DirectorySelection::Child(directory) => Some(directory),
        }
    } else {
        None
    };

    let mut diagnostics = Diagnostics::start("menu");
    diagnostics.stage(format!("menu load {name:?}: validation"));
    load_named_layout(
        &name,
        Ok((path, config)),
        selected_directory,
        Some(snapshot.workspace_id()),
        &mut diagnostics,
    )
    .map_err(|error| diagnostics.report_failure(error))
}

fn report_menu_failure(error: menu::Error) -> String {
    let _ = diagnostics::notify_menu_failure(&error);
    format!("menu: {}", error.detail)
}
