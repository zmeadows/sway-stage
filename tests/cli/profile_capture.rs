use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

fn tiled_window(container_id: i64, app_id: &str, focused: bool) -> Value {
    let mut window = sway_node(container_id, "con", 960, vec![]);
    window["shell"] = json!("xdg_shell");
    window["app_id"] = json!(app_id);
    window["focused"] = json!(focused);
    window
}

fn populated_workspace(id: i64, number: i32, name: &str, focused: bool) -> Value {
    let children = vec![
        tiled_window(id + 1, "first", false),
        tiled_window(id + 2, "second", focused),
    ];
    let mut workspace = sway_node(id, "workspace", 1920, children);
    workspace["num"] = json!(number);
    workspace["name"] = json!(name);
    workspace["focus"] = json!([id + 2, id + 1]);
    workspace
}

fn tree() -> Value {
    let mut tabs = populated_workspace(30, -1, "music player", false);
    tabs["layout"] = json!("tabbed");
    let mut stack = populated_workspace(50, -1, "music", false);
    stack["layout"] = json!("stacked");
    let mut empty = sway_node(70, "workspace", 1920, vec![]);
    empty["num"] = json!(7);
    empty["name"] = json!("7");
    let mut internal = sway_node(
        90,
        "output",
        1920,
        vec![populated_workspace(100, -1, "__i3_scratch", false)],
    );
    internal["name"] = json!("__i3");
    let mut output = sway_node(
        2,
        "output",
        1920,
        vec![
            tabs,
            populated_workspace(10, 3, "3:development", false),
            empty,
            populated_workspace(20, 1, "1", true),
            stack,
        ],
    );
    output["name"] = json!("DP-1");
    sway_node(1, "root", 1920, vec![output, internal])
}

fn run_capture(fixture: &CliFixture, tree: Value, arguments: &[&str], expected: i32) -> Output {
    let socket = fixture.path.join("capture.sock");
    let server = serve_capture(UnixListener::bind(&socket).unwrap(), tree, None);
    let state = fixture.path.join("state/sway-stage");
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("last-run.log"), "previous launch\n").unwrap();
    let output = fixture
        .command()
        .env("SWAYSOCK", &socket)
        .env("XDG_STATE_HOME", fixture.path.join("state"))
        .args([
            "--layouts-dir",
            "layouts",
            "--profiles-dir",
            "profiles",
            "profile",
            "capture",
        ])
        .args(arguments)
        .output()
        .unwrap();
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(expected), "{output:?}");
    assert_eq!(
        fs::read_to_string(state.join("last-run.log")).unwrap(),
        "previous launch\n"
    );
    output
}

