use super::*;

#[test]
fn monitor_lines_decode_by_their_distinct_keys() {
    let tick = decode_event(br#"{"first": false, "payload": "sway-stage-integration:00"}"#);
    assert!(matches!(
        tick,
        Ok(Event::Tick(TickEvent { first: false, ref payload })) if payload == "sway-stage-integration:00"
    ));

    let window = br#"{"change": "new", "container": {"id": 20, "type": "con", "app_id": "foot",
        "pid": 1, "shell": "xdg_shell", "window_properties": null, "nodes": []}}"#;
    let Ok(Event::Window(window)) = decode_event(window) else {
        panic!("expected a window event");
    };
    assert_eq!(window.change, WindowChange::New);
    assert_eq!(window.container.id, 20);
    assert_eq!(window.container.app_id.as_deref(), Some("foot"));

    let xwayland = br#"{"change": "title", "container": {"id": 21, "app_id": null,
        "window_properties": {"class": "XTerm", "instance": "xterm"}}}"#;
    let Ok(Event::Window(xwayland)) = decode_event(xwayland) else {
        panic!("expected a window event");
    };
    let class = xwayland
        .container
        .window_properties
        .and_then(|properties| properties.class);
    assert_eq!(class.as_deref(), Some("XTerm"));

    let workspace = br#"{"change": "focus", "current": {"id": 7, "name": "9", "type": "workspace"},
        "old": {"id": 5, "name": "1", "type": "workspace"}}"#;
    let Ok(Event::Workspace(workspace)) = decode_event(workspace) else {
        panic!("expected a workspace event");
    };
    assert_eq!(workspace.change, WorkspaceChange::Focus);
    assert_eq!(workspace.current.map(|current| current.id), Some(7));
    assert_eq!(workspace.old.and_then(|old| old.name).as_deref(), Some("1"));

    // Sway reports `old: null` when no workspace had focus.
    let initial = br#"{"change": "init", "current": {"id": 8, "name": "10"}, "old": null}"#;
    assert!(matches!(decode_event(initial), Ok(Event::Workspace(_))));
}

#[test]
fn malformed_or_unknown_events_are_errors() {
    for line in [
        &b"not json"[..],
        b"[]",
        b"{}",
        br#"{"change": "teleport", "container": {"id": 20}}"#,
        br#"{"change": "new", "container": {"app_id": "missing-id"}}"#,
        br#"{"change": "focus", "current": {"id": "seven"}}"#,
        br#"{"first": "no", "payload": "x"}"#,
    ] {
        assert!(
            decode_event(line).is_err(),
            "{}",
            String::from_utf8_lossy(line)
        );
    }
}
