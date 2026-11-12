use super::*;
use crate::observation::fixtures::*;
use serde_json::{Value, json};
use std::fs;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::thread;

struct LaunchFixture {
    path: PathBuf,
}

impl LaunchFixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = env::temp_dir().join(format!(
            "sway-stage-launch-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        // A fixture application records every invocation, then delegates/exits. It has
        // no compositor/socket access and creates no desktop windows.
        fs::write(path.join("app"), "#!/bin/sh\nprintf 'launch\\n' >> launches\nprintf '%s\\n' \"$PWD\" \"$MARKER\" \"$PATH\" \"$@\" > settings\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path.join("app"), fs::Permissions::from_mode(0o700)).unwrap();
        Self { path }
    }

    fn config(&self) -> Config {
        let path = self.path.join("layout.yaml");
        fs::write(&path, format!("version: 1\nwindow_timeout_ms: 500\nlayout:\n  id: test\n  command: [./app, '', '$HOME', '~', '*.txt', 'two words', '; touch bad']\n  match: {{app_id: expected}}\n  cwd: '{}'\n  env: {{MARKER: child-value, PATH: /fixture-child-path}}\n", self.path.display())).unwrap();
        crate::config::read(&path).unwrap()
    }

    fn run(&self, config: &Config, steps: Vec<IpcStep>) -> Result<(), String> {
        self.serve_transcript(steps, |connect, diagnostics| {
            load_with(config, None, connect, diagnostics).map(|_| ())
        })
    }

    fn run_with_finalization(
        &self,
        config: &Config,
        mut steps: Vec<IpcStep>,
    ) -> Result<(), String> {
        let tree = steps.last().unwrap().tree.clone();
        fn append_sizing_snapshots(steps: &mut Vec<IpcStep>, tree: &Value, node: &LayoutNode) {
            if let NodeKind::Group { layout, children } = &node.kind {
                if matches!(layout, GroupLayout::SplitH | GroupLayout::SplitV) {
                    steps.push(snapshot_step(tree.clone()));
                }
                for child in children {
                    append_sizing_snapshots(steps, tree, &child.value);
                }
            }
        }
        append_sizing_snapshots(&mut steps, &tree, &config.layout.value);
        // Derive expected initialization commands from the observed fixture hierarchy.
        fn append_group_selection_steps(
            steps: &mut Vec<IpcStep>,
            current: &mut Value,
            group: &Value,
        ) {
            for child in group["nodes"].as_array().unwrap() {
                append_group_selection_steps(steps, current, child);
            }
            if matches!(group["layout"].as_str(), Some("tabbed" | "stacked")) {
                let mut first_leaf = group;
                while let Some(child) = first_leaf["nodes"].as_array().unwrap().first() {
                    first_leaf = child;
                }
                let id = first_leaf["id"].as_i64().unwrap();
                append_final_focus_steps(steps, current, id);
                focus_only(current, id);
            }
        }
        let workspace = &tree["nodes"][0]["nodes"][0];
        let mut selected = tree.clone();
        append_group_selection_steps(&mut steps, &mut selected, workspace);
        append_final_focus_steps(&mut steps, &selected, 20);
        self.run(config, steps)
    }

    fn serve_transcript(
        &self,
        steps: Vec<IpcStep>,
        action: impl FnOnce(
            &mut dyn FnMut() -> Result<UnixStream, String>,
            &mut Diagnostics,
        ) -> Result<(), String>,
    ) -> Result<(), String> {
        let (event_client, mut events) = socket_pair();
        let (request_sender, request_sockets) = mpsc::channel();
        let expected_step_count = steps.len();
        let server = thread::spawn(move || {
            let mut consumed_steps = 0;
            let subscription: Value =
                serde_json::from_slice(&read_request(&mut events, 2)).unwrap();
            assert_eq!(subscription, json!(["workspace", "window", "tick"]));
            send_message(&mut events, 2, json!({"success": true}));
            for (index, mut socket) in request_sockets.into_iter().enumerate() {
                consumed_steps += 1;
                let step = &steps[index.min(steps.len() - 1)];
                if let Some((command, replies)) = &step.command {
                    assert_eq!(
                        String::from_utf8(read_request(&mut socket, 0)).unwrap(),
                        *command
                    );
                    for (kind, event) in &step.events {
                        send_message(&mut events, *kind, event.clone());
                    }
                    send_message(&mut socket, 0, replies.clone());
                    continue;
                }
                read_request(&mut socket, 1);
                if step.disconnect {
                    drop(events);
                    return consumed_steps;
                }
                send_message(&mut socket, 1, step.workspaces.clone());
                read_request(&mut socket, 4);
                send_message(&mut socket, 4, step.tree.clone());
                let token = String::from_utf8(read_request(&mut socket, 10)).unwrap();
                for (kind, event) in &step.events {
                    send_message(&mut events, *kind, event.clone());
                }
                send_message(
                    &mut events,
                    TICK_EVENT,
                    json!({"first": false, "payload": token}),
                );
                send_message(&mut socket, 10, json!({"success": true}));
            }
            consumed_steps
        });
        let mut event_client = Some(event_client);
        let mut connect = || {
            if let Some(socket) = event_client.take() {
                return Ok(socket);
            }
            let (client, server) = socket_pair();
            request_sender
                .send(server)
                .map_err(|_| "fixture disconnected".to_owned())?;
            Ok(client)
        };
        // Avoid global environment changes or operational files outside the fixture.
        let mut diagnostics = Diagnostics::for_test(self.path.join("last-run.log"));
        let result = action(&mut connect, &mut diagnostics);
        drop(request_sender);
        let consumed_steps = server.join().unwrap();
        if result.is_ok() {
            assert!(
                consumed_steps >= expected_step_count,
                "unconsumed successful transcript steps"
            );
        }
        result
    }

    fn wait_for_launch_count(&self) -> usize {
        // The child is intentionally not waited on by load. Wait only in this fixture
        // for its marker, without changing the application's real lifecycle behavior.
        let deadline = ipc_deadline();
        loop {
            if let Ok(text) = fs::read_to_string(self.path.join("launches"))
                && !text.is_empty()
            {
                return text.lines().count();
            }
            assert!(Instant::now() < deadline, "fixture application never ran");
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for LaunchFixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}

#[derive(Clone)]
struct IpcStep {
    tree: Value,
    workspaces: Value,
    events: Vec<(u32, Value)>,
    disconnect: bool,
    command: Option<(String, Value)>,
}

fn snapshot_step(tree: Value) -> IpcStep {
    IpcStep {
        tree,
        workspaces: focused_workspace_reply(),
        events: Vec::new(),
        disconnect: false,
        command: None,
    }
}

fn wayland_window(id: i64, app_id: Option<&str>) -> Value {
    let mut wayland_window = node(id, "con", true);
    wayland_window["app_id"] = json!(app_id);
    wayland_window["shell"] = json!("xdg_shell");
    wayland_window["fullscreen_mode"] = json!(0);
    wayland_window
}

fn tree_with_target_windows(views: Vec<Value>) -> Value {
    let mut tree = empty_workspace_tree();
    let workspace = &mut tree["nodes"][0]["nodes"][0];
    workspace["focused"] = json!(views.is_empty());
    workspace["nodes"] = json!(views);
    tree
}

fn with_window_on_other_workspace(mut tree: Value, mut wayland_window: Value) -> Value {
    wayland_window["focused"] = json!(false);
    let mut other = node(11, "workspace", false);
    other["nodes"] = json!([wayland_window]);
    tree["nodes"][0]["nodes"]
        .as_array_mut()
        .unwrap()
        .push(other);
    tree
}

fn nodes(tree: &Value) -> Vec<&Value> {
    let mut all = vec![tree];
    for child in tree["nodes"].as_array().unwrap() {
        all.extend(nodes(child));
    }
    all
}
// Transcripts record trees, not Sway's focus algorithm. Window events are derived from
// each recorded focus transition: Sway 1.10.1's seat_set_workspace_focus emits
// window::focus only when focus moves to a different view, never for workspace or
// structural-container focus (including split wrappers and `focus parent`).
fn focus_events(before: &Value, after: &Value) -> Vec<(u32, Value)> {
    fn focused(tree: &Value) -> Option<&Value> {
        nodes(tree).into_iter().find(|node| node["focused"] == true)
    }
    match focused(after) {
        Some(wayland_window)
            if wayland_window["shell"].is_string()
                && focused(before).map(|node| &node["id"]) != Some(&wayland_window["id"]) =>
        {
            vec![(
                WINDOW_EVENT,
                json!({"change": "focus", "container": wayland_window}),
            )]
        }
        _ => Vec::new(),
    }
}
// The first tree showing new windows carries Sway's map events in its fence:
// window::new for each mapped view, then window::focus if one takes focus.
fn launched(before: &Value, after: &Value) -> IpcStep {
    let preexisting_container_ids: Vec<_> =
        nodes(before).into_iter().map(|node| &node["id"]).collect();
    let mut mapped_snapshot = snapshot_step(after.clone());
    for wayland_window in nodes(after) {
        if wayland_window["shell"].is_string()
            && !preexisting_container_ids.contains(&&wayland_window["id"])
        {
            let mut container = wayland_window.clone();
            container["focused"] = json!(false);
            mapped_snapshot.events.push((
                WINDOW_EVENT,
                json!({"change": "new", "container": container}),
            ));
        }
    }
    mapped_snapshot.events.extend(focus_events(before, after));
    mapped_snapshot
}

fn successful_steps() -> Vec<IpcStep> {
    let mapped_snapshot = tree_with_target_windows(vec![wayland_window(20, Some("expected"))]);
    vec![
        snapshot_step(empty_workspace_tree()),
        snapshot_step(empty_workspace_tree()),
        launched(&empty_workspace_tree(), &mapped_snapshot),
        snapshot_step(mapped_snapshot),
    ]
}

fn assert_load_fails(fixture: &LaunchFixture, steps: Vec<IpcStep>, expected: &str) {
    let error = fixture.run(&fixture.config(), steps).unwrap_err();
    assert!(error.contains(expected), "expected {expected:?}: {error}");
    assert_eq!(fixture.wait_for_launch_count(), 1);
}

// The foreign window deliberately shares the requested app ID: baseline ancestry,
// rather than selector mismatch, is the evidence that permits its close.
fn foreign_close_command_steps() -> Vec<IpcStep> {
    let mut steps = successful_steps();
    for step in &mut steps {
        step.tree =
            with_window_on_other_workspace(step.tree.clone(), wayland_window(90, Some("expected")));
    }
    let before = steps.last().unwrap().tree.clone();
    let after = tree_with_target_windows(vec![wayland_window(20, Some("expected"))]);
    let mut command = command_step("[con_id=20] focus");
    command.events.push((
        WINDOW_EVENT,
        json!({"change": "close", "container": wayland_window(90, Some("expected"))}),
    ));
    steps.extend([
        snapshot_step(before),
        command,
        snapshot_step(after.clone()),
        snapshot_step(after), // The close still requires another, quiet fence.
    ]);
    steps
}

#[test]
fn baseline_foreign_close_during_command_verification_settles_and_completes() {
    let fixture = LaunchFixture::new();
    fixture
        .run(&fixture.config(), foreign_close_command_steps())
        .unwrap();

    assert_eq!(fixture.wait_for_launch_count(), 1);
}

#[test]
fn foreign_close_does_not_bypass_ownership_history_hierarchy_or_workspace_guards() {
    for scenario in [
        "accepted-window-close",
        "unknown-window-close",
        "later-baseline-close",
        "move-instead-of-close",
        "changed-hierarchy",
        "focus-away-and-back",
        "workspace-migration",
        "changed-output",
        "foreign-window-move",
        "preexisting-new",
        "preexisting-floating",
        "preexisting-fullscreen",
    ] {
        let fixture = LaunchFixture::new();
        let mut steps = foreign_close_command_steps();
        let expected = match scenario {
            "accepted-window-close" => {
                steps[5].events[0].1["container"] = wayland_window(20, Some("expected"));
                "accepted window/group"
            }
            "unknown-window-close" => {
                steps[5].events[0].1["container"] = wayland_window(99, Some("unrelated"));
                "ownership is uncertain"
            }
            "later-baseline-close" => {
                // First seen during this launch, then added to preexisting_ids by the
                // command's before-fence: this is still not initial-baseline evidence.
                for step in steps.iter_mut().take(2) {
                    step.tree["nodes"][0]["nodes"].as_array_mut().unwrap().pop();
                }
                for step in steps.iter_mut().take(5).skip(2) {
                    step.tree["nodes"][0]["nodes"][1]["nodes"][0]["app_id"] = json!("unrelated");
                }
                "ownership is uncertain"
            }
            "move-instead-of-close" => {
                steps[5].events[0].1["change"] = json!("move");
                "ownership is uncertain"
            }
            "changed-hierarchy" => {
                steps[6].tree = arranged_tree(
                    "splith",
                    vec![group(
                        100,
                        "splith",
                        vec![wayland_window(20, Some("expected"))],
                    )],
                    20,
                );
                "unexpected hierarchy"
            }
            "focus-away-and-back" => {
                steps[5]
                    .events
                    .push((WORKSPACE_EVENT, workspace_focus_event(11, 10)));
                steps[5]
                    .events
                    .push((WORKSPACE_EVENT, workspace_focus_event(10, 11)));
                "focus changed"
            }
            "workspace-migration" => {
                steps[5]
                    .events
                    .push((WORKSPACE_EVENT, workspace_move_event(10)));
                "moved to another output"
            }
            "changed-output" => {
                steps[6].tree["nodes"][0]["id"] = json!(3);
                "changed output"
            }
            "foreign-window-move" => {
                steps[5].events.push((
                    WINDOW_EVENT,
                    json!({"change": "move", "container": wayland_window(90, Some("expected"))}),
                ));
                "ownership is uncertain"
            }
            "preexisting-new" | "preexisting-floating" | "preexisting-fullscreen" => {
                // These preexisting-window events retain their existing launch-wait
                // behavior, but revoke the evidence needed for a later safe close.
                let change = match scenario {
                    "preexisting-new" => "new",
                    "preexisting-floating" => "floating",
                    "preexisting-fullscreen" => "fullscreen_mode",
                    _ => unreachable!(),
                };
                steps[2].events.push((
                    WINDOW_EVENT,
                    json!({"change": change, "container": wayland_window(90, Some("expected"))}),
                ));
                "ownership is uncertain"
            }
            _ => unreachable!(),
        };
        assert_load_fails(&fixture, steps, expected);
    }
}

#[test]
fn foreign_close_requires_the_following_quiet_snapshot_to_verify_too() {
    let fixture = LaunchFixture::new();
    let mut steps = foreign_close_command_steps();
    steps[7].tree = empty_workspace_tree();

    assert_load_fails(&fixture, steps, "disappeared");
}

#[test]
fn launch_wait_still_tolerates_preexisting_close_and_refuses_provisional_or_unknown_close() {
    for scenario in ["preexisting-close", "provisional-close", "unknown-close"] {
        let fixture = LaunchFixture::new();
        let mut steps = successful_steps();
        let id = match scenario {
            "preexisting-close" => {
                for step in steps.iter_mut().take(2) {
                    step.tree = with_window_on_other_workspace(
                        step.tree.clone(),
                        wayland_window(90, Some("expected")),
                    );
                }
                90
            }
            "provisional-close" => 20, // Already provisional after the first mapped snapshot.
            _ => 99,
        };
        let app_id = if id == 20 { "expected" } else { "unrelated" };
        steps[3].events.push((
            WINDOW_EVENT,
            json!({"change": "close", "container": wayland_window(id, Some(app_id))}),
        ));
        if scenario == "preexisting-close" {
            steps.push(snapshot_step(tree_with_target_windows(vec![
                wayland_window(20, Some("expected")),
            ])));
            fixture
                .run_with_finalization(&fixture.config(), steps)
                .unwrap();
        } else {
            assert_load_fails(&fixture, steps, "closed");
        }
    }
}

#[test]
fn launch_once_preserves_literal_arguments_child_cwd_environment_and_delegation() {
    let fixture = LaunchFixture::new();
    let original_path = env::var_os("PATH");
    let original_cwd = env::current_dir().unwrap();
    fixture
        .run_with_finalization(&fixture.config(), successful_steps())
        .unwrap();

    assert_eq!(fixture.wait_for_launch_count(), 1);
    let limit = ipc_deadline();
    let settings = loop {
        let text = fs::read_to_string(fixture.path.join("settings")).unwrap_or_default();
        if text.contains("; touch bad") {
            break text;
        }
        assert!(Instant::now() < limit);
        thread::yield_now();
    };
    assert!(
        settings.starts_with(&format!(
            "{}\nchild-value\n/fixture-child-path\n\n$HOME\n~\n*.txt\ntwo words\n; touch bad\n",
            fixture.path.display()
        )),
        "{settings:?}"
    );
    assert!(!fixture.path.join("bad").exists());
    assert_eq!(env::var_os("PATH"), original_path);
    assert_eq!(env::current_dir().unwrap(), original_cwd);
}

#[test]
fn effective_directory_uses_window_then_cli_then_layout_then_parent() {
    let parent_cwd = env::current_dir().unwrap();
    let parent_environment = env::vars_os().collect::<BTreeMap<_, _>>();
    for (default, cli_cwd, override_cwd, bare_executable) in [
        (None, None, None, false),
        (Some("absolute"), None, None, false),
        (Some("default"), None, None, false),
        (Some("default"), None, None, true),
        (Some("default"), None, Some("override"), false),
        (Some("default"), Some("cli"), None, false),
        (Some("default"), Some("cli"), None, true),
        (Some("default"), Some("cli"), Some("override"), false),
        (None, Some("cli"), None, false),
    ] {
        let fixture = LaunchFixture::new();
        for directory in ["default", "override", "cli"] {
            fs::create_dir(fixture.path.join(directory)).unwrap();
            fs::copy(
                fixture.path.join("app"),
                fixture.path.join(directory).join("app"),
            )
            .unwrap();
        }
        let expected_cwd = match override_cwd.or(cli_cwd).or(default) {
            Some("absolute") => fixture.path.clone(),
            Some(directory) => fixture.path.join(directory),
            None => parent_cwd.clone(),
        };
        let default_field = match default {
            Some("absolute") => format!("cwd: '{}'\n", fixture.path.display()),
            Some(directory) => format!("cwd: '{}'\n", fixture.path.join(directory).display()),
            None => String::new(),
        };
        let window_field = override_cwd
            .map(|directory| format!("cwd: '{}', ", fixture.path.join(directory).display()))
            .unwrap_or_default();
        let executable = if bare_executable {
            "app".to_owned()
        } else if default.is_some() || cli_cwd.is_some() {
            "./app".to_owned()
        } else {
            fixture.path.join("app").display().to_string()
        };
        // Record outside the child cwd so the omitted-directory case remains isolated.
        let app = expected_cwd.join("app");
        let app = if default.is_none() && cli_cwd.is_none() {
            fixture.path.join("app")
        } else {
            app
        };
        fs::write(
            &app,
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"$MARKER\" > \"$OUTPUT\"\n",
        )
        .unwrap();
        let output = fixture.path.join("result");
        let yaml = format!(
            "version: 1\n{default_field}window_timeout_ms: 500\nlayout: {{id: test, command: ['{executable}'], match: {{app_id: expected}}, {window_field}env: {{OUTPUT: '{}', MARKER: child-value, PATH: '{}'}}}}\n",
            output.display(),
            expected_cwd.display()
        );
        let mut config = crate::config::parse(&yaml).unwrap();
        let expected_default = default.map(|directory| {
            if directory == "absolute" {
                fixture.path.clone()
            } else {
                fixture.path.join(directory)
            }
        });
        assert_eq!(config.cwd, expected_default);
        if let Some(directory) = cli_cwd {
            let mut directory = fixture.path.join(directory);
            crate::config::resolve_directory(&mut directory).unwrap();
            config.cwd = Some(directory);
        }
        fixture
            .run_with_finalization(&config, successful_steps())
            .unwrap();
        let limit = ipc_deadline();
        let expected = format!("{}\nchild-value\n", expected_cwd.display());
        loop {
            if fs::read_to_string(&output).unwrap_or_default() == expected {
                break;
            }
            assert!(
                Instant::now() < limit,
                "child did not record expected directory"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(env::current_dir().unwrap(), parent_cwd);
        assert_eq!(
            env::vars_os().collect::<BTreeMap<_, _>>(),
            parent_environment
        );
    }
}

#[test]
fn menu_target_and_standalone_target_complete_with_unrelated_windows_on_another_output() {
    for (expected_workspace, target, move_unrelated) in [
        (Some(10), 10, false),
        (Some(10), 10, true),
        (None, 88, false),
        (None, 88, true),
    ] {
        let fixture = LaunchFixture::new();
        let config = fixture.config();
        let mut steps = successful_steps();
        let completed = steps.last().unwrap().tree.clone();
        append_final_focus_steps(&mut steps, &completed, 20);
        for step in &mut steps {
            step.tree["nodes"][0]["nodes"][0]["id"] = json!(target);
            step.tree = with_second_output(step.tree.clone());
            step.workspaces = two_output_workspace_reply();
            step.workspaces[0]["id"] = json!(target);
        }
        // A move involving only the unrelated workspace does not invalidate the run.
        if move_unrelated {
            let unrelated = steps[1].tree["nodes"][1]["nodes"][0].clone();
            for step in steps.iter_mut().skip(1) {
                step.tree["nodes"][1]["nodes"] = json!([]);
                step.tree["nodes"][0]["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .push(unrelated.clone());
                step.workspaces[1]["output"] = json!("fixture");
            }
            steps[1]
                .events
                .push((WORKSPACE_EVENT, workspace_move_event(11)));
        }
        fixture
            .serve_transcript(steps, |connect, diagnostics| {
                load_with(&config, expected_workspace, connect, diagnostics).map(|_| ())
            })
            .unwrap();
        assert_eq!(fixture.wait_for_launch_count(), 1);
    }
}

#[test]
fn mismatched_menu_target_refuses_before_any_launch_or_command() {
    let fixture = LaunchFixture::new();
    let config = fixture.config();
    let error = fixture
        .serve_transcript(
            vec![snapshot_step(with_second_output(empty_workspace_tree()))],
            |connect, diagnostics| load_with(&config, Some(88), connect, diagnostics).map(|_| ()),
        )
        .unwrap_err();

    assert!(
        error.contains("differs from pre-menu workspace id=88"),
        "{error}"
    );
    assert!(!fixture.path.join("launches").exists());
    let log = fs::read_to_string(fixture.path.join("last-run.log")).unwrap();
    assert!(!log.contains("launch leaf="), "{log}");
    assert!(!log.contains("command replies"), "{log}");
}

#[test]
fn workspace_migration_stops_before_launch_or_any_further_construction() {
    for during_construction in [false, true] {
        for with_event in [false, true] {
            let fixture = LaunchFixture::new();
            let config = layout_config(
                &fixture,
                &format!(
                    "{{group: splith, children: [{}, {}]}}",
                    configured_window_yaml(&fixture, 0),
                    configured_window_yaml(&fixture, 1)
                ),
            );
            let mut steps = if during_construction {
                successful_steps()
            } else {
                vec![snapshot_step(empty_workspace_tree())]
            };
            // Stop at the first fence before the split command, or the first launch.
            let last_tree = steps.last().unwrap().tree.clone();
            steps.push(snapshot_step(last_tree));
            for step in &mut steps {
                step.tree = with_second_output(step.tree.clone());
                step.workspaces = two_output_workspace_reply();
            }
            let last = steps.last_mut().unwrap();
            let expected_error = if with_event {
                // Even a move away and back with unchanged tree/focus must fail.
                last.events = vec![
                    (WORKSPACE_EVENT, workspace_move_event(10)),
                    (WORKSPACE_EVENT, workspace_move_event(10)),
                ];
                "moved to another output"
            } else {
                last.tree = move_target_to_second_output(last.tree.clone());
                last.workspaces[0]["output"] = json!("HDMI-A-1");
                "changed output"
            };
            let expected_connections = steps.len() + 1;
            let error = fixture
                .serve_transcript(steps, |connect, diagnostics| {
                    let mut connections = 0;
                    let result = load_with(
                        &config,
                        None,
                        &mut || {
                            connections += 1;
                            assert!(
                                connections <= expected_connections,
                                "IPC after migration failure"
                            );
                            connect()
                        },
                        diagnostics,
                    );
                    assert_eq!(connections, expected_connections);
                    result.map(|_| ())
                })
                .unwrap_err();
            assert!(error.contains(expected_error), "{error}");
            if during_construction {
                assert_eq!(fixture.wait_for_launch_count(), 1);
            } else {
                assert!(!fixture.path.join("launches").exists());
            }
            let log = fs::read_to_string(fixture.path.join("last-run.log")).unwrap();
            assert!(!log.contains("command replies"), "{log}");
            assert_eq!(
                log.matches("launching once").count(),
                usize::from(during_construction)
            );
        }
    }
}

#[test]
fn empty_wrappers_tiled_and_floating_occupancy_refuse_before_spawn() {
    for floating in [false, true] {
        let fixture = LaunchFixture::new();
        let mut occupied = empty_workspace_tree();
        let workspace = &mut occupied["nodes"][0]["nodes"][0];
        workspace["focused"] = json!(false);
        workspace[if floating { "floating_nodes" } else { "nodes" }] =
            json!([node(20, "con", true)]);
        let error = fixture
            .run(&fixture.config(), vec![snapshot_step(occupied)])
            .unwrap_err();
        assert!(error.contains("completely empty"), "{error}");
        assert!(!fixture.path.join("launches").exists());
    }
}

#[test]
fn delayed_identity_without_event_and_existing_matching_window_elsewhere() {
    let fixture = LaunchFixture::new();
    let preexisting_window = wayland_window(50, Some("expected"));
    let empty = with_window_on_other_workspace(empty_workspace_tree(), preexisting_window.clone());
    let unresolved = with_window_on_other_workspace(
        tree_with_target_windows(vec![wayland_window(20, None)]),
        preexisting_window.clone(),
    );
    let resolved = with_window_on_other_workspace(
        tree_with_target_windows(vec![wayland_window(20, Some("expected"))]),
        preexisting_window,
    );
    let steps = vec![
        snapshot_step(empty.clone()),
        snapshot_step(empty.clone()),
        // Map events precede the identity, which then arrives without an event.
        launched(&empty, &unresolved),
        snapshot_step(resolved.clone()),
        snapshot_step(resolved),
    ];
    fixture
        .run_with_finalization(&fixture.config(), steps)
        .unwrap();

    assert_eq!(fixture.wait_for_launch_count(), 1);
}

#[test]
fn wrong_workspace_scratchpad_ambiguity_unexpected_and_preexisting_entries_fail() {
    for scenario in [
        "wrong-workspace",
        "scratchpad",
        "ambiguous-match",
        "unexpected-window",
        "preexisting-window",
    ] {
        let fixture = LaunchFixture::new();
        let mut steps = vec![
            snapshot_step(empty_workspace_tree()),
            snapshot_step(empty_workspace_tree()),
        ];
        let (bad, expected) = match scenario {
            "wrong-workspace" => (
                with_window_on_other_workspace(
                    empty_workspace_tree(),
                    wayland_window(20, Some("expected")),
                ),
                "wrong workspace",
            ),
            "scratchpad" => {
                let mut bad = with_window_on_other_workspace(
                    empty_workspace_tree(),
                    wayland_window(20, Some("expected")),
                );
                bad["nodes"][0]["nodes"][1]["name"] = json!("__i3_scratch");
                (bad, "scratchpad")
            }
            "ambiguous-match" => {
                let mut other = wayland_window(21, Some("expected"));
                other["focused"] = json!(false);
                (
                    tree_with_target_windows(vec![wayland_window(20, Some("expected")), other]),
                    "ambiguous",
                )
            }
            "unexpected-window" => (
                tree_with_target_windows(vec![wayland_window(20, Some("unrelated"))]),
                "unexpected target-workspace",
            ),
            _ => {
                steps = vec![
                    snapshot_step(with_window_on_other_workspace(
                        empty_workspace_tree(),
                        wayland_window(20, Some("expected")),
                    )),
                    snapshot_step(with_window_on_other_workspace(
                        empty_workspace_tree(),
                        wayland_window(20, Some("expected")),
                    )),
                ];
                (
                    tree_with_target_windows(vec![wayland_window(20, Some("expected"))]),
                    "preexisting",
                )
            }
        };
        steps.push(snapshot_step(bad));
        assert_load_fails(&fixture, steps, expected);
    }
}

#[test]
fn floating_fullscreen_and_unknown_fullscreen_are_not_accepted() {
    for mode in 0..3 {
        let fixture = LaunchFixture::new();
        let mut bad = tree_with_target_windows(vec![wayland_window(20, Some("expected"))]);
        let window = &mut bad["nodes"][0]["nodes"][0]["nodes"][0];
        match mode {
            0 => window["floating"] = json!("user_on"),
            1 => window["fullscreen_mode"] = json!(1),
            _ => window["fullscreen_mode"] = Value::Null,
        }
        assert_load_fails(
            &fixture,
            vec![
                snapshot_step(empty_workspace_tree()),
                snapshot_step(empty_workspace_tree()),
                snapshot_step(bad),
            ],
            "floating/fullscreen",
        );
    }
}

#[test]
fn focus_switch_away_and_back_and_ipc_disconnect_abort_without_relaunch() {
    let fixture = LaunchFixture::new();
    let mut switched = snapshot_step(empty_workspace_tree());
    switched.events = vec![
        (WORKSPACE_EVENT, workspace_focus_event(11, 10)),
        (WORKSPACE_EVENT, workspace_focus_event(10, 11)),
    ];

    assert_load_fails(
        &fixture,
        vec![
            snapshot_step(empty_workspace_tree()),
            snapshot_step(empty_workspace_tree()),
            switched,
        ],
        "focus changed",
    );
    let fixture = LaunchFixture::new();
    let mut disconnected = snapshot_step(empty_workspace_tree());
    disconnected.disconnect = true;
    assert_load_fails(
        &fixture,
        vec![
            snapshot_step(empty_workspace_tree()),
            snapshot_step(empty_workspace_tree()),
            disconnected,
        ],
        "get workspaces",
    );
}

#[test]
fn stale_tree_cannot_hide_later_close_move_or_second_match() {
    for (change, expected) in [
        ("close", "closed"),
        ("move", "uncertain"),
        ("new", "ambiguous"),
    ] {
        let fixture = LaunchFixture::new();
        let mut final_step = successful_steps().pop().unwrap();
        let id = if change == "new" { 21 } else { 20 };
        final_step.events.push((
            WINDOW_EVENT,
            json!({"change": change, "container": wayland_window(id, Some("expected"))}),
        ));
        let mut steps = successful_steps();
        steps.pop();
        steps.push(final_step);
        assert_load_fails(&fixture, steps, expected);
    }
}

#[test]
fn event_newer_than_tree_requires_another_snapshot_and_final_window_must_remain() {
    let fixture = LaunchFixture::new();
    let mut steps = successful_steps();
    // Map events can share a fence with a tree queried just before the window mapped.
    let mut stale = snapshot_step(empty_workspace_tree());
    stale.events = std::mem::take(&mut steps[2].events);
    steps.insert(2, stale);
    fixture
        .run_with_finalization(&fixture.config(), steps)
        .unwrap();
    let fixture = LaunchFixture::new();
    let mut steps = successful_steps();
    *steps.last_mut().unwrap() = snapshot_step(empty_workspace_tree());

    assert_load_fails(&fixture, steps, "disappeared");
}

#[test]
fn timeout_distinguishes_no_new_windows_and_unresolved_identity() {
    for unresolved in [false, true] {
        let fixture = LaunchFixture::new();
        let mut config = fixture.config();
        config.window_timeout_ms = 160;
        let waiting = if unresolved {
            tree_with_target_windows(vec![wayland_window(20, None)])
        } else {
            empty_workspace_tree()
        };
        let error = fixture
            .run(
                &config,
                vec![
                    snapshot_step(empty_workspace_tree()),
                    snapshot_step(empty_workspace_tree()),
                    snapshot_step(waiting),
                ],
            )
            .unwrap_err();
        assert!(
            error.contains(if unresolved {
                "new windows observed"
            } else {
                "no new windows observed"
            }),
            "{error}"
        );
        assert_eq!(fixture.wait_for_launch_count(), 1);
    }
}

#[test]
fn launcher_exit_is_timeout_context_and_never_shortens_the_wait() {
    for (script_end, expected_context) in [
        ("exit 1", Some("Launch process exited with code 1.")),
        (
            "kill -TERM $$",
            Some("Launch process terminated by signal 15."),
        ),
        (
            "exit 0",
            Some("Launch process exited successfully (delegation is allowed)."),
        ),
        ("/bin/sleep 0.5", None),
    ] {
        let fixture = LaunchFixture::new();
        let app_path = fixture.path.join("app");
        let script = fs::read_to_string(&app_path).unwrap();
        fs::write(&app_path, format!("{script}{script_end}\n")).unwrap();
        let mut config = fixture.config();
        config.window_timeout_ms = 260;
        let started_at = Instant::now();
        let error = fixture
            .run(
                &config,
                vec![
                    snapshot_step(empty_workspace_tree()),
                    snapshot_step(empty_workspace_tree()),
                    snapshot_step(empty_workspace_tree()),
                ],
            )
            .unwrap_err();

        assert!(started_at.elapsed() >= Duration::from_millis(config.window_timeout_ms));
        assert!(error.starts_with("window timeout:"), "{error}");
        assert!(error.contains("no new windows observed"), "{error}");
        if let Some(context) = expected_context {
            assert!(error.ends_with(context), "{error}");
        } else {
            assert!(!error.contains("Launch process"), "{error}");
        }
        assert_eq!(fixture.wait_for_launch_count(), 1);
    }
}

#[test]
fn matching_window_is_accepted_after_nonzero_launcher_exit() {
    let fixture = LaunchFixture::new();
    let app_path = fixture.path.join("app");
    let script = fs::read_to_string(&app_path).unwrap();
    fs::write(&app_path, format!("{script}exit 1\n")).unwrap();
    let mut steps = successful_steps();
    // Empty snapshots force observation to continue before the recorded window appears.
    steps.insert(2, snapshot_step(empty_workspace_tree()));
    steps.insert(2, snapshot_step(empty_workspace_tree()));
    fixture
        .run_with_finalization(&fixture.config(), steps)
        .unwrap();

    let log = fs::read_to_string(fixture.path.join("last-run.log")).unwrap();
    let exit_record = log.find("launch process exited: exit status: 1").unwrap();
    let acceptance_record = log.find("accepted leaf=").unwrap();
    assert!(exit_record < acceptance_record, "{log}");
    assert_eq!(fixture.wait_for_launch_count(), 1);
}

#[test]
fn spawn_failure_does_not_retry_and_child_path_lookup_is_used() {
    let fixture = LaunchFixture::new();
    let mut config = fixture.config();
    let NodeKind::Window { command, .. } = &mut config.layout.value.kind else {
        unreachable!()
    };
    command[0] = "missing-fixture-executable".into();
    let error = fixture
        .run(
            &config,
            vec![
                snapshot_step(empty_workspace_tree()),
                snapshot_step(empty_workspace_tree()),
            ],
        )
        .unwrap_err();

    assert!(error.contains("spawn application"), "{error}");
    assert!(!fixture.path.join("launches").exists());

    let mut config = fixture.config();
    let NodeKind::Window { command, env, .. } = &mut config.layout.value.kind else {
        unreachable!()
    };
    command[0] = "app".into();
    env.insert("PATH".into(), fixture.path.to_str().unwrap().into());
    fixture
        .run_with_finalization(&config, successful_steps())
        .unwrap();
    assert_eq!(fixture.wait_for_launch_count(), 1);
}

#[test]
fn failure_leaves_application_running_and_never_relaunches() {
    let fixture = LaunchFixture::new();
    fs::write(fixture.path.join("app"), "#!/bin/sh\nprintf 'launch\\n' >> launches\n/bin/sleep 0.1\nprintf 'survived\\n' > survived\n").unwrap();

    assert_load_fails(
        &fixture,
        vec![
            snapshot_step(empty_workspace_tree()),
            snapshot_step(empty_workspace_tree()),
            snapshot_step(tree_with_target_windows(vec![wayland_window(
                20,
                Some("unrelated"),
            )])),
        ],
        "unexpected target-workspace",
    );
    let limit = ipc_deadline();
    while !fixture.path.join("survived").exists() {
        assert!(
            Instant::now() < limit,
            "application did not survive loader failure"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fixture.wait_for_launch_count(), 1);
}

#[test]
fn failure_reports_mention_partial_layouts_only_after_a_successful_spawn() {
    const PARTIAL: &str = "inspect windows before retrying";
    // Report as main does. Unit tests never reach a desktop notification service.
    fn report(fixture: &LaunchFixture, config: &Config, steps: Vec<IpcStep>) -> (String, String) {
        let error = fixture
            .serve_transcript(steps, |connect, diagnostics| {
                let result = load_with(config, None, connect, diagnostics).map(|_| ());
                result.map_err(|error| diagnostics.report_failure(error))
            })
            .unwrap_err();
        let log = fs::read_to_string(fixture.path.join("last-run.log")).unwrap();
        (error, log)
    }

    let fixture = LaunchFixture::new();
    let occupied = tree_with_target_windows(vec![wayland_window(50, Some("other"))]);
    let (error, log) = report(&fixture, &fixture.config(), vec![snapshot_step(occupied)]);

    assert!(error.contains("completely empty"), "{error}");
    assert!(
        !error.contains(PARTIAL) && !log.contains(PARTIAL),
        "{error}"
    );
    assert!(!fixture.path.join("launches").exists());

    // A spawn that fails to start the only application leaves nothing behind.
    let fixture = LaunchFixture::new();
    let mut config = fixture.config();
    let NodeKind::Window { command, .. } = &mut config.layout.value.kind else {
        unreachable!()
    };
    command[0] = "missing-fixture-executable".into();
    let (error, log) = report(
        &fixture,
        &config,
        vec![
            snapshot_step(empty_workspace_tree()),
            snapshot_step(empty_workspace_tree()),
        ],
    );
    assert!(error.contains("spawn application"), "{error}");
    assert!(
        !error.contains(PARTIAL) && !log.contains(PARTIAL),
        "{error}"
    );

    let fixture = LaunchFixture::new();
    let unexpected = tree_with_target_windows(vec![wayland_window(20, Some("unrelated"))]);
    let steps = vec![
        snapshot_step(empty_workspace_tree()),
        snapshot_step(empty_workspace_tree()),
        snapshot_step(unexpected),
    ];
    let (error, log) = report(&fixture, &fixture.config(), steps);
    assert!(error.contains("unexpected target-workspace"), "{error}");
    assert!(error.contains(PARTIAL) && log.contains(PARTIAL), "{error}");
    assert_eq!(fixture.wait_for_launch_count(), 1);

    // A later failed spawn still follows an earlier application that was started.
    let fixture = LaunchFixture::new();
    let missing = "{id: w1, command: [missing-fixture-executable], match: {app_id: expected}}";
    let config = layout_config(
        &fixture,
        &format!(
            "{{group: splith, children: [{}, {missing}]}}",
            configured_window_yaml(&fixture, 0)
        ),
    );
    let first = arranged_tree("splith", vec![expected_window(20)], 20);
    let mut steps = vec![snapshot_step(empty_workspace_tree())];
    append_launch_steps(&mut steps, &empty_workspace_tree(), &first);
    append_command_steps(&mut steps, "[con_id=20] splith", &first, &first);
    append_command_steps(&mut steps, "[con_id=20] focus", &first, &first);
    let (error, log) = report(&fixture, &config, steps);
    assert!(error.contains("leaf=\"w1\"") && error.contains("spawn application"));
    assert!(error.contains(PARTIAL) && log.contains(PARTIAL), "{error}");
    assert_eq!(fixture.wait_for_launch_count(), 1);
}

#[test]
fn exact_xwayland_class_matching_and_global_floating_ancestry() {
    let fixture = LaunchFixture::new();
    let mut config = fixture.config();
    let NodeKind::Window { matcher, .. } = &mut config.layout.value.kind else {
        unreachable!()
    };
    *matcher = Matcher::Class("ExactClass".into());
    let mut window = wayland_window(20, None);
    window["shell"] = json!("xwayland");
    window["window"] = json!(100);
    window["window_properties"] = json!({"class": "ExactClass"});
    let target = tree_with_target_windows(vec![window.clone()]);
    fixture
        .run_with_finalization(
            &config,
            vec![
                snapshot_step(empty_workspace_tree()),
                snapshot_step(empty_workspace_tree()),
                launched(&empty_workspace_tree(), &target),
                snapshot_step(target),
            ],
        )
        .unwrap();

    assert_eq!(fixture.wait_for_launch_count(), 1);

    let fixture = LaunchFixture::new();
    let mut target = empty_workspace_tree();
    target["nodes"][0]["nodes"][0]["focused"] = json!(false);
    let mut wrapper = node(30, "floating_con", false);
    wrapper["nodes"] = json!([wayland_window(20, Some("expected"))]);
    target["nodes"][0]["nodes"][0]["floating_nodes"] = json!([wrapper]);
    assert_load_fails(
        &fixture,
        vec![
            snapshot_step(empty_workspace_tree()),
            snapshot_step(empty_workspace_tree()),
            snapshot_step(target),
        ],
        "floating/fullscreen",
    );
}

#[test]
fn later_events_cannot_hide_identity_or_fullscreen_changes() {
    for change in ["title", "fullscreen_mode"] {
        let fixture = LaunchFixture::new();
        let mut last = successful_steps().pop().unwrap();
        let mut changed = wayland_window(20, Some("expected"));
        if change == "title" {
            changed["app_id"] = json!("other");
        } else {
            changed["fullscreen_mode"] = json!(1);
        }
        last.events.push((
            WINDOW_EVENT,
            json!({"change": change, "container": changed}),
        ));
        let mut steps = successful_steps();
        steps.pop();
        steps.push(last);
        assert_load_fails(
            &fixture,
            steps,
            if change == "title" {
                "changed identity"
            } else {
                "floating/fullscreen"
            },
        );
    }
}

#[test]
fn workspace_fullscreen_metadata_does_not_mark_tiled_child_fullscreen() {
    let fixture = LaunchFixture::new();
    let mut window = wayland_window(20, Some("expected"));
    window["floating"] = json!("auto_off");
    let mut mapped_snapshot = snapshot_step(tree_with_target_windows(vec![window.clone()]));

    assert_eq!(
        mapped_snapshot.tree["nodes"][0]["nodes"][0]["fullscreen_mode"],
        1
    );
    mapped_snapshot.events.push((
        WINDOW_EVENT,
        json!({"change": "new", "container": window.clone()}),
    ));
    fixture
        .run_with_finalization(
            &fixture.config(),
            vec![
                snapshot_step(empty_workspace_tree()),
                snapshot_step(empty_workspace_tree()),
                mapped_snapshot,
                snapshot_step(tree_with_target_windows(vec![window])),
            ],
        )
        .unwrap();
    assert_eq!(fixture.wait_for_launch_count(), 1);
}

// A transcript supplies recorded tree shapes and exact commands. It does not execute
// layout operations or model Sway's insertion, focus, or simplification algorithms.
// Only window events follow from the recorded trees; see focus_events and launched.
fn group(id: i64, layout: &str, children: Vec<Value>) -> Value {
    let mut value = node(id, "con", false);
    value["layout"] = json!(layout);
    value["nodes"] = json!(children);
    value
}

fn focus_only(value: &mut Value, id: i64) {
    value["focused"] = json!(value["id"] == id);
    for child in value["nodes"].as_array_mut().unwrap() {
        focus_only(child, id);
    }
}

fn arranged_tree(layout: &str, children: Vec<Value>, focused: i64) -> Value {
    let mut value = tree_with_target_windows(children);
    value["nodes"][0]["nodes"][0]["layout"] = json!(layout);
    focus_only(&mut value, focused);
    value
}

fn command_step(command: &str) -> IpcStep {
    let mut value = snapshot_step(empty_workspace_tree());
    value.command = Some((command.into(), json!([{"success": true}])));
    value
}

fn append_command_steps(steps: &mut Vec<IpcStep>, command: &str, before: &Value, after: &Value) {
    // Sway emits a command's events before its reply, so they fall in the next fence.
    // That tree cannot authorize anything further until a quiet fence follows.
    let mut command = command_step(command);
    command.events = focus_events(before, after);
    let resettle = !command.events.is_empty();
    steps.extend([
        snapshot_step(before.clone()),
        command,
        snapshot_step(after.clone()),
    ]);
    if resettle {
        steps.push(snapshot_step(after.clone()));
    }
}

fn append_final_focus_steps(steps: &mut Vec<IpcStep>, before: &Value, id: i64) {
    let mut after = before.clone();
    focus_only(&mut after, id);
    append_command_steps(steps, &format!("[con_id={id}] focus"), before, &after);
}

fn append_launch_steps(steps: &mut Vec<IpcStep>, before: &Value, after: &Value) {
    steps.extend([
        snapshot_step(before.clone()),
        launched(before, after),
        snapshot_step(after.clone()),
    ]);
}

fn layout_config(fixture: &LaunchFixture, layout: &str) -> Config {
    let path = fixture.path.join("nested.yaml");
    fs::write(
        &path,
        format!("version: 1\nwindow_timeout_ms: 1000\nlayout: {layout}\n"),
    )
    .unwrap();
    crate::config::read(&path).unwrap()
}

fn configured_window_yaml(fixture: &LaunchFixture, id: usize) -> String {
    format!(
        "{{id: w{id}, command: [./app, '{id}'], cwd: '{}', match: {{app_id: expected}}}}",
        fixture.path.display()
    )
}

fn expected_window(id: i64) -> Value {
    wayland_window(id, Some("expected"))
}

fn assert_completed_launches(
    fixture: &LaunchFixture,
    config: &Config,
    steps: Vec<IpcStep>,
    count: usize,
) {
    fixture.run_with_finalization(config, steps).unwrap();
    let limit = ipc_deadline();
    loop {
        let text = fs::read_to_string(fixture.path.join("launches")).unwrap_or_default();
        if text.lines().count() == count {
            break;
        }
        assert!(
            Instant::now() < limit,
            "expected {count} launches, got {text:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let log = fs::read_to_string(fixture.path.join("last-run.log")).unwrap();
    let mut previous = 0;
    for id in 0..count {
        let offset = log
            .find(&format!("accepted leaf=\\\"w{id}\\\" id={}", 20 + id))
            .unwrap();
        assert!(offset > previous);
        previous = offset;
    }
}

#[test]
fn horizontal_vertical_and_three_siblings_use_exact_ids_with_repeated_app_ids() {
    for (axis, layout, count) in [
        ("splith", "splith", 2),
        ("splitv", "splitv", 2),
        ("splith", "splith", 3),
    ] {
        let fixture = LaunchFixture::new();
        let leaves = (0..count)
            .map(|i| configured_window_yaml(&fixture, i))
            .collect::<Vec<_>>()
            .join(",");
        let yaml_layout = if count == 3 {
            let app_path = fixture.path.join("app");
            let script = fs::read_to_string(&app_path).unwrap().replace(
                "#!/bin/sh\n",
                "#!/bin/sh\nprintf '%s\\n' \"$PWD\" >> directories\n",
            );
            fs::write(app_path, script).unwrap();
            let explicit_cwd = format!("cwd: '{}', ", fixture.path.display());
            format!(
                "{{group: {axis}, children: [{}]}}\ncwd: '{}'",
                leaves.replace(&explicit_cwd, ""),
                fixture.path.display()
            )
        } else {
            format!("{{group: {axis}, children: [{leaves}]}}")
        };
        let config = layout_config(&fixture, &yaml_layout);
        let mut steps = vec![snapshot_step(empty_workspace_tree())];
        let first = arranged_tree("splith", vec![expected_window(20)], 20);
        append_launch_steps(&mut steps, &empty_workspace_tree(), &first);
        let mut current = arranged_tree(layout, vec![expected_window(20)], 20);
        append_command_steps(
            &mut steps,
            &format!("[con_id=20] {layout}"),
            &first,
            &current,
        );
        for index in 1..count {
            let previous = 19 + index as i64;
            append_command_steps(
                &mut steps,
                &format!("[con_id={previous}] focus"),
                &current.clone(),
                &current.clone(),
            );
            let next = arranged_tree(
                layout,
                (0..=index)
                    .map(|i| expected_window(20 + i as i64))
                    .collect(),
                20 + index as i64,
            );
            append_launch_steps(&mut steps, &current, &next);
            current = next;
        }
        assert_completed_launches(&fixture, &config, steps, count);
        if count == 3 {
            let directories = fs::read_to_string(fixture.path.join("directories")).unwrap();
            assert_eq!(
                directories,
                format!("{}\n", fixture.path.display()).repeat(count)
            );
        }
    }
}

#[test]
fn nested_right_group_and_three_meaningful_levels() {
    for deep in [false, true] {
        let fixture = LaunchFixture::new();
        let window_yaml = (0..4)
            .map(|i| configured_window_yaml(&fixture, i))
            .collect::<Vec<_>>();
        let right = if deep {
            format!(
                "{{group: splith, children: [{}, {}]}}",
                window_yaml[2], window_yaml[3]
            )
        } else {
            window_yaml[2].clone()
        };
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: splith, children: [{}, {{group: splitv, children: [{}, {right}]}}]}}",
                window_yaml[0], window_yaml[1]
            ),
        );
        let first_window_tree = arranged_tree("splith", vec![expected_window(20)], 20);
        let two_window_tree =
            arranged_tree("splith", vec![expected_window(20), expected_window(21)], 21);
        let right_column_tree = arranged_tree(
            "splith",
            vec![
                expected_window(20),
                group(100, "splitv", vec![expected_window(21)]),
            ],
            21,
        );
        let three_window_tree = arranged_tree(
            "splith",
            vec![
                expected_window(20),
                group(
                    100,
                    "splitv",
                    vec![expected_window(21), expected_window(22)],
                ),
            ],
            22,
        );
        let mut steps = vec![snapshot_step(empty_workspace_tree())];
        append_launch_steps(&mut steps, &empty_workspace_tree(), &first_window_tree);
        append_command_steps(
            &mut steps,
            "[con_id=20] splith",
            &first_window_tree,
            &first_window_tree,
        );
        append_command_steps(
            &mut steps,
            "[con_id=20] focus",
            &first_window_tree,
            &first_window_tree,
        );
        append_launch_steps(&mut steps, &first_window_tree, &two_window_tree);
        append_command_steps(
            &mut steps,
            "[con_id=21] splitv",
            &two_window_tree,
            &right_column_tree,
        );
        append_command_steps(
            &mut steps,
            "[con_id=21] focus",
            &right_column_tree,
            &right_column_tree,
        );
        append_launch_steps(&mut steps, &right_column_tree, &three_window_tree);
        if deep {
            let nested_row_tree = arranged_tree(
                "splith",
                vec![
                    expected_window(20),
                    group(
                        100,
                        "splitv",
                        vec![
                            expected_window(21),
                            group(101, "splith", vec![expected_window(22)]),
                        ],
                    ),
                ],
                22,
            );
            let four_window_tree = arranged_tree(
                "splith",
                vec![
                    expected_window(20),
                    group(
                        100,
                        "splitv",
                        vec![
                            expected_window(21),
                            group(
                                101,
                                "splith",
                                vec![expected_window(22), expected_window(23)],
                            ),
                        ],
                    ),
                ],
                23,
            );
            append_command_steps(
                &mut steps,
                "[con_id=22] splith",
                &three_window_tree,
                &nested_row_tree,
            );
            append_command_steps(
                &mut steps,
                "[con_id=22] focus",
                &nested_row_tree,
                &nested_row_tree,
            );
            append_launch_steps(&mut steps, &nested_row_tree, &four_window_tree);
        }
        assert_completed_launches(&fixture, &config, steps, if deep { 4 } else { 3 });
    }
}

#[test]
fn nested_first_child_wraps_populated_workspace_and_preserves_equal_axis_groups() {
    for equal_axis in [false, true] {
        let fixture = LaunchFixture::new();
        let window_yaml = (0..4)
            .map(|i| configured_window_yaml(&fixture, i))
            .collect::<Vec<_>>();
        let axis = if equal_axis { "splith" } else { "splitv" };
        let layout = if equal_axis { "splith" } else { "splitv" };
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: splith, children: [{{group: {axis}, children: [{}, {}]}}, {{group: splitv, children: [{}, {}]}}]}}",
                window_yaml[0], window_yaml[1], window_yaml[2], window_yaml[3]
            ),
        );
        let first_window_tree = arranged_tree("splith", vec![expected_window(20)], 20);
        let first_split_tree = arranged_tree(layout, vec![expected_window(20)], 20);
        let first_pair_tree =
            arranged_tree(layout, vec![expected_window(20), expected_window(21)], 21);
        let first_window_focused_tree =
            arranged_tree(layout, vec![expected_window(20), expected_window(21)], 20);
        let workspace_focused_tree =
            arranged_tree(layout, vec![expected_window(20), expected_window(21)], 10);
        let wrapped_pair_tree = arranged_tree(
            "splith",
            vec![group(
                100,
                layout,
                vec![expected_window(20), expected_window(21)],
            )],
            100,
        );
        let third_window_tree = arranged_tree(
            "splith",
            vec![
                group(100, layout, vec![expected_window(20), expected_window(21)]),
                expected_window(22),
            ],
            22,
        );
        let second_split_tree = arranged_tree(
            "splith",
            vec![
                group(100, layout, vec![expected_window(20), expected_window(21)]),
                group(101, "splitv", vec![expected_window(22)]),
            ],
            22,
        );
        let completed_tree = arranged_tree(
            "splith",
            vec![
                group(100, layout, vec![expected_window(20), expected_window(21)]),
                group(
                    101,
                    "splitv",
                    vec![expected_window(22), expected_window(23)],
                ),
            ],
            23,
        );
        let mut steps = vec![snapshot_step(empty_workspace_tree())];
        append_launch_steps(&mut steps, &empty_workspace_tree(), &first_window_tree);
        append_command_steps(
            &mut steps,
            &format!("[con_id=20] {layout}"),
            &first_window_tree,
            &first_split_tree,
        );
        append_command_steps(
            &mut steps,
            "[con_id=20] focus",
            &first_split_tree,
            &first_split_tree,
        );
        append_launch_steps(&mut steps, &first_split_tree, &first_pair_tree);
        append_command_steps(
            &mut steps,
            "[con_id=20] focus",
            &first_pair_tree,
            &first_window_focused_tree,
        );
        append_command_steps(
            &mut steps,
            "[con_id=20] focus parent",
            &first_window_focused_tree,
            &workspace_focused_tree,
        );
        append_command_steps(
            &mut steps,
            "splith",
            &workspace_focused_tree,
            &wrapped_pair_tree,
        );
        append_command_steps(
            &mut steps,
            "[con_id=100] focus",
            &wrapped_pair_tree,
            &wrapped_pair_tree,
        );
        append_launch_steps(&mut steps, &wrapped_pair_tree, &third_window_tree);
        append_command_steps(
            &mut steps,
            "[con_id=22] splitv",
            &third_window_tree,
            &second_split_tree,
        );
        append_command_steps(
            &mut steps,
            "[con_id=22] focus",
            &second_split_tree,
            &second_split_tree,
        );
        append_launch_steps(&mut steps, &second_split_tree, &completed_tree);
        assert_completed_launches(&fixture, &config, steps, 4);
    }
}

#[test]
fn construction_failures_stop_before_another_launch() {
    for scenario in [
        "empty-replies",
        "rejected-reply",
        "unchanged-hierarchy",
        "missing-window",
        "window-move",
        "focus-away-and-back",
        "unexpected-wrapper",
        "unexpected-window",
        "unsuccessful-reply",
        "disconnected-query",
    ] {
        let fixture = LaunchFixture::new();
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: splitv, children: [{}, {}]}}",
                configured_window_yaml(&fixture, 0),
                configured_window_yaml(&fixture, 1)
            ),
        );
        let before_split_tree = arranged_tree("splith", vec![expected_window(20)], 20);
        let after_split_tree = arranged_tree("splitv", vec![expected_window(20)], 20);
        let mut steps = vec![snapshot_step(empty_workspace_tree())];
        append_launch_steps(&mut steps, &empty_workspace_tree(), &before_split_tree);
        let mut command = command_step("[con_id=20] splitv");
        let mut before = snapshot_step(before_split_tree.clone());
        let mut after = snapshot_step(after_split_tree.clone());
        let expected = match scenario {
            "empty-replies" => {
                command.command.as_mut().unwrap().1 = json!([]);
                "command failed"
            }
            "rejected-reply" => {
                command.command.as_mut().unwrap().1 =
                    json!([{"success": true}, {"success": false, "error": "fixture rejection"}]);
                "fixture rejection"
            }
            "unchanged-hierarchy" => {
                after.tree = before_split_tree.clone();
                "unexpected hierarchy"
            }
            "missing-window" => {
                after.tree = empty_workspace_tree();
                "disappeared"
            }
            "window-move" => {
                command.events.push((
                    WINDOW_EVENT,
                    json!({"change": "move", "container": expected_window(20)}),
                ));
                "uncertain"
            }
            "focus-away-and-back" => {
                before
                    .events
                    .push((WORKSPACE_EVENT, workspace_focus_event(11, 10)));
                before
                    .events
                    .push((WORKSPACE_EVENT, workspace_focus_event(10, 11)));
                "focus changed"
            }
            "unexpected-wrapper" => {
                before.tree = arranged_tree(
                    "splith",
                    vec![group(90, "splith", vec![expected_window(20)])],
                    20,
                );
                "unexpected hierarchy"
            }
            "unexpected-window" => {
                after.tree =
                    arranged_tree("splitv", vec![expected_window(20), expected_window(99)], 20);
                "unexpected hierarchy"
            }
            "unsuccessful-reply" => {
                command.command.as_mut().unwrap().1 = json!([{"success": false}]);
                "success=false"
            }
            _ => {
                before.disconnect = true;
                "get workspaces"
            }
        };
        steps.extend([before, command, after]);
        let error = fixture.run(&config, steps).unwrap_err();
        assert!(error.contains(expected), "scenario={scenario}: {error}");
        assert_eq!(fixture.wait_for_launch_count(), 1);
    }
}