#[test]
fn explicit_numbers_names_and_mixed_targets_keep_order_and_independent_focus() {
    for (arguments, expected) in [
        (
            vec!["work", "3", "1", "--no-infer-commands"],
            vec![json!(3), json!(1)],
        ),
        (
            vec!["work", "music player", "music"],
            vec![json!("music player"), json!("music")],
        ),
        (
            vec!["work", "music", "1", "music player", "3"],
            vec![json!("music"), json!(1), json!("music player"), json!(3)],
        ),
    ] {
        let fixture = CliFixture::new();
        let output = run_capture(&fixture, tree(), &arguments, 0);
        assert!(output.stdout.is_empty());
        let profile_yaml = fs::read_to_string(fixture.path.join("profiles/work.yaml")).unwrap();
        assert!(profile_yaml.contains("\nversion: 1  # ← Required; do not remove\n"));
        let profile: Value = serde_saphyr::from_str(&profile_yaml).unwrap();
        assert_eq!(profile["version"], 1);
        for (index, target) in expected.iter().enumerate() {
            let name = format!("work-{:03}", index + 1);
            assert_eq!(profile["workspaces"][index]["workspace"], *target);
            assert_eq!(profile["workspaces"][index]["layout"], name);
            let path = fixture.path.join(format!("layouts/{name}.yaml"));
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let yaml = fs::read_to_string(path).unwrap();
            assert!(yaml.contains("\nversion: 1  # ← Required; do not remove\n"));
            let draft: Value = serde_saphyr::from_str(&yaml).unwrap();
            assert_eq!(draft["version"], 1);
            assert_eq!(draft["focus"], "window2");
            if target.is_string() {
                assert_eq!(draft["layout"]["active"], 2);
            }
            assert!(yaml.contains("command: null"));
        }
        assert_eq!(
            fs::read_dir(fixture.path.join("layouts")).unwrap().count(),
            expected.len()
        );
        assert_eq!(
            fs::metadata(fixture.path.join("layouts"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("captured profile \"work\""));
        assert!(
            stderr.contains(if arguments.contains(&"--no-infer-commands") {
                "inference disabled"
            } else {
                "Commands inferred"
            })
        );
        fixture.run(
            &[
                "--layouts-dir",
                "layouts",
                "--profiles-dir",
                "profiles",
                "profile",
                "check",
                "work",
            ],
            1,
        );
    }
}

#[test]
fn all_orders_numbered_then_named_and_omits_only_empty_and_internal() {
    let fixture = CliFixture::new();
    run_capture(
        &fixture,
        tree(),
        &["work", "--all", "--no-infer-commands"],
        0,
    );
    let profile: Value = serde_saphyr::from_str(
        &fs::read_to_string(fixture.path.join("profiles/work.yaml")).unwrap(),
    )
    .unwrap();
    let targets: Vec<_> = profile["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["workspace"].clone())
        .collect();
    assert_eq!(
        targets,
        vec![json!(1), json!(3), json!("music"), json!("music player")]
    );
}

#[test]
fn cli_selection_and_name_errors_precede_ipc_and_filesystem_writes() {
    for arguments in [
        vec!["work"],
        vec!["work", "1", "--all"],
        vec!["work", "--all", "music"],
    ] {
        let fixture = CliFixture::new();
        let mut args = vec!["profile", "capture"];
        args.extend(arguments);
        fixture.run(&args, 2);
    }
    for arguments in [
        vec!["bad.name", "1"],
        vec!["work", "0"],
        vec!["work", "1000000000"],
        vec!["work", "99999999999999999999999999"],
        vec!["work", "3:development"],
        vec!["work", "music;exec"],
        vec!["work", "next"],
        vec!["work", "1", "01"],
        vec!["work", "music", "music"],
        vec!["work", "music "],
    ] {
        let fixture = CliFixture::new();
        let mut args = vec!["profile", "capture"];
        args.extend(arguments);
        let output = fixture.run(&args, 1);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("SWAYSOCK"));
        assert!(!fixture.path.join(".config").exists());
    }
    let fixture = CliFixture::new();
    fixture.run(&["profile", "capture", &"a".repeat(247), "--all"], 1);
    run_capture(&fixture, tree(), &[&"a".repeat(246), "1"], 0);
}

#[test]
fn invalid_or_unsupported_observations_publish_nothing() {
    for case in [
        "missing",
        "duplicate-number",
        "duplicate-name",
        "missing-num",
        "inconsistent",
        "named-number",
        "unsupported-name",
        "floating",
        "fullscreen",
        "structural",
        "history",
        "empty-all",
    ] {
        let fixture = CliFixture::new();
        let mut tree = tree();
        let mut arguments = vec!["work", "--all"];
        let workspaces = tree["nodes"][0]["nodes"].as_array_mut().unwrap();
        match case {
            "missing" => arguments = vec!["work", "8"],
            "duplicate-number" => {
                workspaces[0]["num"] = json!(3);
                workspaces[0]["name"] = json!("3:other");
            }
            "duplicate-name" => workspaces[0]["name"] = json!("music"),
            "missing-num" => {
                workspaces[0].as_object_mut().unwrap().remove("num");
            }
            "inconsistent" => workspaces[1]["num"] = json!(2),
            "named-number" => workspaces[0]["num"] = json!(0),
            "unsupported-name" => workspaces[0]["name"] = json!("music;exec"),
            "floating" => {
                workspaces[0]["floating_nodes"] =
                    json!([sway_node(110, "floating_con", 100, vec![])])
            }
            "fullscreen" => workspaces[0]["nodes"][0]["fullscreen_mode"] = json!(1),
            "structural" => {
                workspaces[0]["nodes"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("shell");
                workspaces[0]["nodes"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("app_id");
            }
            "history" => workspaces[1]["focus"] = json!([999]),
            "empty-all" => {
                for workspace in workspaces {
                    workspace["nodes"] = json!([]);
                    workspace["focused"] = json!(workspace["num"] == 1);
                }
            }
            _ => unreachable!(),
        }
        let output = run_capture(&fixture, tree, &arguments, 1);
        assert!(output.stdout.is_empty());
        assert!(!fixture.path.join("layouts").exists());
        assert!(!fixture.path.join("profiles").exists());
        assert!(String::from_utf8_lossy(&output.stderr).contains("profile capture:"));
    }
}

#[test]
fn collisions_and_occupied_paths_never_replace_files_or_publish_a_profile() {
    for destination in [
        "layouts/work-002.yaml",
        "layouts/work-001.yml",
        "profiles/work.yaml",
        "profiles/work.yml",
    ] {
        for kind in ["file", "symlink", "directory"] {
            let fixture = CliFixture::new();
            fs::create_dir(fixture.path.join("layouts")).unwrap();
            fs::create_dir(fixture.path.join("profiles")).unwrap();
            let path = fixture.path.join(destination);
            match kind {
                "file" => fs::write(&path, "original").unwrap(),
                "symlink" => symlink("missing", &path).unwrap(),
                "directory" => fs::create_dir(&path).unwrap(),
                _ => unreachable!(),
            }
            let output = run_capture(&fixture, tree(), &["work", "1", "3"], 1);
            assert!(String::from_utf8_lossy(&output.stderr).contains("already exists"));
            if kind == "file" {
                assert_eq!(fs::read_to_string(&path).unwrap(), "original");
            }
            assert!(!fixture.path.join("layouts/work-001.yaml").exists());
        }
    }
}

#[test]
fn directory_and_staging_errors_expose_no_completed_yaml() {
    for blocked in ["layouts", "profiles"] {
        let fixture = CliFixture::new();
        fixture.file(blocked, "not a directory");
        run_capture(&fixture, tree(), &["work", "1"], 1);
        assert!(!fixture.path.join("profiles/work.yaml").exists());
        assert!(!fixture.path.join("layouts/work-001.yaml").exists());
    }
}

#[test]
fn inference_uses_saved_window_pids_and_keeps_unresolved_selectors() {
    for infer_commands in [true, false] {
        let fixture = CliFixture::new();
        let mut tree = tree();
        // Only this test process is inspected, never another application.
        tree["nodes"][0]["nodes"][1]["nodes"][0]["pid"] = json!(std::process::id());
        tree["nodes"][0]["nodes"][1]["nodes"][0]["app_id"] = Value::Null;
        let mut arguments = vec!["work", "3"];
        if !infer_commands {
            arguments.push("--no-infer-commands");
        }
        let output = run_capture(&fixture, tree, &arguments, 0);
        let yaml = fs::read_to_string(fixture.path.join("layouts/work-001.yaml")).unwrap();
        let document: Value = serde_saphyr::from_str(&yaml).unwrap();
        assert_eq!(
            document["layout"]["children"][0]["command"].is_array(),
            infer_commands
        );
        assert!(document["layout"]["children"][0]["match"].is_null());
        assert!(document["layout"]["children"][1]["command"].is_null());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(if infer_commands {
                "Commands inferred: 1; unresolved: 1"
            } else {
                "unresolved commands: 2"
            })
        );
    }
}

#[test]
fn private_directory_permission_failures_preserve_files_and_expose_no_profile() {
    for blocked in ["layouts", "profiles"] {
        let fixture = CliFixture::new();
        let directory = fixture.path.join(blocked);
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o500)).unwrap();
        let output = run_capture(&fixture, tree(), &["work", "1"], 1);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(String::from_utf8_lossy(&output.stderr).contains("no configuration published"));
        assert!(!fixture.path.join("profiles/work.yaml").exists());
        assert!(!fixture.path.join("layouts/work-001.yaml").exists());
    }
}

#[test]
fn all_workspace_count_limit_precedes_any_files() {
    let fixture = CliFixture::new();
    let mut tree = tree();
    tree["nodes"][0]["nodes"] = json!(
        (1..=257)
            .map(|number| {
                populated_workspace(
                    1000 + i64::from(number) * 3,
                    number,
                    &number.to_string(),
                    number == 1,
                )
            })
            .collect::<Vec<_>>()
    );
    let output = run_capture(&fixture, tree, &["work", "--all"], 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("1 to 256"));
    assert!(!fixture.path.join("layouts").exists());
    assert!(!fixture.path.join("profiles").exists());
}

#[test]
fn inactive_nested_tabs_and_stacks_keep_non_first_selections_and_final_focus() {
    let fixture = CliFixture::new();
    let mut tree = tree();
    let mut stack = sway_node(
        120,
        "con",
        960,
        vec![
            tiled_window(121, "stack-first", false),
            tiled_window(122, "stack-second", false),
        ],
    );
    stack["layout"] = json!("stacked");
    stack["focus"] = json!([122, 121]);
    let mut tabs = sway_node(
        130,
        "con",
        960,
        vec![tiled_window(111, "tab-first", false), stack],
    );
    tabs["layout"] = json!("tabbed");
    tabs["focus"] = json!([120, 111]);
    let workspace = &mut tree["nodes"][0]["nodes"][1];
    workspace["nodes"] = json!([tabs]);
    workspace["focus"] = json!([130]);
    run_capture(&fixture, tree, &["work", "3", "--no-infer-commands"], 0);
    let yaml = fs::read_to_string(fixture.path.join("layouts/work-001.yaml")).unwrap();
    let draft: Value = serde_saphyr::from_str(&yaml).unwrap();
    assert_eq!(draft["focus"], "window3");
    assert_eq!(draft["layout"]["group"], "tabbed");
    assert_eq!(draft["layout"]["active"], 2);
    assert_eq!(draft["layout"]["children"][1]["group"], "stacked");
    assert_eq!(draft["layout"]["children"][1]["active"], 2);
}
