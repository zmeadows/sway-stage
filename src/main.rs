mod config;
mod discovery;

use clap::{CommandFactory, Parser, Subcommand};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(version, about = "Validate and discover declarative Sway layouts")]
#[command(after_help = "Layout construction, capture, and the picker are not implemented yet.")]
struct Cli {
    /// Read layouts from PATH instead of the XDG configuration directory
    #[arg(long, global = true, value_name = "PATH")]
    layouts_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
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
    };
    let directory = discovery::Directory::resolve(cli.layouts_dir)
        .map_err(|error| format!("{operation}: {error}"))?;
    let layouts = directory
        .discover()
        .map_err(|error| format!("{operation}: {error}"))?;

    let mut stdout = io::stdout().lock();
    match command {
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