#[test]
fn accepted_windows_are_revalidated_during_the_next_window_wait() {
    for scenario in [
        "missing-window",
        "closed-window",
        "moved-window",
        "reordered-windows",
        "unexpected-wrapper",
    ] {
        let fixture = LaunchFixture::new();
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: splith, children: [{}, {}]}}",
                configured_window_yaml(&fixture, 0),
                configured_window_yaml(&fixture, 1)
            ),
        );
        let first_window_tree = arranged_tree("splith", vec![expected_window(20)], 20);
        let second_window_tree =
            arranged_tree("splith", vec![expected_window(20), expected_window(21)], 21);
        let mut steps = vec![snapshot_step(empty_workspace_tree())];
        append_launch_steps(&mut steps, &empty_workspace_tree(), &first_window_tree);
        append_command_steps(
            &mut steps,
            "[con_id=20] splith",
            &first_window_tree,
            &first_window_tree,
        );
        append_command_steps(
            &mut steps,
            "[con_id=20] focus",
            &first_window_tree,
            &first_window_tree,
        );
        steps.push(snapshot_step(first_window_tree));
        let mut bad = snapshot_step(second_window_tree);
        let expected = match scenario {
            "missing-window" => {
                bad.tree = arranged_tree("splith", vec![expected_window(21)], 21);
                "disappeared"
            }
            "closed-window" => {
                bad.events.push((
                    WINDOW_EVENT,
                    json!({"change": "close", "container": expected_window(20)}),
                ));
                "closed"
            }
            "moved-window" => {
                bad.events.push((
                    WINDOW_EVENT,
                    json!({"change": "move", "container": expected_window(20)}),
                ));
                "uncertain"
            }
            "reordered-windows" => {
                bad.tree =
                    arranged_tree("splith", vec![expected_window(21), expected_window(20)], 21);
                "unexpected hierarchy"
            }
            _ => {
                bad.tree = arranged_tree(
                    "splith",
                    vec![group(
                        90,
                        "splitv",
                        vec![expected_window(20), expected_window(21)],
                    )],
                    21,
                );
                "unexpected hierarchy"
            }
        };
        steps.push(bad);
        let error = fixture.run(&config, steps).unwrap_err();
        assert!(error.contains(expected), "scenario={scenario}: {error}");
        let limit = ipc_deadline();
        while fixture.wait_for_launch_count() != 2 {
            assert!(Instant::now() < limit);
            thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn command_deadline_and_truncated_reply_are_fatal_without_retry() {
    for truncated in [false, true] {
        let (client, mut server) = socket_pair();
        let (done, wait) = mpsc::channel();
        let worker = thread::spawn(move || {
            assert_eq!(read_request(&mut server, 0), b"[con_id=20] splitv");
            if truncated {
                use std::io::Write;
                server.write_all(b"i3-ipc").unwrap();
            } else {
                let _ = wait.recv_timeout(Duration::from_secs(1));
            }
        });
        let started = Instant::now();
        let error = run_command(
            client,
            "[con_id=20] splitv".into(),
            started + Duration::from_millis(40),
        )
        .unwrap_err();
        assert!(
            error.contains(if truncated { "command IPC" } else { "deadline" }),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        let _ = done.send(());
        worker.join().unwrap();
    }
}

#[test]
fn four_window_reference_transcript_uses_three_root_children() {
    let fixture = LaunchFixture::new();
    let path = fixture.path.join("reference.yaml");
    fs::write(&path, include_str!("../examples/four-windows.yaml")).unwrap();
    let mut config = crate::config::read(&path).unwrap();
    fn substitute(node: &mut LayoutNode, cwd: &PathBuf) {
        match &mut node.kind {
            NodeKind::Window {
                command,
                cwd: directory,
                ..
            } => {
                *command = vec!["./app".into()];
                *directory = Some(cwd.clone());
            }
            NodeKind::Group { children, .. } => {
                for child in children {
                    substitute(&mut child.value, cwd);
                }
            }
        }
    }
    substitute(&mut config.layout.value, &fixture.path);
    let views = [
        wayland_window(20, Some("firefox-esr")),
        wayland_window(21, Some("sway-stage-middle")),
        wayland_window(22, Some("sway-stage-top")),
        wayland_window(23, Some("sway-stage-bottom")),
    ];
    let browser_tree = arranged_tree("splith", views[..1].to_vec(), 20);
    let middle_window_tree = arranged_tree("splith", views[..2].to_vec(), 21);
    let top_window_tree = arranged_tree("splith", views[..3].to_vec(), 22);
    let right_column_tree = arranged_tree(
        "splith",
        vec![
            views[0].clone(),
            views[1].clone(),
            group(100, "splitv", views[2..3].to_vec()),
        ],
        22,
    );
    let mut completed_tree = arranged_tree(
        "splith",
        vec![
            views[0].clone(),
            views[1].clone(),
            group(100, "splitv", views[2..].to_vec()),
        ],
        23,
    );
    set_dimension(&mut completed_tree, 10, "width", 1220); // two 10px inner gaps
    set_dimension(&mut completed_tree, 10, "height", 1000);
    for id in [20, 21, 100] {
        set_dimension(&mut completed_tree, id, "width", 400);
        set_dimension(&mut completed_tree, id, "height", 1000);
    }
    for id in [22, 23] {
        set_dimension(&mut completed_tree, id, "width", 400);
        set_dimension(&mut completed_tree, id, "height", 500);
    }
    let mut steps = vec![snapshot_step(empty_workspace_tree())];
    append_launch_steps(&mut steps, &empty_workspace_tree(), &browser_tree);
    append_command_steps(
        &mut steps,
        "[con_id=20] splith",
        &browser_tree,
        &browser_tree,
    );
    append_command_steps(
        &mut steps,
        "[con_id=20] focus",
        &browser_tree,
        &browser_tree,
    );
    append_launch_steps(&mut steps, &browser_tree, &middle_window_tree);
    append_command_steps(
        &mut steps,
        "[con_id=21] focus",
        &middle_window_tree,
        &middle_window_tree,
    );
    append_launch_steps(&mut steps, &middle_window_tree, &top_window_tree);
    append_command_steps(
        &mut steps,
        "[con_id=22] splitv",
        &top_window_tree,
        &right_column_tree,
    );
    append_command_steps(
        &mut steps,
        "[con_id=22] focus",
        &right_column_tree,
        &right_column_tree,
    );
    append_launch_steps(&mut steps, &right_column_tree, &completed_tree);
    steps.push(snapshot_step(completed_tree.clone())); // root sizing inspection
    // As observed on Sway, a resize takes its whole delta from the following sibling.
    let mut first_sized = completed_tree.clone();
    set_dimension(&mut first_sized, 20, "width", 528);
    set_dimension(&mut first_sized, 21, "width", 272);
    append_command_steps(
        &mut steps,
        "[con_id=20] resize set width 528 px",
        &completed_tree,
        &first_sized,
    );
    let mut sized_root = first_sized.clone();
    set_dimension(&mut sized_root, 21, "width", 384);
    for id in [100, 22, 23] {
        set_dimension(&mut sized_root, id, "width", 288);
    }
    append_command_steps(
        &mut steps,
        "[con_id=21] resize set width 384 px",
        &first_sized,
        &sized_root,
    );
    steps.push(snapshot_step(sized_root.clone())); // nested sizing inspection
    let mut sized = sized_root.clone();
    set_dimension(&mut sized, 22, "height", 770);
    set_dimension(&mut sized, 23, "height", 230);
    append_command_steps(
        &mut steps,
        "[con_id=22] resize set height 770 px",
        &sized_root,
        &sized,
    );
    append_final_focus_steps(&mut steps, &sized, 20);
    fixture.run(&config, steps).unwrap();
    let log = fs::read_to_string(fixture.path.join("last-run.log")).unwrap();

    assert!(log.contains("weights=[44.0, 32.0, 24.0]"));
    assert!(log.contains("targets=[44.00%, 32.00%, 24.00%]"));
    // The first resize leaves the middle window short; resizing it fixes the column too.
    assert!(log.contains("observed=[44.00%, 22.67%, 33.33%]"));
    assert!(log.contains("observed=[44.00%, 32.00%, 24.00%]"));
    assert!(log.contains("targets=[77.00%, 23.00%]"));
    assert!(log.contains("final focus verified leaf=\\\"browser\\\" id=20"));
    let limit = ipc_deadline();
    while fixture.wait_for_launch_count() != 4 {
        assert!(Instant::now() < limit);
        thread::sleep(Duration::from_millis(5));
    }
}

// These workspace-layout transitions use a workspace-focused command. Targeting the
// leaf with layout would wrap the workspace's children in upstream Sway instead.
#[test]
fn initial_tabbed_and_stacked_workspaces_change_layout_without_extra_wrappers() {
    for initial_layout in ["tabbed", "stacked"] {
        let fixture = LaunchFixture::new();
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: splitv, children: [{}, {}]}}",
                configured_window_yaml(&fixture, 0),
                configured_window_yaml(&fixture, 1)
            ),
        );
        let initial = arranged_tree(initial_layout, vec![], 10);
        let first = arranged_tree(initial_layout, vec![expected_window(20)], 20);
        let oriented = arranged_tree("splitv", vec![expected_window(20)], 10);
        let complete = arranged_tree("splitv", vec![expected_window(20), expected_window(21)], 21);
        let mut steps = vec![snapshot_step(initial.clone())];
        append_launch_steps(&mut steps, &initial, &first);
        workspace_focus_steps(&mut steps, &first);
        let mut workspace_focused = first.clone();
        focus_only(&mut workspace_focused, 10);
        append_command_steps(&mut steps, "layout splitv", &workspace_focused, &oriented);
        append_final_focus_steps(&mut steps, &oriented, 20);
        let mut insertion = oriented.clone();
        focus_only(&mut insertion, 20);
        append_launch_steps(&mut steps, &insertion, &complete);
        assert_completed_launches(&fixture, &config, steps, 2);
    }
}

