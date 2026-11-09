use super::*;
use crate::observation::fixtures::*;
use serde_json::{Value, json};
use std::fs;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::thread;

struct Fixture {
    path: PathBuf,
}

impl Fixture {
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
        fs::write(&path, "version: 1\nwindow_timeout_ms: 500\nlayout:\n  id: test\n  command: [./app, '', '$HOME', '~', '*.txt', 'two words', '; touch bad']\n  match: {app_id: expected}\n  cwd: .\n  env: {MARKER: child-value, PATH: /fixture-child-path}\n").unwrap();
        crate::config::read(&path).unwrap()
    }

    fn run(&self, config: &Config, steps: Vec<Step>) -> Result<(), String> {
        let (event_client, mut events) = pair();
        let (requests, sockets) = mpsc::channel();
        let server = thread::spawn(move || {
            let subscription: Value = serde_json::from_slice(&request(&mut events, 2)).unwrap();
            assert_eq!(subscription, json!(["workspace", "window", "tick"]));
            send(&mut events, 2, json!({"success": true}));
            for (index, mut socket) in sockets.into_iter().enumerate() {
                let step = &steps[index.min(steps.len() - 1)];
                if let Some((command, replies)) = &step.command {
                    assert_eq!(
                        String::from_utf8(request(&mut socket, 0)).unwrap(),
                        *command
                    );
                    for (kind, event) in &step.events {
                        send(&mut events, *kind, event.clone());
                    }
                    send(&mut socket, 0, replies.clone());
                    continue;
                }
                request(&mut socket, 1);
                if step.disconnect {
                    drop(events);
                    return;
                }
                send(&mut socket, 1, workspaces());
                request(&mut socket, 4);
                send(&mut socket, 4, step.tree.clone());
                let token = String::from_utf8(request(&mut socket, 10)).unwrap();
                for (kind, event) in &step.events {
                    send(&mut events, *kind, event.clone());
                }
                send(&mut events, TICK, json!({"first": false, "payload": token}));
                send(&mut socket, 10, json!({"success": true}));
            }
        });
        let mut event_client = Some(event_client);
        let mut connect = || {
            if let Some(socket) = event_client.take() {
                return Ok(socket);
            }
            let (client, server) = pair();
            requests
                .send(server)
                .map_err(|_| "fixture disconnected".to_owned())?;
            Ok(client)
        };
        // Avoid global environment changes or operational files outside the fixture.
        let mut diagnostics = Diagnostics::for_test(self.path.join("last-run.log"));
        let result = load_with(config, &mut connect, &mut diagnostics);
        drop(requests);
        server.join().unwrap();
        result
    }

    fn launches(&self) -> usize {
        // The child is intentionally not waited on by load. Wait only in this fixture
        // for its marker, without changing the application's real lifecycle behavior.
        let deadline = deadline();
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
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}

#[derive(Clone)]
struct Step {
    tree: Value,
    events: Vec<(u32, Value)>,
    disconnect: bool,
    command: Option<(String, Value)>,
}
fn step(tree: Value) -> Step {
    Step {
        tree,
        events: Vec::new(),
        disconnect: false,
        command: None,
    }
}
fn view(id: i64, app_id: Option<&str>) -> Value {
    let mut view = node(id, "con", true);
    view["app_id"] = json!(app_id);
    view["shell"] = json!("xdg_shell");
    view["fullscreen_mode"] = json!(0);
    view
}
fn with_target(views: Vec<Value>) -> Value {
    let mut tree = tree();
    let workspace = &mut tree["nodes"][0]["nodes"][0];
    workspace["focused"] = json!(views.is_empty());
    workspace["nodes"] = json!(views);
    tree
}
fn elsewhere(mut tree: Value, mut view: Value) -> Value {
    view["focused"] = json!(false);
    let mut other = node(11, "workspace", false);
    other["nodes"] = json!([view]);
    tree["nodes"][0]["nodes"]
        .as_array_mut()
        .unwrap()
        .push(other);
    tree
}
fn successful_steps() -> Vec<Step> {
    vec![
        step(tree()),
        step(tree()),
        step(with_target(vec![view(20, Some("expected"))])),
        step(with_target(vec![view(20, Some("expected"))])),
    ]
}
fn fails(fixture: &Fixture, steps: Vec<Step>, expected: &str) {
    let error = fixture.run(&fixture.config(), steps).unwrap_err();
    assert!(error.contains(expected), "expected {expected:?}: {error}");
    assert_eq!(fixture.launches(), 1);
}

