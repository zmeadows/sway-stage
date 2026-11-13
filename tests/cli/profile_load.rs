use super::{CliFixture, read_ipc_message, sway_node, write_ipc_message};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Output;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const WORKSPACE_EVENT: u32 = 1 << 31;
const WINDOW_EVENT: u32 = (1 << 31) | 3;
const TICK_EVENT: u32 = (1 << 31) | 7;

// A finite transcript, including exact commands, is the safety assertion: no rollback,
// retry, extra selection, or later layout can be served after an expected failure.
enum Step {
    Snapshot {
        tree: Value,
        workspace_reply: Option<Value>,
        events: Vec<(u32, Value)>,
    },
    Command {
        command: String,
        replies: Option<Value>,
        events: Vec<(u32, Value)>,
    },
}

fn snapshot(tree: &Value) -> Step {
    Step::Snapshot {
        tree: tree.clone(),
        workspace_reply: None,
        events: vec![],
    }
}

fn command(command: String) -> Step {
    Step::Command {
        command,
        replies: Some(json!([{"success": true}])),
        events: vec![],
    }
}

fn workspaces(tree: &Value) -> Value {
    let mut workspaces = vec![];
    for output in tree["nodes"].as_array().unwrap() {
        for workspace in output["nodes"].as_array().unwrap() {
            let focused = workspace["focused"] == true
                || workspace["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|node| node["focused"] == true);
            workspaces.push(json!({
                "id": workspace["id"], "num": workspace["num"], "name": workspace["name"],
                "focused": focused, "visible": focused, "urgent": false,
                "rect": workspace["rect"], "output": output["name"]
            }));
        }
    }
    json!(workspaces)
}

fn workspace(number: i64, focused: bool) -> Value {
    let mut workspace = sway_node(100 + number, "workspace", 100, vec![]);
    workspace["num"] = json!(number);
    workspace["name"] = json!(format!("{number}: descriptive"));
    workspace["focused"] = json!(focused);
    workspace
}

fn window(id: i64, focused: bool) -> Value {
    let mut window = sway_node(id, "con", 100, vec![]);
    window["app_id"] = json!("profile-test");
    window["shell"] = json!("xdg_shell");
    window["focused"] = json!(focused);
    window
}

fn tree() -> Value {
    let mut original = workspace(90, false);
    original["nodes"] = json!([window(900, true)]);
    let mut output = sway_node(2, "output", 100, vec![original, workspace(3, false)]);
    output["name"] = json!("fixture-output");
    sway_node(1, "root", 100, vec![output])
}

fn focus_workspace(tree: &Value, number: i64) -> Value {
    let mut tree = tree.clone();
    for workspace in tree["nodes"][0]["nodes"].as_array_mut().unwrap() {
        workspace["focused"] = json!(workspace["num"] == number);
        for window in workspace["nodes"].as_array_mut().unwrap() {
            window["focused"] = json!(false);
        }
    }
    tree
}

fn focus_event(number: i64, old: i64) -> (u32, Value) {
    (
        WORKSPACE_EVENT,
        json!({"change": "focus", "current": workspace(number, true), "old": workspace(old, false)}),
    )
}

fn selection_steps(
    before: &Value,
    after: &Value,
    number: i64,
    old: i64,
    create: bool,
) -> Vec<Step> {
    let mut select = command(format!(
        "workspace --no-auto-back-and-forth number {number}"
    ));
    if let Step::Command { events, .. } = &mut select {
        if create {
            events.push((
                WORKSPACE_EVENT,
                json!({"change": "init", "current": workspace(number, false), "old": null}),
            ));
        }
        events.push(focus_event(number, old));
    }
    vec![snapshot(before), select, snapshot(after)]
}

fn construction_steps(empty: &Value, number: i64, window_id: i64) -> (Vec<Step>, Value) {
    let mut mapped = empty.clone();
    let target = mapped["nodes"][0]["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|workspace| workspace["num"] == number)
        .unwrap();
    target["focused"] = json!(false);
    target["nodes"] = json!([window(window_id, true)]);
    target["focus"] = json!([window_id]);
    let mut map = snapshot(&mapped);
    if let Step::Snapshot { events, .. } = &mut map {
        events.push((
            WINDOW_EVENT,
            json!({"change": "new", "container": window(window_id, false)}),
        ));
        events.push((
            WINDOW_EVENT,
            json!({"change": "focus", "container": window(window_id, true)}),
        ));
    }
    (
        vec![
            snapshot(empty),
            snapshot(empty),
            map,
            snapshot(&mapped),
            snapshot(&mapped),
            command(format!("[con_id={window_id}] focus")),
            snapshot(&mapped),
            snapshot(&mapped), // Coordinator checks history after the loader returns.
        ],
        mapped,
    )
}

fn fixture() -> CliFixture {
    let fixture = CliFixture::new();
    fixture.file("work.yaml", "version: 1\nworkspaces: [{workspace: 3, layout: first}, {workspace: 1, layout: second}, {workspace: 2, layout: third}]\n");
    for name in ["first", "second", "third"] {
        fixture.file(&format!("{name}.yaml"), &format!(
            "version: 1\nwindow_timeout_ms: 1000\ncwd: '{}'\nlayout: {{id: t, command: ['./app', '{name}'], match: {{app_id: profile-test}}}}\n",
            fixture.path.display()
        ));
    }
    fixture.file("app", "#!/bin/sh\nprintf '%s\\n' \"$1\" >> launches\n");
    fixture.file(
        "notify-send",
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> notifications\n",
    );
    for executable in ["app", "notify-send"] {
        fs::set_permissions(
            fixture.path.join(executable),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    fixture
}

fn run(fixture: &CliFixture, steps: Vec<Step>, remove_layouts: bool) -> Output {
    run_transcript(fixture, steps, remove_layouts, None)
}

fn run_transcript(
    fixture: &CliFixture,
    steps: Vec<Step>,
    remove_layouts: bool,
    menu_snapshot: Option<(Value, &str)>,
) -> Output {
    let (menu_tree, menu_selection) = match menu_snapshot {
        Some((tree, selection)) => (Some(tree), Some(selection)),
        None => (None, None),
    };
    let socket_path = fixture.path.join("ipc.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    let (done, finished) = mpsc::channel();
    let layouts_path = fixture.path.clone();
    let server = thread::spawn(move || {
        if let Some(tree) = menu_tree {
            let (mut query, _) = listener.accept().unwrap();
            query
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            assert_eq!(read_ipc_message(&mut query).0, 4);
            fs::write(layouts_path.join("snapshot-ready"), "").unwrap();
            write_ipc_message(&mut query, 4, &tree);
            assert_eq!(query.read(&mut [0]).unwrap(), 0);
        }
        listener.set_nonblocking(true).unwrap();
        let mut subscriptions: Vec<UnixStream> = vec![];
        for (index, step) in steps.into_iter().enumerate() {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut socket = loop {
                let (mut socket, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "unconsumed transcript step {index}"
                        );
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept: {error}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let (kind, payload) = read_ipc_message(&mut socket);
                if kind == 2 {
                    assert_eq!(
                        serde_json::from_slice::<Value>(&payload).unwrap(),
                        json!(["workspace", "window", "tick"])
                    );
                    write_ipc_message(&mut socket, 2, &json!({"success": true}));
                    subscriptions.push(socket);
                    continue;
                }
                match &step {
                    Step::Snapshot {
                        tree,
                        workspace_reply,
                        ..
                    } => {
                        assert_eq!(kind, 1, "step {index}");
                        write_ipc_message(
                            &mut socket,
                            1,
                            workspace_reply.as_ref().unwrap_or(&workspaces(tree)),
                        );
                    }
                    Step::Command { command, .. } => {
                        assert_eq!(kind, 0, "step {index}");
                        assert_eq!(
                            String::from_utf8(payload).unwrap(),
                            *command,
                            "step {index}"
                        );
                    }
                }
                break socket;
            };
            if index == 0 && remove_layouts {
                // First compositor query proves preparation finished. Construction must
                // use the retained configs even when every source layout disappears.
                for name in ["first", "second", "third"] {
                    fs::remove_file(layouts_path.join(format!("{name}.yaml"))).unwrap();
                }
            }
            let broadcast = |subscriptions: &mut Vec<UnixStream>, kind: u32, value: &Value| {
                let payload = serde_json::to_vec(value).unwrap();
                let mut frame = b"i3-ipc".to_vec();
                frame.extend((payload.len() as u32).to_ne_bytes());
                frame.extend(kind.to_ne_bytes());
                frame.extend(payload);
                subscriptions.retain_mut(|stream| stream.write_all(&frame).is_ok());
            };
            match step {
                Step::Snapshot { tree, events, .. } => {
                    assert_eq!(read_ipc_message(&mut socket).0, 4);
                    write_ipc_message(&mut socket, 4, &tree);
                    let (kind, token) = read_ipc_message(&mut socket);
                    assert_eq!(kind, 10);
                    for (kind, event) in events {
                        broadcast(&mut subscriptions, kind, &event);
                    }
                    broadcast(
                        &mut subscriptions,
                        TICK_EVENT,
                        &json!({"first": false, "payload": String::from_utf8(token).unwrap()}),
                    );
                    write_ipc_message(&mut socket, 10, &json!({"success": true}));
                }
                Step::Command {
                    replies, events, ..
                } => {
                    for (kind, event) in events {
                        broadcast(&mut subscriptions, kind, &event);
                    }
                    if let Some(replies) = replies {
                        write_ipc_message(&mut socket, 0, &replies);
                    } else {
                        // Wait for the client's bounded command cancellation, with no reply.
                        assert_eq!(socket.read(&mut [0]).unwrap(), 0);
                    }
                }
            }
        }
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    });
    let mut process = match menu_selection {
        Some(selection) => super::menu::menu_command(fixture, selection, ""),
        None => fixture.command(),
    };
    process
        .env("SWAYSOCK", socket_path)
        .env("PATH", &fixture.path)
        .args([
            "--profiles-dir",
            if menu_selection.is_some() {
                "profiles"
            } else {
                "."
            },
            "--layouts-dir",
            ".",
        ]);
    if menu_selection.is_some() {
        process.args(["menu", "--no-infer-commands"]);
    } else {
        process.args(["profile", "load", "work"]);
    }
    let output = process.output().unwrap();
    done.send(()).unwrap();
    server.join().unwrap();
    output
}

fn assert_failure(fixture: &CliFixture, output: &Output, expected: &str, completed: usize) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(expected), "{error}");
    assert!(error.contains("profile \"work\""), "{error}");
    assert!(
        error.contains(&format!("completed {completed} workspaces")),
        "{error}"
    );
    let notifications = fs::read_to_string(fixture.path.join("notifications")).unwrap();
    assert_eq!(
        notifications
            .matches("sway-stage profile load failed")
            .count(),
        1,
        "{notifications}"
    );
    assert!(!notifications.contains("menu"));
    let log =
        fs::read_to_string(fixture.path.join(".local/state/sway-stage/last-run.log")).unwrap();
    assert!(log.contains(expected), "{log}");
}

#[test]
fn two_workspace_profile_uses_prepared_configs_yaml_order_and_existing_named_workspace() {
    let fixture = fixture();
    fixture.file(
        "work.yaml",
        "version: 1\nworkspaces: [{workspace: 3, layout: first}, {workspace: 1, layout: second}]\n",
    );
    let initial = tree();
    let selected = focus_workspace(&initial, 3);
    let mut steps = vec![snapshot(&initial)];
    steps.extend(selection_steps(&initial, &selected, 3, 90, false));
    let (construction, first_done) = construction_steps(&selected, 3, 20);
    steps.extend(construction);
    let mut second_selected = first_done.clone();
    second_selected["nodes"][0]["nodes"]
        .as_array_mut()
        .unwrap()
        .push(workspace(1, false));
    second_selected = focus_workspace(&second_selected, 1);
    steps.extend(selection_steps(&first_done, &second_selected, 1, 3, true));
    let (construction, _) = construction_steps(&second_selected, 1, 21);
    steps.extend(construction);
    let output = run(&fixture, steps, true);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(
        fs::read_to_string(fixture.path.join("launches")).unwrap(),
        "first\nsecond\n"
    );
    assert!(!fixture.path.join("notifications").exists());
    let log =
        fs::read_to_string(fixture.path.join(".local/state/sway-stage/last-run.log")).unwrap();
    assert!(
        log.contains("completed 2 workspaces [(3, 103), (1, 101)]"),
        "{log}"
    );
}

#[test]
fn every_layout_is_validated_before_connecting_or_selecting_a_workspace() {
    let fixture = fixture();
    fixture.file(
        "third.yaml",
        "version: 1\nlayout: {id: t, command: [], match: {app_id: t}}\n",
    );
    let socket_path = fixture.path.join("no-connection.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let output = fixture
        .command()
        .env("SWAYSOCK", socket_path)
        .env("PATH", &fixture.path)
        .args([
            "--profiles-dir",
            ".",
            "--layouts-dir",
            ".",
            "profile",
            "load",
            "work",
        ])
        .output()
        .unwrap();
    assert_failure(&fixture, &output, "invalid layout", 0);
    assert!(String::from_utf8_lossy(&output.stderr).contains("workspaces[2].layout"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no applications launched"));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(!fixture.path.join("launches").exists());
    let notifications = fs::read_to_string(fixture.path.join("notifications")).unwrap();
    assert!(notifications.contains("validate the profile and its layouts"));
}

#[test]
fn preflight_refuses_occupied_later_targets_and_insufficient_observations() {
    for scenario in [
        "window",
        "empty-container",
        "floating-container",
        "duplicate-number",
        "focus-away-back",
        "workspace-move",
        "window-during-fence",
    ] {
        let fixture = fixture();
        let mut initial = tree();
        let mut later = workspace(1, false);
        let expected = match scenario {
            "window" | "empty-container" => {
                later["nodes"] = json!([if scenario == "window" {
                    window(800, false)
                } else {
                    sway_node(800, "con", 100, vec![])
                }]);
                "must be completely empty"
            }
            "floating-container" => {
                later["floating_nodes"] = json!([sway_node(800, "floating_con", 100, vec![])]);
                "must be completely empty"
            }
            "duplicate-number" => {
                later["num"] = json!(3);
                "ambiguous workspace identities or numbers"
            }
            _ => "unexpected workspace change",
        };
        initial["nodes"][0]["nodes"]
            .as_array_mut()
            .unwrap()
            .push(later);
        let mut step = snapshot(&initial);
        if let Step::Snapshot { events, .. } = &mut step {
            match scenario {
                "focus-away-back" => {
                    events.extend([focus_event(3, 90), focus_event(90, 3)]);
                }
                "workspace-move" => {
                    events.push((
                        WORKSPACE_EVENT,
                        json!({"change": "move", "current": workspace(3, false), "old": null}),
                    ));
                }
                "window-during-fence" => {
                    events.push((
                        WINDOW_EVENT,
                        json!({"change": "new", "container": window(800, false)}),
                    ));
                }
                _ => {}
            }
        }
        let expected = if scenario == "window-during-fence" {
            "window activity"
        } else {
            expected
        };
        let output = run(&fixture, vec![step], false);
        assert_failure(&fixture, &output, expected, 0);
        assert!(!fixture.path.join("launches").exists());
    }
}

#[test]
fn selection_rejection_malformed_replies_and_timeout_never_retry() {
    for named in [false, true] {
        for (reply, expected) in [
            (
                Some(json!([{"success": false, "error": "selection denied"}])),
                "selection denied",
            ),
            (Some(json!([])), "expected exactly one successful reply"),
            (
                Some(json!([{"success": true}, {"success": true}])),
                "expected exactly one successful reply",
            ),
            (
                Some(json!([{"error": "missing success"}])),
                "command IPC decode",
            ),
            (None, "command deadline/channel failure"),
        ] {
            let fixture = fixture();
            if named {
                let profile = fs::read_to_string(fixture.path.join("work.yaml")).unwrap();
                fixture.file(
                    "work.yaml",
                    &profile.replace("workspace: 3", "workspace: 'music player'"),
                );
            }
            let initial = tree();
            let mut select = command("workspace --no-auto-back-and-forth number 3".into());
            if let Step::Command { replies, .. } = &mut select {
                *replies = reply;
            }
            let output = run(
                &fixture,
                if named {
                    named_steps(
                        vec![snapshot(&initial), snapshot(&initial), select],
                        &[(3, "music player")],
                    )
                } else {
                    vec![snapshot(&initial), snapshot(&initial), select]
                },
                false,
            );
            assert_failure(&fixture, &output, expected, 0);
            assert!(String::from_utf8_lossy(&output.stderr).contains("workspace selection"));
            assert!(!fixture.path.join("launches").exists());
        }
    }
}

#[test]
fn occupancy_identity_and_history_changes_stop_selection_without_launches() {
    for scenario in [
        "occupied-before",
        "replaced-before",
        "moved-output-before",
        "occupied-after",
        "wrong-focus",
        "wrong-number",
        "wrong-id",
        "missing-focus-event",
        "away-back-before",
        "away-back-after",
        "reload-after",
        "unexpected-init",
    ] {
        let fixture = fixture();
        let initial = tree();
        let mut before = initial.clone();
        let mut after = focus_workspace(&initial, 3);
        let expected = match scenario {
            "occupied-before" => {
                before["nodes"][0]["nodes"][1]["nodes"] = json!([window(800, false)]);
                "must be completely empty"
            }
            "replaced-before" => {
                before["nodes"][0]["nodes"][1]["id"] = json!(444);
                "changed since previous observation"
            }
            "moved-output-before" => {
                before["nodes"][0]["id"] = json!(44);
                "containing output before selection"
            }
            "occupied-after" => {
                after["nodes"][0]["nodes"][1]["focused"] = json!(false);
                after["nodes"][0]["nodes"][1]["nodes"] = json!([window(800, true)]);
                "must be completely empty"
            }
            "wrong-focus" => {
                after = initial.clone();
                "selected workspace number"
            }
            "wrong-number" => {
                after["nodes"][0]["nodes"][1]["num"] = json!(4);
                "selected workspace number"
            }
            "wrong-id" => {
                after["nodes"][0]["nodes"][1]["id"] = json!(444);
                "identity or output differs"
            }
            "missing-focus-event" => "expected focus event was not observed",
            _ => "unexpected workspace change",
        };
        let mut steps = vec![snapshot(&initial)];
        let mut selection = selection_steps(&before, &after, 3, 90, false);
        if let Step::Snapshot { events, .. } = &mut selection[0]
            && scenario == "away-back-before"
        {
            events.extend([focus_event(3, 90), focus_event(90, 3)]);
        }
        if let Step::Command { events, .. } = &mut selection[1] {
            match scenario {
                "missing-focus-event" => events.clear(),
                "away-back-after" => events.extend([focus_event(90, 3), focus_event(3, 90)]),
                "reload-after" => events.push((
                    WORKSPACE_EVENT,
                    json!({"change": "reload", "current": null, "old": null}),
                )),
                "unexpected-init" => events.insert(
                    0,
                    (
                        WORKSPACE_EVENT,
                        json!({"change": "init", "current": workspace(3, false), "old": null}),
                    ),
                ),
                _ => {}
            }
        }
        if scenario.ends_with("-before") {
            selection.truncate(1);
        }
        steps.extend(selection);
        let output = run(&fixture, steps, false);
        let expected = if scenario == "reload-after" {
            "workspace reload: revalidation required"
        } else {
            expected
        };
        assert_failure(&fixture, &output, expected, 0);
        assert!(!fixture.path.join("launches").exists());
    }
}

#[test]
fn failure_on_second_layout_retains_first_and_partial_windows_without_selecting_third() {
    for named in [false, true] {
        let fixture = fixture();
        if named {
            let profile = fs::read_to_string(fixture.path.join("work.yaml")).unwrap();
            fixture.file(
                "work.yaml",
                &profile.replace("workspace: 1", "workspace: 'music player'"),
            );
        }
        fixture.file("second.yaml", &format!("version: 1\ncwd: '{}'\nlayout: {{id: t, command: ['./app', second], match: {{app_id: profile-test}}}}\n", fixture.path.display()));
        let initial = tree();
        let selected = focus_workspace(&initial, 3);
        let mut steps = vec![snapshot(&initial)];
        steps.extend(selection_steps(&initial, &selected, 3, 90, false));
        let (construction, first_done) = construction_steps(&selected, 3, 20);
        steps.extend(construction);
        let mut second_selected = first_done.clone();
        second_selected["nodes"][0]["nodes"]
            .as_array_mut()
            .unwrap()
            .push(workspace(1, false));
        second_selected = focus_workspace(&second_selected, 1);
        steps.extend(selection_steps(&first_done, &second_selected, 1, 3, true));
        let (mut second_construction, _) = construction_steps(&second_selected, 1, 21);
        // Mapping a second unexpected window fails after the second application launches.
        if let Step::Snapshot { tree, .. } = &mut second_construction[2] {
            tree["nodes"][0]["nodes"][2]["nodes"]
                .as_array_mut()
                .unwrap()
                .push(window(22, false));
        }
        second_construction.truncate(3);
        steps.extend(second_construction);
        let steps = if named {
            named_steps(steps, &[(1, "music player")])
        } else {
            steps
        };
        let output = run(&fixture, steps, false);
        assert_failure(&fixture, &output, "layout construction", 1);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(if named {
                "workspace \"music player\", layout \"second\""
            } else {
                "workspace 1, layout \"second\""
            }),
            "{error}"
        );
        assert!(error.contains("[(3, 103)]"), "{error}");
        assert!(
            error.contains("application launches may have left partial layouts"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(fixture.path.join("launches")).unwrap(),
            "first\nsecond\n"
        );
    }
}

#[test]
fn inconsistent_workspace_and_tree_replies_fail_read_only_preflight() {
    for scenario in [
        "number",
        "name",
        "output",
        "focus",
        "missing",
        "duplicate-id",
    ] {
        let fixture = fixture();
        let initial = tree();
        let mut reply = workspaces(&initial);
        match scenario {
            "number" => reply[1]["num"] = json!(4),
            "name" => reply[1]["name"] = json!("different"),
            "output" => reply[1]["output"] = json!("different"),
            "focus" => reply[1]["focused"] = json!(true),
            "missing" => {
                reply.as_array_mut().unwrap().pop();
            }
            "duplicate-id" => reply[1]["id"] = json!(190),
            _ => unreachable!(),
        }
        let mut preflight = snapshot(&initial);
        if let Step::Snapshot {
            workspace_reply, ..
        } = &mut preflight
        {
            *workspace_reply = Some(reply);
        }
        let output = run(&fixture, vec![preflight], false);
        assert_failure(&fixture, &output, "workspace snapshot", 0);
        assert!(!fixture.path.join("launches").exists());
    }
}

#[test]
fn coordinator_history_across_a_completed_load_prevents_the_next_switch() {
    for named in [false, true] {
        let fixture = fixture();
        if named {
            let profile = fs::read_to_string(fixture.path.join("work.yaml")).unwrap();
            fixture.file(
                "work.yaml",
                &profile.replace("workspace: 3", "workspace: 'music'"),
            );
        }
        let initial = tree();
        let selected = focus_workspace(&initial, 3);
        let mut steps = vec![snapshot(&initial)];
        steps.extend(selection_steps(&initial, &selected, 3, 90, false));
        let (mut construction, _) = construction_steps(&selected, 3, 20);
        if let Step::Snapshot { events, .. } = construction.last_mut().unwrap() {
            events.extend([focus_event(90, 3), focus_event(3, 90)]);
        }
        steps.extend(construction);
        let steps = if named {
            named_steps(steps, &[(3, "music")])
        } else {
            steps
        };
        let output = run(&fixture, steps, false);
        assert_failure(&fixture, &output, "unexpected workspace change", 1);
        assert_eq!(
            fs::read_to_string(fixture.path.join("launches")).unwrap(),
            "first\n"
        );
    }
}

#[test]
fn selecting_from_an_empty_workspace_allows_sway_to_destroy_only_that_workspace() {
    let fixture = fixture();
    fixture.file(
        "work.yaml",
        "version: 1\nworkspaces: [{workspace: 3, layout: first}]\n",
    );
    let mut initial = tree();
    initial["nodes"][0]["nodes"][0]["nodes"] = json!([]);
    initial["nodes"][0]["nodes"][0]["focused"] = json!(true);
    let mut selected = focus_workspace(&initial, 3);
    selected["nodes"][0]["nodes"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    let mut steps = vec![snapshot(&initial)];
    let mut selection = selection_steps(&initial, &selected, 3, 90, false);
    if let Step::Command { events, .. } = &mut selection[1] {
        events.push((
            WORKSPACE_EVENT,
            json!({"change": "empty", "current": workspace(90, false), "old": null}),
        ));
    }
    steps.extend(selection);
    let (construction, _) = construction_steps(&selected, 3, 20);
    steps.extend(construction);
    let output = run(&fixture, steps, false);
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn an_authorized_empty_workspace_disappearance_can_be_recreated_as_a_later_target() {
    let fixture = fixture();
    fixture.file("work.yaml", "version: 1\nworkspaces: [{workspace: 3, layout: first}, {workspace: 90, layout: second}]\n");
    let mut initial = tree();
    initial["nodes"][0]["nodes"][0]["nodes"] = json!([]);
    initial["nodes"][0]["nodes"][0]["focused"] = json!(true);
    let mut selected = focus_workspace(&initial, 3);
    selected["nodes"][0]["nodes"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    let mut steps = vec![snapshot(&initial)];
    let mut selection = selection_steps(&initial, &selected, 3, 90, false);
    if let Step::Command { events, .. } = &mut selection[1] {
        events.push((
            WORKSPACE_EVENT,
            json!({"change": "empty", "current": workspace(90, false), "old": null}),
        ));
    }
    steps.extend(selection);
    let (construction, first_done) = construction_steps(&selected, 3, 20);
    steps.extend(construction);

    let mut second_selected = first_done.clone();
    let mut recreated = workspace(90, false);
    recreated["id"] = json!(290);
    second_selected["nodes"][0]["nodes"]
        .as_array_mut()
        .unwrap()
        .push(recreated);
    second_selected = focus_workspace(&second_selected, 90);
    let mut selection = selection_steps(&first_done, &second_selected, 90, 3, true);
    if let Step::Command { events, .. } = &mut selection[1] {
        for (_, event) in events {
            event["current"]["id"] = json!(290);
        }
    }
    steps.extend(selection);
    let (construction, _) = construction_steps(&second_selected, 90, 21);
    steps.extend(construction);
    let output = run(&fixture, steps, false);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        fs::read_to_string(fixture.path.join("launches")).unwrap(),
        "first\nsecond\n"
    );
}

#[test]
fn selecting_the_already_focused_target_does_not_require_a_focus_change() {
    let fixture = fixture();
    fixture.file(
        "work.yaml",
        "version: 1\nworkspaces: [{workspace: 3, layout: first}]\n",
    );
    let initial = focus_workspace(&tree(), 3);
    let mut steps = vec![
        snapshot(&initial),
        snapshot(&initial),
        command("workspace --no-auto-back-and-forth number 3".into()),
        snapshot(&initial),
    ];
    let (construction, _) = construction_steps(&initial, 3, 20);
    steps.extend(construction);
    let output = run(&fixture, steps, false);
    assert!(output.status.success(), "{output:?}");
}

// Retain the existing numeric transcript builders, then translate only workspace nodes
// and selection commands. Window/container IDs remain independent of workspace numbers.
fn name_workspace_nodes(value: &mut Value, number: i64, name: &str) {
    match value {
        Value::Object(fields) => {
            if fields.get("type").is_some_and(|kind| kind == "workspace")
                && fields.get("num").is_some_and(|num| num == number)
            {
                fields.insert("num".into(), json!(-1));
                fields.insert("name".into(), json!(name));
            }
            for child in fields.values_mut() {
                name_workspace_nodes(child, number, name);
            }
        }
        Value::Array(children) => {
            for child in children {
                name_workspace_nodes(child, number, name);
            }
        }
        _ => {}
    }
}

fn named_steps(mut steps: Vec<Step>, targets: &[(i64, &str)]) -> Vec<Step> {
    for step in &mut steps {
        for &(number, name) in targets {
            match step {
                Step::Snapshot { tree, events, .. } => {
                    name_workspace_nodes(tree, number, name);
                    for (_, event) in events {
                        name_workspace_nodes(event, number, name);
                    }
                }
                Step::Command {
                    command, events, ..
                } => {
                    if *command == format!("workspace --no-auto-back-and-forth number {number}") {
                        *command = format!("workspace --no-auto-back-and-forth \"{name}\"");
                    }
                    for (_, event) in events {
                        name_workspace_nodes(event, number, name);
                    }
                }
            }
        }
    }
    steps
}

#[test]
fn exact_named_targets_existing_missing_and_already_focused() {
    for scenario in ["existing", "missing", "focused", "mixed", "named-only"] {
        let fixture = fixture();
        let multiple = matches!(scenario, "mixed" | "named-only");
        fixture.file("work.yaml", if multiple {
            "version: 1\nworkspaces: [{workspace: 'music player', layout: first}, {workspace: 1, layout: second}]\n"
        } else {
            "version: 1\nworkspaces: [{workspace: 'music player', layout: first}]\n"
        });
        if scenario == "named-only" {
            fixture.file("work.yaml", "version: 1\nworkspaces: [{workspace: 'music player', layout: first}, {workspace: monitoring, layout: second}]\n");
        }
        let mut initial = tree();
        // Unrelated named workspaces also have num=-1 and must never be selected.
        initial["nodes"][0]["nodes"]
            .as_array_mut()
            .unwrap()
            .push(workspace(7, false));
        if scenario == "focused" {
            initial = focus_workspace(&initial, 3);
        } else if scenario == "missing" {
            initial["nodes"][0]["nodes"]
                .as_array_mut()
                .unwrap()
                .remove(1);
        }
        let mut selected = initial.clone();
        if scenario == "missing" {
            selected["nodes"][0]["nodes"]
                .as_array_mut()
                .unwrap()
                .push(workspace(3, false));
        }
        selected = focus_workspace(&selected, 3);
        let mut selection = selection_steps(&initial, &selected, 3, 90, scenario == "missing");
        if scenario == "focused"
            && let Step::Command { events, .. } = &mut selection[1]
        {
            events.clear();
        }
        let mut steps = vec![snapshot(&initial)];
        steps.extend(selection);
        let (construction, first_done) = construction_steps(&selected, 3, 20);
        steps.extend(construction);
        if multiple {
            let mut second_selected = first_done.clone();
            second_selected["nodes"][0]["nodes"]
                .as_array_mut()
                .unwrap()
                .push(workspace(1, false));
            second_selected = focus_workspace(&second_selected, 1);
            steps.extend(selection_steps(&first_done, &second_selected, 1, 3, true));
            steps.extend(construction_steps(&second_selected, 1, 21).0);
        }
        let mut targets = vec![(3, "music player"), (7, "unrelated")];
        if scenario == "named-only" {
            targets.push((1, "monitoring"));
        }
        let output = run(&fixture, named_steps(steps, &targets), true);
        assert!(output.status.success(), "{scenario}: {output:?}");
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty(), "{output:?}");
        assert_eq!(
            fs::read_to_string(fixture.path.join("launches")).unwrap(),
            if multiple {
                "first\nsecond\n"
            } else {
                "first\n"
            }
        );
    }
}

#[test]
fn named_later_target_preflight_is_read_only() {
    for scenario in [
        "window",
        "structural",
        "floating",
        "duplicate-name",
        "inconsistent-number",
    ] {
        let fixture = fixture();
        fixture.file("work.yaml", "version: 1\nworkspaces: [{workspace: 3, layout: first}, {workspace: music, layout: second}]\n");
        let mut initial = tree();
        let mut later = workspace(1, false);
        later["name"] = json!("music");
        later["num"] = json!(-1);
        let expected = match scenario {
            "window" | "structural" => {
                later["nodes"] = json!([if scenario == "window" {
                    window(800, false)
                } else {
                    sway_node(800, "con", 100, vec![])
                }]);
                "must be completely empty"
            }
            "floating" => {
                later["floating_nodes"] = json!([sway_node(800, "floating_con", 100, vec![])]);
                "must be completely empty"
            }
            "duplicate-name" => {
                initial["nodes"][0]["nodes"][1]["name"] = json!("music");
                "ambiguous workspace"
            }
            _ => {
                later["num"] = json!(1);
                "inconsistent number/name"
            }
        };
        initial["nodes"][0]["nodes"]
            .as_array_mut()
            .unwrap()
            .push(later);
        let output = run(&fixture, vec![snapshot(&initial)], false);
        assert_failure(&fixture, &output, expected, 0);
        assert!(!fixture.path.join("launches").exists());
    }
}

#[test]
fn named_selection_rejects_bad_history_and_changed_identity() {
    for scenario in [
        "rename",
        "missing-focus",
        "bad-current-name",
        "bad-old-name",
        "bad-number",
        "missing-current",
        "away-back",
        "moved-output",
        "wrong-id",
        "wrong-name",
        "wrong-number",
        "missing-init",
        "bad-init-name",
        "bad-empty-name",
    ] {
        let fixture = fixture();
        fixture.file(
            "work.yaml",
            "version: 1\nworkspaces: [{workspace: music, layout: first}]\n",
        );
        let mut initial = tree();
        let create = matches!(scenario, "missing-init" | "bad-init-name");
        if create {
            initial["nodes"][0]["nodes"]
                .as_array_mut()
                .unwrap()
                .remove(1);
        }
        let mut after = initial.clone();
        if create {
            after["nodes"][0]["nodes"]
                .as_array_mut()
                .unwrap()
                .push(workspace(3, false));
        }
        after = focus_workspace(&after, 3);
        let mut steps = vec![snapshot(&initial)];
        steps.extend(selection_steps(&initial, &after, 3, 90, create));
        let mut steps = named_steps(steps, &[(3, "music")]);
        if let Step::Command { events, .. } = &mut steps[2] {
            match scenario {
                "rename" => events.push((
                    WORKSPACE_EVENT,
                    json!({"change": "rename", "current": workspace(3, true), "old": null}),
                )),
                "missing-focus" => events.clear(),
                "bad-current-name" => events[0].1["current"]["name"] = json!("other"),
                "bad-old-name" => events[0].1["old"]["name"] = json!("other"),
                "bad-number" => events[0].1["current"]["num"] = json!(3),
                "missing-current" => events[0].1["current"] = Value::Null,
                "away-back" => events.extend([focus_event(90, 3), focus_event(3, 90)]),
                "missing-init" => {
                    events.remove(0);
                }
                "bad-init-name" => events[0].1["current"]["name"] = json!("other"),
                "bad-empty-name" => events.push((
                    WORKSPACE_EVENT,
                    json!({"change": "empty", "current": workspace(90, false), "old": null}),
                )),
                _ => {}
            }
        }
        if let Step::Snapshot { tree, .. } = &mut steps[3] {
            match scenario {
                "moved-output" => tree["nodes"][0]["id"] = json!(44),
                "wrong-id" => tree["nodes"][0]["nodes"][1]["id"] = json!(444),
                "wrong-name" => tree["nodes"][0]["nodes"][1]["name"] = json!("other"),
                "wrong-number" => tree["nodes"][0]["nodes"][1]["num"] = json!(3),
                _ => {}
            }
        }
        let output = run(&fixture, steps, false);
        assert_failure(&fixture, &output, "workspace selection", 0);
        assert!(!fixture.path.join("launches").exists());
    }
}

#[test]
fn named_selection_checks_identity_of_the_empty_previous_workspace_event() {
    for scenario in ["name", "number", "old"] {
        let fixture = fixture();
        fixture.file(
            "work.yaml",
            "version: 1\nworkspaces: [{workspace: music, layout: first}]\n",
        );
        let mut initial = tree();
        initial["nodes"][0]["nodes"][0]["nodes"] = json!([]);
        initial["nodes"][0]["nodes"][0]["focused"] = json!(true);
        let mut selected = focus_workspace(&initial, 3);
        selected["nodes"][0]["nodes"]
            .as_array_mut()
            .unwrap()
            .remove(0);
        let mut selection = selection_steps(&initial, &selected, 3, 90, false);
        let mut destroyed = workspace(90, false);
        let old = match scenario {
            "name" => {
                destroyed["name"] = json!("other");
                Value::Null
            }
            "number" => {
                destroyed["num"] = json!(-1);
                Value::Null
            }
            _ => workspace(3, false),
        };
        if let Step::Command { events, .. } = &mut selection[1] {
            events.push((
                WORKSPACE_EVENT,
                json!({"change": "empty", "current": destroyed, "old": old}),
            ));
        }
        let mut steps = vec![snapshot(&initial)];
        steps.extend(selection);
        let output = run(&fixture, named_steps(steps, &[(3, "music")]), false);
        assert_failure(&fixture, &output, "unexpected workspace change", 0);
        assert!(!fixture.path.join("launches").exists());
    }
}

#[test]
fn unified_menu_profiles_use_fresh_coordinator_state_and_one_diagnostics_session() {
    for initial_snapshot in [tree(), json!({})] {
        let fixture = fixture();
        super::menu::prepare_menu_fixture(&fixture);
        fs::create_dir(fixture.path.join("profiles")).unwrap();
        fixture.file(
            "profiles/work.yaml",
            "version: 1\nworkspaces: [{workspace: 3, layout: first}]\n",
        );
        let initial = tree();
        let selected = focus_workspace(&initial, 3);
        let mut steps = vec![snapshot(&initial)];
        steps.extend(selection_steps(&initial, &selected, 3, 90, false));
        let (construction, _) = construction_steps(&selected, 3, 20);
        steps.extend(construction);
        let selection = if initial_snapshot == json!({}) {
            "0\n"
        } else {
            "2\n"
        };
        let output = run_transcript(&fixture, steps, false, Some((initial_snapshot, selection)));
        assert!(output.status.success(), "{output:?}");
        assert!(
            output.stdout.is_empty() && output.stderr.is_empty(),
            "{output:?}"
        );
        assert!(!fixture.path.join("notification").exists());
        assert_eq!(
            fs::read_to_string(fixture.path.join("launches")).unwrap(),
            "first\n"
        );
        let log = fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
        assert!(log.contains("completed 1 workspaces [(3, 103)]"), "{log}");
        assert_eq!(
            log.matches(r#"[profile \"work\": validation] begin"#)
                .count(),
            1,
            "{log}"
        );
    }
}

#[test]
fn unified_menu_profile_preflights_all_targets_before_any_selection() {
    let fixture = fixture();
    super::menu::prepare_menu_fixture(&fixture);
    fs::create_dir(fixture.path.join("profiles")).unwrap();
    fixture.file("profiles/work.yaml", "version: 1\nworkspaces: [{workspace: 3, layout: first}, {workspace: 90, layout: second}]\n");
    let initial = tree();
    let output = run_transcript(
        &fixture,
        vec![snapshot(&initial)],
        false,
        Some((initial, "2\n")),
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("must be completely empty"),
        "{output:?}"
    );
    assert!(!fixture.path.join("launches").exists());
    let notifications = fs::read_to_string(fixture.path.join("notification")).unwrap();
    assert_eq!(notifications.matches("--app-name=sway-stage").count(), 1);
    assert!(notifications.contains("sway-stage profile load failed"));
    let log = fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
    assert!(log.contains("completed 0 workspaces"), "{log}");
}

#[test]
fn output_expectations_accept_existing_and_created_numeric_and_named_targets() {
    for missing in [false, true] {
        for named in [false, true] {
            let fixture = fixture();
            let target = if named { "'music player'" } else { "3" };
            fixture.file("work.yaml", &format!("version: 1\nworkspaces: [{{workspace: {target}, layout: first, output: fixture-output}}]\n"));
            let mut initial = tree();
            if missing {
                initial["nodes"][0]["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .remove(1);
            }
            let mut selected = initial.clone();
            if missing {
                selected["nodes"][0]["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .push(workspace(3, false));
            }
            selected = focus_workspace(&selected, 3);
            let mut steps = vec![snapshot(&initial)];
            steps.extend(selection_steps(&initial, &selected, 3, 90, missing));
            steps.extend(construction_steps(&selected, 3, 20).0);
            let steps = if named {
                named_steps(steps, &[(3, "music player")])
            } else {
                steps
            };
            let output = run(&fixture, steps, true);
            assert!(output.status.success(), "{output:?}");
            assert_eq!(
                fs::read_to_string(fixture.path.join("launches")).unwrap(),
                "first\n"
            );
        }
    }
}

#[test]
fn output_preflight_rejects_first_later_and_missing_targets_without_launching() {
    for scenario in ["first", "later", "missing", "case", "scratchpad"] {
        let fixture = fixture();
        let target = if matches!(scenario, "first" | "case") {
            3
        } else {
            1
        };
        let expected_output = match scenario {
            "case" => "FIXTURE-OUTPUT",
            "scratchpad" => "__i3",
            _ => "other-output",
        };
        fixture.file("work.yaml", &format!("version: 1\nworkspaces: [{{workspace: 3, layout: first{}}}, {{workspace: 1, layout: second{}}}]\n",
            if target == 3 { format!(", output: {expected_output}") } else { String::new() },
            if target == 1 { format!(", output: {expected_output}") } else { String::new() }));
        let mut initial = tree();
        if scenario == "scratchpad" {
            let mut internal_output = sway_node(4, "output", 100, vec![]);
            internal_output["name"] = json!("__i3");
            initial["nodes"]
                .as_array_mut()
                .unwrap()
                .push(internal_output);
        }
        if scenario == "later" {
            initial["nodes"][0]["nodes"]
                .as_array_mut()
                .unwrap()
                .push(workspace(1, false));
        }
        let expected = if matches!(scenario, "first" | "later" | "case") {
            "observed output"
        } else {
            "is not active"
        };
        let output = run(&fixture, vec![snapshot(&initial)], false);
        assert_failure(&fixture, &output, expected, 0);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(&format!("workspace {target}: expected output")),
            "{error}"
        );
        assert!(
            error.contains("Sway's workspace/output rules control placement"),
            "{error}"
        );
        assert!(!fixture.path.join("launches").exists());
    }
}

#[test]
fn output_expectations_are_rechecked_before_selection_and_after_creation() {
    for scenario in [
        "before-existing",
        "before-missing",
        "created-wrong",
        "later-created-wrong",
    ] {
        let fixture = fixture();
        let later = scenario == "later-created-wrong";
        fixture.file("work.yaml", if later {
            "version: 1\nworkspaces: [{workspace: 3, layout: first}, {workspace: 1, layout: second, output: expected-output}]\n"
        } else {
            "version: 1\nworkspaces: [{workspace: 3, layout: first, output: expected-output}]\n"
        });
        let mut initial = tree();
        let mut expected_display = sway_node(4, "output", 100, vec![]);
        expected_display["name"] = json!("expected-output");
        if scenario == "before-existing" {
            let target = initial["nodes"][0]["nodes"]
                .as_array_mut()
                .unwrap()
                .remove(1);
            expected_display["nodes"] = json!([target]);
        } else if !later {
            initial["nodes"][0]["nodes"]
                .as_array_mut()
                .unwrap()
                .remove(1);
        }
        initial["nodes"]
            .as_array_mut()
            .unwrap()
            .push(expected_display);
        let mut steps = vec![snapshot(&initial)];
        let mut before = initial.clone();
        if later {
            let selected = focus_workspace(&initial, 3);
            steps.extend(selection_steps(&initial, &selected, 3, 90, false));
            let (construction, finished) = construction_steps(&selected, 3, 20);
            steps.extend(construction);
            before = finished;
        }
        if scenario == "before-existing" {
            let target = before["nodes"][1]["nodes"]
                .as_array_mut()
                .unwrap()
                .remove(0);
            before["nodes"][0]["nodes"]
                .as_array_mut()
                .unwrap()
                .push(target);
        } else if scenario == "before-missing" {
            before["nodes"].as_array_mut().unwrap().pop();
        }
        if scenario.starts_with("before-") {
            steps.push(snapshot(&before));
        } else {
            let target_number = if later { 1 } else { 3 };
            let mut after = before.clone();
            after["nodes"][0]["nodes"]
                .as_array_mut()
                .unwrap()
                .push(workspace(target_number, false));
            after = focus_workspace(&after, target_number);
            steps.extend(selection_steps(
                &before,
                &after,
                target_number,
                if later { 3 } else { 90 },
                true,
            ));
        }
        let output = run(&fixture, steps, false);
        assert_failure(&fixture, &output, "expected output", usize::from(later));
        if later {
            assert_eq!(
                fs::read_to_string(fixture.path.join("launches")).unwrap(),
                "first\n"
            );
        } else {
            assert!(!fixture.path.join("launches").exists());
        }
    }
}

#[test]
fn configured_outputs_preserve_loader_migration_and_history_guards() {
    for scenario in ["output-identity", "move-away-back", "focus-away-back"] {
        let fixture = fixture();
        fixture.file(
            "work.yaml",
            "version: 1\nworkspaces: [{workspace: 3, layout: first, output: fixture-output}]\n",
        );
        let initial = tree();
        let selected = focus_workspace(&initial, 3);
        let mut steps = vec![snapshot(&initial)];
        steps.extend(selection_steps(&initial, &selected, 3, 90, false));
        let mut changed = snapshot(&selected);
        if let Step::Snapshot { tree, events, .. } = &mut changed {
            match scenario {
                "output-identity" => tree["nodes"][0]["id"] = json!(44),
                "move-away-back" => {
                    for _ in 0..2 {
                        events.push((
                            WORKSPACE_EVENT,
                            json!({"change": "move", "current": workspace(3, true), "old": null}),
                        ));
                    }
                }
                _ => events.extend([focus_event(90, 3), focus_event(3, 90)]),
            }
        }
        // The loader freezes its own initial snapshot, then checks again before launch.
        steps.push(snapshot(&selected));
        steps.push(changed);
        let output = run(&fixture, steps, false);
        assert_failure(&fixture, &output, "layout construction", 0);
        assert!(!fixture.path.join("launches").exists());
    }
}
