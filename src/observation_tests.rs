use super::fixtures::*;
use super::*;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::time::Duration;

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
fn target_workspace_move_away_and_back_is_a_sticky_failure() {
    let (mut observer, mut events) = initialized();
    // The final tree retains both the original focus and output; event history still wins.
    send(&mut events, WORKSPACE, workspace_move(10));
    send(&mut events, WORKSPACE, workspace_move(10));
    let error = snapshot(&mut observer, &mut events, workspaces(), tree())
        .err()
        .unwrap();
    assert!(error.contains("moved to another output"), "{error}");
    assert!(observer.next_until(deadline()).is_err());
}

#[test]
fn target_move_during_initial_fence_is_rejected() {
    let (mut observer, mut events) = subscribed();
    send(&mut events, WORKSPACE, workspace_move(10));
    let error = snapshot(&mut observer, &mut events, workspaces(), tree())
        .err()
        .unwrap();
    assert!(error.contains("moved to another output"), "{error}");
}

#[test]
fn changed_output_in_tree_without_move_event_is_a_sticky_failure() {
    let (mut observer, mut events) = subscribed();
    let original = multi_output(tree());
    snapshot(
        &mut observer,
        &mut events,
        multi_output_workspaces(),
        original.clone(),
    )
    .unwrap();
    let migrated = move_target_to_second_output(original);
    let mut workspaces = multi_output_workspaces();
    workspaces[0]["output"] = json!("HDMI-A-1");
    let error = snapshot(&mut observer, &mut events, workspaces, migrated)
        .err()
        .unwrap();
    assert!(
        error.contains("changed output from id=2 to id=3"),
        "{error}"
    );
    assert!(observer.next_until(deadline()).is_err());
}