#[test]
fn launch_once_preserves_literal_arguments_child_cwd_environment_and_delegation() {
    let fixture = Fixture::new();
    let original_path = env::var_os("PATH");
    fixture.run(&fixture.config(), successful_steps()).unwrap();
    assert_eq!(fixture.launches(), 1);
    let limit = deadline();
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
}

#[test]
fn empty_wrappers_tiled_and_floating_occupancy_refuse_before_spawn() {
    for floating in [false, true] {
        let fixture = Fixture::new();
        let mut occupied = tree();
        let workspace = &mut occupied["nodes"][0]["nodes"][0];
        workspace["focused"] = json!(false);
        workspace[if floating { "floating_nodes" } else { "nodes" }] =
            json!([node(20, "con", true)]);
        let error = fixture
            .run(&fixture.config(), vec![step(occupied)])
            .unwrap_err();
        assert!(error.contains("completely empty"), "{error}");
        assert!(!fixture.path.join("launches").exists());
    }
}

#[test]
fn delayed_identity_without_event_and_existing_matching_window_elsewhere() {
    let fixture = Fixture::new();
    let old = view(50, Some("expected"));
    let steps = vec![
        step(elsewhere(tree(), old.clone())),
        step(elsewhere(tree(), old.clone())),
        step(elsewhere(with_target(vec![view(20, None)]), old.clone())),
        step(elsewhere(
            with_target(vec![view(20, Some("expected"))]),
            old.clone(),
        )),
        step(elsewhere(
            with_target(vec![view(20, Some("expected"))]),
            old,
        )),
    ];
    fixture.run(&fixture.config(), steps).unwrap();
    assert_eq!(fixture.launches(), 1);
}

#[test]
fn wrong_workspace_scratchpad_ambiguity_unexpected_and_preexisting_entries_fail() {
    for scenario in 0..5 {
        let fixture = Fixture::new();
        let mut steps = vec![step(tree()), step(tree())];
        let (bad, expected) = match scenario {
            0 => (
                elsewhere(tree(), view(20, Some("expected"))),
                "wrong workspace",
            ),
            1 => {
                let mut bad = elsewhere(tree(), view(20, Some("expected")));
                bad["nodes"][0]["nodes"][1]["name"] = json!("__i3_scratch");
                (bad, "scratchpad")
            }
            2 => {
                let mut other = view(21, Some("expected"));
                other["focused"] = json!(false);
                (
                    with_target(vec![view(20, Some("expected")), other]),
                    "ambiguous",
                )
            }
            3 => (
                with_target(vec![view(20, Some("unrelated"))]),
                "unexpected target-workspace",
            ),
            _ => {
                steps = vec![
                    step(elsewhere(tree(), view(20, Some("expected")))),
                    step(elsewhere(tree(), view(20, Some("expected")))),
                ];
                (with_target(vec![view(20, Some("expected"))]), "preexisting")
            }
        };
        steps.push(step(bad));
        fails(&fixture, steps, expected);
    }
}

#[test]
fn floating_fullscreen_and_unknown_fullscreen_are_not_accepted() {
    for mode in 0..3 {
        let fixture = Fixture::new();
        let mut bad = with_target(vec![view(20, Some("expected"))]);
        let window = &mut bad["nodes"][0]["nodes"][0]["nodes"][0];
        match mode {
            0 => window["floating"] = json!("user_on"),
            1 => window["fullscreen_mode"] = json!(1),
            _ => window["fullscreen_mode"] = Value::Null,
        }
        fails(
            &fixture,
            vec![step(tree()), step(tree()), step(bad)],
            "floating/fullscreen",
        );
    }
}

