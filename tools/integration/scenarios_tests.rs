use super::*;
use crate::fixtures::{FOOT, RUN_ID, SLEEP, assert_contains, standard_scenario};
use std::collections::HashSet;

/// Groups use a short layout name; only split children carry weights after `@`.
fn configured_layout_outline(node: &ConfiguredNode) -> String {
    match node {
        ConfiguredNode::Window(window) => window.layout_id.clone(),
        ConfiguredNode::Group {
            layout, children, ..
        } => {
            let label = match layout {
                GroupLayout::Horizontal => "h",
                GroupLayout::Vertical => "v",
                GroupLayout::Tabbed => "t",
                GroupLayout::Stacked => "s",
            };
            let children: Vec<String> = children
                .iter()
                .map(|child| {
                    if layout.is_split() {
                        format!("{}@{}", configured_layout_outline(child), child.weight())
                    } else {
                        configured_layout_outline(child)
                    }
                })
                .collect();
            format!("{label}({})", children.join(", "))
        }
    }
}

#[test]
fn standard_scenarios_keep_their_structures_weights_and_focus() {
    let observed_summaries: Vec<(&str, String, &str)> = standard_scenarios(RUN_ID, FOOT, SLEEP)
        .iter()
        .map(|scenario| {
            (
                scenario.name,
                configured_layout_outline(&scenario.layout),
                scenario.expected_focus,
            )
        })
        .collect();
    let expected = [
        ("single-window", "window1", "window1"),
        ("horizontal-pair", "h(window1@3, window2@2)", "window2"),
        ("vertical-pair", "v(window1@2, window2@3)", "window1"),
        (
            "uneven-siblings",
            "h(window1@5, window2@3, window3@2)",
            "window2",
        ),
        (
            "mixed-nesting",
            "h(window1@4, v(window2@3, window3@2)@3, window4@3)",
            "window3",
        ),
        (
            "deep-nesting",
            "v(h(window1@2, h(window2@2, v(window3@3, window4@2)@3)@3)@3, window5@2)",
            "window4",
        ),
        (
            "sibling-nested-groups",
            "h(v(window1@2, window2@3)@3, v(window3@3, window4@1)@2)",
            "window3",
        ),
        ("process-execution", "window1", "window1"),
        (
            "default-focus",
            "h(v(window1@2, window2@3)@3, window3@2)",
            "window1",
        ),
        (
            "occupied-workspace-refusal",
            "h(window1@3, window2@2)",
            "window2",
        ),
        (
            "capture-reload-roundtrip",
            "h(window1@4, v(window2@3, window3@2)@3, window4@3)",
            "window3",
        ),
        ("root-tabbed", "t(window1, window2, window3)", "window1"),
        ("root-stacked", "s(window1, window2, window3)", "window2"),
        ("singleton-groups", "s(t(window1), window2)", "window1"),
        (
            "mixed-native-nesting",
            "h(t(window1, v(window2@3, window3@2))@3, window4@2)",
            "window3",
        ),
        (
            "independent-native-groups",
            "h(t(window1, s(window2, window3))@3, s(t(window4, window5), window6)@2)",
            "window3",
        ),
        (
            "mixed-capture-reload",
            "v(t(s(window1), h(window2@3, window3@2))@3, window4@2)",
            "window3",
        ),
        (
            "initially-tabbed",
            "h(t(window1, v(window2@3, window3@2))@3, window4@2)",
            "window3",
        ),
        (
            "initially-stacked",
            "h(t(window1, v(window2@3, window3@2))@3, window4@2)",
            "window3",
        ),
        (
            "outside-focus-capture-reload",
            "v(t(s(window1), h(window2@3, window3@2))@3, window4@2)",
            "window4",
        ),
        ("duplicate-foot-ids", "h(window1@3, window2@2)", "window2"),
    ];
    let expected: Vec<(&str, String, &str)> = expected
        .into_iter()
        .map(|(name, layout, focus)| (name, layout.to_owned(), focus))
        .collect();

    assert_eq!(observed_summaries, expected);

    for scenario in standard_scenarios(RUN_ID, FOOT, SLEEP) {
        assert_eq!(scenario.window_timeout_ms, 20_000);
    }
}

