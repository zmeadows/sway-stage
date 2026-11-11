use super::*;
use crate::fixtures::{
    OTHER_WORKSPACE_ID, PREEXISTING_WINDOW_ID, SCRATCHPAD_WORKSPACE_ID, TARGET_WORKSPACE_ID,
    accepted, assert_contains, container_ids, desktop, extended_scenario, node_mut, parse_tree,
    rect, selector_value, standard_scenario, uncertain_message, wayland_window, window_event,
    xwayland_window,
};
use crate::scenarios::Scenario;
use serde_json::{Value, json};
use std::env;

/// Every container ID before the launch: the empty target and the preexisting window.
fn preexisting_ids() -> HashSet<i64> {
    container_ids(&parse_tree(desktop(
        "splith",
        Vec::new(),
        TARGET_WORKSPACE_ID,
    )))
}

/// horizontal-pair as loaded: windows 20 and 21 on the target, window2 focused.
fn loaded_pair(scenario: &Scenario) -> Value {
    desktop(
        "splith",
        vec![
            wayland_window(
                20,
                &selector_value(scenario, "window1"),
                rect(0, 0, 1152, 1080),
            ),
            wayland_window(
                21,
                &selector_value(scenario, "window2"),
                rect(1152, 0, 768, 1080),
            ),
        ],
        21,
    )
}

fn owned_pair() -> AcceptedWindows {
    accepted(&[(20, "window1"), (21, "window2")])
}

fn check_ownership(
    tree: Value,
    scenario: &Scenario,
    owned: &AcceptedWindows,
) -> Result<(), Failure> {
    let tree = parse_tree(tree);
    verify_ownership(
        &tree,
        TARGET_WORKSPACE_ID,
        &scenario.layout,
        &preexisting_ids(),
        owned,
    )
}

#[test]
fn ordered_new_events_bind_launches_to_leaves() {
    let scenario = standard_scenario("horizontal-pair");
    let window2 = selector_value(&scenario, "window2");
    // The first window's app ID arrives after its new event, which is not a mismatch.
    let events = [
        window_event("new", 20, None),
        window_event("focus", 20, None),
        window_event("title", 20, Some("anything")),
        window_event("new", 21, Some(&window2)),
        window_event("focus", 21, Some(&window2)),
        window_event("mark", 21, None),
        window_event("urgent", PREEXISTING_WINDOW_ID, Some("firefox-esr")),
    ];
    let bound = bind_launches(&events, &scenario.layout, &preexisting_ids()).unwrap();
    assert_eq!(bound, owned_pair());
}

#[test]
fn binding_rejects_preexisting_repeated_missing_or_extra_new_windows() {
    let scenario = standard_scenario("horizontal-pair");
    let bind = |events: &[WindowEvent]| bind_launches(events, &scenario.layout, &preexisting_ids());

    let preexisting = [
        window_event("new", 20, None),
        window_event("new", PREEXISTING_WINDOW_ID, None),
    ];
    assert_contains(
        &uncertain_message(bind(&preexisting)),
        "preexisting or repeated new event: id=6",
    );

    let repeated = [window_event("new", 20, None), window_event("new", 20, None)];
    assert_contains(
        &uncertain_message(bind(&repeated)),
        "preexisting or repeated new event: id=20",
    );

    let missing = [window_event("new", 20, None)];
    assert_contains(
        &uncertain_message(bind(&missing)),
        "expected 2 sequential new windows, observed IDs [20]",
    );

    let extra = [
        window_event("new", 20, None),
        window_event("new", 21, None),
        window_event("new", 22, None),
    ];
    assert_contains(
        &uncertain_message(bind(&extra)),
        "observed IDs [20, 21, 22]",
    );
}

