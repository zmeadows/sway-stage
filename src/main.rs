mod config;
mod diagnostics;
mod discovery;
mod launch;
mod observation;

use clap::{CommandFactory, Parser, Subcommand};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    version,
    about = "Validate Sway layouts and safely launch a single window"
)]
#[command(
    after_help = "Load supports a single root window only. Nested layouts, capture, and the picker are not implemented yet."
)]
struct Cli {
    /// Read layouts from PATH instead of the XDG configuration directory
    #[arg(long, global = true, value_name = "PATH")]
    layouts_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Launch one root window on the focused empty Sway workspace; no layout manipulation
    Load {
        /// Layout name (the YAML filename without its extension)
        name: String,
    },
    /// Print available layout names in lexical order without parsing their contents
    List,
    /// Validate a layout without contacting Sway or launching applications
    Check {
        /// Layout name (the YAML filename without its extension)
        name: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "sway-stage: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    if let Some(Command::Load { name }) = &cli.command {
        let mut diagnostics = diagnostics::Diagnostics::start();
        diagnostics.stage(format!("load {name:?}: validation"));
        let result = load_named(name, cli.layouts_dir, &mut diagnostics);
        return result.map_err(|error| diagnostics.failure(error));
    }
    let Some(command) = cli.command else {
        let mut help = Cli::command();
        let mut stdout = io::stdout().lock();
        help.write_help(&mut stdout)
            .map_err(|error| format!("help: stdout: {error}"))?;
        writeln!(stdout).map_err(|error| format!("help: stdout: {error}"))?;
        return Ok(());
    };

    if let Command::Check { name } = &command
        && !discovery::valid_name(name)
    {
        return Err(format!(
            "check: invalid layout name {name:?}; use [A-Za-z0-9][A-Za-z0-9_-]*"
        ));
    }

    let operation = match &command {
        Command::List => "list",
        Command::Check { .. } => "check",
        Command::Load { .. } => unreachable!("load handled above"),
    };
    let directory = discovery::Directory::resolve(cli.layouts_dir)
        .map_err(|error| format!("{operation}: {error}"))?;
    let layouts = directory
        .discover()
        .map_err(|error| format!("{operation}: {error}"))?;

    let mut stdout = io::stdout().lock();
    match command {
        Command::Load { .. } => unreachable!("load handled above"),
        Command::List => {
            for name in layouts.keys() {
                writeln!(stdout, "{name}").map_err(|error| format!("list: stdout: {error}"))?;
            }
        }
        Command::Check { name } => {
            let path = layouts.get(&name).ok_or_else(|| {
                format!(
                    "check: layout {name:?} not found in {}; use list to see available layouts",
                    directory.path.display()
                )
            })?;
            config::read(path).map_err(|error| format!("check: {error}"))?;
            writeln!(stdout, "{name}: valid").map_err(|error| format!("check: stdout: {error}"))?;
        }
    }
    Ok(())
}

fn load_named(
    name: &str,
    layouts_dir: Option<PathBuf>,
    diagnostics: &mut diagnostics::Diagnostics,
) -> Result<(), String> {
    if !discovery::valid_name(name) {
        return Err(format!(
            "invalid layout name {name:?}; use [A-Za-z0-9][A-Za-z0-9_-]*"
        ));
    }
    let directory = discovery::Directory::resolve(layouts_dir)?;
    let layouts = directory.discover()?;
    let path = layouts.get(name).ok_or_else(|| {
        format!(
            "layout {name:?} not found in {}; use list to see available layouts",
            directory.path.display()
        )
    })?;
    let config = config::read(path)?;
    diagnostics.record(&format!(
        "validated layout {name:?} from {}",
        path.display()
    ));
    launch::load(&config, diagnostics).map_err(|error| format!("layout {name:?}: {error}"))
}
