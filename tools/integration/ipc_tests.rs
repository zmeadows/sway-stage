use super::*;

#[test]
fn commands_accept_only_validated_workspace_names_and_exact_ids() {
    assert_eq!(
        switch_command("2").unwrap(),
        "workspace --no-auto-back-and-forth \"2\""
    );
    assert_eq!(
        switch_command("1: Main +_a.b-c").unwrap(),
        "workspace --no-auto-back-and-forth \"1: Main +_a.b-c\""
    );
    assert_eq!(close_command(42).unwrap(), "[con_id=42] kill");
    assert_eq!(
        close_command(i64::MAX).unwrap(),
        format!("[con_id={}] kill", i64::MAX)
    );
    for invalid in [
        "", "-a", "a\"", "a\\b", "a;b", "a,b", "a\nb", "a\tb", "a\0b", "é", "9;kill", "９",
    ] {
        assert!(switch_command(invalid).is_err(), "{invalid:?}");
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

    // Another client's Xwayland window has no app ID; its class is not modeled here.
    let xwayland = br#"{"change": "title", "container": {"id": 21, "app_id": null,
        "window_properties": {"class": "XTerm", "instance": "xterm"}}}"#;
    let Ok(Event::Window(xwayland)) = decode_event(xwayland) else {
        panic!("expected a window event");
    };
    assert_eq!(xwayland.container.id, 21);
    assert_eq!(xwayland.container.app_id, None);

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

#[test]
fn tree_child_focus_history_is_required_and_distinct_from_global_focus() {
    use crate::fixtures::{mixed_native_desktop, node_mut, standard_scenario};
    let scenario = standard_scenario("mixed-capture-reload");
    let tree = mixed_native_desktop(&scenario);
    let observed: TreeNode = serde_json::from_value(tree.clone()).unwrap();
    let tabs = crate::fixtures::find(&observed, 31);

    assert!(!tabs.focused);
    assert_eq!(tabs.focus, [33, 32]);
    for invalid in [serde_json::Value::Null, serde_json::json!(["33"])] {
        let mut changed = tree.clone();
        node_mut(&mut changed, 31)["focus"] = invalid;
        assert!(serde_json::from_value::<TreeNode>(changed).is_err());
    }
    let mut missing = tree;
    node_mut(&mut missing, 31)
        .as_object_mut()
        .unwrap()
        .remove("focus");
    assert!(serde_json::from_value::<TreeNode>(missing).is_err());
}

#[test]
fn outputs_decode_active_and_disabled_entries() {
    let reply = r#"[
        {"id": 4, "type": "output", "name": "HEADLESS-1", "make": "headless",
         "model": "headless", "active": true, "scale": 1.0, "current_workspace": "1",
         "rect": {"x": 0, "y": 0, "width": 2560, "height": 1440},
         "current_mode": {"width": 2560, "height": 1440, "refresh": 60000}},
        {"type": "output", "name": "HEADLESS-2", "active": false,
         "current_workspace": null, "rect": {"x": 0, "y": 0, "width": 0, "height": 0}}
    ]"#;
    let outputs: Vec<Output> = serde_json::from_str(reply).unwrap();

    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[0].name, "HEADLESS-1");
    assert!(outputs[0].active);
    assert_eq!(
        outputs[0].rect,
        Rect {
            x: 0,
            y: 0,
            width: 2560,
            height: 1440
        }
    );
    assert_eq!(outputs[0].current_workspace.as_deref(), Some("1"));
    assert!(!outputs[1].active);
    assert_eq!(outputs[1].current_workspace, None);
}

#[test]
fn empty_workspace_layout_commands_use_native_sway_spelling() {
    for (initial_layout, command, observed_layout) in [
        (
            InitialWorkspaceLayout::SplitH,
            "layout splith",
            Layout::SplitH,
        ),
        (
            InitialWorkspaceLayout::SplitV,
            "layout splitv",
            Layout::SplitV,
        ),
        (
            InitialWorkspaceLayout::Tabbed,
            "layout tabbed",
            Layout::Tabbed,
        ),
        (
            InitialWorkspaceLayout::Stacked,
            "layout stacking",
            Layout::Stacked,
        ),
    ] {
        assert_eq!(initial_layout.command(), command);
        assert_eq!(initial_layout.observed_layout(), observed_layout);
    }
}

#[test]
fn exec_commands_accept_only_plain_absolute_script_paths() {
    assert_eq!(
        exec_command(Path::new(
            "/tmp/sway-stage-headless-1-2/tmp/run_a.b-c/entry.sh"
        ))
        .unwrap(),
        "exec /bin/sh /tmp/sway-stage-headless-1-2/tmp/run_a.b-c/entry.sh"
    );
    for invalid in [
        "",
        "entry.sh",
        "./entry.sh",
        "/tmp/a b/entry.sh",
        "/tmp/a'b",
        "/tmp/a\"b",
        "/tmp/a;exit",
        "/tmp/a,b",
        "/tmp/$HOME",
        "/tmp/a\nb",
        "/tmp/a\\b",
        "/tmp/é",
    ] {
        assert!(exec_command(Path::new(invalid)).is_err(), "{invalid:?}");
    }
}

#[test]
fn assign_rules_name_one_exact_plain_app_id_and_a_safe_workspace() {
    assert_eq!(
        assign_command("sway-stage-it-0a1b-assigned-window1", "4").unwrap(),
        "assign [app_id=\"^sway-stage-it-0a1b-assigned-window1$\"] workspace \"4\""
    );
    for invalid_app_id in [
        "", "Upper", "a.b", "a b", "a\"b", "a]b", "a$", "a;exit", "a,b", "é",
    ] {
        assert!(
            assign_command(invalid_app_id, "4").is_err(),
            "{invalid_app_id:?}"
        );
    }
    for invalid_workspace in ["", " 4", "4\"", "4;exit"] {
        assert!(
            assign_command("plain", invalid_workspace).is_err(),
            "{invalid_workspace:?}"
        );
    }
}
