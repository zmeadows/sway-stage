use std::env;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use swayipc::{Connection, Node, NodeType};

fn main() -> ExitCode {
    match inspect() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("inspect_sway: {error}");
            ExitCode::FAILURE
        }
    }
}

fn inspect() -> Result<(), String> {
    let socket = env::var_os("SWAYSOCK")
        .filter(|path| !path.is_empty())
        .ok_or("connect: SWAYSOCK is unset or empty; run this example from a Sway session")?;

    // Connection::new() can spawn socket-discovery processes and prefers I3SOCK.
    // Use Sway's explicit socket so inspection never launches another process.
    let stream = UnixStream::connect(socket)
        .map_err(|error| format!("connect to SWAYSOCK: {error}; check that Sway is running"))?;
    let mut connection = Connection::from(stream);

    let workspaces = connection
        .get_workspaces()
        .map_err(|error| format!("get workspaces: {error}"))?;
    let mut focused = workspaces.iter().filter(|workspace| workspace.focused);
    let focused = match (focused.next(), focused.next()) {
        (Some(workspace), None) => workspace,
        (None, _) => return Err("identify workspace: Sway reported no focused workspace".into()),
        (Some(_), Some(_)) => {
            return Err("identify workspace: Sway reported multiple focused workspaces".into());
        }
    };

    let tree = connection
        .get_tree()
        .map_err(|error| format!("get container tree: {error}"))?;
    let workspace = tree
        .find_as_ref(|node| node.node_type == NodeType::Workspace && node.id == focused.id)
        .ok_or_else(|| {
            format!(
                "locate workspace: focused workspace id={} is absent from the tree; \
                 it may have changed between reads; rerun the example",
                focused.id
            )
        })?;

    // A workspace's focused flag is usually false when one of its windows has focus.
    if workspace.find_as_ref(|node| node.focused).is_none() {
        return Err("inspect workspace: focus changed between reads; rerun the example".into());
    }

    // Empty containers still count as occupancy, even when they contain no windows.
    let occupancy = if workspace.nodes.is_empty() && workspace.floating_nodes.is_empty() {
        "empty"
    } else {
        "occupied"
    };
    let name = workspace.name.as_deref().unwrap_or(&focused.name);
    println!(
        "Workspace {name:?} id={} layout={:?} orientation={:?}",
        workspace.id, workspace.layout, workspace.orientation
    );
    println!(
        "Occupancy: {occupancy} ({} tiled, {} floating direct children)",
        workspace.nodes.len(),
        workspace.floating_nodes.len()
    );
    println!("id = Sway container ID; parent_area = Sway percent; '-' = unavailable");
    print_children(workspace, 1);
    Ok(())
}

fn print_children(parent: &Node, depth: usize) {
    for (branch, children) in [
        ("tiled", &parent.nodes),
        ("floating", &parent.floating_nodes),
    ] {
        for node in children {
            print!(
                "{}[{branch}] {:?} id={} layout={:?} orientation={:?}",
                "  ".repeat(depth),
                node.node_type,
                node.id,
                node.layout,
                node.orientation
            );
            match node.percent {
                Some(percent) => print!(" parent_area={:.1}%", percent * 100.0),
                None => print!(" parent_area=-"),
            }
            if let Some(app_id) = &node.app_id {
                print!(" app_id={app_id:?}");
            }
            if let Some(window) = node.window {
                print!(" x11_id={window}");
            }
            if let Some(class) = node
                .window_properties
                .as_ref()
                .and_then(|properties| properties.class.as_ref())
            {
                print!(" class={class:?}");
            }
            if node.focused {
                print!(" focused");
            }
            println!();

            print_children(node, depth + 1);
        }
    }
}