#[test]
fn malformed_command_frames_fail_closed() {
    use std::io::Write;
    for bytes in [
        b"wrong!\0\0\0\0\0\0\0\0".to_vec(),
        ipc_frame(4, b"[]"),
        ipc_frame(0, b"[{}]"),
        ipc_frame(0, b"not-json"),
        ipc_frame(0, b"[]")
            .into_iter()
            .take(10)
            .chain(1_u32.to_ne_bytes())
            .collect(),
    ] {
        let (client, mut server) = socket_pair();
        let worker = thread::spawn(move || {
            read_request(&mut server, 0);
            server.write_all(&bytes).unwrap();
        });
        assert!(run_command(client, "[con_id=20] splith".into(), ipc_deadline()).is_err());
        worker.join().unwrap();
    }
}

#[test]
fn insertion_focus_interference_is_detected_before_spawning() {
    let fixture = LaunchFixture::new();
    let config = layout_config(
        &fixture,
        &format!(
            "{{group: splith, children: [{}, {}, {}]}}",
            configured_window_yaml(&fixture, 0),
            configured_window_yaml(&fixture, 1),
            configured_window_yaml(&fixture, 2)
        ),
    );
    let first_window_tree = arranged_tree("splith", vec![expected_window(20)], 20);
    let second_window_tree =
        arranged_tree("splith", vec![expected_window(20), expected_window(21)], 21);
    let interfered = arranged_tree("splith", vec![expected_window(20), expected_window(21)], 20);
    let mut steps = vec![snapshot_step(empty_workspace_tree())];
    append_launch_steps(&mut steps, &empty_workspace_tree(), &first_window_tree);
    append_command_steps(
        &mut steps,
        "[con_id=20] splith",
        &first_window_tree,
        &first_window_tree,
    );
    append_command_steps(
        &mut steps,
        "[con_id=20] focus",
        &first_window_tree,
        &first_window_tree,
    );
    append_launch_steps(&mut steps, &first_window_tree, &second_window_tree);
    append_command_steps(
        &mut steps,
        "[con_id=21] focus",
        &second_window_tree,
        &second_window_tree,
    );
    // A click on the first window emits focus; the quiet fence after it is authoritative.
    let mut clicked = snapshot_step(interfered.clone());
    clicked.events = focus_events(&second_window_tree, &interfered);
    steps.extend([clicked, snapshot_step(interfered)]);
    let error = fixture.run(&config, steps).unwrap_err();

    assert!(error.contains("insertion focus left"), "{error}");
    let limit = ipc_deadline();
    while fixture.wait_for_launch_count() != 2 {
        assert!(Instant::now() < limit);
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn harmless_window_events_require_another_fence_before_continuing() {
    let fixture = LaunchFixture::new();
    let config = layout_config(
        &fixture,
        &format!(
            "{{group: splith, children: [{}, {}]}}",
            configured_window_yaml(&fixture, 0),
            configured_window_yaml(&fixture, 1)
        ),
    );
    let mut foreign = wayland_window(50, Some("editor"));
    foreign["focused"] = json!(false);
    let first_window_tree = arranged_tree("splith", vec![expected_window(20)], 20);
    let second_window_tree =
        arranged_tree("splith", vec![expected_window(20), expected_window(21)], 21);
    let mut steps = vec![snapshot_step(empty_workspace_tree())];
    append_launch_steps(&mut steps, &empty_workspace_tree(), &first_window_tree);
    // A title change on another workspace lands in the next command's first fence.
    let mut retitled = snapshot_step(first_window_tree.clone());
    retitled.events.push((
        WINDOW_EVENT,
        json!({"change": "title", "container": foreign}),
    ));
    steps.push(retitled);
    append_command_steps(
        &mut steps,
        "[con_id=20] splith",
        &first_window_tree,
        &first_window_tree,
    );
    append_command_steps(
        &mut steps,
        "[con_id=20] focus",
        &first_window_tree,
        &first_window_tree,
    );
    append_launch_steps(&mut steps, &first_window_tree, &second_window_tree);
    for entry in &mut steps {
        entry.tree = with_window_on_other_workspace(entry.tree.clone(), foreign.clone());
    }

    assert_completed_launches(&fixture, &config, steps, 2);

    // Each pass is also forced by the transcript: had a fence with events authorized
    // the next request, that command would meet a snapshot step and fail the fixture.
    let log = fs::read_to_string(fixture.path.join("last-run.log")).unwrap();
    let passes: Vec<_> = log
        .lines()
        .filter(|line| line.contains("observing again"))
        .collect();
    assert_eq!(passes.len(), 2, "{log}");
    assert!(passes[0].contains("Title id=50"), "{log}");
    // Final focus moves from the second window to the first; Sway reports that too.
    assert!(passes[1].contains("Focus id=20"), "{log}");
}

#[test]
fn workspace_split_requires_workspace_focus_on_its_settled_fence() {
    for clicked in [false, true] {
        let fixture = LaunchFixture::new();
        let window_yaml = (0..3)
            .map(|i| configured_window_yaml(&fixture, i))
            .collect::<Vec<_>>();
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: splith, children: [{{group: splitv, children: [{}, {}]}}, {}]}}",
                window_yaml[0], window_yaml[1], window_yaml[2]
            ),
        );
        let first_window_tree = arranged_tree("splith", vec![expected_window(20)], 20);
        let vertical_split_tree = arranged_tree("splitv", vec![expected_window(20)], 20);
        let second_window_tree =
            arranged_tree("splitv", vec![expected_window(20), expected_window(21)], 21);
        let first_window_focused_tree =
            arranged_tree("splitv", vec![expected_window(20), expected_window(21)], 20);
        let workspace_focused_tree =
            arranged_tree("splitv", vec![expected_window(20), expected_window(21)], 10);
        let mut steps = vec![snapshot_step(empty_workspace_tree())];
        append_launch_steps(&mut steps, &empty_workspace_tree(), &first_window_tree);
        append_command_steps(
            &mut steps,
            "[con_id=20] splitv",
            &first_window_tree,
            &vertical_split_tree,
        );
        append_command_steps(
            &mut steps,
            "[con_id=20] focus",
            &vertical_split_tree,
            &vertical_split_tree,
        );
        append_launch_steps(&mut steps, &vertical_split_tree, &second_window_tree);
        append_command_steps(
            &mut steps,
            "[con_id=20] focus",
            &second_window_tree,
            &first_window_focused_tree,
        );
        if clicked {
            // Focus parent succeeds, then a click on a window emits focus. Only the
            // fence after that event shows the workspace has lost focus.
            append_command_steps(
                &mut steps,
                "[con_id=20] focus parent",
                &first_window_focused_tree,
                &workspace_focused_tree,
            );
            let mut click = snapshot_step(workspace_focused_tree.clone());
            click.events = focus_events(&workspace_focused_tree, &second_window_tree);
            steps.extend([click, snapshot_step(second_window_tree)]);
        } else {
            // Sway accepts focus parent, but the workspace never becomes focused.
            append_command_steps(
                &mut steps,
                "[con_id=20] focus parent",
                &first_window_focused_tree,
                &first_window_focused_tree,
            );
            steps.push(snapshot_step(first_window_focused_tree));
        }
        // A split request would meet the repeated final snapshot step and fail the fixture.
        let error = fixture.run(&config, steps).unwrap_err();
        assert!(
            error.contains("\"splith\" requires focused container id=10"),
            "clicked={clicked}: {error}"
        );
        let limit = ipc_deadline();
        while fixture.wait_for_launch_count() != 2 {
            assert!(Instant::now() < limit);
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn set_dimension(tree: &mut Value, id: i64, axis: &str, pixels: i32) {
    if tree["id"] == id {
        tree["rect"][axis] = json!(pixels);
    }
    for child in tree["nodes"].as_array_mut().unwrap() {
        set_dimension(child, id, axis, pixels);
    }
}

// Start at a completed, owned hierarchy to exercise sizing/focus independently of
// insertion transcripts. All observation and command traffic still uses real sockets.
fn with_completed_session(
    fixture: &LaunchFixture,
    config: &Config,
    steps: Vec<IpcStep>,
    action: impl FnOnce(&mut Construction<'_>) -> Result<(), String>,
) -> Result<(), String> {
    fixture.serve_transcript(steps, |connect, diagnostics| {
        let mut observer = Observer::subscribe(connect()?, ipc_deadline())?;
        let initial = observer.snapshot(connect()?, ipc_deadline())?;
        let workspace = initial
            .tree
            .find_as_ref(|n| n.id == initial.target_workspace_id)
            .unwrap();
        fn owned<'a>(
            config: &'a LayoutNode,
            observed_node: &Node,
            accepted: &mut BTreeMap<i64, &'a Matcher>,
        ) -> ExpectedHierarchy {
            match &config.kind {
                NodeKind::Window { matcher, .. } => {
                    accepted.insert(observed_node.id, matcher);
                    ExpectedHierarchy::Window(observed_node.id)
                }
                NodeKind::Group { children, .. } => ExpectedHierarchy::Group {
                    id: Some(observed_node.id),
                    layout: observed_node.layout,
                    children: children
                        .iter()
                        .zip(&observed_node.nodes)
                        .map(|(child, observed_child)| {
                            owned(&child.value, observed_child, accepted)
                        })
                        .collect(),
                },
            }
        }
        let mut accepted = BTreeMap::new();
        let expected = owned(&config.layout.value, workspace, &mut accepted);
        let mut session = Construction {
            observer,
            connect,
            diagnostics,
            target_workspace_id: initial.target_workspace_id,
            preexisting_container_ids: HashSet::new(),
            baseline_foreign_window_ids: HashSet::new(),
            accepted_windows: accepted,
            expected_hierarchy: expected,
            default_cwd: None,
            window_timeout_ms: config.window_timeout_ms,
        };
        session.verify(&initial)?;
        action(&mut session)
    })
}

fn weighted_window_yaml(fixture: &LaunchFixture, id: usize, weight: f64) -> String {
    configured_window_yaml(fixture, id).replacen("{id:", &format!("{{weight: {weight}, id:"), 1)
}

fn tree_with_sibling_sizes(layout: &str, dimensions: &[i32]) -> Value {
    let mut tree = arranged_tree(
        layout,
        (0..dimensions.len())
            .map(|i| expected_window(20 + i as i64))
            .collect(),
        20,
    );
    for (index, size) in dimensions.iter().enumerate() {
        set_dimension(
            &mut tree,
            20 + index as i64,
            if layout == "splith" {
                "width"
            } else {
                "height"
            },
            *size,
        );
    }
    tree
}

#[test]
fn two_sibling_weights_use_dimensions_and_ignore_percent() {
    let fixture = LaunchFixture::new();
    let config = layout_config(
        &fixture,
        &format!(
            "{{group: splith, children: [{}, {}]}}",
            weighted_window_yaml(&fixture, 0, 3.0),
            weighted_window_yaml(&fixture, 1, 1.0)
        ),
    );
    let mut before = tree_with_sibling_sizes("splith", &[300, 700]);
    // The parent is only a viewport; gaps/borders and parent-area values do not
    // enter the denominator. Sway's content dimensions are not container sizes.
    before["nodes"][0]["nodes"][0]["rect"]["width"] = json!(1100);
    before["nodes"][0]["nodes"][0]["nodes"][0]["percent"] = json!(0.9);
    before["nodes"][0]["nodes"][0]["nodes"][1]["percent"] = json!(0.1);
    let mut after = before.clone();
    set_dimension(&mut after, 20, "width", 750);
    set_dimension(&mut after, 21, "width", 250);
    let mut steps = vec![snapshot_step(before.clone()), snapshot_step(before.clone())];
    append_command_steps(
        &mut steps,
        "[con_id=20] resize set width 750 px",
        &before,
        &after,
    );
    with_completed_session(&fixture, &config, steps, |session| {
        let mut warnings = Vec::new();
        session.apply_sizing(&config.layout.value, &[], &mut warnings)?;
        assert!(warnings.is_empty());
        Ok(())
    })
    .unwrap();
}

#[test]
fn three_siblings_reobserve_redistribution_and_converge_over_multiple_passes() {
    let fixture = LaunchFixture::new();
    let config = layout_config(
        &fixture,
        &format!(
            "{{group: splith, children: [{}, {}, {}]}}",
            weighted_window_yaml(&fixture, 0, 20.0),
            weighted_window_yaml(&fixture, 1, 30.0),
            weighted_window_yaml(&fixture, 2, 50.0)
        ),
    );
    // As observed on Sway, each resize takes its delta from the following sibling, so
    // resizing children in order reaches every target in one pass.
    let initial_tree = tree_with_sibling_sizes("splith", &[600, 200, 200]);
    let first_child_resized_tree = tree_with_sibling_sizes("splith", &[200, 600, 200]);
    let second_child_resized_tree = tree_with_sibling_sizes("splith", &[200, 300, 500]);
    let mut steps = vec![
        snapshot_step(initial_tree.clone()),
        snapshot_step(initial_tree.clone()),
    ];
    append_command_steps(
        &mut steps,
        "[con_id=20] resize set width 200 px",
        &initial_tree,
        &first_child_resized_tree,
    );
    append_command_steps(
        &mut steps,
        "[con_id=21] resize set width 300 px",
        &first_child_resized_tree,
        &second_child_resized_tree,
    );
    with_completed_session(&fixture, &config, steps, |session| {
        let mut warnings = Vec::new();
        session.apply_sizing(&config.layout.value, &[], &mut warnings)?;
        assert!(warnings.is_empty());
        Ok(())
    })
    .unwrap();

    // Recorded responses may make only partial progress (constraints/rounding).
    // Never assume an accepted resize achieved its requested pixel dimension.
    // Three passes end within the 1-point target: 20.5%, 30%, 49.5%.
    let initial_tree = tree_with_sibling_sizes("splith", &[600, 200, 200]);
    let first_child_resized_tree = tree_with_sibling_sizes("splith", &[400, 350, 250]);
    let second_child_resized_tree = tree_with_sibling_sizes("splith", &[400, 300, 300]);
    let first_pass_tree = tree_with_sibling_sizes("splith", &[350, 300, 350]);
    let second_pass_first_child_tree = tree_with_sibling_sizes("splith", &[250, 330, 420]);
    let second_pass_second_child_tree = tree_with_sibling_sizes("splith", &[250, 300, 450]);
    let second_pass_tree = tree_with_sibling_sizes("splith", &[215, 300, 485]);
    let third_pass_first_child_tree = tree_with_sibling_sizes("splith", &[205, 300, 495]);
    let mut steps = vec![
        snapshot_step(initial_tree.clone()),
        snapshot_step(initial_tree.clone()),
    ];
    append_command_steps(
        &mut steps,
        "[con_id=20] resize set width 200 px",
        &initial_tree,
        &first_child_resized_tree,
    );
    append_command_steps(
        &mut steps,
        "[con_id=21] resize set width 300 px",
        &first_child_resized_tree,
        &second_child_resized_tree,
    );
    append_command_steps(
        &mut steps,
        "[con_id=22] resize set width 500 px",
        &second_child_resized_tree,
        &first_pass_tree,
    );
    append_command_steps(
        &mut steps,
        "[con_id=20] resize set width 200 px",
        &first_pass_tree,
        &second_pass_first_child_tree,
    );
    append_command_steps(
        &mut steps,
        "[con_id=21] resize set width 300 px",
        &second_pass_first_child_tree,
        &second_pass_second_child_tree,
    );
    append_command_steps(
        &mut steps,
        "[con_id=22] resize set width 500 px",
        &second_pass_second_child_tree,
        &second_pass_tree,
    );
    append_command_steps(
        &mut steps,
        "[con_id=20] resize set width 200 px",
        &second_pass_tree,
        &third_pass_first_child_tree,
    );
    with_completed_session(&fixture, &config, steps, |session| {
        let mut warnings = Vec::new();
        session.apply_sizing(&config.layout.value, &[], &mut warnings)?;
        assert!(warnings.is_empty());
        Ok(())
    })
    .unwrap();
}

#[test]
fn minimum_constraints_stagnation_and_three_pass_limit_warn_without_failing() {
    for stagnant in [true, false] {
        let fixture = LaunchFixture::new();
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: splith, children: [{}, {}]}}",
                weighted_window_yaml(&fixture, 0, 90.0),
                weighted_window_yaml(&fixture, 1, 10.0)
            ),
        );
        let mut current = tree_with_sibling_sizes("splith", &[500, 500]);
        let mut steps = vec![
            snapshot_step(current.clone()),
            snapshot_step(current.clone()),
        ];
        for pass in 0..if stagnant { 1 } else { 3 } {
            for (id, target) in [(20, 900), (21, 100)] {
                let next = if stagnant {
                    current.clone()
                } else {
                    let amount = 510 + pass * 20 + if id == 21 { 10 } else { 0 };
                    tree_with_sibling_sizes("splith", &[amount, 1000 - amount])
                };
                append_command_steps(
                    &mut steps,
                    &format!("[con_id={id}] resize set width {target} px"),
                    &current,
                    &next,
                );
                current = next;
            }
        }
        with_completed_session(&fixture, &config, steps, |session| {
            let mut warnings = Vec::new();
            session.apply_sizing(&config.layout.value, &[], &mut warnings)?;
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains("group=10"));
            assert!(warnings[0].contains("targets=[90.00%, 10.00%]"));
            assert!(warnings[0].contains(if stagnant {
                "observed=[50.00%, 50.00%]"
            } else {
                "observed=[56.00%, 44.00%]"
            }));
            Ok(())
        })
        .unwrap();
    }
}

