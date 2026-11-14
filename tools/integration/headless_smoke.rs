//! Temporary headless smoke test: the real `sway-stage` builds a two-Foot layout in a
//! private headless Sway compositor, verified through that compositor's own IPC. The
//! integration scenarios will replace it.
//!
//! User-run only, through `tools/integration/run.sh`. Agents must never execute it.

#![allow(
    dead_code,
    reason = "shares harness modules whose live-harness code this test does not use"
)]

mod headless;
mod ipc;
mod process;
// Needed only because `process` names the scenario window type in its probe checks.
mod scenarios;

use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use headless::{HeadlessSession, OUTPUT_NAME, SessionDirectory, VIRTUAL_OUTPUT_RECT};
use ipc::{IPC_TIMEOUT, Layout, NodeType, Rect, Shell, TreeNode, Workspace};

const SWAY_STAGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/release/sway-stage");
const LAYOUT_NAME: &str = "two-foot";
const LEFT_APP_ID: &str = "headless-smoke-left";
const RIGHT_APP_ID: &str = "headless-smoke-right";
/// Each terminal's `sleep` bounds how long a window can outlive a failed run.
const LAYOUT_YAML: &str = r#"version: 1
layout:
  group: splith
  children:
    - id: left
      command: [foot, --app-id=headless-smoke-left, sleep, "300"]
      match:
        app_id: headless-smoke-left
    - id: right
      command: [foot, --app-id=headless-smoke-right, sleep, "300"]
      match:
        app_id: headless-smoke-right
"#;
/// sway-stage's default 20-second wait for each of the two windows, plus a margin.
const LOAD_TIMEOUT: Duration = Duration::from_secs(2 * 20 + 30);