#[test]
fn unrelated_workspace_move_and_output_reconfiguration_keep_target_valid() {
    let (mut observer, mut events) = subscribed();
    let mut desktop = multi_output(tree());
    snapshot(
        &mut observer,
        &mut events,
        multi_output_workspaces(),
        desktop.clone(),
    )
    .unwrap();

    let unrelated = desktop["nodes"][1]["nodes"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    desktop["nodes"][0]["nodes"]
        .as_array_mut()
        .unwrap()
        .push(unrelated);
    desktop["nodes"][0]["rect"]["width"] = json!(200);
    let mut workspaces = multi_output_workspaces();
    workspaces[1]["output"] = json!("fixture");
    send(&mut events, WORKSPACE, workspace_move(11));
    let snapshot = snapshot(&mut observer, &mut events, workspaces, desktop).unwrap();
    assert_eq!(snapshot.target_workspace_id, 10);
    assert_eq!(snapshot.events.len(), 1);
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
    for _ in 0..=EVENT_QUEUE_CAPACITY {
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

fn view(id: i64, app_id: &str) -> Value {
    let mut view = node(id, "con", false);
    view["app_id"] = json!(app_id);
    view["shell"] = json!("xdg_shell");
    view
}

fn parsed(value: Value) -> Node {
    serde_json::from_value(value).unwrap()
}

#[test]
fn located_windows_keep_workspace_floating_fullscreen_and_scratchpad_ancestry() {
    // Structural groups and wrappers are not windows; an empty container still counts.
    let mut group = node(30, "con", false);
    group["layout"] = json!("splitv");
    group["nodes"] = json!([view(20, "tiled"), view(21, "tiled")]);
    let mut fullscreen = node(31, "con", false);
    fullscreen["fullscreen_mode"] = json!(1);
    fullscreen["nodes"] = json!([view(22, "fullscreen")]);
    let mut wrapper = node(32, "floating_con", false);
    wrapper["nodes"] = json!([view(23, "wrapped")]);
    // Sway reports fullscreen_mode=1 on every workspace; it is not inherited.
    let mut target = node(10, "workspace", true);
    target["nodes"] = json!([group, fullscreen, node(33, "con", false)]);
    target["floating_nodes"] = json!([wrapper]);
    let mut scratch = node(11, "workspace", false);
    scratch["name"] = json!("__i3_scratch");
    scratch["floating_nodes"] = json!([view(24, "hidden")]);
    let mut marked = view(25, "marked");
    marked["scratchpad_state"] = json!("fresh");
    let mut flagged = view(26, "flagged");
    flagged["floating"] = json!("user_on");
    let mut other = node(12, "workspace", false);
    other["nodes"] = json!([marked, flagged]);
    let mut tree = tree();
    tree["nodes"][0]["nodes"] = json!([target, scratch, other]);
    let tree = parsed(tree);

    let located: Vec<_> = locate_windows(&tree)
        .iter()
        .map(|window| {
            (
                window.node.id,
                window.workspace_id,
                window.floating,
                window.fullscreen,
                window.scratchpad,
            )
        })
        .collect();
    #[rustfmt::skip]
    assert_eq!(located, [
        // (id, workspace, floating, fullscreen, scratchpad), tiled before floating
        (20, Some(10), false, false, false),
        (21, Some(10), false, false, false),
        (22, Some(10), false, true, false),
        (33, Some(10), false, false, false),
        (23, Some(10), true, false, false),
        (24, Some(11), true, false, true),
        (25, Some(12), false, false, true),
        (26, Some(12), true, false, false),
    ]);
}

#[test]
fn fullscreen_container_ancestry_still_marks_its_window_fullscreen() {
    for (kind, expected_fullscreen) in [("workspace", false), ("con", true), ("floating_con", true)]
    {
        let mut parent = node(30, kind, false);
        parent["fullscreen_mode"] = json!(1);
        parent["nodes"] = json!([view(20, "expected")]);
        let parent = parsed(parent);
        let windows = locate_windows(&parent);
        let window = windows.iter().find(|window| window.node.id == 20).unwrap();
        assert_eq!(window.fullscreen, expected_fullscreen, "parent kind={kind}");
    }
}

#[test]
fn identifiers_keep_their_kind_and_exact_value_and_reject_empty_values() {
    let mut native = node(20, "con", false);
    native["app_id"] = json!("Foot");
    let mut xwayland = node(21, "con", false);
    xwayland["window_properties"] = json!({"class": "Firefox"});
    let mut empty = node(22, "con", false);
    empty["app_id"] = json!("");
    empty["window_properties"] = json!({"class": ""});
    let [native, xwayland, empty] = [native, xwayland, empty].map(parsed);
    assert_eq!((app_id(&native), class(&native)), (Some("Foot"), None));
    assert_eq!(
        (app_id(&xwayland), class(&xwayland)),
        (None, Some("Firefox"))
    );
    assert_eq!((app_id(&empty), class(&empty)), (None, None));
}

#[test]
fn sibling_dimensions_follow_the_group_axis_and_reject_unusable_geometry() {
    fn group(layout: &str, sizes: &[(i32, i32)]) -> Node {
        let mut group = node(30, "con", false);
        group["layout"] = json!(layout);
        // Neither the group's own extent nor parent-area percent enters the shares.
        group["rect"]["width"] = json!(5000);
        let children: Vec<_> = sizes
            .iter()
            .enumerate()
            .map(|(index, (width, height))| {
                let mut child = node(20 + index as i64, "con", false);
                child["rect"]["width"] = json!(width);
                child["rect"]["height"] = json!(height);
                child["percent"] = json!(0.9);
                child
            })
            .collect();
        group["nodes"] = json!(children);
        parsed(group)
    }

    let widths = sibling_dimensions(&group("splith", &[(300, 10), (700, 20)])).unwrap();
    assert_eq!(widths, [300.0, 700.0]);
    assert_eq!(proportions(&widths), [0.3, 0.7]);
    let heights = sibling_dimensions(&group("splitv", &[(300, 10), (700, 30)])).unwrap();
    assert_eq!(proportions(&heights), [0.25, 0.75]);
    for (layout, sizes, expected) in [
        ("tabbed", &[(300, 10), (700, 20)][..], "no split axis"),
        ("stacked", &[(300, 10), (700, 20)], "no split axis"),
        ("splith", &[(300, 10)], "missing"),
        ("splith", &[(0, 10), (700, 10)], "nonpositive"),
        ("splitv", &[(300, -1), (700, 10)], "nonpositive"),
    ] {
        let error = sibling_dimensions(&group(layout, sizes)).unwrap_err();
        assert!(error.contains(expected), "{layout} {sizes:?}: {error}");
    }
}