#[test]
fn already_close_and_uninterpretable_geometry_never_issue_resize_commands() {
    for (dimensions, warning) in [
        ([750, 250], false),
        // Half a percentage point is inside the 1-point sizing target.
        ([745, 255], false),
        ([0, 1000], true),
        ([-1, 1000], true),
    ] {
        let fixture = LaunchFixture::new();
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: splith, children: [{}, {}]}}",
                weighted_window_yaml(&fixture, 0, 3.0),
                weighted_window_yaml(&fixture, 1, 1.0)
            ),
        );
        let tree = tree_with_sibling_sizes("splith", &dimensions);
        with_completed_session(
            &fixture,
            &config,
            vec![snapshot_step(tree.clone()), snapshot_step(tree)],
            |session| {
                let mut warnings = Vec::new();
                session.apply_sizing(&config.layout.value, &[], &mut warnings)?;
                assert_eq!(!warnings.is_empty(), warning);
                if warning {
                    assert!(warnings[0].contains("observed=unavailable"));
                }
                Ok(())
            },
        )
        .unwrap();
    }
}

#[test]
fn residuals_between_the_target_and_tolerance_are_adjusted_without_a_warning() {
    let fixture = LaunchFixture::new();
    let config = layout_config(
        &fixture,
        &format!(
            "{{group: splith, children: [{}, {}]}}",
            weighted_window_yaml(&fixture, 0, 3.0),
            weighted_window_yaml(&fixture, 1, 1.0)
        ),
    );
    // Three points off: within the warning tolerance, so formerly left alone, but beyond
    // the target. Minimum sizes then stall both resizes; the stall ends sizing quietly.
    let stalled = tree_with_sibling_sizes("splith", &[720, 280]);
    let mut steps = vec![
        snapshot_step(stalled.clone()),
        snapshot_step(stalled.clone()),
    ];
    append_command_steps(
        &mut steps,
        "[con_id=20] resize set width 750 px",
        &stalled,
        &stalled,
    );
    append_command_steps(
        &mut steps,
        "[con_id=21] resize set width 250 px",
        &stalled,
        &stalled,
    );
    with_completed_session(&fixture, &config, steps, |session| {
        let mut warnings = Vec::new();
        session.apply_sizing(&config.layout.value, &[], &mut warnings)?;
        assert!(warnings.is_empty(), "{warnings:?}");
        Ok(())
    })
    .unwrap();
    let log = fs::read_to_string(fixture.path.join("last-run.log")).unwrap();

    assert!(
        log.contains("stopping sizing: no meaningful progress"),
        "{log}"
    );
}

