mod capture;
mod config;
mod diagnostics;
mod discovery;
mod launch;
mod observation;

use clap::{CommandFactory, Parser, Subcommand};
use diagnostics::Diagnostics;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    version,
    about = "Validate, construct, and capture nested Sway layouts",
    after_help = "Load applies sibling weights and final focus on the focused empty workspace. Sizing aims within 1 percentage point and warns beyond 5, after at most three passes per split. Capture only reads Sway; fill in its null command and match placeholders before checking or loading a draft. The picker is not implemented yet."
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
    /// Construct a weighted tiled layout and select final focus on the focused empty workspace
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
    /// Print an editable YAML draft of the focused workspace's tiled layout, read-only
    Capture,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Only load writes the launch log; load and capture also notify on failure. The other
    // commands report solely to stdout/stderr.
    let result = match cli.command {
        None => help().map_err(|error| format!("help: stdout: {error}")),
        Some(Command::List) => list(cli.layouts_dir).map_err(|error| format!("list: {error}")),
        Some(Command::Check { name }) => {
            check(&name, cli.layouts_dir).map_err(|error| format!("check: {error}"))
        }
        Some(Command::Load { name }) => load(&name, cli.layouts_dir),
        Some(Command::Capture) => capture(),
    };
    match result {
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
    let layouts = discovery::Directory::resolve(layouts_dir)?.discover()?;
    let mut stdout = io::stdout().lock();
    for name in layouts.keys() {
        writeln!(stdout, "{name}").map_err(|error| format!("stdout: {error}"))?;
    }
    Ok(())
}

fn check(name: &str, layouts_dir: Option<PathBuf>) -> Result<(), String> {
    let path = find_layout(name, layouts_dir)?;
    config::read(&path)?;
    writeln!(io::stdout().lock(), "{name}: valid").map_err(|error| format!("stdout: {error}"))
}

fn load(name: &str, layouts_dir: Option<PathBuf>) -> Result<(), String> {
    let mut diagnostics = Diagnostics::start("load");
    diagnostics.stage(format!("load {name:?}: validation"));
    load_named(name, layouts_dir, &mut diagnostics)
        .map_err(|error| diagnostics.report_failure(error))
}

/// Validates and constructs a named layout under launch diagnostics. This is the single
/// loading path: the planned picker calls it only after a layout has been selected.
fn load_named(
    name: &str,
    layouts_dir: Option<PathBuf>,
    diagnostics: &mut Diagnostics,
) -> Result<(), String> {
    let path = find_layout(name, layouts_dir)?;
    let config = config::read(&path)?;
    diagnostics.record(&format!(
        "validated layout {name:?} from {}",
        path.display()
    ));
    launch::load(&config, diagnostics).map_err(|error| format!("layout {name:?}: {error}"))
}

/// Resolves a layout name to its discovered file. Parses no layout contents.
fn find_layout(name: &str, layouts_dir: Option<PathBuf>) -> Result<PathBuf, String> {
    if !discovery::valid_name(name) {
        return Err(format!(
            "invalid layout name {name:?}; use [A-Za-z0-9][A-Za-z0-9_-]*"
        ));
    }
    let directory = discovery::Directory::resolve(layouts_dir)?;
    let mut layouts = directory.discover()?;
    layouts.remove(name).ok_or_else(|| {
        format!(
            "layout {name:?} not found in {}; use list to see available layouts",
            directory.path.display()
        )
    })
}

/// Reports failure with a notification, like load, but never touches `last-run.log`, which
/// records layout launches only. Capture does not use layout discovery.
fn capture() -> Result<(), String> {
    print_draft().map_err(|error| {
        // notify_failure reports its own failure to stderr; the capture error stays primary.
        let _ = diagnostics::notify_failure("capture", &error);
        format!("capture: {error}")
    })
}

/// Writes the draft only once it is complete. Flushing surfaces a write error before exit.
fn print_draft() -> Result<(), String> {
    let draft = capture::capture()?;
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(draft.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("stdout: {error}"))
}