#[test]
fn binding_rejects_structural_activity_and_contradicting_identities() {
    let scenario = standard_scenario("horizontal-pair");
    let window1 = selector_value(&scenario, "window1");
    let window2 = selector_value(&scenario, "window2");
    let launches = || vec![window_event("new", 20, None), window_event("new", 21, None)];

    for (change, container_id) in [
        ("close", 20),
        ("move", 21),
        ("floating", 20),
        ("fullscreen_mode", 21),
        ("focus", PREEXISTING_WINDOW_ID),
    ] {
        let mut events = launches();
        events.push(window_event(change, container_id, None));
        let message =
            uncertain_message(bind_launches(&events, &scenario.layout, &preexisting_ids()));
        assert_contains(&message, "unexpected activity during load");
        assert_contains(&message, &format!("id={container_id}"));
    }

    let mut swapped_identity = launches();
    swapped_identity.push(window_event("focus", 20, Some(&window2)));
    let message = uncertain_message(bind_launches(
        &swapped_identity,
        &scenario.layout,
        &preexisting_ids(),
    ));
    assert_contains(
        &message,
        &format!("event identity differs: id=20 app_id=Some({window2:?})"),
    );
    assert_contains(&message, &format!("expected {window1:?}"));
}

#[test]
fn quiet_history_allows_only_harmless_events_or_the_requested_close() {
    let harmless = [
        window_event("title", 20, None),
        window_event("urgent", 21, None),
        window_event("mark", PREEXISTING_WINDOW_ID, None),
    ];
    require_quiet(&harmless, None).unwrap();

    let focus = [window_event("focus", 20, None)];
    assert_contains(
        &uncertain_message(require_quiet(&focus, None)),
        "unexpected window activity",
    );

    let remaining = accepted(&[(21, "window2")]);
    let closing = ExpectedClose {
        closing_id: 20,
        remaining: &remaining,
    };
    let expected = [
        window_event("close", 20, None),
        window_event("focus", 21, None),
    ];
    require_quiet(&expected, Some(&closing)).unwrap();

    for unexpected in [
        window_event("close", 21, None),
        window_event("focus", 20, None),
        window_event("new", 22, None),
        window_event("focus", PREEXISTING_WINDOW_ID, None),
    ] {
        uncertain_message(require_quiet(&[unexpected], Some(&closing)));
    }
}

#[test]
fn ownership_accepts_exactly_the_owned_windows() {
    let scenario = standard_scenario("horizontal-pair");
    // The preexisting browser window on workspace 1 is ignored.
    check_ownership(loaded_pair(&scenario), &scenario, &owned_pair()).unwrap();
}