#[test]
fn nested_groups_are_sized_outer_first_including_equal_axes() {
    for equal_axis in [false, true] {
        let fixture = LaunchFixture::new();
        let axis = if equal_axis { "splith" } else { "splitv" };
        let layout = if equal_axis { "splith" } else { "splitv" };
        let dimension = if equal_axis { "width" } else { "height" };
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: splith, children: [{{weight: 3, group: {axis}, children: [{}, {}]}}, {{group: splitv, children: [{}, {}]}}]}}",
                weighted_window_yaml(&fixture, 0, 3.0),
                weighted_window_yaml(&fixture, 1, 1.0),
                weighted_window_yaml(&fixture, 2, 1.0),
                weighted_window_yaml(&fixture, 3, 3.0)
            ),
        );
        let mut initial_tree = arranged_tree(
            "splith",
            vec![
                group(100, layout, vec![expected_window(20), expected_window(21)]),
                group(
                    101,
                    "splitv",
                    vec![expected_window(22), expected_window(23)],
                ),
            ],
            23,
        );
        set_dimension(&mut initial_tree, 10, "width", 1000);
        set_dimension(&mut initial_tree, 10, "height", 800);
        for id in [100, 101] {
            set_dimension(&mut initial_tree, id, "width", 500);
            set_dimension(&mut initial_tree, id, "height", 800);
        }
        for id in [20, 21, 22, 23] {
            set_dimension(
                &mut initial_tree,
                id,
                "width",
                if equal_axis && id < 22 { 250 } else { 500 },
            );
            set_dimension(
                &mut initial_tree,
                id,
                "height",
                if equal_axis && id < 22 { 800 } else { 400 },
            );
        }
        let mut root_sized_tree = initial_tree.clone();
        set_dimension(&mut root_sized_tree, 100, "width", 750);
        set_dimension(&mut root_sized_tree, 101, "width", 250);
        for id in [20, 21] {
            set_dimension(
                &mut root_sized_tree,
                id,
                "width",
                if equal_axis { 375 } else { 750 },
            );
        }
        for id in [22, 23] {
            set_dimension(&mut root_sized_tree, id, "width", 250);
        }
        let mut first_group_sized_tree = root_sized_tree.clone();
        set_dimension(
            &mut first_group_sized_tree,
            20,
            dimension,
            if equal_axis { 563 } else { 600 },
        );
        set_dimension(
            &mut first_group_sized_tree,
            21,
            dimension,
            if equal_axis { 187 } else { 200 },
        );
        let mut both_groups_sized_tree = first_group_sized_tree.clone();
        set_dimension(&mut both_groups_sized_tree, 22, "height", 200);
        set_dimension(&mut both_groups_sized_tree, 23, "height", 600);
        let mut steps = vec![
            snapshot_step(initial_tree.clone()),
            snapshot_step(initial_tree.clone()),
        ];
        append_command_steps(
            &mut steps,
            "[con_id=100] resize set width 750 px",
            &initial_tree,
            &root_sized_tree,
        );
        steps.push(snapshot_step(root_sized_tree.clone()));
        append_command_steps(
            &mut steps,
            &format!(
                "[con_id=20] resize set {dimension} {} px",
                if equal_axis { 563 } else { 600 }
            ),
            &root_sized_tree,
            &first_group_sized_tree,
        );
        steps.push(snapshot_step(first_group_sized_tree.clone()));
        append_command_steps(
            &mut steps,
            "[con_id=22] resize set height 200 px",
            &first_group_sized_tree,
            &both_groups_sized_tree,
        );
        with_completed_session(&fixture, &config, steps, |session| {
            let mut warnings = Vec::new();
            session.apply_sizing(&config.layout.value, &[], &mut warnings)?;
            assert!(warnings.is_empty());
            Ok(())
        })
        .unwrap();
    }
}

