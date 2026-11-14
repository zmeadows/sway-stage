use super::*;
use crate::fixtures::{
    OTHER_WORKSPACE_ID, PREEXISTING_WINDOW_ID, SCRATCHPAD_WORKSPACE_ID, TARGET_WORKSPACE_ID,
    accepted_windows, assert_contains, container_ids, desktop, node_mut, parse_tree, rect,
    selector_value, standard_scenario, uncertain_message, wayland_window, window_event,
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
fn loaded_pair_tree(scenario: &Scenario) -> Value {
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

fn accepted_pair_windows() -> AcceptedWindows {
    accepted_windows(&[(20, "window1"), (21, "window2")])
}

fn verify_recorded_ownership(
    tree: Value,
    scenario: &Scenario,
    owned_windows: &AcceptedWindows,
) -> Result<(), Failure> {
    let tree = parse_tree(tree);
    verify_ownership(
        &tree,
        TARGET_WORKSPACE_ID,
        &scenario.layout,
        &preexisting_ids(),
        owned_windows,
    )
}

#[test]
fn ordered_new_events_bind_launches_to_leaves() {
    let scenario = standard_scenario("horizontal-pair");
    let second_window_app_id = selector_value(&scenario, "window2");
    // The first window's app ID arrives after its new event, which is not a mismatch.
    let events = [
        window_event("new", 20, None),
        window_event("focus", 20, None),
        window_event("title", 20, Some("anything")),
        window_event("new", 21, Some(&second_window_app_id)),
        window_event("focus", 21, Some(&second_window_app_id)),
        window_event("mark", 21, None),
        window_event("urgent", PREEXISTING_WINDOW_ID, Some("firefox-esr")),
    ];
    let bound = bind_launches(&events, &scenario.layout, &preexisting_ids()).unwrap();

    assert_eq!(bound, accepted_pair_windows());
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
    let first_window_app_id = selector_value(&scenario, "window1");
    let second_window_app_id = selector_value(&scenario, "window2");
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
    swapped_identity.push(window_event("focus", 20, Some(&second_window_app_id)));
    let message = uncertain_message(bind_launches(
        &swapped_identity,
        &scenario.layout,
        &preexisting_ids(),
    ));

    assert_contains(
        &message,
        &format!("event identity differs: id=20 app_id=Some({second_window_app_id:?})"),
    );
    assert_contains(&message, &format!("expected {first_window_app_id:?}"));
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

    let remaining = accepted_windows(&[(21, "window2")]);
    let closing = ExpectedClose {
        closing_window_id: 20,
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
    verify_recorded_ownership(
        loaded_pair_tree(&scenario),
        &scenario,
        &accepted_pair_windows(),
    )
    .unwrap();
}

#[test]
fn ownership_rejects_uncertain_windows_anywhere() {
    let scenario = standard_scenario("horizontal-pair");
    let first_window_app_id = selector_value(&scenario, "window1");
    let second_window_app_id = selector_value(&scenario, "window2");
    let placement = "ownership/placement uncertain";
    let mut cases: Vec<(Value, String)> = Vec::new();

    // A matching new window on another workspace is fatal, not ignored or moved.
    let mut matching_elsewhere = loaded_pair_tree(&scenario);
    let other = node_mut(&mut matching_elsewhere, OTHER_WORKSPACE_ID);
    other["nodes"].as_array_mut().unwrap().push(wayland_window(
        30,
        &first_window_app_id,
        rect(0, 0, 960, 1080),
    ));
    cases.push((matching_elsewhere, format!("{placement}: id=30")));

    // So is a matching window in the scratchpad.
    let mut in_scratchpad = loaded_pair_tree(&scenario);
    let mut hidden = wayland_window(31, &second_window_app_id, rect(0, 0, 960, 1080));
    hidden["scratchpad_state"] = json!("fresh");
    node_mut(&mut in_scratchpad, SCRATCHPAD_WORKSPACE_ID)["floating_nodes"] = json!([hidden]);
    cases.push((in_scratchpad, format!("{placement}: id=31")));

    // A preexisting window moved onto the target.
    let mut moved_in = loaded_pair_tree(&scenario);
    let preexisting = node_mut(&mut moved_in, OTHER_WORKSPACE_ID)["nodes"][0].take();
    node_mut(&mut moved_in, OTHER_WORKSPACE_ID)["nodes"] = json!([]);
    node_mut(&mut moved_in, TARGET_WORKSPACE_ID)["nodes"]
        .as_array_mut()
        .unwrap()
        .push(preexisting);
    cases.push((moved_in, format!("{placement}: id=6")));

    // An unrelated new window on the target.
    let mut unrelated = loaded_pair_tree(&scenario);
    node_mut(&mut unrelated, TARGET_WORKSPACE_ID)["nodes"]
        .as_array_mut()
        .unwrap()
        .push(wayland_window(32, "unrelated", rect(0, 0, 10, 10)));
    cases.push((unrelated, format!("{placement}: id=32")));

    // An owned window that became floating or fullscreen.
    let mut floating = loaded_pair_tree(&scenario);
    let window = node_mut(&mut floating, TARGET_WORKSPACE_ID)["nodes"][1].take();
    node_mut(&mut floating, TARGET_WORKSPACE_ID)["nodes"] =
        json!([node_mut(&mut floating, 20).take()]);
    node_mut(&mut floating, TARGET_WORKSPACE_ID)["floating_nodes"] = json!([window]);
    cases.push((floating, format!("{placement}: id=21")));

    let mut fullscreen = loaded_pair_tree(&scenario);
    node_mut(&mut fullscreen, 20)["fullscreen_mode"] = json!(1);
    cases.push((fullscreen, format!("{placement}: id=20")));

    // An owned window whose identity changed, or that disappeared.
    let mut changed_identity = loaded_pair_tree(&scenario);
    node_mut(&mut changed_identity, 20)["app_id"] = json!(second_window_app_id);
    cases.push((changed_identity, "owned identity changed: id=20".into()));

    let mut closed = loaded_pair_tree(&scenario);
    node_mut(&mut closed, TARGET_WORKSPACE_ID)["nodes"] = json!([node_mut(&mut closed, 21).take()]);
    cases.push((closed, "owned windows changed".into()));

    for (tree, expected) in cases {
        let message = uncertain_message(verify_recorded_ownership(
            tree,
            &scenario,
            &accepted_pair_windows(),
        ));
        assert_contains(&message, &expected);
    }
}

#[test]
fn ownership_requires_native_wayland_windows_for_app_ids() {
    let scenario = standard_scenario("horizontal-pair");
    let owned_windows = accepted_pair_windows();
    verify_recorded_ownership(loaded_pair_tree(&scenario), &scenario, &owned_windows).unwrap();

    // An Xwayland window reporting the expected app ID is not the launched Foot window.
    let mut app_id_under_xwayland = loaded_pair_tree(&scenario);
    node_mut(&mut app_id_under_xwayland, 21)["shell"] = json!("xwayland");
    let message = uncertain_message(verify_recorded_ownership(
        app_id_under_xwayland,
        &scenario,
        &owned_windows,
    ));
    assert_contains(&message, "expected a native Wayland window: 21:");

    let mut app_id_with_x11_window = loaded_pair_tree(&scenario);
    node_mut(&mut app_id_with_x11_window, 21)["window"] = json!(1234);
    let message = uncertain_message(verify_recorded_ownership(
        app_id_with_x11_window,
        &scenario,
        &owned_windows,
    ));
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
    let mut scenario = standard_scenario("duplicate-foot-ids");
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
    let scenario = standard_scenario("duplicate-foot-ids");
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
    let message = uncertain_message(verify_recorded_ownership(
        loaded,
        &scenario,
        &accepted_pair_windows(),
    ));

    assert_contains(
        &message,
        "process argv does not prove this launch: container=20",
    );
}