#[test]
fn leaves_are_sequential_and_focus_selects_one_of_them() {
    let mut names = HashSet::new();
    for scenario in standard_scenarios(RUN_ID, FOOT, SLEEP) {
        assert!(names.insert(scenario.name), "repeated {}", scenario.name);

        let layout_ids: Vec<String> = scenario
            .layout
            .leaves()
            .iter()
            .map(|leaf| leaf.layout_id.clone())
            .collect();
        let sequential: Vec<String> = (1..=layout_ids.len())
            .map(|number| format!("window{number}"))
            .collect();
        assert_eq!(layout_ids, sequential, "{}", scenario.name);
        assert!(
            layout_ids.iter().any(|id| id == scenario.expected_focus),
            "{}",
            scenario.name
        );
    }
}

#[test]
fn foot_windows_have_unique_identifiers_and_bounded_lifetimes() {
    let mut app_ids = HashSet::new();
    for scenario in standard_scenarios(RUN_ID, FOOT, SLEEP) {
        if scenario.name == "duplicate-foot-ids" {
            continue;
        }
        for leaf in scenario.layout.leaves() {
            let app_id = format!(
                "sway-stage-it-{RUN_ID}-{}-{}",
                scenario.name, leaf.layout_id
            );
            let expected_command = [FOOT, &format!("--app-id={app_id}"), SLEEP, "300"];
            assert_eq!(leaf.command, expected_command);
            assert!(leaf.env.is_empty());
            assert!(!leaf.requires_process_evidence);
            assert_eq!(leaf.selector, Selector::AppId(app_id.clone()));
            assert!(app_ids.insert(app_id), "repeated app ID");
        }
    }
}

#[test]
fn duplicate_foot_ids_share_a_selector_but_not_launch_arguments() {
    let scenario = standard_scenario("duplicate-foot-ids");
    let shared_app_id = format!("sway-stage-it-{RUN_ID}-duplicate-foot");
    let leaves = scenario.layout.leaves();

    assert_eq!(leaves.len(), 2);
    for leaf in &leaves {
        let title = format!("--title={shared_app_id}-{}", leaf.layout_id);
        let expected_command = [
            FOOT,
            &format!("--app-id={shared_app_id}"),
            &title,
            SLEEP,
            "300",
        ];
        assert_eq!(leaf.command, expected_command);
        assert_eq!(leaf.selector, Selector::AppId(shared_app_id.clone()));
        assert!(leaf.requires_process_evidence);
    }
    assert_ne!(leaves[0].command, leaves[1].command);
}