#[test]
fn sizing_interference_and_ipc_errors_are_fatal_even_when_sizes_are_nonfatal() {
    for scenario in [
        "rejected-resize",
        "focus-away-and-back",
        "missing-window",
        "unexpected-nested-window",
        "reordered-windows",
        "moved-window",
        "new-window-event",
        "unexpected-window-before-resize",
        "disconnected-query",
    ] {
        let fixture = LaunchFixture::new();
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: splith, children: [{}, {}]}}",
                weighted_window_yaml(&fixture, 0, 3.0),
                weighted_window_yaml(&fixture, 1, 1.0)
            ),
        );
        let before_resize_tree = tree_with_sibling_sizes("splith", &[500, 500]);
        let after_resize_tree = tree_with_sibling_sizes("splith", &[750, 250]);
        let mut steps = vec![
            snapshot_step(before_resize_tree.clone()),
            snapshot_step(before_resize_tree.clone()),
            snapshot_step(before_resize_tree.clone()),
            command_step("[con_id=20] resize set width 750 px"),
            snapshot_step(after_resize_tree.clone()),
        ];
        let verification_step_index = steps.len() - 1;
        let expected = match scenario {
            "rejected-resize" => {
                steps[verification_step_index - 1]
                    .command
                    .as_mut()
                    .unwrap()
                    .1 = json!([{"success": false, "error": "resize rejection"}]);
                "resize rejection"
            }
            "focus-away-and-back" => {
                steps[verification_step_index].events = vec![
                    (WORKSPACE_EVENT, workspace_focus_event(11, 10)),
                    (WORKSPACE_EVENT, workspace_focus_event(10, 11)),
                ];
                "focus changed"
            }
            "missing-window" => {
                steps[verification_step_index].tree =
                    arranged_tree("splith", vec![expected_window(20)], 20);
                "disappeared"
            }
            "unexpected-nested-window" => {
                steps[verification_step_index].tree = arranged_tree(
                    "splith",
                    vec![
                        expected_window(20),
                        group(
                            100,
                            "splitv",
                            vec![expected_window(21), expected_window(99)],
                        ),
                    ],
                    20,
                );
                "unexpected hierarchy"
            }
            "reordered-windows" => {
                steps[verification_step_index].tree =
                    arranged_tree("splith", vec![expected_window(21), expected_window(20)], 20);
                "unexpected hierarchy"
            }
            "moved-window" => {
                steps[verification_step_index].events.push((
                    WINDOW_EVENT,
                    json!({"change": "move", "container": expected_window(20)}),
                ));
                "uncertain"
            }
            "new-window-event" => {
                steps[verification_step_index].events.push((
                    WINDOW_EVENT,
                    json!({"change": "new", "container": expected_window(99)}),
                ));
                "ownership is uncertain"
            }
            "unexpected-window-before-resize" => {
                steps[2].tree = arranged_tree(
                    "splith",
                    vec![
                        expected_window(20),
                        expected_window(21),
                        expected_window(99),
                    ],
                    20,
                );
                "unexpected hierarchy"
            }
            _ => {
                steps[verification_step_index].disconnect = true;
                "get workspaces"
            }
        };
        let error = with_completed_session(&fixture, &config, steps, |session| {
            session.apply_sizing(&config.layout.value, &[], &mut Vec::new())
        })
        .unwrap_err();
        assert!(error.contains(expected), "scenario={scenario}: {error}");
    }
}

