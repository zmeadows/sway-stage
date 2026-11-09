use super::*;
use std::io::{Read, Write};
use std::time::Duration;

use serde_json::{Value, json};

const WORKSPACE: u32 = 1 << 31;
const WINDOW: u32 = (1 << 31) | 3;
const TICK: u32 = (1 << 31) | 7;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}

fn pair() -> (UnixStream, UnixStream) {
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

fn frame(kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = b"i3-ipc".to_vec();
    bytes.extend((payload.len() as u32).to_ne_bytes());
    bytes.extend(kind.to_ne_bytes());
    bytes.extend(payload);
    bytes
}

fn send(socket: &mut UnixStream, kind: u32, value: Value) {
    socket
        .write_all(&frame(kind, &serde_json::to_vec(&value).unwrap()))
        .unwrap();
}

fn request(socket: &mut UnixStream, expected: u32) -> Vec<u8> {
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

fn subscribed() -> (Observer, UnixStream) {
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

fn node(id: i64, kind: &str, focused: bool) -> Value {
    let rect = json!({"x": 0, "y": 0, "width": 100, "height": 100});
    json!({
        "id": id, "type": kind, "name": null,
        "border": "none", "current_border_width": 0,
        "layout": "splith", "orientation": "horizontal",
        "rect": rect, "window_rect": rect, "deco_rect": rect, "geometry": rect,
        "urgent": false, "focused": focused, "focus": [],
        "nodes": [], "floating_nodes": [], "sticky": false
    })
}

fn tree() -> Value {
    let mut root = node(1, "root", false);
    let mut output = node(2, "output", false);
    output["nodes"] = json!([node(10, "workspace", true)]);
    root["nodes"] = json!([output]);
    root
}

fn workspaces() -> Value {
    json!([{
        "id": 10, "num": 1, "name": "1", "visible": true, "focused": true,
        "urgent": false, "rect": {"x": 0, "y": 0, "width": 100, "height": 100},
        "output": "fixture"
    }])
}

fn focus(id: i64, old: i64) -> Value {
    json!({"change": "focus", "current": node(id, "workspace", true),
        "old": node(old, "workspace", false)})
}

fn window(change: &str, id: i64) -> Value {
    json!({"change": change, "container": node(id, "con", true)})
}

fn snapshot(
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

fn initialized() -> (Observer, UnixStream) {
    let (mut observer, mut events) = subscribed();
    let snapshot = snapshot(&mut observer, &mut events, workspaces(), tree()).unwrap();
    assert_eq!(snapshot.target, 10);
    assert_eq!(snapshot.tree.iter().count(), 3);
    assert!(snapshot.events.is_empty());
    (observer, events)
}

#[test]
fn subscription_precedes_snapshot_and_queued_events_keep_wire_order() {
    let (mut observer, mut events) = subscribed();
    for (index, change) in [
        "new",
        "move",
        "title",
        "fullscreen_mode",
        "floating",
        "close",
    ]
    .into_iter()
    .enumerate()
    {
        send(&mut events, WINDOW, window(change, index as i64 + 20));
    }
    // A fence from another client must not end this snapshot's history.
    send(
        &mut events,
        TICK,
        json!({"first": false, "payload": "other-client"}),
    );
    let snapshot = snapshot(&mut observer, &mut events, workspaces(), tree()).unwrap();
    let ids: Vec<_> = snapshot
        .events
        .into_iter()
        .map(|event| {
            let Event::Window(window) = event else {
                panic!("expected window")
            };
            window.container.id
        })
        .collect();
    assert_eq!(ids, [20, 21, 22, 23, 24, 25]);
}

#[test]
fn focus_away_and_back_before_next_query_is_a_sticky_failure() {
    let (mut observer, mut events) = initialized();
    send(&mut events, WORKSPACE, focus(11, 10));
    send(&mut events, WORKSPACE, focus(10, 11));
    let error = snapshot(&mut observer, &mut events, workspaces(), tree())
        .err()
        .unwrap();
    assert!(error.contains("focus changed"), "{error}");
    assert!(observer.next_until(deadline()).is_err());
}

#[test]
fn focus_during_initial_snapshot_is_not_discarded() {
    let (mut observer, mut events) = subscribed();
    send(&mut events, WORKSPACE, focus(11, 10));
    send(&mut events, WORKSPACE, focus(10, 11));
    assert!(snapshot(&mut observer, &mut events, workspaces(), tree()).is_err());
}

#[test]
fn idle_deadlines_preserve_later_events_and_partial_frames() {
    for split in [5, 14, 30] {
        let (mut observer, mut events) = initialized();
        let bytes = frame(WINDOW, &serde_json::to_vec(&window("new", 20)).unwrap());
        let start = Instant::now();
        assert!(
            observer
                .next_until(start + Duration::from_millis(15))
                .unwrap()
                .is_none()
        );
        assert!(start.elapsed() >= Duration::from_millis(15));

        events.write_all(&bytes[..split]).unwrap();
        assert!(
            observer
                .next_until(Instant::now() + Duration::from_millis(15))
                .unwrap()
                .is_none()
        );
        events.write_all(&bytes[split..]).unwrap();
        let Event::Window(event) = observer.next_until(deadline()).unwrap().unwrap() else {
            panic!("expected window");
        };
        assert_eq!(event.container.id, 20);
        send(&mut events, WINDOW, window("title", 20));
        assert!(matches!(
            observer.next_until(deadline()).unwrap(),
            Some(Event::Window(_))
        ));
    }
}

#[test]
fn queued_traffic_does_not_extend_an_expired_deadline() {
    let (mut observer, mut events) = initialized();
    send(&mut events, WINDOW, window("new", 20));
    assert!(observer.next_until(Instant::now()).unwrap().is_none());
    assert!(observer.next_until(deadline()).unwrap().is_some());
}

#[test]
fn snapshots_refresh_properties_without_events_and_find_focused_descendants() {
    let (mut observer, mut events) = initialized();
    let mut tree = tree();
    let workspace = &mut tree["nodes"][0]["nodes"][0];
    workspace["focused"] = json!(false);
    workspace["nodes"] = json!([node(20, "con", true)]);
    workspace["floating_nodes"] = json!([node(21, "floating_con", false)]);
    let first = snapshot(&mut observer, &mut events, workspaces(), tree.clone()).unwrap();
    assert!(
        first
            .tree
            .find_as_ref(|node| node.id == 20)
            .unwrap()
            .app_id
            .is_none()
    );
    tree["nodes"][0]["nodes"][0]["nodes"][0]["app_id"] = json!("late-id");
    let second = snapshot(&mut observer, &mut events, workspaces(), tree).unwrap();
    assert_eq!(
        second
            .tree
            .find_as_ref(|node| node.id == 20)
            .unwrap()
            .app_id
            .as_deref(),
        Some("late-id")
    );
    assert_eq!(second.tree.iter().count(), 5);
    assert!(second.events.is_empty());
}

#[test]
fn closed_malformed_unknown_and_truncated_events_are_fatal() {
    for bytes in [
        Vec::new(),
        b"bad-magic!!!!!".to_vec(),
        frame(WINDOW, b"{broken"),
        frame(WINDOW, br#"{"change":"new"}"#),
        frame(
            WINDOW,
            &serde_json::to_vec(&window("unknown-change", 20)).unwrap(),
        ),
        frame((1 << 31) | 99, b"{}"),
        frame(WINDOW, b"{}")[..8].to_vec(),
        frame(WINDOW, b"{}a")[..16].to_vec(),
    ] {
        let (mut observer, mut events) = initialized();
        events.write_all(&bytes).unwrap();
        drop(events);
        assert!(
            observer.next_until(deadline()).is_err(),
            "accepted {bytes:?}"
        );
        assert!(observer.next_until(deadline()).is_err());
    }
}

#[test]
fn failed_malformed_closed_and_stalled_subscriptions_fail() {
    for bytes in [
        frame(2, br#"{"success":false}"#),
        frame(2, b"{}"),
        frame(1, br#"{"success":true}"#),
        Vec::new(),
    ] {
        let (client, mut server) = pair();
        let worker = thread::spawn(move || {
            request(&mut server, 2);
            server.write_all(&bytes).unwrap();
        });
        assert!(Observer::subscribe(client, deadline()).is_err());
        worker.join().unwrap();
    }
    // Keep the peer open with an incomplete acknowledgement. Cancellation must wake
    // both subscription reads and normal event reads without waiting for peer closure.
    let (client, mut server) = pair();
    server
        .write_all(&frame(2, br#"{"success":true}"#)[..17])
        .unwrap();
    let start = Instant::now();
    assert!(Observer::subscribe(client, start + Duration::from_millis(20)).is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn overflow_invalidates_all_history() {
    let (mut observer, mut events) = initialized();
    for _ in 0..=EVENT_CAPACITY {
        send(
            &mut events,
            TICK,
            json!({"first": false, "payload": "queued"}),
        );
    }
    let limit = deadline();
    while !observer.reader.as_ref().unwrap().is_finished() {
        assert!(Instant::now() < limit, "reader failed to detect overflow");
        thread::yield_now();
    }
    let error = observer.next_until(deadline()).unwrap_err();
    assert!(error.contains("overflow"), "{error}");
}

#[test]
fn drop_wakes_idle_and_incomplete_event_reads() {
    for bytes in [Vec::new(), frame(WINDOW, b"{}")[..15].to_vec()] {
        let (observer, mut events) = initialized();
        events.write_all(&bytes).unwrap();
        let start = Instant::now();
        drop(observer);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(events.read(&mut [0]).unwrap(), 0);
    }
}

#[test]
fn inconsistent_snapshots_are_rejected() {
    let mut missing = tree();
    missing["nodes"][0]["nodes"] = json!([]);
    let mut no_focus = tree();
    no_focus["nodes"][0]["nodes"][0]["focused"] = json!(false);
    let mut duplicate = tree();
    duplicate["nodes"][0]["id"] = json!(10);
    let mut other_focus = tree();
    other_focus["nodes"][0]["nodes"] =
        json!([node(10, "workspace", false), node(11, "workspace", true)]);
    let mut double_focus = workspaces();
    let mut other = double_focus[0].clone();
    other["id"] = json!(11);
    double_focus.as_array_mut().unwrap().push(other);
    for (workspaces, tree) in [
        (workspaces(), missing),
        (workspaces(), no_focus),
        (workspaces(), duplicate),
        (workspaces(), other_focus),
        (json!([]), tree()),
        (double_focus, tree()),
    ] {
        let (mut observer, mut events) = subscribed();
        assert!(snapshot(&mut observer, &mut events, workspaces, tree).is_err());
        assert!(observer.next_until(deadline()).is_err());
    }
}

#[test]
fn query_deadline_cancels_stalled_or_fragmented_replies_and_poison_observation() {
    for bytes in [Vec::new(), frame(1, b"[]")[..15].to_vec()] {
        let (mut observer, _events) = initialized();
        let (client, mut server) = pair();
        server.write_all(&bytes).unwrap();
        let start = Instant::now();
        assert!(
            observer
                .snapshot(client, start + Duration::from_millis(20))
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(observer.next_until(deadline()).is_err());
    }
}

#[test]
fn malformed_queries_and_missing_fence_fail() {
    for bytes in [frame(1, b"{}"), frame(4, b"[]"), frame(1, b"[broken")] {
        let (mut observer, _events) = initialized();
        let (client, mut server) = pair();
        server.write_all(&bytes).unwrap();
        assert!(observer.snapshot(client, deadline()).is_err());
    }

    for tick_reply in [json!({"success": true}), json!({"success": false})] {
        let (mut observer, _events) = initialized();
        let (client, mut server) = pair();
        let worker = thread::spawn(move || {
            request(&mut server, 1);
            send(&mut server, 1, workspaces());
            request(&mut server, 4);
            send(&mut server, 4, tree());
            request(&mut server, 10);
            send(&mut server, 10, tick_reply);
        });
        let error = observer
            .snapshot(client, Instant::now() + Duration::from_millis(100))
            .err()
            .unwrap();
        assert!(error.contains("fence"), "{error}");
        worker.join().unwrap();
        assert!(observer.next_until(deadline()).is_err());
    }
}

#[test]
fn unusable_focus_events_reload_and_target_destruction_fail() {
    for event in [
        json!({"change": "focus", "current": null, "old": null}),
        json!({"change": "focus", "current": node(10, "con", true)}),
        focus(11, 10),
        json!({"change": "reload"}),
        json!({"change": "empty", "current": node(10, "workspace", false)}),
    ] {
        let (mut observer, mut events) = initialized();
        send(&mut events, WORKSPACE, event);
        assert!(observer.next_until(deadline()).is_err());
    }
}