#[test]
fn configuration_yaml_matches_the_established_format() {
    let window1 = format!("sway-stage-it-{RUN_ID}-single-window-window1");
    let expected = format!(
        "\
version: 1
focus: window1
window_timeout_ms: 20000
layout:
  id: window1
  command: [\"/usr/bin/foot\", \"--app-id={window1}\", \"/usr/bin/sleep\", \"300\"]
  match:
    app_id: {window1}
"
    );

    assert_eq!(
        configuration_yaml(&standard_scenario("single-window")),
        expected
    );

    let app_id = |layout_id: &str| format!("sway-stage-it-{RUN_ID}-mixed-nesting-{layout_id}");
    let command = |layout_id: &str| {
        format!(
            "[\"/usr/bin/foot\", \"--app-id={}\", \"/usr/bin/sleep\", \"300\"]",
            app_id(layout_id)
        )
    };
    let expected = format!(
        "\
version: 1
focus: window3
window_timeout_ms: 20000
layout:
  group: splith
  children:
    - id: window1
      weight: 4
      command: {}
      match:
        app_id: {}
    - group: splitv
      weight: 3
      children:
        - id: window2
          weight: 3
          command: {}
          match:
            app_id: {}
        - id: window3
          weight: 2
          command: {}
          match:
            app_id: {}
    - id: window4
      weight: 3
      command: {}
      match:
        app_id: {}
",
        command("window1"),
        app_id("window1"),
        command("window2"),
        app_id("window2"),
        command("window3"),
        app_id("window3"),
        command("window4"),
        app_id("window4"),
    );
    assert_eq!(
        configuration_yaml(&standard_scenario("mixed-nesting")),
        expected
    );
}

#[test]
fn configuration_yaml_writes_environment_and_escapes_as_json() {
    let mut scenario = standard_scenario("single-window");
    let ConfiguredNode::Window(leaf) = &mut scenario.layout else {
        panic!("expected a root window");
    };
    leaf.command = vec![
        "/opt/app".into(),
        "two words".into(),
        "quote\"and\\slash".into(),
    ];
    leaf.env = vec![
        ("HOME".into(), "/tmp/home".into()),
        ("EMPTY".into(), String::new()),
    ];

    let yaml = configuration_yaml(&scenario);

    assert_contains(
        &yaml,
        "  command: [\"/opt/app\", \"two words\", \"quote\\\"and\\\\slash\"]\n",
    );
    assert_contains(&yaml, "  env: {\"HOME\": \"/tmp/home\", \"EMPTY\": \"\"}\n");
}

#[test]
fn expected_draft_has_measured_weights_and_placeholders_only() {
    let scenario = standard_scenario("horizontal-pair");
    let window1 = format!("sway-stage-it-{RUN_ID}-horizontal-pair-window1");
    let window2 = format!("sway-stage-it-{RUN_ID}-horizontal-pair-window2");
    let shares = BTreeMap::from([(vec![0], 59.75), (vec![1], 40.25)]);
    let expected = format!(
        "\
version: 1
focus: window2
layout:
  group: splith
  children:
    - id: window1
      weight: 59.75
      command: null
      match:
        app_id: {window1}
    - id: window2
      weight: 40.25
      command: null
      match:
        app_id: {window2}
"
    );

    assert_eq!(expected_draft(&scenario, &shares).unwrap(), expected);

    let incomplete = BTreeMap::from([(vec![0], 59.75)]);
    let error = expected_draft(&scenario, &incomplete).unwrap_err();
    assert_eq!(error, "no measured share for layout.children[1]");
}

#[test]
fn describe_path_uses_field_path_notation() {
    assert_eq!(describe_path(&[]), "layout");
    assert_eq!(describe_path(&[2, 0]), "layout.children[2].children[0]");
}

#[test]
fn process_settings_preserve_literals_and_absolute_cwd() {
    let mut scenario = standard_scenario("process-execution");
    configure_process_execution(
        &mut scenario,
        Path::new("/private/harness"),
        Path::new("/private/process cwd"),
        RUN_ID,
    )
    .unwrap();
    let leaf = scenario.layout.leaves()[0];

    assert_eq!(leaf.cwd.as_deref(), Some("/private/process cwd"));
    assert_eq!(leaf.command[2], "/private/harness");
    assert_eq!(leaf.command[3], crate::process::PROBE_FLAG);
    assert_eq!(leaf.command[4], format!("{RUN_ID} two words"));
    assert_eq!(
        leaf.command[5],
        format!("{RUN_ID} $HOME ; $(literal) * ' \"")
    );
    assert_eq!(leaf.command[6], "");
    assert_eq!(
        leaf.env,
        [(
            format!("SWAY_STAGE_IT_{RUN_ID}"),
            format!("{RUN_ID} override")
        )]
    );
    let yaml = configuration_yaml(&scenario);
    assert!(yaml.contains("cwd: \"/private/process cwd\""));
    assert!(yaml.contains(&format!(
        "env: {{\"SWAY_STAGE_IT_{RUN_ID}\": \"{RUN_ID} override\"}}"
    )));
    assert!(yaml.contains(&json_sequence(&leaf.command)));
    assert!(leaf.requires_process_evidence);
    assert!(!diagnostic_yaml(&scenario).contains("override"));
    assert!(!diagnostic_yaml(&scenario).contains("$HOME"));
}

#[test]
fn omitted_focus_still_has_an_independent_expected_leaf_and_capture_focus() {
    let scenario = standard_scenario("default-focus");

    assert_eq!(scenario.configured_focus, None);
    assert_eq!(scenario.expected_focus, "window1");
    assert_eq!(
        scenario.layout.leaves()[0].layout_id,
        scenario.expected_focus
    );
    assert_ne!(
        scenario.layout.leaves().last().unwrap().layout_id,
        scenario.expected_focus
    );
    assert!(!configuration_yaml(&scenario).contains("focus:"));
    assert!(diagnostic_yaml(&scenario).contains("focus: window1"));
}

#[test]
fn native_group_yaml_uses_parent_dependent_weights_and_no_root_weight() {
    let scenario = standard_scenario("mixed-capture-reload");
    let yaml = configuration_yaml(&scenario);
    let document: serde_json::Value = serde_saphyr::from_str(&yaml).unwrap();
    let root = &document["layout"];

    assert_eq!(root["group"], "splitv");
    assert!(root.get("weight").is_none());
    let tabs = &root["children"][0];
    assert_eq!(tabs["group"], "tabbed");
    assert_eq!(tabs["weight"], 3);
    let stack = &tabs["children"][0];
    assert_eq!(stack["group"], "stacked");
    assert!(stack.get("weight").is_none());
    assert!(stack["children"][0].get("weight").is_none());
    let row = &tabs["children"][1];
    assert_eq!(row["group"], "splith");
    assert!(row.get("weight").is_none());
    assert_eq!(row["children"][0]["weight"], 3);
    assert_eq!(row["children"][1]["weight"], 2);
    assert_eq!(root["children"][1]["weight"], 2);
    assert_eq!(yaml.matches("weight:").count(), 4);

    for name in ["root-tabbed", "root-stacked", "singleton-groups"] {
        let scenario = standard_scenario(name);
        assert!(!configuration_yaml(&scenario).contains("weight:"));
        let draft = expected_draft(&scenario, &BTreeMap::new()).unwrap();
        assert!(!draft.contains("weight:"));
        assert_contains(&draft, &format!("focus: {}", scenario.expected_focus));
    }
    for name in ["root-tabbed", "singleton-groups"] {
        let scenario = standard_scenario(name);
        assert_eq!(scenario.configured_focus, None);
        assert!(!configuration_yaml(&scenario).contains("focus:"));
    }
}

#[test]
fn only_initial_layout_cases_prepare_layout_and_reuse_mixed_configuration() {
    let standard = standard_scenarios(RUN_ID, FOOT, SLEEP);
    let mixed = standard_scenario("mixed-native-nesting");
    assert_eq!(standard.len(), 21);
    assert!(
        standard
            .iter()
            .filter(|scenario| !matches!(scenario.name, "initially-tabbed" | "initially-stacked"))
            .all(|scenario| scenario.initial_workspace_layout.is_none())
    );
    assert!(
        foreign_fixture_scenario(RUN_ID, FOOT, SLEEP)
            .initial_workspace_layout
            .is_none()
    );
    for (scenario, expected) in standard[17..19].iter().zip([
        InitialWorkspaceLayout::Tabbed,
        InitialWorkspaceLayout::Stacked,
    ]) {
        assert_eq!(scenario.initial_workspace_layout, Some(expected));
        assert_eq!(scenario.configured_focus, mixed.configured_focus);
        assert_eq!(scenario.expected_focus, mixed.expected_focus);
        assert_eq!(
            configured_layout_outline(&scenario.layout),
            configured_layout_outline(&mixed.layout)
        );
        // Split-root construction returns the workspace to an ordinary split layout.
        assert!(matches!(
            scenario.layout,
            ConfiguredNode::Group {
                layout: GroupLayout::Horizontal,
                ..
            }
        ));
    }
}