#[test]
fn resettled_fences_still_detect_unsafe_activity_and_lost_observation() {
    for scenario in [
        "new-window",
        "moved-window",
        "focus-away-and-back",
        "disconnected-query",
    ] {
        let fixture = LaunchFixture::new();
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: splith, children: [{}, {}]}}",
                configured_window_yaml(&fixture, 0),
                configured_window_yaml(&fixture, 1)
            ),
        );
        let loaded_tree =
            arranged_tree("splith", vec![expected_window(20), expected_window(21)], 21);
        let mut retitled = snapshot_step(loaded_tree.clone());
        retitled.events.push((
            WINDOW_EVENT,
            json!({"change": "title", "container": expected_window(21)}),
        ));
        let mut next = snapshot_step(loaded_tree.clone());
        let expected = match scenario {
            "new-window" => {
                next.events.push((
                    WINDOW_EVENT,
                    json!({"change": "new", "container": expected_window(99)}),
                ));
                "ownership is uncertain"
            }
            "moved-window" => {
                next.events.push((
                    WINDOW_EVENT,
                    json!({"change": "move", "container": expected_window(20)}),
                ));
                "window move"
            }
            "focus-away-and-back" => {
                next.events = vec![
                    (WORKSPACE_EVENT, workspace_focus_event(11, 10)),
                    (WORKSPACE_EVENT, workspace_focus_event(10, 11)),
                ];
                "focus changed"
            }
            _ => {
                next.disconnect = true;
                "get workspaces"
            }
        };
        let steps = vec![snapshot_step(loaded_tree.clone()), retitled, next];
        let error = with_completed_session(&fixture, &config, steps, |session| {
            session.focus_container(&[0])
        })
        .unwrap_err();
        assert!(error.contains(expected), "scenario={scenario}: {error}");
        let log = fs::read_to_string(fixture.path.join("last-run.log")).unwrap();
        assert!(log.contains("Title id=21; observing again"), "{log}");
        assert!(!log.contains("command replies"), "{log}");
    }
}

#[test]
fn final_focus_resolves_explicit_and_depth_first_leaf_and_verifies_result() {
    for (outer, inner) in [
        ("splith", "splitv"),
        ("tabbed", "stacked"),
        ("stacked", "tabbed"),
    ] {
        for explicit in [false, true] {
            let fixture = LaunchFixture::new();
            let mut config = layout_config(
                &fixture,
                &format!(
                    "{{group: {outer}, children: [{{group: {inner}, children: [{}, {}]}}, {}]}}",
                    configured_window_yaml(&fixture, 0),
                    configured_window_yaml(&fixture, 1),
                    configured_window_yaml(&fixture, 2)
                ),
            );
            if explicit {
                config.focus = Some("w1".into());
            }
            let loaded_tree = arranged_tree(
                outer,
                vec![
                    group(100, inner, vec![expected_window(20), expected_window(21)]),
                    expected_window(22),
                ],
                22,
            );
            let id = if explicit { 21 } else { 20 };
            let mut steps = vec![snapshot_step(loaded_tree.clone())];
            append_final_focus_steps(&mut steps, &loaded_tree, id);
            with_completed_session(&fixture, &config, steps, |session| {
                let (configured_window_id, configured_window_path) =
                    find_focus_leaf(&config.layout.value, config.focus.as_deref(), &[]).unwrap();
                assert_eq!(configured_window_id, if explicit { "w1" } else { "w0" });
                assert_eq!(configured_window_path, [0, if explicit { 1 } else { 0 }]);
                session.focus_container(&configured_window_path)
            })
            .unwrap();
        }
    }
}

#[test]
fn final_focus_rejects_missing_foreign_relocated_and_incorrect_focus() {
    for scenario in [
        "missing-window",
        "relocated-window",
        "unexpected-window",
        "unobserved-focus",
        "focus-away-and-back",
    ] {
        let fixture = LaunchFixture::new();
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: splith, children: [{}, {}]}}",
                configured_window_yaml(&fixture, 0),
                configured_window_yaml(&fixture, 1)
            ),
        );
        let loaded_tree =
            arranged_tree("splith", vec![expected_window(20), expected_window(21)], 21);
        let mut before = snapshot_step(loaded_tree.clone());
        let mut after = loaded_tree.clone();
        focus_only(&mut after, 20);
        let mut after = snapshot_step(after);
        let expected = match scenario {
            "missing-window" => {
                before.tree = arranged_tree("splith", vec![expected_window(21)], 21);
                "disappeared"
            }
            "relocated-window" => {
                before.tree = with_window_on_other_workspace(
                    arranged_tree("splith", vec![expected_window(21)], 21),
                    expected_window(20),
                );
                "lost placement"
            }
            "unexpected-window" => {
                before.tree = arranged_tree(
                    "splith",
                    vec![
                        expected_window(20),
                        expected_window(21),
                        expected_window(99),
                    ],
                    21,
                );
                "unexpected hierarchy"
            }
            "unobserved-focus" => {
                after.tree = loaded_tree.clone();
                "was not observed"
            }
            _ => {
                after.events = vec![
                    (WORKSPACE_EVENT, workspace_focus_event(11, 10)),
                    (WORKSPACE_EVENT, workspace_focus_event(10, 11)),
                ];
                "focus changed"
            }
        };
        let steps = vec![
            snapshot_step(loaded_tree),
            before,
            command_step("[con_id=20] focus"),
            after,
        ];
        let error = with_completed_session(&fixture, &config, steps, |session| {
            session.focus_container(&[0])
        })
        .unwrap_err();
        assert!(error.contains(expected), "scenario={scenario}: {error}");
    }
}

fn workspace_focus_steps(steps: &mut Vec<IpcStep>, before: &Value) {
    let child_id = before["nodes"][0]["nodes"][0]["nodes"][0]["id"]
        .as_i64()
        .unwrap();
    let mut child_focused = before.clone();
    focus_only(&mut child_focused, child_id);
    append_command_steps(
        steps,
        &format!("[con_id={child_id}] focus"),
        before,
        &child_focused,
    );
    let mut workspace_focused = before.clone();
    focus_only(&mut workspace_focused, 10);
    append_command_steps(
        steps,
        &format!("[con_id={child_id}] focus parent"),
        &child_focused,
        &workspace_focused,
    );
}

#[test]
fn root_tabs_and_stacks_preserve_singletons_order_and_initial_workspace_layout() {
    for (layout, command, initial_layout, count) in [
        ("tabbed", "tabbed", "splith", 1),
        ("stacked", "stacking", "splith", 1),
        ("tabbed", "tabbed", "splitv", 3),
        ("stacked", "stacking", "splith", 2),
        ("tabbed", "tabbed", "tabbed", 2),
        ("stacked", "stacking", "stacked", 1),
    ] {
        let fixture = LaunchFixture::new();
        let leaves = (0..count)
            .map(|i| configured_window_yaml(&fixture, i))
            .collect::<Vec<_>>()
            .join(",");
        let config = layout_config(
            &fixture,
            &format!("{{group: {layout}, children: [{leaves}]}}"),
        );
        let initial = arranged_tree(initial_layout, vec![], 10);
        let first = arranged_tree(initial_layout, vec![expected_window(20)], 20);
        let mut steps = vec![snapshot_step(initial.clone())];
        append_launch_steps(&mut steps, &initial, &first);
        workspace_focus_steps(&mut steps, &first);
        let mut before = first.clone();
        focus_only(&mut before, 10);
        let mut current = arranged_tree(layout, vec![expected_window(20)], 10);
        append_command_steps(&mut steps, &format!("layout {command}"), &before, &current);
        for child_index in 1..count {
            let previous_id = 19 + child_index as i64;
            let mut insertion = current.clone();
            focus_only(&mut insertion, previous_id);
            append_command_steps(
                &mut steps,
                &format!("[con_id={previous_id}] focus"),
                &current,
                &insertion,
            );
            let next = arranged_tree(
                layout,
                (0..=child_index)
                    .map(|i| expected_window(20 + i as i64))
                    .collect(),
                20 + child_index as i64,
            );
            append_launch_steps(&mut steps, &insertion, &next);
            current = next;
        }
        assert_completed_launches(&fixture, &config, steps, count);
    }
}

#[test]
fn nested_tabs_and_stacks_between_split_siblings_preserve_insertion_level() {
    for (parent_layout, layout, command, count) in [
        ("splith", "tabbed", "tabbed", 2),
        ("splitv", "stacked", "stacking", 2),
        ("splitv", "tabbed", "tabbed", 1),
        ("splith", "stacked", "stacking", 1),
    ] {
        let fixture = LaunchFixture::new();
        let nested = (1..=count)
            .map(|i| configured_window_yaml(&fixture, i))
            .collect::<Vec<_>>()
            .join(",");
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: {parent_layout}, children: [{}, {{group: {layout}, children: [{nested}]}}, {}]}}",
                configured_window_yaml(&fixture, 0),
                configured_window_yaml(&fixture, count + 1)
            ),
        );
        let first = arranged_tree("splith", vec![expected_window(20)], 20);
        let oriented = arranged_tree(parent_layout, vec![expected_window(20)], 20);
        let siblings = arranged_tree(
            parent_layout,
            vec![expected_window(20), expected_window(21)],
            21,
        );
        let staged = arranged_tree(
            parent_layout,
            vec![
                expected_window(20),
                group(100, "splith", vec![expected_window(21)]),
            ],
            21,
        );
        let mut current = arranged_tree(
            parent_layout,
            vec![
                expected_window(20),
                group(100, layout, vec![expected_window(21)]),
            ],
            21,
        );
        let mut steps = vec![snapshot_step(empty_workspace_tree())];
        append_launch_steps(&mut steps, &empty_workspace_tree(), &first);
        append_command_steps(
            &mut steps,
            &format!("[con_id=20] {parent_layout}"),
            &first,
            &oriented,
        );
        append_command_steps(&mut steps, "[con_id=20] focus", &oriented, &oriented);
        append_launch_steps(&mut steps, &oriented, &siblings);
        append_command_steps(&mut steps, "[con_id=21] splith", &siblings, &staged);
        append_command_steps(
            &mut steps,
            &format!("[con_id=21] layout {command}"),
            &staged,
            &current,
        );
        if count == 2 {
            let next = arranged_tree(
                parent_layout,
                vec![
                    expected_window(20),
                    group(100, layout, vec![expected_window(21), expected_window(22)]),
                ],
                22,
            );
            append_command_steps(&mut steps, "[con_id=21] focus", &current, &current);
            append_launch_steps(&mut steps, &current, &next);
            current = next;
        }
        let mut insertion = current.clone();
        focus_only(&mut insertion, 100);
        append_command_steps(&mut steps, "[con_id=100] focus", &current, &insertion);
        let mut complete = insertion.clone();
        complete["nodes"][0]["nodes"][0]["nodes"]
            .as_array_mut()
            .unwrap()
            .push(expected_window(21 + count as i64));
        focus_only(&mut complete, 21 + count as i64);
        append_launch_steps(&mut steps, &insertion, &complete);
        assert_completed_launches(&fixture, &config, steps, count + 2);
    }
}

#[test]
fn singleton_tab_stack_ancestry_preserves_first_split_group_and_later_sibling() {
    let fixture = LaunchFixture::new();
    let config = layout_config(
        &fixture,
        &format!(
            "{{group: tabbed, children: [{{group: stacked, children: [{{group: splitv, children: [{}, {}]}}]}}, {}]}}",
            configured_window_yaml(&fixture, 0),
            configured_window_yaml(&fixture, 1),
            configured_window_yaml(&fixture, 2)
        ),
    );
    let first = arranged_tree("splith", vec![expected_window(20)], 20);
    let vertical = arranged_tree("splitv", vec![expected_window(20)], 20);
    let split_complete =
        arranged_tree("splitv", vec![expected_window(20), expected_window(21)], 21);
    let staged_stack = arranged_tree(
        "splith",
        vec![group(
            100,
            "splitv",
            vec![expected_window(20), expected_window(21)],
        )],
        100,
    );
    let stack_complete = arranged_tree(
        "stacked",
        vec![group(
            100,
            "splitv",
            vec![expected_window(20), expected_window(21)],
        )],
        10,
    );
    let staged_tabs = arranged_tree(
        "splith",
        vec![group(
            101,
            "stacked",
            vec![group(
                100,
                "splitv",
                vec![expected_window(20), expected_window(21)],
            )],
        )],
        101,
    );
    let tabs_complete = arranged_tree(
        "tabbed",
        vec![group(
            101,
            "stacked",
            vec![group(
                100,
                "splitv",
                vec![expected_window(20), expected_window(21)],
            )],
        )],
        10,
    );
    let insertion = arranged_tree(
        "tabbed",
        vec![group(
            101,
            "stacked",
            vec![group(
                100,
                "splitv",
                vec![expected_window(20), expected_window(21)],
            )],
        )],
        101,
    );
    let complete = arranged_tree(
        "tabbed",
        vec![
            group(
                101,
                "stacked",
                vec![group(
                    100,
                    "splitv",
                    vec![expected_window(20), expected_window(21)],
                )],
            ),
            expected_window(22),
        ],
        22,
    );
    let mut steps = vec![snapshot_step(empty_workspace_tree())];
    append_launch_steps(&mut steps, &empty_workspace_tree(), &first);
    append_command_steps(&mut steps, "[con_id=20] splitv", &first, &vertical);
    append_command_steps(&mut steps, "[con_id=20] focus", &vertical, &vertical);
    append_launch_steps(&mut steps, &vertical, &split_complete);
    workspace_focus_steps(&mut steps, &split_complete);
    let mut before = split_complete.clone();
    focus_only(&mut before, 10);
    append_command_steps(&mut steps, "splith", &before, &staged_stack);
    workspace_focus_steps(&mut steps, &staged_stack);
    before = staged_stack.clone();
    focus_only(&mut before, 10);
    append_command_steps(&mut steps, "layout stacking", &before, &stack_complete);
    workspace_focus_steps(&mut steps, &stack_complete);
    append_command_steps(&mut steps, "splith", &stack_complete, &staged_tabs);
    workspace_focus_steps(&mut steps, &staged_tabs);
    before = staged_tabs.clone();
    focus_only(&mut before, 10);
    append_command_steps(&mut steps, "layout tabbed", &before, &tabs_complete);
    append_command_steps(&mut steps, "[con_id=101] focus", &tabs_complete, &insertion);
    append_launch_steps(&mut steps, &insertion, &complete);

    assert_completed_launches(&fixture, &config, steps, 3);
}

#[test]
fn split_inside_stack_after_a_window_uses_a_new_container() {
    let fixture = LaunchFixture::new();
    let config = layout_config(
        &fixture,
        &format!(
            "{{group: stacked, children: [{}, {{group: splith, children: [{}, {}]}}]}}",
            configured_window_yaml(&fixture, 0),
            configured_window_yaml(&fixture, 1),
            configured_window_yaml(&fixture, 2)
        ),
    );
    let first = arranged_tree("splith", vec![expected_window(20)], 20);
    let stack = arranged_tree("stacked", vec![expected_window(20)], 10);
    let siblings = arranged_tree(
        "stacked",
        vec![expected_window(20), expected_window(21)],
        21,
    );
    let nested = arranged_tree(
        "stacked",
        vec![
            expected_window(20),
            group(100, "splith", vec![expected_window(21)]),
        ],
        21,
    );
    let complete = arranged_tree(
        "stacked",
        vec![
            expected_window(20),
            group(
                100,
                "splith",
                vec![expected_window(21), expected_window(22)],
            ),
        ],
        22,
    );
    let mut steps = vec![snapshot_step(empty_workspace_tree())];
    append_launch_steps(&mut steps, &empty_workspace_tree(), &first);
    workspace_focus_steps(&mut steps, &first);
    let mut before = first.clone();
    focus_only(&mut before, 10);
    append_command_steps(&mut steps, "layout stacking", &before, &stack);
    let mut insertion = stack.clone();
    focus_only(&mut insertion, 20);
    append_command_steps(&mut steps, "[con_id=20] focus", &stack, &insertion);
    append_launch_steps(&mut steps, &insertion, &siblings);
    append_command_steps(&mut steps, "[con_id=21] splith", &siblings, &nested);
    append_command_steps(&mut steps, "[con_id=21] focus", &nested, &nested);
    append_launch_steps(&mut steps, &nested, &complete);

    assert_completed_launches(&fixture, &config, steps, 3);
}