#[test]
fn ownership_rejects_uncertain_windows_anywhere() {
    let scenario = standard_scenario("horizontal-pair");
    let window1 = selector_value(&scenario, "window1");
    let window2 = selector_value(&scenario, "window2");
    let placement = "ownership/placement uncertain";
    let mut cases: Vec<(Value, String)> = Vec::new();

    // A matching new window on another workspace is fatal, not ignored or moved.
    let mut matching_elsewhere = loaded_pair(&scenario);
    let other = node_mut(&mut matching_elsewhere, OTHER_WORKSPACE_ID);
    other["nodes"].as_array_mut().unwrap().push(wayland_window(
        30,
        &window1,
        rect(0, 0, 960, 1080),
    ));
    cases.push((matching_elsewhere, format!("{placement}: id=30")));

    // So is a matching window in the scratchpad.
    let mut in_scratchpad = loaded_pair(&scenario);
    let mut hidden = wayland_window(31, &window2, rect(0, 0, 960, 1080));
    hidden["scratchpad_state"] = json!("fresh");
    node_mut(&mut in_scratchpad, SCRATCHPAD_WORKSPACE_ID)["floating_nodes"] = json!([hidden]);
    cases.push((in_scratchpad, format!("{placement}: id=31")));

    // A preexisting window moved onto the target.
    let mut moved_in = loaded_pair(&scenario);
    let preexisting = node_mut(&mut moved_in, OTHER_WORKSPACE_ID)["nodes"][0].take();
    node_mut(&mut moved_in, OTHER_WORKSPACE_ID)["nodes"] = json!([]);
    node_mut(&mut moved_in, TARGET_WORKSPACE_ID)["nodes"]
        .as_array_mut()
        .unwrap()
        .push(preexisting);
    cases.push((moved_in, format!("{placement}: id=6")));

    // An unrelated new window on the target.
    let mut unrelated = loaded_pair(&scenario);
    node_mut(&mut unrelated, TARGET_WORKSPACE_ID)["nodes"]
        .as_array_mut()
        .unwrap()
        .push(wayland_window(32, "unrelated", rect(0, 0, 10, 10)));
    cases.push((unrelated, format!("{placement}: id=32")));

    // An owned window that became floating or fullscreen.
    let mut floating = loaded_pair(&scenario);
    let window = node_mut(&mut floating, TARGET_WORKSPACE_ID)["nodes"][1].take();
    node_mut(&mut floating, TARGET_WORKSPACE_ID)["nodes"] =
        json!([node_mut(&mut floating, 20).take()]);
    node_mut(&mut floating, TARGET_WORKSPACE_ID)["floating_nodes"] = json!([window]);
    cases.push((floating, format!("{placement}: id=21")));

    let mut fullscreen = loaded_pair(&scenario);
    node_mut(&mut fullscreen, 20)["fullscreen_mode"] = json!(1);
    cases.push((fullscreen, format!("{placement}: id=20")));

    // An owned window whose identity changed, or that disappeared.
    let mut changed_identity = loaded_pair(&scenario);
    node_mut(&mut changed_identity, 20)["app_id"] = json!(window2);
    cases.push((changed_identity, "owned identity changed: id=20".into()));

    let mut closed = loaded_pair(&scenario);
    node_mut(&mut closed, TARGET_WORKSPACE_ID)["nodes"] = json!([node_mut(&mut closed, 21).take()]);
    cases.push((closed, "owned windows changed".into()));

    for (tree, expected) in cases {
        let message = uncertain_message(check_ownership(tree, &scenario, &owned_pair()));
        assert_contains(&message, &expected);
    }
}

/// xterm-foot as `prepare_extended` configures it, without looking for Xterm or Xwayland.
fn xterm_scenario() -> (Scenario, String) {
    let mut scenario = extended_scenario("xterm-foot");
    let window_class = "sway-stage-it-run-xterm".to_owned();
    scenario.layout.first_leaf_mut().selector = Selector::Class(window_class.clone());
    (scenario, window_class)
}

#[test]
fn ownership_distinguishes_native_wayland_from_xwayland_windows() {
    let (scenario, window_class) = xterm_scenario();
    let window2 = selector_value(&scenario, "window2");
    let owned = owned_pair();
    let loaded = || {
        desktop(
            "splith",
            vec![
                xwayland_window(20, &window_class, rect(0, 0, 1152, 1080)),
                wayland_window(21, &window2, rect(1152, 0, 768, 1080)),
            ],
            21,
        )
    };
    check_ownership(loaded(), &scenario, &owned).unwrap();

    let mut xwayland_with_app_id = loaded();
    node_mut(&mut xwayland_with_app_id, 20)["app_id"] = json!("xterm");
    let message = uncertain_message(check_ownership(xwayland_with_app_id, &scenario, &owned));
    assert_contains(&message, "expected an Xwayland window with class: 20:xterm");

    let mut class_without_x11_window = loaded();
    node_mut(&mut class_without_x11_window, 20)["window"] = Value::Null;
    let message = uncertain_message(check_ownership(class_without_x11_window, &scenario, &owned));
    assert_contains(&message, "expected an Xwayland window");

    let mut app_id_under_xwayland = loaded();
    node_mut(&mut app_id_under_xwayland, 21)["shell"] = json!("xwayland");
    let message = uncertain_message(check_ownership(app_id_under_xwayland, &scenario, &owned));
    assert_contains(&message, "expected a native Wayland window: 21:");

    let mut app_id_with_x11_window = loaded();
    node_mut(&mut app_id_with_x11_window, 21)["window"] = json!(1234);
    let message = uncertain_message(check_ownership(app_id_with_x11_window, &scenario, &owned));
    assert_contains(&message, "expected a native Wayland window");
}