#[test]
fn focus_switch_away_and_back_and_ipc_disconnect_abort_without_relaunch() {
    let fixture = Fixture::new();
    let mut switched = step(tree());
    switched.events = vec![(WORKSPACE, focus(11, 10)), (WORKSPACE, focus(10, 11))];
    fails(
        &fixture,
        vec![step(tree()), step(tree()), switched],
        "focus changed",
    );
    let fixture = Fixture::new();
    let mut disconnected = step(tree());
    disconnected.disconnect = true;
    fails(
        &fixture,
        vec![step(tree()), step(tree()), disconnected],
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
        let fixture = Fixture::new();
        let mut final_step = successful_steps().pop().unwrap();
        let id = if change == "new" { 21 } else { 20 };
        final_step.events.push((
            WINDOW,
            json!({"change": change, "container": view(id, Some("expected"))}),
        ));
        let mut steps = successful_steps();
        steps.pop();
        steps.push(final_step);
        fails(&fixture, steps, expected);
    }
}

#[test]
fn event_newer_than_tree_requires_another_snapshot_and_final_window_must_remain() {
    let fixture = Fixture::new();
    let mut stale = step(tree());
    stale.events.push((
        WINDOW,
        json!({"change": "new", "container": view(20, Some("expected"))}),
    ));
    let mut steps = successful_steps();
    steps.insert(2, stale);
    fixture.run(&fixture.config(), steps).unwrap();
    let fixture = Fixture::new();
    let mut steps = successful_steps();
    *steps.last_mut().unwrap() = step(tree());
    fails(&fixture, steps, "disappeared");
}

#[test]
fn timeout_distinguishes_no_new_windows_and_unresolved_identity() {
    for unresolved in [false, true] {
        let fixture = Fixture::new();
        let mut config = fixture.config();
        config.window_timeout_ms = 160;
        let waiting = if unresolved {
            with_target(vec![view(20, None)])
        } else {
            tree()
        };
        let error = fixture
            .run(&config, vec![step(tree()), step(tree()), step(waiting)])
            .unwrap_err();
        assert!(
            error.contains(if unresolved {
                "new windows observed"
            } else {
                "no new windows observed"
            }),
            "{error}"
        );
        assert_eq!(fixture.launches(), 1);
    }
}

#[test]
fn spawn_failure_does_not_retry_and_child_path_lookup_is_used() {
    let fixture = Fixture::new();
    let mut config = fixture.config();
    let NodeKind::Window { command, .. } = &mut config.layout.value.kind else {
        unreachable!()
    };
    command[0] = "missing-fixture-executable".into();
    let error = fixture
        .run(&config, vec![step(tree()), step(tree())])
        .unwrap_err();
    assert!(error.contains("spawn application"), "{error}");
    assert!(!fixture.path.join("launches").exists());

    let mut config = fixture.config();
    let NodeKind::Window { command, env, .. } = &mut config.layout.value.kind else {
        unreachable!()
    };
    command[0] = "app".into();
    env.insert("PATH".into(), fixture.path.to_str().unwrap().into());
    fixture.run(&config, successful_steps()).unwrap();
    assert_eq!(fixture.launches(), 1);
}