fn main() -> ExitCode {
    // As in the harness, Ctrl-C only records a request; ordinary control flow stops.
    if let Err(error) = process::install_sigint_handler() {
        eprintln!("FAIL  setup: {error}");
        return ExitCode::FAILURE;
    }
    let sway_stage = match process::find_executable(SWAY_STAGE) {
        Ok(sway_stage) => sway_stage,
        Err(error) => {
            eprintln!("FAIL  setup: {error}");
            return ExitCode::FAILURE;
        }
    };
    let directory = match SessionDirectory::create() {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("FAIL  session directory: {error}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(error) = run_in_headless_session(&directory, &sway_stage) {
        eprintln!("FAIL  {error}");
        eprintln!(
            "Diagnostics preserved in {}: sway.log, layouts/, and \
             home/.local/state/sway-stage/last-run.log",
            directory.path().display()
        );
        return ExitCode::FAILURE;
    }

    if let Err(error) = directory.remove() {
        eprintln!("FAIL  cleanup: {error}");
        return ExitCode::FAILURE;
    }
    println!("Headless Sway stopped; session files removed.");
    ExitCode::SUCCESS
}

/// Starts the compositor, runs the smoke test, and stops the compositor on every outcome.
fn run_in_headless_session(directory: &SessionDirectory, sway_stage: &Path) -> Result<(), String> {
    let session = HeadlessSession::start(directory).map_err(|error| format!("startup: {error}"))?;
    println!(
        "Headless Sway ready: pid {}, output {OUTPUT_NAME} {}, WAYLAND_DISPLAY={}",
        session.compositor_pid(),
        geometry(VIRTUAL_OUTPUT_RECT),
        session.wayland_display().display()
    );
    println!("      session directory {}", directory.path().display());

    let smoke_result = load_two_foot_layout(&session, directory, sway_stage);
    let shutdown_result = session.stop();

    match (smoke_result, shutdown_result) {
        (Ok(summary), Ok(())) => {
            println!("PASS  {summary}");
            Ok(())
        }
        (Err(smoke_error), Ok(())) => Err(format!("smoke test: {smoke_error}")),
        (Ok(_), Err(shutdown_error)) => Err(format!("shutdown: {shutdown_error}")),
        (Err(smoke_error), Err(shutdown_error)) => Err(format!(
            "smoke test: {smoke_error}; shutdown: {shutdown_error}"
        )),
    }
}

/// Builds the layout with the real `sway-stage` and returns a one-line summary.
fn load_two_foot_layout(
    session: &HeadlessSession,
    directory: &SessionDirectory,
    sway_stage: &Path,
) -> Result<String, String> {
    let layouts = directory.create_private_subdirectory("layouts")?;
    fs::write(layouts.join(format!("{LAYOUT_NAME}.yaml")), LAYOUT_YAML)
        .map_err(|error| format!("write smoke layout: {error}"))?;

    let mut check = session.command(sway_stage);
    check
        .arg("--layouts-dir")
        .arg(&layouts)
        .args(["check", LAYOUT_NAME]);
    let checked = process::run(check, "sway-stage check", IPC_TIMEOUT)?;
    if checked != format!("{LAYOUT_NAME}: valid\n") {
        return Err(format!("unexpected sway-stage check output {checked:?}"));
    }

    let target = focused_workspace(session)?;

    let mut load = session.command(sway_stage);
    load.arg("--layouts-dir")
        .arg(&layouts)
        .args(["load", LAYOUT_NAME]);
    let loaded = process::run(load, "sway-stage load", LOAD_TIMEOUT)?;
    if !loaded.is_empty() {
        return Err(format!(
            "sway-stage load printed unexpected stdout {loaded:?}"
        ));
    }

    let focused_after_load = focused_workspace(session)?;
    if focused_after_load.id != target.id {
        return Err(format!(
            "workspace focus moved from {:?} to {:?}",
            target.name, focused_after_load.name
        ));
    }
    let tree = session.sway().get_tree()?;
    verify_two_foot_layout(&tree, &target)
}

fn focused_workspace(session: &HeadlessSession) -> Result<Workspace, String> {
    let mut focused: Vec<Workspace> = session
        .sway()
        .get_workspaces()?
        .into_iter()
        .filter(|workspace| workspace.focused)
        .collect();
    if focused.len() != 1 {
        return Err(format!(
            "expected one focused workspace, found {}",
            focused.len()
        ));
    }
    Ok(focused.remove(0))
}

/// The target workspace on the virtual output holds exactly the two windows side by side,
/// each covering half of the output, with final focus on the left one.
fn verify_two_foot_layout(tree: &TreeNode, target: &Workspace) -> Result<String, String> {
    let output = tree
        .nodes
        .iter()
        .find(|node| {
            node.node_type == NodeType::Output && node.name.as_deref() == Some(OUTPUT_NAME)
        })
        .ok_or_else(|| format!("output {OUTPUT_NAME} is missing from the tree"))?;
    let workspace = output
        .nodes
        .iter()
        .find(|node| node.id == target.id)
        .ok_or_else(|| format!("workspace {:?} is not on {OUTPUT_NAME}", target.name))?;

    if workspace.layout != Layout::SplitH {
        return Err(format!(
            "workspace layout is {:?}, expected SplitH",
            workspace.layout
        ));
    }
    if !workspace.floating_nodes.is_empty() {
        return Err(format!(
            "workspace has {} floating windows",
            workspace.floating_nodes.len()
        ));
    }
    let [left, right] = workspace.nodes.as_slice() else {
        return Err(format!(
            "expected two windows directly on the workspace, found {} children",
            workspace.nodes.len()
        ));
    };

    // Without gaps or borders, each window covers exactly half of the output.
    let half_width = VIRTUAL_OUTPUT_RECT.width / 2;
    let left_rect = Rect {
        width: half_width,
        ..VIRTUAL_OUTPUT_RECT
    };
    let right_rect = Rect {
        x: VIRTUAL_OUTPUT_RECT.x + half_width,
        width: half_width,
        ..VIRTUAL_OUTPUT_RECT
    };
    verify_window(left, LEFT_APP_ID, left_rect)?;
    verify_window(right, RIGHT_APP_ID, right_rect)?;
    if !left.focused || right.focused {
        return Err("expected final focus on the left window only".into());
    }

    Ok(format!(
        "headless two-Foot smoke test: workspace {:?} splith; {LEFT_APP_ID} {}, \
         {RIGHT_APP_ID} {}; left window focused",
        target.name,
        geometry(left_rect),
        geometry(right_rect)
    ))
}

/// A native Wayland window with the expected app ID and exact geometry.
fn verify_window(window: &TreeNode, app_id: &str, expected_rect: Rect) -> Result<(), String> {
    let is_native_window = window.node_type == NodeType::Con
        && window.nodes.is_empty()
        && window.shell == Some(Shell::XdgShell);
    if !is_native_window {
        return Err(format!(
            "container {} is not a native Wayland window",
            window.id
        ));
    }
    if window.app_id.as_deref() != Some(app_id) {
        return Err(format!(
            "container {} has app ID {:?}, expected {app_id:?}",
            window.id, window.app_id
        ));
    }
    if window.rect != expected_rect {
        return Err(format!(
            "{app_id} is {}, expected {}",
            geometry(window.rect),
            geometry(expected_rect)
        ));
    }
    Ok(())
}

fn geometry(rect: Rect) -> String {
    format!("{}x{} at {},{}", rect.width, rect.height, rect.x, rect.y)
}