/// Raw `/proc/PID/cmdline` contents: each argument followed by a NUL terminator.
fn command_line(arguments: &[&str]) -> Vec<u8> {
    arguments
        .iter()
        .flat_map(|argument| argument.bytes().chain([0]))
        .collect()
}

#[test]
fn process_evidence_compares_every_argument_after_the_executable() {
    let launched = [
        "/usr/bin/foot",
        "--app-id=shared",
        "--title=shared-window1",
        "/usr/bin/sleep",
        "300",
    ];
    let command: Vec<String> = launched.map(str::to_owned).to_vec();
    let proves = |raw: &[u8]| command_line_proves_launch(raw, &command);
    assert!(proves(&command_line(&launched)));

    // A distribution wrapper may exec the real binary under another name.
    let mut wrapped = launched;
    wrapped[0] = "/usr/lib/foot/foot";
    assert!(proves(&command_line(&wrapped)));

    let mut other_title = launched;
    other_title[2] = "--title=shared-window2";
    let mut unterminated = command_line(&launched);
    unterminated.pop();
    let mut extra_empty_argument = command_line(&launched);
    extra_empty_argument.push(0);
    let unproven = [
        command_line(&other_title),
        unterminated,
        extra_empty_argument,
        command_line(&[&launched[..], &["extra"]].concat()),
        command_line(&launched[..4]),
        command_line(&[
            launched[0],
            launched[1],
            "--title=shared-window1 /usr/bin/sleep",
            "300",
        ]),
        Vec::new(),
        vec![0],
    ];
    for raw in unproven {
        assert!(!proves(&raw), "{}", String::from_utf8_lossy(&raw));
    }

    // Empty arguments are boundaries too.
    let with_empty: Vec<String> = ["app", "", "last"].map(str::to_owned).to_vec();
    assert!(command_line_proves_launch(
        &command_line(&["app", "", "last"]),
        &with_empty
    ));
    assert!(!command_line_proves_launch(
        &command_line(&["app", "last"]),
        &with_empty
    ));
}

/// A window whose Sway PID is this test process, so `/proc/PID/cmdline` is our own argv.
fn window_of_this_process(pid: Option<i64>) -> TreeNode {
    let mut window = wayland_window(20, "shared", rect(0, 0, 100, 100));
    window["pid"] = json!(pid);
    serde_json::from_value(window).unwrap()
}

#[test]
fn process_evidence_reads_the_windows_process_arguments() {
    let mut scenario = extended_scenario("duplicate-foot-ids");
    let this_process = Some(i64::from(std::process::id()));
    let leaf = scenario.layout.first_leaf_mut();

    leaf.command = env::args().collect();
    leaf.command[0] = "argv0 may differ".into();
    require_process_command(&window_of_this_process(this_process), leaf).unwrap();

    leaf.command.push("an argument this process lacks".into());
    let message = uncertain_message(require_process_command(
        &window_of_this_process(this_process),
        leaf,
    ));
    assert_eq!(
        message,
        format!(
            "process argv does not prove this launch: container=20 pid={} leaf=window1; \
             refusing cleanup",
            std::process::id()
        )
    );

    for unusable in [None, Some(0), Some(-1)] {
        let message = uncertain_message(require_process_command(
            &window_of_this_process(unusable),
            leaf,
        ));
        assert_contains(
            &message,
            "no usable Sway process identity: container=20 leaf=window1",
        );
    }

    let message = uncertain_message(require_process_command(
        &window_of_this_process(Some(5_000_000)),
        leaf,
    ));
    assert_contains(
        &message,
        "cannot verify process argv: container=20 pid=5000000",
    );
}

#[test]
fn ownership_requires_process_evidence_for_repeated_app_ids() {
    let scenario = extended_scenario("duplicate-foot-ids");
    let shared = selector_value(&scenario, "window1");
    let mut loaded = desktop(
        "splith",
        vec![
            wayland_window(20, &shared, rect(0, 0, 1152, 1080)),
            wayland_window(21, &shared, rect(1152, 0, 768, 1080)),
        ],
        21,
    );
    // This test process cannot have Foot's arguments.
    node_mut(&mut loaded, 20)["pid"] = json!(std::process::id());
    let message = uncertain_message(check_ownership(loaded, &scenario, &owned_pair()));
    assert_contains(
        &message,
        "process argv does not prove this launch: container=20",
    );
}