#[test]
fn tab_conversion_rejects_interference_and_unexpected_structure_before_next_launch() {
    for scenario in [
        "flattened-group",
        "changed-root-layout",
        "merged-siblings",
        "reordered-siblings",
        "replaced-container",
        "moved-container",
        "focus-away-and-back",
        "new-window",
        "rejected-command",
    ] {
        let fixture = LaunchFixture::new();
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: splith, children: [{}, {{group: tabbed, children: [{}, {}]}}]}}",
                configured_window_yaml(&fixture, 0),
                configured_window_yaml(&fixture, 1),
                configured_window_yaml(&fixture, 2)
            ),
        );
        let first = arranged_tree("splith", vec![expected_window(20)], 20);
        let siblings = arranged_tree("splith", vec![expected_window(20), expected_window(21)], 21);
        let staged = arranged_tree(
            "splith",
            vec![
                expected_window(20),
                group(100, "splith", vec![expected_window(21)]),
            ],
            21,
        );
        let converted = arranged_tree(
            "splith",
            vec![
                expected_window(20),
                group(100, "tabbed", vec![expected_window(21)]),
            ],
            21,
        );
        let mut steps = vec![snapshot_step(empty_workspace_tree())];
        append_launch_steps(&mut steps, &empty_workspace_tree(), &first);
        append_command_steps(&mut steps, "[con_id=20] splith", &first, &first);
        append_command_steps(&mut steps, "[con_id=20] focus", &first, &first);
        append_launch_steps(&mut steps, &first, &siblings);
        append_command_steps(&mut steps, "[con_id=21] splith", &siblings, &staged);
        let mut before = snapshot_step(staged.clone());
        let mut command = command_step("[con_id=21] layout tabbed");
        let mut after = snapshot_step(converted.clone());
        let expected = match scenario {
            "flattened-group" => {
                after.tree = siblings; // Unexpected flattening of the new group.
                "unexpected hierarchy"
            }
            "changed-root-layout" => {
                after.tree = arranged_tree(
                    "tabbed",
                    vec![
                        expected_window(20),
                        group(100, "splith", vec![expected_window(21)]),
                    ],
                    21,
                );
                "unexpected hierarchy"
            }
            "merged-siblings" => {
                after.tree = arranged_tree(
                    "splith",
                    vec![group(
                        100,
                        "tabbed",
                        vec![expected_window(20), expected_window(21)],
                    )],
                    21,
                );
                "unexpected hierarchy"
            }
            "reordered-siblings" => {
                after.tree = arranged_tree(
                    "splith",
                    vec![
                        group(100, "tabbed", vec![expected_window(21)]),
                        expected_window(20),
                    ],
                    21,
                );
                "unexpected hierarchy"
            }
            "replaced-container" => {
                after.tree = arranged_tree(
                    "splith",
                    vec![
                        expected_window(20),
                        group(101, "tabbed", vec![expected_window(21)]),
                    ],
                    21,
                );
                "unexpected hierarchy" // Bound structural IDs cannot be replaced.
            }
            "moved-container" => {
                before.events.push((
                    WINDOW_EVENT,
                    json!({"change": "move", "container": group(100, "splith", vec![expected_window(21)])}),
                ));
                "uncertain"
            }
            "focus-away-and-back" => {
                command
                    .events
                    .push((WORKSPACE_EVENT, workspace_focus_event(11, 10)));
                command
                    .events
                    .push((WORKSPACE_EVENT, workspace_focus_event(10, 11)));
                "focus changed"
            }
            "new-window" => {
                before.events.push((
                    WINDOW_EVENT,
                    json!({"change": "new", "container": expected_window(99)}),
                ));
                "ownership is uncertain"
            }
            _ => {
                command.command.as_mut().unwrap().1 =
                    json!([{"success": false, "error": "fixture rejection"}]);
                "fixture rejection"
            }
        };
        steps.extend([before, command, after]);
        let error = fixture.run(&config, steps).unwrap_err();
        assert!(error.contains(expected), "scenario={scenario}: {error}");
        let log = fs::read_to_string(fixture.path.join("last-run.log")).unwrap();
        assert!(!log.contains("launch leaf=\\\"w2\\\""), "{log}");
    }
}

#[test]
fn workspace_tab_layout_requires_workspace_focus_at_the_command_fence() {
    for wrong_focus in [true, false] {
        let fixture = LaunchFixture::new();
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: tabbed, children: [{}, {}]}}",
                configured_window_yaml(&fixture, 0),
                configured_window_yaml(&fixture, 1)
            ),
        );
        let first = arranged_tree("splith", vec![expected_window(20)], 20);
        let mut steps = vec![snapshot_step(empty_workspace_tree())];
        append_launch_steps(&mut steps, &empty_workspace_tree(), &first);
        workspace_focus_steps(&mut steps, &first);
        let mut before = first;
        focus_only(&mut before, if wrong_focus { 20 } else { 10 });
        let mut fence = snapshot_step(before);
        if !wrong_focus {
            fence
                .events
                .push((WORKSPACE_EVENT, workspace_move_event(10)));
        }
        steps.push(fence);
        let error = fixture.run(&config, steps).unwrap_err();
        assert!(
            error.contains(if wrong_focus {
                "requires focused container id=10"
            } else {
                "moved to another output"
            }),
            "{error}"
        );
        assert_eq!(fixture.wait_for_launch_count(), 1);
        let log = fs::read_to_string(fixture.path.join("last-run.log")).unwrap();
        assert!(!log.contains("launch leaf=\\\"w1\\\""), "{log}");
    }
}

#[test]
fn sizing_reaches_weighted_splits_through_singleton_tabs_and_stacks() {
    let fixture = LaunchFixture::new();
    let config = layout_config(
        &fixture,
        &format!(
            "{{group: tabbed, children: [{{group: stacked, children: [{{group: splitv, children: [{}, {}]}}]}}]}}",
            configured_window_yaml(&fixture, 0).replace("{id:", "{weight: 3, id:"),
            configured_window_yaml(&fixture, 1)
        ),
    );
    let mut before = arranged_tree(
        "tabbed",
        vec![group(
            100,
            "stacked",
            vec![group(
                101,
                "splitv",
                vec![expected_window(20), expected_window(21)],
            )],
        )],
        21,
    );
    set_dimension(&mut before, 20, "height", 500);
    set_dimension(&mut before, 21, "height", 500);
    let mut after = before.clone();
    set_dimension(&mut after, 20, "height", 750);
    set_dimension(&mut after, 21, "height", 250);
    let mut steps = vec![snapshot_step(before.clone()), snapshot_step(before.clone())];
    append_command_steps(
        &mut steps,
        "[con_id=20] resize set height 750 px",
        &before,
        &after,
    );
    with_completed_session(&fixture, &config, steps, |session| {
        let mut warnings = Vec::new();
        session.apply_sizing(&config.layout.value, &[], &mut warnings)?;
        assert!(warnings.is_empty(), "{warnings:?}");
        Ok(())
    })
    .unwrap();
}

#[test]
fn nested_singleton_groups_after_a_completed_tab_group_keep_every_wrapper() {
    let fixture = LaunchFixture::new();
    let config = layout_config(
        &fixture,
        &format!(
            "{{group: splith, children: [{{group: tabbed, children: [{}]}}, {{group: tabbed, children: [{{group: stacked, children: [{}]}}]}}]}}",
            configured_window_yaml(&fixture, 0),
            configured_window_yaml(&fixture, 1)
        ),
    );
    let first = arranged_tree("splith", vec![expected_window(20)], 20);
    let first_tab = arranged_tree("tabbed", vec![expected_window(20)], 10);
    let wrapped_first = arranged_tree(
        "splith",
        vec![group(100, "tabbed", vec![expected_window(20)])],
        100,
    );
    let siblings = arranged_tree(
        "splith",
        vec![
            group(100, "tabbed", vec![expected_window(20)]),
            expected_window(21),
        ],
        21,
    );
    let staged_stack = arranged_tree(
        "splith",
        vec![
            group(100, "tabbed", vec![expected_window(20)]),
            group(101, "splith", vec![expected_window(21)]),
        ],
        21,
    );
    let stack = arranged_tree(
        "splith",
        vec![
            group(100, "tabbed", vec![expected_window(20)]),
            group(101, "stacked", vec![expected_window(21)]),
        ],
        21,
    );
    let staged_tab = arranged_tree(
        "splith",
        vec![
            group(100, "tabbed", vec![expected_window(20)]),
            group(
                102,
                "splith",
                vec![group(101, "stacked", vec![expected_window(21)])],
            ),
        ],
        21,
    );
    let complete = arranged_tree(
        "splith",
        vec![
            group(100, "tabbed", vec![expected_window(20)]),
            group(
                102,
                "tabbed",
                vec![group(101, "stacked", vec![expected_window(21)])],
            ),
        ],
        21,
    );
    let mut steps = vec![snapshot_step(empty_workspace_tree())];
    append_launch_steps(&mut steps, &empty_workspace_tree(), &first);
    workspace_focus_steps(&mut steps, &first);
    let mut before = first.clone();
    focus_only(&mut before, 10);
    append_command_steps(&mut steps, "layout tabbed", &before, &first_tab);
    workspace_focus_steps(&mut steps, &first_tab);
    append_command_steps(&mut steps, "splith", &first_tab, &wrapped_first);
    append_command_steps(
        &mut steps,
        "[con_id=100] focus",
        &wrapped_first,
        &wrapped_first,
    );
    append_launch_steps(&mut steps, &wrapped_first, &siblings);
    append_command_steps(&mut steps, "[con_id=21] splith", &siblings, &staged_stack);
    append_command_steps(
        &mut steps,
        "[con_id=21] layout stacking",
        &staged_stack,
        &stack,
    );
    append_command_steps(&mut steps, "[con_id=101] splith", &stack, &staged_tab);
    // The criteria select the structural child, not its leaf. Selecting window 21
    // would hit singleton parent/grandparent flattening and destroy group 101.
    append_command_steps(
        &mut steps,
        "[con_id=101] layout tabbed",
        &staged_tab,
        &complete,
    );

    assert_completed_launches(&fixture, &config, steps, 2);
}

// A limited simulation for these fixtures: a leaf focus promotes only the direct
// child on each ancestor's focus list. Unrelated groups retain their lists. This
// supplies IPC trees; it does not verify an installed compositor's focus behavior.
fn select_fixture_leaf(tree: &mut Value, id: i64) -> bool {
    tree["focused"] = json!(tree["id"] == id);
    let mut selected_child = None;
    for child in tree["nodes"].as_array_mut().unwrap() {
        if select_fixture_leaf(child, id) {
            selected_child = child["id"].as_i64();
        }
    }
    if let Some(child_id) = selected_child {
        let mut history: Vec<_> = tree["focus"].as_array().cloned().unwrap_or_default();
        history.retain(|entry| *entry != json!(child_id));
        history.insert(0, json!(child_id));
        tree["focus"] = json!(history);
    }
    selected_child.is_some() || tree["id"] == id
}

#[test]
fn group_selection_is_postorder_and_global_focus_wins_only_along_its_ancestry() {
    for (outer, inner) in [("tabbed", "stacked"), ("stacked", "tabbed")] {
        for shape in ["independent", "nested", "singleton", "split"] {
            for explicit in [false, true] {
                let fixture = LaunchFixture::new();
                let left = if shape == "singleton" {
                    vec![expected_window(20)]
                } else {
                    vec![expected_window(20), expected_window(21)]
                };
                let right = vec![expected_window(22), expected_window(23)];
                let root_layout = if shape == "nested" { outer } else { "splith" };
                let child_layout = if shape == "split" { "splitv" } else { inner };
                let left_leaves = if shape == "singleton" {
                    configured_window_yaml(&fixture, 0)
                } else {
                    format!(
                        "{}, {}",
                        configured_window_yaml(&fixture, 0),
                        configured_window_yaml(&fixture, 1)
                    )
                };
                let mut config = layout_config(
                    &fixture,
                    &format!(
                        "{{group: {root_layout}, children: [{{group: {child_layout}, children: [{left_leaves}]}}, {{group: {child_layout}, children: [{}, {}]}}]}}",
                        configured_window_yaml(&fixture, 2),
                        configured_window_yaml(&fixture, 3)
                    ),
                );
                if explicit {
                    config.focus = Some("w3".into());
                }
                let mut current = arranged_tree(
                    root_layout,
                    vec![
                        group(100, child_layout, left),
                        group(101, child_layout, right),
                    ],
                    23,
                );
                // Begin with non-first selections, including in the inactive left group.
                for id in [20, 21, 22, 23] {
                    if shape != "singleton" || id != 21 {
                        select_fixture_leaf(&mut current, id);
                    }
                }
                let mut steps = vec![snapshot_step(current.clone())];
                let mut expected_focus_ids = Vec::new();
                if shape != "split" {
                    expected_focus_ids.extend([20, 22]);
                }
                if shape == "nested" {
                    expected_focus_ids.push(20);
                }
                expected_focus_ids.push(if explicit { 23 } else { 20 });
                for id in expected_focus_ids {
                    let mut after = current.clone();
                    select_fixture_leaf(&mut after, id);
                    append_command_steps(
                        &mut steps,
                        &format!("[con_id={id}] focus"),
                        &current,
                        &after,
                    );
                    current = after;
                }
                let workspace = &current["nodes"][0]["nodes"][0];
                assert_eq!(
                    workspace["nodes"][0]["focus"][0],
                    if shape == "split" && explicit { 21 } else { 20 }
                );
                assert_eq!(
                    workspace["nodes"][1]["focus"][0],
                    if explicit || shape == "split" { 23 } else { 22 }
                );
                if shape == "nested" {
                    assert_eq!(workspace["focus"][0], if explicit { 101 } else { 100 });
                }
                let final_id = if explicit { 23 } else { 20 };
                assert!(
                    nodes(&current)
                        .iter()
                        .any(|node| node["id"] == final_id && node["focused"] == true)
                );
                with_completed_session(&fixture, &config, steps, |session| {
                    session.initialize_group_selection(&config.layout.value, &[])?;
                    let (_, path) =
                        find_focus_leaf(&config.layout.value, config.focus.as_deref(), &[])
                            .unwrap();
                    session.focus_container(&path)
                })
                .unwrap();
            }
        }
    }
}

#[test]
fn group_selection_failure_stops_before_later_groups_and_global_focus() {
    for scenario in [
        "rejected-selection",
        "unobserved-focus",
        "focus-away-and-back",
        "changed-layout",
        "disconnected-query",
        "new-window",
    ] {
        let fixture = LaunchFixture::new();
        let config = layout_config(
            &fixture,
            &format!(
                "{{group: tabbed, children: [{{group: stacked, children: [{}, {}]}}, {{group: tabbed, children: [{}]}}]}}",
                configured_window_yaml(&fixture, 0),
                configured_window_yaml(&fixture, 1),
                configured_window_yaml(&fixture, 2)
            ),
        );
        let initial = arranged_tree(
            "tabbed",
            vec![
                group(
                    100,
                    "stacked",
                    vec![expected_window(20), expected_window(21)],
                ),
                group(101, "tabbed", vec![expected_window(22)]),
            ],
            22,
        );
        let mut before = snapshot_step(initial.clone());
        let mut command = command_step("[con_id=20] focus");
        let mut after = initial.clone();
        focus_only(&mut after, 20);
        let mut after = snapshot_step(after);
        let expected = match scenario {
            "rejected-selection" => {
                command.command.as_mut().unwrap().1 =
                    json!([{"success": false, "error": "selection rejected"}]);
                "selection rejected"
            }
            "unobserved-focus" => {
                after.tree = initial.clone();
                "was not observed"
            }
            "focus-away-and-back" => {
                command.events = vec![
                    (WORKSPACE_EVENT, workspace_focus_event(11, 10)),
                    (WORKSPACE_EVENT, workspace_focus_event(10, 11)),
                ];
                "focus changed"
            }
            "changed-layout" => {
                before.tree["nodes"][0]["nodes"][0]["nodes"][0]["layout"] = json!("tabbed");
                "unexpected hierarchy"
            }
            "disconnected-query" => {
                after.disconnect = true;
                "get workspaces"
            }
            _ => {
                before.events.push((
                    WINDOW_EVENT,
                    json!({"change": "new", "container": expected_window(99)}),
                ));
                "ownership is uncertain"
            }
        };
        // No later command is supplied: any attempted continuation fails the transcript.
        let steps = if scenario == "rejected-selection" {
            vec![snapshot_step(initial), before, command]
        } else {
            vec![snapshot_step(initial), before, command, after]
        };
        let error = with_completed_session(&fixture, &config, steps, |session| {
            session.initialize_group_selection(&config.layout.value, &[])?;
            session.focus_container(&[0, 0])
        })
        .unwrap_err();
        assert!(error.contains(expected), "scenario={scenario}: {error}");
    }
}
