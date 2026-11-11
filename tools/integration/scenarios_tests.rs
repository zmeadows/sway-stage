use super::*;
use crate::fixtures::{FOOT, RUN_ID, SLEEP, assert_contains, extended_scenario, standard_scenario};
use std::collections::HashSet;
use std::os::unix::fs::{PermissionsExt, symlink};

/// Splits as `h(...)` or `v(...)`, with each non-root node's weight after `@`.
fn outline(node: &ConfiguredNode) -> String {
    match node {
        ConfiguredNode::Window(window) => window.layout_id.clone(),
        ConfiguredNode::Split { axis, children, .. } => {
            let axis = match axis {
                SplitAxis::Horizontal => "h",
                SplitAxis::Vertical => "v",
            };
            let children: Vec<String> = children
                .iter()
                .map(|child| format!("{}@{}", outline(child), child.weight()))
                .collect();
            format!("{axis}({})", children.join(", "))
        }
    }
}

fn all_scenarios() -> Vec<Scenario> {
    let mut scenarios = standard_scenarios(RUN_ID, FOOT, SLEEP);
    scenarios.extend(extended_scenarios(RUN_ID, FOOT, SLEEP));
    scenarios
}

#[test]
fn standard_scenarios_keep_their_structures_weights_and_focus() {
    let summaries: Vec<(&str, String, &str)> = standard_scenarios(RUN_ID, FOOT, SLEEP)
        .iter()
        .map(|scenario| {
            (
                scenario.name,
                outline(&scenario.layout),
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
    ];
    let expected: Vec<(&str, String, &str)> = expected
        .into_iter()
        .map(|(name, layout, focus)| (name, layout.to_owned(), focus))
        .collect();
    assert_eq!(summaries, expected);

    for scenario in standard_scenarios(RUN_ID, FOOT, SLEEP) {
        assert_eq!(scenario.window_timeout_ms, 20_000);
        assert_eq!(scenario.extension, None);
        assert_eq!(scenario.skip, None);
    }
}

#[test]
fn extended_scenarios_keep_their_structures_timeouts_and_extensions() {
    let summaries: Vec<_> = extended_scenarios(RUN_ID, FOOT, SLEEP)
        .iter()
        .map(|scenario| {
            (
                scenario.name,
                outline(&scenario.layout),
                scenario.expected_focus,
                scenario.window_timeout_ms,
                scenario.extension,
            )
        })
        .collect();
    let expected = [
        (
            "firefox-two-foot",
            "h(window1@44, window2@32, window3@24)",
            "window2",
            60_000,
            Some(Extension::Firefox),
        ),
        (
            "xterm-foot",
            "h(window1@3, window2@2)",
            "window2",
            20_000,
            Some(Extension::Xterm),
        ),
        (
            "nested-firefox-foot",
            "h(window1@44, window2@32, v(window3@77, window4@23)@24)",
            "window4",
            60_000,
            Some(Extension::Firefox),
        ),
        (
            "duplicate-foot-ids",
            "h(window1@3, window2@2)",
            "window2",
            20_000,
            Some(Extension::DuplicateFootIds),
        ),
    ];
    let expected: Vec<_> = expected
        .into_iter()
        .map(|(name, layout, focus, timeout, extension)| {
            (name, layout.to_owned(), focus, timeout, extension)
        })
        .collect();
    assert_eq!(summaries, expected);
}

#[test]
fn leaves_are_sequential_and_focus_selects_one_of_them() {
    let mut names = HashSet::new();
    for scenario in all_scenarios() {
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
    for scenario in all_scenarios() {
        if scenario.extension == Some(Extension::DuplicateFootIds) {
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
    let scenario = extended_scenario("duplicate-foot-ids");
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
fn preparation_leaves_standard_and_duplicate_scenarios_runnable() {
    let applications = OptionalApplications {
        firefox: None,
        firefox_app_id_override: None,
        xterm: None,
    };
    let mut retained_profiles = Vec::new();
    for name in ["horizontal-pair", "duplicate-foot-ids"] {
        let mut scenario = match name {
            "horizontal-pair" => standard_scenario(name),
            _ => extended_scenario(name),
        };
        let configured_before = configuration_yaml(&scenario);
        prepare_extended(
            &mut scenario,
            RUN_ID,
            SLEEP,
            &applications,
            &mut retained_profiles,
        )
        .unwrap();
        assert_eq!(scenario.skip, None);
        assert_eq!(configuration_yaml(&scenario), configured_before);
    }
    assert!(retained_profiles.is_empty());
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
  split: horizontal
  children:
    - id: window1
      weight: 4
      command: {}
      match:
        app_id: {}
    - split: vertical
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
  split: horizontal
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
fn remoting_name_comes_only_from_the_app_section() {
    let application_ini = "\
; comment
[Gecko]
RemotingName=wrong
[App]
Vendor=Mozilla
# remotingname=commented
remotingname = firefox-esr
";
    assert_eq!(
        remoting_name(application_ini).unwrap().as_deref(),
        Some("firefox-esr")
    );
    assert_eq!(remoting_name("[App]\nName = Firefox\n").unwrap(), None);
    assert_eq!(
        remoting_name("[App]\nRemotingName: firefox\n")
            .unwrap()
            .as_deref(),
        Some("firefox")
    );

    let repeated = "[App]\nRemotingName=firefox\nREMOTINGNAME=other\n";
    assert_contains(&remoting_name(repeated).unwrap_err(), "repeated");
    assert_contains(
        &remoting_name("[App]\nRemotingName\n").unwrap_err(),
        "malformed",
    );
}

#[test]
fn app_ids_must_be_simple_identifiers() {
    for valid in ["firefox", "firefox-esr", "org.mozilla.firefox", "Fx_1"] {
        assert!(is_simple_identifier(valid), "{valid}");
    }
    for invalid in [
        "",
        "1firefox",
        "-firefox",
        "fire fox",
        "firefox:",
        "fire\"fox",
        "fírefox",
    ] {
        assert!(!is_simple_identifier(invalid), "{invalid}");
    }
}

/// A private scratch directory for one test, removed afterwards.
struct ScratchDirectory(PathBuf);

impl ScratchDirectory {
    fn new(test_name: &str) -> Self {
        let path = env::temp_dir().join(format!(
            "sway-stage-integration-test-{}-{test_name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// An installation like Debian's: a `bin` symlink to the executable beside its metadata.
/// The fake executable is never run.
fn fake_firefox(scratch: &ScratchDirectory, application_ini: Option<&str>) -> PathBuf {
    let installation = scratch.0.join("lib");
    fs::create_dir(&installation).unwrap();
    fs::write(installation.join("firefox-esr"), "").unwrap();
    if let Some(application_ini) = application_ini {
        fs::write(installation.join("application.ini"), application_ini).unwrap();
    }

    let bin = scratch.0.join("bin");
    fs::create_dir(&bin).unwrap();
    symlink("../lib/firefox-esr", bin.join("firefox-esr")).unwrap();
    bin.join("firefox-esr")
}

#[test]
fn firefox_gets_a_private_retained_profile_and_unresolved_executable() {
    let scratch = ScratchDirectory::new("firefox-profile");
    let firefox = fake_firefox(&scratch, Some("[App]\nRemotingName=firefox-esr\n"));
    let profile_root = scratch.0.join("profile-root");
    let mut scenario = extended_scenario("firefox-two-foot");
    let mut retained_profiles = Vec::new();

    configure_firefox(
        scenario.layout.first_leaf_mut(),
        &firefox,
        None,
        &profile_root,
        &mut retained_profiles,
    )
    .unwrap();

    assert_eq!(retained_profiles, [profile_root.as_path()]);
    let root_mode = fs::metadata(&profile_root).unwrap().permissions().mode();
    assert_eq!(root_mode & 0o777, 0o700);
    let preferences = fs::read_to_string(profile_root.join("profile/user.js")).unwrap();
    assert_eq!(preferences, FIREFOX_PREFERENCES);

    let root = profile_root.to_str().unwrap();
    let leaf = scenario.layout.first_leaf_mut();
    let profile = format!("{root}/profile");
    let expected_command = [
        firefox.to_str().unwrap(),
        "--no-remote",
        "--new-instance",
        "--profile",
        &profile,
        "--new-window",
        "about:blank",
    ];
    assert_eq!(leaf.command, expected_command);
    assert_eq!(leaf.selector, Selector::AppId("firefox-esr".into()));
    assert!(leaf.requires_process_evidence);

    let env: Vec<(&str, &str)> = leaf
        .env
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let config = format!("{root}/config");
    let cache = format!("{root}/cache");
    let data = format!("{root}/data");
    let state = format!("{root}/state");
    let expected_env = [
        ("HOME", root),
        ("XDG_CONFIG_HOME", &config),
        ("XDG_CACHE_HOME", &cache),
        ("XDG_DATA_HOME", &data),
        ("XDG_STATE_HOME", &state),
        ("MOZ_ENABLE_WAYLAND", "1"),
        ("GDK_BACKEND", "wayland"),
        ("XDG_ACTIVATION_TOKEN", ""),
        ("DESKTOP_STARTUP_ID", ""),
        ("XRE_PROFILE_PATH", ""),
        ("XRE_PROFILE_LOCAL_PATH", ""),
        ("XRE_PROFILE_NAME", ""),
    ];
    assert_eq!(env, expected_env);

    // The other leaves stay Foot windows, and the configuration carries the environment.
    let yaml = configuration_yaml(&scenario);
    assert_contains(&yaml, "window_timeout_ms: 60000\n");
    assert_contains(&yaml, &format!("      env: {{\"HOME\": \"{root}\", "));
    assert_contains(&yaml, "        app_id: firefox-esr\n");
    assert_eq!(yaml.matches("/usr/bin/foot").count(), 2);

    // An existing path is never reused, and nothing more is retained.
    let mut another = extended_scenario("firefox-two-foot");
    let error = configure_firefox(
        another.layout.first_leaf_mut(),
        &firefox,
        None,
        &profile_root,
        &mut retained_profiles,
    )
    .unwrap_err();
    assert_contains(&error, "create Firefox directory");
    assert_eq!(retained_profiles.len(), 1);
}

#[test]
fn firefox_app_id_failures_still_retain_the_created_profile() {
    let scratch = ScratchDirectory::new("firefox-app-id");
    let firefox = fake_firefox(&scratch, None);
    let mut retained_profiles = Vec::new();

    let missing_metadata_root = scratch.0.join("missing-metadata");
    let mut scenario = extended_scenario("nested-firefox-foot");
    let error = configure_firefox(
        scenario.layout.first_leaf_mut(),
        &firefox,
        None,
        &missing_metadata_root,
        &mut retained_profiles,
    )
    .unwrap_err();
    assert_contains(&error, "cannot determine Firefox Wayland app ID from");
    assert_contains(&error, "lib/application.ini");
    assert_contains(&error, "supply --firefox-app-id");

    let invalid_override_root = scratch.0.join("invalid-override");
    let error = configure_firefox(
        scenario.layout.first_leaf_mut(),
        &firefox,
        Some("firefox esr"),
        &invalid_override_root,
        &mut retained_profiles,
    )
    .unwrap_err();
    assert_eq!(error, "Firefox app ID must be a simple literal identifier");

    assert_eq!(
        retained_profiles,
        [missing_metadata_root, invalid_override_root]
    );
    assert!(
        retained_profiles
            .iter()
            .all(|root| root.join("profile/user.js").is_file())
    );

    // A valid override needs no metadata.
    let override_root = scratch.0.join("override");
    configure_firefox(
        scenario.layout.first_leaf_mut(),
        &firefox,
        Some("org.mozilla.firefox"),
        &override_root,
        &mut retained_profiles,
    )
    .unwrap();
    assert_eq!(
        scenario.layout.first_leaf_mut().selector,
        Selector::AppId("org.mozilla.firefox".into())
    );
}

#[test]
fn process_settings_preserve_literals_and_relative_cwd() {
    let mut scenario = standard_scenario("process-execution");
    configure_process_execution(&mut scenario, Path::new("/private/harness"), RUN_ID).unwrap();
    let leaf = scenario.layout.leaves()[0];
    assert_eq!(leaf.cwd.as_deref(), Some("process cwd"));
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
    assert!(yaml.contains("cwd: \"process cwd\""));
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