#[test]
fn desktop_ownership_requires_exact_foot_argv_and_independent_glib_identity() {
    use crate::scenarios::configure_desktop_launcher;
    let directory = env::temp_dir().join(format!(
        "sway-stage-desktop-evidence-{}",
        std::process::id()
    ));
    fs::create_dir(&directory).unwrap();
    let process_directory = directory.join("101");
    fs::create_dir(&process_directory).unwrap();
    let mut scenario = extended_scenario("desktop-launcher-roundtrip");
    configure_desktop_launcher(&mut scenario, &directory).unwrap();
    let leaf = scenario.layout.leaves()[0];
    let fixture = leaf.desktop_launcher.as_ref().unwrap();
    let argv = fixture.foot_command.join("\0") + "\0";
    fs::write(process_directory.join("cmdline"), &argv).unwrap();
    let environment = format!(
        "PRIVATE=\u{00ff}\0GIO_LAUNCHED_DESKTOP_FILE={}\0GIO_LAUNCHED_DESKTOP_FILE_PID=101\0",
        fixture.path.display()
    );
    fs::write(process_directory.join("environ"), &environment).unwrap();
    verify_process_evidence(&process_directory, 101, leaf).unwrap();
    for bad_environment in [
        environment.replace("PID=101", "PID=102"),
        environment.replace("PID=101", "PID=bad"),
        environment.replace("PID=101\0", "PID=101"),
        environment.replace(fixture.path.to_str().unwrap(), "/foreign.desktop"),
        format!("{environment}GIO_LAUNCHED_DESKTOP_FILE_PID=101\0"),
        format!("{environment}GIO_LAUNCHED_DESKTOP_FILE\0"),
        "PRIVATE=secret\0".into(),
    ] {
        fs::write(process_directory.join("environ"), &bad_environment).unwrap();
        let error = verify_process_evidence(&process_directory, 101, leaf).unwrap_err();
        assert_eq!(
            error,
            "desktop launcher metadata does not prove this launch"
        );
        assert!(!error.contains("secret"));
    }
    fs::write(process_directory.join("environ"), &environment).unwrap();
    // The intermediary argv cannot substitute for the observed Foot process's argv.
    fs::write(
        process_directory.join("cmdline"),
        leaf.command.join("\0") + "\0",
    )
    .unwrap();
    assert_eq!(
        verify_process_evidence(&process_directory, 101, leaf),
        Err("process argv does not prove this launch")
    );
    fs::write(process_directory.join("cmdline"), &argv).unwrap();
    fs::write(&fixture.path, "changed").unwrap();
    assert_eq!(
        verify_process_evidence(&process_directory, 101, leaf),
        Err("private desktop fixture changed")
    );
    fs::write(&fixture.path, &fixture.contents).unwrap();
    fs::remove_file(process_directory.join("environ")).unwrap();
    assert_eq!(
        verify_process_evidence(&process_directory, 101, leaf),
        Err("cannot verify desktop launcher environment")
    );
    fs::write(process_directory.join("environ"), vec![b'x'; 65537]).unwrap();
    assert_eq!(
        verify_process_evidence(&process_directory, 101, leaf),
        Err("cannot verify desktop launcher environment")
    );

    // Even the exact run-unique app ID and ordered event binding do not authorize a
    // close when independent process provenance cannot be read.
    let window = wayland_window(20, leaf.selector.value(), rect(0, 0, 1920, 1080));
    let tree = desktop("splith", vec![window], 20);
    let message = uncertain_message(check_ownership(
        tree,
        &scenario,
        &accepted(&[(20, "window1")]),
    ));
    assert_contains(&message, "cannot verify process argv");
    fs::remove_dir_all(directory).unwrap();
}
