use super::*;
use std::io::{Read, Write};
use std::time::Duration;

use serde_json::{Value, json};

pub(crate) const WORKSPACE: u32 = 1 << 31;
pub(crate) const WINDOW: u32 = (1 << 31) | 3;
pub(crate) const TICK: u32 = (1 << 31) | 7;

pub(crate) fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}

pub(crate) fn pair() -> (UnixStream, UnixStream) {
    let (client, server) = UnixStream::pair().unwrap();
    // Fixture bugs must fail rather than hang waiting for a request that never comes.
    server
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    server
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    (client, server)
}

pub(crate) fn frame(kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = b"i3-ipc".to_vec();
    bytes.extend((payload.len() as u32).to_ne_bytes());
    bytes.extend(kind.to_ne_bytes());
    bytes.extend(payload);
    bytes
}

pub(crate) fn send(socket: &mut UnixStream, kind: u32, value: Value) {
    socket
        .write_all(&frame(kind, &serde_json::to_vec(&value).unwrap()))
        .unwrap();
}

pub(crate) fn request(socket: &mut UnixStream, expected: u32) -> Vec<u8> {
    let mut header = [0; 14];
    socket.read_exact(&mut header).unwrap();
    assert_eq!(&header[..6], b"i3-ipc");
    assert_eq!(
        u32::from_ne_bytes(header[10..].try_into().unwrap()),
        expected
    );
    let len = u32::from_ne_bytes(header[6..10].try_into().unwrap());
    let mut payload = vec![0; len as usize];
    socket.read_exact(&mut payload).unwrap();
    payload
}

pub(crate) fn subscribed() -> (Observer, UnixStream) {
    let (client, mut server) = pair();
    let worker = thread::spawn(move || {
        let subscription: Value = serde_json::from_slice(&request(&mut server, 2)).unwrap();
        assert_eq!(subscription, json!(["workspace", "window", "tick"]));
        send(&mut server, 2, json!({"success": true}));
        send(&mut server, TICK, json!({"first": true, "payload": ""}));
        server
    });
    let observer = Observer::subscribe(client, deadline()).unwrap();
    (observer, worker.join().unwrap())
}

pub(crate) fn node(id: i64, kind: &str, focused: bool) -> Value {
    let rect = json!({"x": 0, "y": 0, "width": 100, "height": 100});
    json!({
        "id": id, "type": kind, "name": null,
        "border": "none", "current_border_width": 0,
        "layout": "splith", "orientation": "horizontal",
        "rect": rect, "window_rect": rect, "deco_rect": rect, "geometry": rect,
        "urgent": false, "focused": focused, "focus": [],
        "nodes": [], "floating_nodes": [], "sticky": false,
        // Sway reports 1 on every workspace, independently of window fullscreen state.
        "fullscreen_mode": if kind == "workspace" { 1 } else { 0 }
    })
}

pub(crate) fn tree() -> Value {
    let mut root = node(1, "root", false);
    let mut output = node(2, "output", false);
    output["nodes"] = json!([node(10, "workspace", true)]);
    root["nodes"] = json!([output]);
    root
}

pub(crate) fn workspaces() -> Value {
    json!([{
        "id": 10, "num": 1, "name": "1", "visible": true, "focused": true,
        "urgent": false, "rect": {"x": 0, "y": 0, "width": 100, "height": 100},
        "output": "fixture"
    }])
}

pub(crate) fn multi_output(mut tree: Value) -> Value {
    tree["nodes"][0]["name"] = json!("fixture");
    let mut other_window = node(90, "con", false);
    other_window["app_id"] = json!("unrelated");
    other_window["shell"] = json!("xdg_shell");
    let mut other_workspace = node(11, "workspace", false);
    other_workspace["nodes"] = json!([other_window]);
    let mut other_output = node(3, "output", false);
    other_output["name"] = json!("HDMI-A-1");
    other_output["nodes"] = json!([other_workspace]);
    tree["nodes"].as_array_mut().unwrap().push(other_output);
    tree
}

pub(crate) fn multi_output_workspaces() -> Value {
    let mut workspaces = workspaces();
    let mut other_workspace = workspaces[0].clone();
    other_workspace["id"] = json!(11);
    other_workspace["num"] = json!(2);
    other_workspace["name"] = json!("2");
    other_workspace["focused"] = json!(false);
    other_workspace["output"] = json!("HDMI-A-1");
    workspaces.as_array_mut().unwrap().push(other_workspace);
    workspaces
}

pub(crate) fn move_target_to_second_output(mut tree: Value) -> Value {
    let target = tree["nodes"][0]["nodes"].as_array_mut().unwrap().remove(0);
    tree["nodes"][1]["nodes"]
        .as_array_mut()
        .unwrap()
        .push(target);
    tree
}

pub(crate) fn workspace_move(id: i64) -> Value {
    json!({"change": "move", "current": node(id, "workspace", id == 10), "old": null})
}

pub(crate) fn focus(id: i64, old: i64) -> Value {
    json!({"change": "focus", "current": node(id, "workspace", true),
        "old": node(old, "workspace", false)})
}

pub(crate) fn window(change: &str, id: i64) -> Value {
    json!({"change": change, "container": node(id, "con", true)})
}

pub(crate) fn snapshot(
    observer: &mut Observer,
    events: &mut UnixStream,
    workspaces: Value,
    tree: Value,
) -> Result<Snapshot, String> {
    let (client, mut server) = pair();
    thread::scope(|scope| {
        let worker = scope.spawn(move || {
            request(&mut server, 1);
            send(&mut server, 1, workspaces);
            request(&mut server, 4);
            send(&mut server, 4, tree);
            let token = String::from_utf8(request(&mut server, 10)).unwrap();
            // The event precedes the command reply, as in Sway's IPC_SEND_TICK handler.
            send(events, TICK, json!({"first": false, "payload": token}));
            send(&mut server, 10, json!({"success": true}));
        });
        let result = observer.snapshot(client, deadline());
        worker.join().unwrap();
        result
    })
}

pub(crate) fn initialized() -> (Observer, UnixStream) {
    let (mut observer, mut events) = subscribed();
    let snapshot = snapshot(&mut observer, &mut events, workspaces(), tree()).unwrap();
    assert_eq!(snapshot.target, 10);
    assert_eq!(snapshot.tree.iter().count(), 3);
    assert!(snapshot.events.is_empty());
    (observer, events)
}
