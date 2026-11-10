use super::*;

#[test]
fn commands_accept_only_validated_workspace_names_numbers_and_exact_ids() {
    assert_eq!(
        disposable_command("9").unwrap(),
        "workspace --no-auto-back-and-forth 9"
    );
    assert_eq!(
        restoration_command("1: Main +_a.b-c").unwrap(),
        "workspace --no-auto-back-and-forth \"1: Main +_a.b-c\""
    );
    assert_eq!(close_command(42).unwrap(), "[con_id=42] kill");
    assert_eq!(
        close_command(i64::MAX).unwrap(),
        format!("[con_id={}] kill", i64::MAX)
    );
    for invalid in ["0", "09", "1000000000", "9;kill", "9\n", "９"] {
        assert!(disposable_command(invalid).is_err());
    }
    for invalid in [
        "", "-a", "a\"", "a\\b", "a;b", "a,b", "a\nb", "a\tb", "a\0b", "é",
    ] {
        assert!(restoration_command(invalid).is_err(), "{invalid:?}");
    }
    assert!(close_command(0).is_err());
    assert!(close_command(-1).is_err());
}

#[test]
fn commands_require_exactly_one_explicit_successful_reply() {
    decode_command_reply(r#"[{"success":true}]"#).unwrap();
    decode_command_reply(r#"[{"success":true,"ignored":"field"}]"#).unwrap();
    for invalid in [
        "",
        "not JSON",
        "null",
        "[]",
        "[true]",
        "[{}]",
        r#"{"success":true}"#,
        r#"[{"success":false}]"#,
        r#"[{"success":"true"}]"#,
        r#"[{"success":1}]"#,
        r#"[{"success":true},{"success":true}]"#,
        r#"[{"success":true},{}]"#,
        r#"[{"success":true}] trailing"#,
    ] {
        assert!(decode_command_reply(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn buffered_events_cannot_extend_an_expired_observation_deadline() {
    // An in-process socket pair, with no helper, process launch, or compositor connection.
    let (output, _peer) = UnixStream::pair().unwrap();
    let line = b"{\"first\":false,\"payload\":\"already queued\"}\n".to_vec();
    let mut subscription = EventSubscription {
        helper: None,
        output,
        pending: line.clone(),
    };
    let error = subscription.next_event(Instant::now()).unwrap_err();
    assert_eq!(error, "event observation deadline expired");
    assert_eq!(subscription.pending, line);
    assert!(matches!(
        subscription.next_event(Instant::now() + IPC_TIMEOUT),
        Ok(Event::Tick(_))
    ));
}

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