#[test]
fn failure_leaves_application_running_and_never_relaunches() {
    let fixture = Fixture::new();
    fs::write(fixture.path.join("app"), "#!/bin/sh\nprintf 'launch\\n' >> launches\n/bin/sleep 0.1\nprintf 'survived\\n' > survived\n").unwrap();
    fails(
        &fixture,
        vec![
            step(tree()),
            step(tree()),
            step(with_target(vec![view(20, Some("unrelated"))])),
        ],
        "unexpected target-workspace",
    );
    let limit = deadline();
    while !fixture.path.join("survived").exists() {
        assert!(
            Instant::now() < limit,
            "application did not survive loader failure"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fixture.launches(), 1);
}

#[test]
fn exact_xwayland_class_matching_and_global_floating_ancestry() {
    let fixture = Fixture::new();
    let mut config = fixture.config();
    let NodeKind::Window { matcher, .. } = &mut config.layout.value.kind else {
        unreachable!()
    };
    *matcher = Matcher::Class("ExactClass".into());
    let mut window = view(20, None);
    window["shell"] = json!("xwayland");
    window["window"] = json!(100);
    window["window_properties"] = json!({"class": "ExactClass"});
    let target = with_target(vec![window.clone()]);
    fixture
        .run(
            &config,
            vec![
                step(tree()),
                step(tree()),
                step(target.clone()),
                step(target),
            ],
        )
        .unwrap();
    assert_eq!(fixture.launches(), 1);

    let fixture = Fixture::new();
    let mut target = tree();
    target["nodes"][0]["nodes"][0]["focused"] = json!(false);
    let mut wrapper = node(30, "floating_con", false);
    wrapper["nodes"] = json!([view(20, Some("expected"))]);
    target["nodes"][0]["nodes"][0]["floating_nodes"] = json!([wrapper]);
    fails(
        &fixture,
        vec![step(tree()), step(tree()), step(target)],
        "floating/fullscreen",
    );
}

#[test]
fn later_events_cannot_hide_identity_or_fullscreen_changes() {
    for change in ["title", "fullscreen_mode"] {
        let fixture = Fixture::new();
        let mut last = successful_steps().pop().unwrap();
        let mut changed = view(20, Some("expected"));
        if change == "title" {
            changed["app_id"] = json!("other");
        } else {
            changed["fullscreen_mode"] = json!(1);
        }
        last.events
            .push((WINDOW, json!({"change": change, "container": changed})));
        let mut steps = successful_steps();
        steps.pop();
        steps.push(last);
        fails(
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
    let fixture = Fixture::new();
    let mut window = view(20, Some("expected"));
    window["floating"] = json!("auto_off");
    let mut mapped = step(with_target(vec![window.clone()]));
    assert_eq!(mapped.tree["nodes"][0]["nodes"][0]["fullscreen_mode"], 1);
    mapped.events.push((
        WINDOW,
        json!({"change": "new", "container": window.clone()}),
    ));
    fixture
        .run(
            &fixture.config(),
            vec![
                step(tree()),
                step(tree()),
                mapped,
                step(with_target(vec![window])),
            ],
        )
        .unwrap();
    assert_eq!(fixture.launches(), 1);
}

#[test]
fn fullscreen_container_ancestry_still_marks_its_window_fullscreen() {
    for (kind, expected_fullscreen) in [("workspace", false), ("con", true), ("floating_con", true)]
    {
        let mut parent = node(30, kind, false);
        parent["fullscreen_mode"] = json!(1);
        parent["nodes"] = json!([view(20, Some("expected"))]);
        let parent: Node = serde_json::from_value(parent).unwrap();
        let mut windows = Vec::new();
        collect(&parent, None, false, false, false, &mut windows);
        let window = windows.iter().find(|window| window.node.id == 20).unwrap();
        assert_eq!(window.fullscreen, expected_fullscreen, "parent kind={kind}");
    }
}

// A transcript supplies recorded tree shapes and exact commands. It does not execute
// layout operations or model Sway's insertion, focus, or simplification algorithms.
fn group(id: i64, layout: &str, children: Vec<Value>) -> Value {
    let mut value = node(id, "con", false);
    value["layout"] = json!(layout);
    value["nodes"] = json!(children);
    value
}
fn arranged(layout: &str, children: Vec<Value>, focused: i64) -> Value {
    fn focus_only(value: &mut Value, id: i64) {
        value["focused"] = json!(value["id"] == id);
        for child in value["nodes"].as_array_mut().unwrap() {
            focus_only(child, id);
        }
    }
    let mut value = with_target(children);
    value["nodes"][0]["nodes"][0]["layout"] = json!(layout);
    focus_only(&mut value, focused);
    value
}
fn command_step(command: &str) -> Step {
    let mut value = step(tree());
    value.command = Some((command.into(), json!([{"success": true}])));
    value
}
fn operation(steps: &mut Vec<Step>, command: &str, before: &Value, after: &Value) {
    steps.extend([
        step(before.clone()),
        command_step(command),
        step(after.clone()),
    ]);
}
fn launch_steps(steps: &mut Vec<Step>, before: &Value, after: &Value) {
    steps.extend([
        step(before.clone()),
        step(after.clone()),
        step(after.clone()),
    ]);
}
fn layout_config(fixture: &Fixture, layout: &str) -> Config {
    let path = fixture.path.join("nested.yaml");
    fs::write(
        &path,
        format!("version: 1\nwindow_timeout_ms: 1000\nlayout: {layout}\n"),
    )
    .unwrap();
    crate::config::read(&path).unwrap()
}
fn leaf(fixture: &Fixture, id: usize) -> String {
    format!(
        "{{id: w{id}, command: [./app, '{id}'], cwd: '{}', match: {{app_id: expected}}}}",
        fixture.path.display()
    )
}
fn window(id: i64) -> Value {
    view(id, Some("expected"))
}
fn finish(fixture: &Fixture, config: &Config, steps: Vec<Step>, count: usize) {
    fixture.run(config, steps).unwrap();
    let limit = deadline();
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
        ("horizontal", "splith", 2),
        ("vertical", "splitv", 2),
        ("horizontal", "splith", 3),
    ] {
        let fixture = Fixture::new();
        let leaves = (0..count)
            .map(|i| leaf(&fixture, i))
            .collect::<Vec<_>>()
            .join(",");
        let config = layout_config(
            &fixture,
            &format!("{{split: {axis}, children: [{leaves}]}}"),
        );
        let mut steps = vec![step(tree())];
        let first = arranged("splith", vec![window(20)], 20);
        launch_steps(&mut steps, &tree(), &first);
        let mut current = arranged(layout, vec![window(20)], 20);
        operation(
            &mut steps,
            &format!("[con_id=20] {layout}"),
            &first,
            &current,
        );
        for index in 1..count {
            let previous = 19 + index as i64;
            operation(
                &mut steps,
                &format!("[con_id={previous}] focus"),
                &current.clone(),
                &current.clone(),
            );
            let next = arranged(
                layout,
                (0..=index).map(|i| window(20 + i as i64)).collect(),
                20 + index as i64,
            );
            launch_steps(&mut steps, &current, &next);
            current = next;
        }
        finish(&fixture, &config, steps, count);
    }
}

#[test]
fn nested_right_group_and_three_meaningful_levels() {
    for deep in [false, true] {
        let fixture = Fixture::new();
        let w = (0..4).map(|i| leaf(&fixture, i)).collect::<Vec<_>>();
        let right = if deep {
            format!("{{split: horizontal, children: [{}, {}]}}", w[2], w[3])
        } else {
            w[2].clone()
        };
        let config = layout_config(
            &fixture,
            &format!(
                "{{split: horizontal, children: [{}, {{split: vertical, children: [{}, {right}]}}]}}",
                w[0], w[1]
            ),
        );
        let a = arranged("splith", vec![window(20)], 20);
        let b = arranged("splith", vec![window(20), window(21)], 21);
        let c = arranged(
            "splith",
            vec![window(20), group(100, "splitv", vec![window(21)])],
            21,
        );
        let d = arranged(
            "splith",
            vec![
                window(20),
                group(100, "splitv", vec![window(21), window(22)]),
            ],
            22,
        );
        let mut steps = vec![step(tree())];
        launch_steps(&mut steps, &tree(), &a);
        operation(&mut steps, "[con_id=20] splith", &a, &a);
        operation(&mut steps, "[con_id=20] focus", &a, &a);
        launch_steps(&mut steps, &a, &b);
        operation(&mut steps, "[con_id=21] splitv", &b, &c);
        operation(&mut steps, "[con_id=21] focus", &c, &c);
        launch_steps(&mut steps, &c, &d);
        if deep {
            let e = arranged(
                "splith",
                vec![
                    window(20),
                    group(
                        100,
                        "splitv",
                        vec![window(21), group(101, "splith", vec![window(22)])],
                    ),
                ],
                22,
            );
            let f = arranged(
                "splith",
                vec![
                    window(20),
                    group(
                        100,
                        "splitv",
                        vec![
                            window(21),
                            group(101, "splith", vec![window(22), window(23)]),
                        ],
                    ),
                ],
                23,
            );
            operation(&mut steps, "[con_id=22] splith", &d, &e);
            operation(&mut steps, "[con_id=22] focus", &e, &e);
            launch_steps(&mut steps, &e, &f);
        }
        finish(&fixture, &config, steps, if deep { 4 } else { 3 });
    }
}

#[test]
fn nested_first_child_wraps_populated_workspace_and_preserves_equal_axis_groups() {
    for equal_axis in [false, true] {
        let fixture = Fixture::new();
        let w = (0..4).map(|i| leaf(&fixture, i)).collect::<Vec<_>>();
        let axis = if equal_axis { "horizontal" } else { "vertical" };
        let layout = if equal_axis { "splith" } else { "splitv" };
        let config = layout_config(
            &fixture,
            &format!(
                "{{split: horizontal, children: [{{split: {axis}, children: [{}, {}]}}, {{split: vertical, children: [{}, {}]}}]}}",
                w[0], w[1], w[2], w[3]
            ),
        );
        let a = arranged("splith", vec![window(20)], 20);
        let b = arranged(layout, vec![window(20)], 20);
        let c = arranged(layout, vec![window(20), window(21)], 21);
        let c_focus = arranged(layout, vec![window(20), window(21)], 20);
        let c_parent = arranged(layout, vec![window(20), window(21)], 10);
        let d = arranged(
            "splith",
            vec![group(100, layout, vec![window(20), window(21)])],
            100,
        );
        let e = arranged(
            "splith",
            vec![group(100, layout, vec![window(20), window(21)]), window(22)],
            22,
        );
        let f = arranged(
            "splith",
            vec![
                group(100, layout, vec![window(20), window(21)]),
                group(101, "splitv", vec![window(22)]),
            ],
            22,
        );
        let g = arranged(
            "splith",
            vec![
                group(100, layout, vec![window(20), window(21)]),
                group(101, "splitv", vec![window(22), window(23)]),
            ],
            23,
        );
        let mut steps = vec![step(tree())];
        launch_steps(&mut steps, &tree(), &a);
        operation(&mut steps, &format!("[con_id=20] {layout}"), &a, &b);
        operation(&mut steps, "[con_id=20] focus", &b, &b);
        launch_steps(&mut steps, &b, &c);
        operation(&mut steps, "[con_id=20] focus", &c, &c_focus);
        operation(&mut steps, "[con_id=20] focus parent", &c_focus, &c_parent);
        operation(&mut steps, "splith", &c_parent, &d);
        operation(&mut steps, "[con_id=100] focus", &d, &d);
        launch_steps(&mut steps, &d, &e);
        operation(&mut steps, "[con_id=22] splitv", &e, &f);
        operation(&mut steps, "[con_id=22] focus", &f, &f);
        launch_steps(&mut steps, &f, &g);
        finish(&fixture, &config, steps, 4);
    }
}

#[test]
fn construction_failures_stop_before_another_launch() {
    for scenario in 0..10 {
        let fixture = Fixture::new();
        let config = layout_config(
            &fixture,
            &format!(
                "{{split: vertical, children: [{}, {}]}}",
                leaf(&fixture, 0),
                leaf(&fixture, 1)
            ),
        );
        let a = arranged("splith", vec![window(20)], 20);
        let b = arranged("splitv", vec![window(20)], 20);
        let mut steps = vec![step(tree())];
        launch_steps(&mut steps, &tree(), &a);
        let mut command = command_step("[con_id=20] splitv");
        let mut before = step(a.clone());
        let mut after = step(b.clone());
        let expected = match scenario {
            0 => {
                command.command.as_mut().unwrap().1 = json!([]);
                "command failed"
            }
            1 => {
                command.command.as_mut().unwrap().1 =
                    json!([{"success": true}, {"success": false, "error": "fixture rejection"}]);
                "fixture rejection"
            }
            2 => {
                after.tree = a.clone();
                "unexpected hierarchy"
            }
            3 => {
                after.tree = tree();
                "disappeared"
            }
            4 => {
                command
                    .events
                    .push((WINDOW, json!({"change": "move", "container": window(20)})));
                "uncertain"
            }
            5 => {
                before.events.push((WORKSPACE, focus(11, 10)));
                before.events.push((WORKSPACE, focus(10, 11)));
                "focus changed"
            }
            6 => {
                before.tree = arranged("splith", vec![group(90, "splith", vec![window(20)])], 20);
                "unexpected hierarchy"
            }
            7 => {
                after.tree = arranged("splitv", vec![window(20), window(99)], 20);
                "unexpected hierarchy"
            }
            8 => {
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
        assert_eq!(fixture.launches(), 1);
    }
}

#[test]
fn accepted_windows_are_revalidated_during_the_next_window_wait() {
    for scenario in 0..5 {
        let fixture = Fixture::new();
        let config = layout_config(
            &fixture,
            &format!(
                "{{split: horizontal, children: [{}, {}]}}",
                leaf(&fixture, 0),
                leaf(&fixture, 1)
            ),
        );
        let a = arranged("splith", vec![window(20)], 20);
        let b = arranged("splith", vec![window(20), window(21)], 21);
        let mut steps = vec![step(tree())];
        launch_steps(&mut steps, &tree(), &a);
        operation(&mut steps, "[con_id=20] splith", &a, &a);
        operation(&mut steps, "[con_id=20] focus", &a, &a);
        steps.push(step(a));
        let mut bad = step(b);
        let expected = match scenario {
            0 => {
                bad.tree = arranged("splith", vec![window(21)], 21);
                "disappeared"
            }
            1 => {
                bad.events
                    .push((WINDOW, json!({"change": "close", "container": window(20)})));
                "closed"
            }
            2 => {
                bad.events
                    .push((WINDOW, json!({"change": "move", "container": window(20)})));
                "uncertain"
            }
            3 => {
                bad.tree = arranged("splith", vec![window(21), window(20)], 21);
                "unexpected hierarchy"
            }
            _ => {
                bad.tree = arranged(
                    "splith",
                    vec![group(90, "splitv", vec![window(20), window(21)])],
                    21,
                );
                "unexpected hierarchy"
            }
        };
        steps.push(bad);
        let error = fixture.run(&config, steps).unwrap_err();
        assert!(error.contains(expected), "scenario={scenario}: {error}");
        let limit = deadline();
        while fixture.launches() != 2 {
            assert!(Instant::now() < limit);
            thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn command_deadline_and_truncated_reply_are_fatal_without_retry() {
    for truncated in [false, true] {
        let (client, mut server) = pair();
        let (done, wait) = mpsc::channel();
        let worker = thread::spawn(move || {
            assert_eq!(request(&mut server, 0), b"[con_id=20] splitv");
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
    let fixture = Fixture::new();
    let path = fixture.path.join("reference.yaml");
    fs::write(&path, include_str!("../examples/four-windows.yaml")).unwrap();
    let mut config = crate::config::read(&path).unwrap();
    fn substitute(node: &mut crate::config::Node, cwd: &PathBuf) {
        match &mut node.kind {
            NodeKind::Window {
                command,
                cwd: directory,
                ..
            } => {
                *command = vec!["./app".into()];
                *directory = Some(cwd.clone());
            }
            NodeKind::Split { children, .. } => {
                for child in children {
                    substitute(&mut child.value, cwd);
                }
            }
        }
    }
    substitute(&mut config.layout.value, &fixture.path);
    let views = [
        view(20, Some("firefox-esr")),
        view(21, Some("sway-stage-middle")),
        view(22, Some("sway-stage-top")),
        view(23, Some("sway-stage-bottom")),
    ];
    let a = arranged("splith", views[..1].to_vec(), 20);
    let b = arranged("splith", views[..2].to_vec(), 21);
    let c = arranged("splith", views[..3].to_vec(), 22);
    let d = arranged(
        "splith",
        vec![
            views[0].clone(),
            views[1].clone(),
            group(100, "splitv", views[2..3].to_vec()),
        ],
        22,
    );
    let e = arranged(
        "splith",
        vec![
            views[0].clone(),
            views[1].clone(),
            group(100, "splitv", views[2..].to_vec()),
        ],
        23,
    );
    let mut steps = vec![step(tree())];
    launch_steps(&mut steps, &tree(), &a);
    operation(&mut steps, "[con_id=20] splith", &a, &a);
    operation(&mut steps, "[con_id=20] focus", &a, &a);
    launch_steps(&mut steps, &a, &b);
    operation(&mut steps, "[con_id=21] focus", &b, &b);
    launch_steps(&mut steps, &b, &c);
    operation(&mut steps, "[con_id=22] splitv", &c, &d);
    operation(&mut steps, "[con_id=22] focus", &d, &d);
    launch_steps(&mut steps, &d, &e);
    fixture.run(&config, steps).unwrap();
    let log = fs::read_to_string(fixture.path.join("last-run.log")).unwrap();
    assert!(log.contains("weights and final focus remain pending"));
    // The configured browser focus and unequal weights issue no extra commands:
    // the transcript ends immediately after hierarchy verification.
    let limit = deadline();
    while fixture.launches() != 4 {
        assert!(Instant::now() < limit);
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn initial_tabbed_workspace_is_oriented_without_an_extra_wrapper() {
    let fixture = Fixture::new();
    let config = layout_config(
        &fixture,
        &format!(
            "{{split: vertical, children: [{}, {}]}}",
            leaf(&fixture, 0),
            leaf(&fixture, 1)
        ),
    );
    let initial = arranged("tabbed", vec![], 10);
    let a = arranged("tabbed", vec![window(20)], 20);
    let b = arranged("splitv", vec![window(20)], 20);
    let c = arranged("splitv", vec![window(20), window(21)], 21);
    let mut steps = vec![step(initial.clone())];
    launch_steps(&mut steps, &initial, &a);
    operation(&mut steps, "[con_id=20] layout splitv", &a, &b);
    operation(&mut steps, "[con_id=20] focus", &b, &b);
    launch_steps(&mut steps, &b, &c);
    finish(&fixture, &config, steps, 2);
}

#[test]
fn malformed_command_frames_fail_closed() {
    use std::io::Write;
    for bytes in [
        b"wrong!\0\0\0\0\0\0\0\0".to_vec(),
        frame(4, b"[]"),
        frame(0, b"[{}]"),
        frame(0, b"not-json"),
        frame(0, b"[]")
            .into_iter()
            .take(10)
            .chain(1_u32.to_ne_bytes())
            .collect(),
    ] {
        let (client, mut server) = pair();
        let worker = thread::spawn(move || {
            request(&mut server, 0);
            server.write_all(&bytes).unwrap();
        });
        assert!(run_command(client, "[con_id=20] splith".into(), deadline()).is_err());
        worker.join().unwrap();
    }
}

#[test]
fn insertion_focus_interference_is_detected_before_spawning() {
    let fixture = Fixture::new();
    let config = layout_config(
        &fixture,
        &format!(
            "{{split: horizontal, children: [{}, {}, {}]}}",
            leaf(&fixture, 0),
            leaf(&fixture, 1),
            leaf(&fixture, 2)
        ),
    );
    let a = arranged("splith", vec![window(20)], 20);
    let b = arranged("splith", vec![window(20), window(21)], 21);
    let interfered = arranged("splith", vec![window(20), window(21)], 20);
    let mut steps = vec![step(tree())];
    launch_steps(&mut steps, &tree(), &a);
    operation(&mut steps, "[con_id=20] splith", &a, &a);
    operation(&mut steps, "[con_id=20] focus", &a, &a);
    launch_steps(&mut steps, &a, &b);
    operation(&mut steps, "[con_id=21] focus", &b, &b);
    steps.push(step(interfered));
    let error = fixture.run(&config, steps).unwrap_err();
    assert!(error.contains("insertion focus left"), "{error}");
    let limit = deadline();
    while fixture.launches() != 2 {
        assert!(Instant::now() < limit);
        thread::sleep(Duration::from_millis(5));
    }
}
