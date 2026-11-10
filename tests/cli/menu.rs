use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

const CAPTURE: &str = "Capture current layout…\n";

fn setup(fixture: &Fixture) {
    fixture.file(
        "fuzzel",
        r#"#!/bin/sh
printf '%s\n' "$@" >> "$MENU_DIRECTORY/arguments"
prompt=''
output=''
keep_open=0
no_sort=0
for argument in "$@"; do
    case "$argument" in
        --prompt=*|--prompt-only=*) prompt="$argument" ;;
        --output=*) output="${argument#--output=}" ;;
        --dmenu) ;;
        --no-exit-on-keyboard-focus-loss) keep_open=1 ;;
        --no-sort) no_sort=1 ;;
        *) exit 92 ;;
    esac
done
test "$keep_open" = 1 || exit 95
printf '%s\n' "$output" >> "$MENU_DIRECTORY/outputs"
case "$prompt" in
  '--prompt-only=Draft name: ')
    test "$no_sort" = 0 || exit 96
    test -f "$MENU_DIRECTORY/snapshot-ready" || exit 91
    : > "$MENU_DIRECTORY/name-opened"
    printf '%b' "$NAME_OUTPUT"
    exit "$NAME_STATUS"
    ;;
  '--prompt=Layout: ')
    test "$no_sort" = 1 || exit 96
    if test "$REQUIRE_SNAPSHOT" = 1; then
        test -f "$MENU_DIRECTORY/snapshot-ready" || exit 94
    fi
    test "$(/bin/cat "$MENU_DIRECTORY/state/sway-stage/last-run.log")" = 'previous launch' || exit 93
    /bin/cat > "$MENU_DIRECTORY/options"
    printf '%s' "$$" > "$MENU_DIRECTORY/action-pid"
    printf '%b' "$MENU_OUTPUT"
    exit "$MENU_STATUS"
    ;;
  *) exit 92 ;;
esac
"#,
    );
    fixture.file(
        "notify-send",
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$MENU_DIRECTORY/notification\"\nexit \"$NOTIFY_STATUS\"\n",
    );
    for executable in ["fuzzel", "notify-send"] {
        fs::set_permissions(
            fixture.path.join(executable),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    fs::create_dir_all(fixture.path.join("state/sway-stage")).unwrap();
    fixture.file("state/sway-stage/last-run.log", "previous launch\n");
}

fn command(fixture: &Fixture, action_output: &str, name_output: &str) -> Command {
    let mut command = fixture.command();
    command
        .env("PATH", &fixture.path)
        .env("XDG_STATE_HOME", fixture.path.join("state"))
        .env("MENU_DIRECTORY", &fixture.path)
        .env("MENU_OUTPUT", action_output)
        .env("MENU_STATUS", "0")
        .env("NAME_OUTPUT", name_output)
        .env("NAME_STATUS", "0")
        .env("NOTIFY_STATUS", "0");
    command
}

fn assert_log_preserved(fixture: &Fixture) {
    assert_eq!(
        fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap(),
        "previous launch\n"
    );
}

fn capture_tree() -> Value {
    let mut window = sway_node(20, "con", 1920, vec![]);
    window["shell"] = json!("xdg_shell");
    window["app_id"] = json!("editor");
    window["focused"] = json!(true);
    // Read this test process's argv, never another application or the user's desktop.
    window["pid"] = json!(std::process::id());
    let workspace = sway_node(10, "workspace", 1920, vec![window]);
    let mut output = sway_node(2, "output", 1920, vec![workspace]);
    output["name"] = json!("DP-1");
    sway_node(1, "root", 1920, vec![output])
}

fn run_capture(fixture: &Fixture, command: &mut Command) -> Output {
    run_snapshot(fixture, command, capture_tree())
}

fn run_snapshot(fixture: &Fixture, command: &mut Command, tree: Value) -> Output {
    let socket = fixture.path.join("fixture.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = serve_capture(listener, tree, Some(fixture.path.clone()));
    let output = command
        .env("SWAYSOCK", &socket)
        .env("REQUIRE_SNAPSHOT", "1")
        .output()
        .unwrap();
    server.join().unwrap();
    assert_log_preserved(fixture);
    assert!(output.stdout.is_empty(), "{output:?}");
    output
}

#[test]
fn cancellation_lists_lexical_labels_without_parsing_or_side_effects() {
    let fixture = Fixture::new();
    setup(&fixture);
    fixture.file("writing.yaml", "malformed: [");
    fixture.file("browsing.yml", "not executable YAML");
    fixture.file("development.yaml", "");
    let output = command(&fixture, "", "")
        .env("MENU_STATUS", "2")
        .args(["--layouts-dir", ".", "menu"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(fixture.path.join("options")).unwrap(),
        format!("Load: browsing\nLoad: development\nLoad: writing\n{CAPTURE}")
    );
    assert_log_preserved(&fixture);
    assert!(!fixture.path.join(".config").exists());
    assert!(!fixture.path.join("notification").exists());
}

#[test]
fn selections_and_subprocess_errors_notify_without_starting_launch_diagnostics() {
    for (response, status, expected) in [
        ("writing\n", "0", "not offered"),
        ("Load: unknown\n", "0", "not offered"),
        ("Capture current layout\n", "0", "not offered"),
        (" Load: writing\n", "0", "not offered"),
        ("", "0", "newline-terminated"),
        ("Load: writing", "0", "newline-terminated"),
        ("Load: writing\nextra\n", "0", "single line"),
        ("Load: writing\r\n", "0", "control characters"),
        ("bad\\000text\n", "0", "control characters"),
        ("\\377\n", "0", "not UTF-8"),
        ("Load: writing\n", "2", "unexpected output while canceling"),
        ("", "1", "exit status: 1"),
        ("", "7", "exit status: 7"),
    ] {
        let fixture = Fixture::new();
        setup(&fixture);
        fixture.file("writing.yaml", WINDOW);
        let output = command(&fixture, response, "")
            .env("MENU_STATUS", status)
            .args(["menu", "--layouts-dir", "."])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{stderr}");
        let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
        assert!(notification.contains("sway-stage menu failed"));
        assert_log_preserved(&fixture);
        assert!(!fixture.path.join(".config").exists());
    }

    let fixture = Fixture::new();
    setup(&fixture);
    fs::remove_file(fixture.path.join("fuzzel")).unwrap();
    let output = command(&fixture, "", "").arg("menu").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("start Fuzzel"));
    assert_log_preserved(&fixture);
}

#[test]
fn exact_load_selection_uses_existing_validation_and_launch_diagnostics() {
    for infer_commands in [false, true] {
        let fixture = Fixture::new();
        setup(&fixture);
        fixture.file("writing.yaml", WINDOW);
        // Unselected layouts must not be parsed.
        fixture.file("broken.yaml", "malformed: [");
        let mut command = command(&fixture, "Load: writing\n", "");
        command.args(["--layouts-dir", ".", "menu"]);
        if infer_commands {
            command.arg("--infer-commands");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("SWAYSOCK is unset"));
        let log = fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
        assert!(log.contains("menu load"));
        assert!(log.contains("validated layout"));
        assert!(log.contains("SWAYSOCK is unset"));
        assert!(!log.contains("previous launch"));
        let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
        assert!(notification.contains("sway-stage menu failed"));
        assert!(!fixture.path.join("name-opened").exists());
    }
}

#[test]
fn capture_failure_with_no_layouts_never_opens_the_name_prompt() {
    let fixture = Fixture::new();
    setup(&fixture);
    let output = command(&fixture, CAPTURE, "unused\n")
        .arg("menu")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("capture: connect: SWAYSOCK is unset")
    );
    assert_eq!(
        fs::read_to_string(fixture.path.join("options")).unwrap(),
        CAPTURE
    );
    assert!(!fixture.path.join("name-opened").exists());
    assert!(!fixture.path.join(".config").exists());
    assert_log_preserved(&fixture);
}

#[test]
fn captures_save_complete_private_layouts_to_xdg_or_home_with_opt_in_inference() {
    for (base, infer_commands, notification_status) in [
        (None, false, "0"),
        (Some(""), false, "0"),
        (Some("xdg"), true, "1"),
    ] {
        let fixture = Fixture::new();
        setup(&fixture);
        // XDG_CONFIG_HOME is unset, set but empty, or an absolute base. The default
        // layouts directory and its parents do not exist before saving.
        let xdg_config_home = base.map(|base| {
            if base.is_empty() {
                PathBuf::new()
            } else {
                fixture.path.join(base)
            }
        });
        let config_home = match &xdg_config_home {
            Some(path) if !path.as_os_str().is_empty() => path.clone(),
            _ => fixture.path.join(".config"),
        };
        let mut command = command(&fixture, CAPTURE, "development\n");
        command
            .arg("menu")
            .env("NOTIFY_STATUS", notification_status);
        if let Some(xdg_config_home) = &xdg_config_home {
            command.env("XDG_CONFIG_HOME", xdg_config_home);
        }
        if infer_commands {
            command.arg("--infer-commands");
        }
        let output = run_capture(&fixture, &mut command);
        assert!(output.status.success(), "{output:?}");
        let path = config_home.join("sway-stage/layouts/development.yaml");
        let draft = fs::read_to_string(&path).unwrap();
        assert!(draft.starts_with("# Layout draft from `sway-stage capture`."));
        assert!(draft.contains("app_id: editor"));
        assert_eq!(
            fs::read_to_string(fixture.path.join("outputs")).unwrap(),
            "DP-1\nDP-1\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.path.join("arguments")).unwrap(),
            concat!(
                "--dmenu\n--no-exit-on-keyboard-focus-loss\n--no-sort\n",
                "--output=DP-1\n--prompt=Layout: \n",
                "--dmenu\n--no-exit-on-keyboard-focus-loss\n",
                "--output=DP-1\n--prompt-only=Draft name: \n",
            )
        );
        if infer_commands {
            assert!(!draft.lines().any(|line| line.trim() == "command: null"));
            assert!(draft.contains("Review recovered commands"));
        } else {
            assert!(draft.lines().any(|line| line.trim() == "command: null"));
            assert!(!draft.contains("Review recovered commands"));
        }
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for parent in [
            &config_home,
            &config_home.join("sway-stage"),
            path.parent().unwrap(),
        ] {
            assert_eq!(
                fs::metadata(parent).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert!(!fixture.path.join("development.yaml").exists());
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
        assert!(notification.contains("sway-stage draft saved"));
        assert!(notification.contains(path.to_str().unwrap()));
        assert!(!notification.contains("command:"));

        // Ordinary discovery in the same environment finds the saved layout.
        let mut list = fixture.command();
        if let Some(xdg_config_home) = &xdg_config_home {
            list.env("XDG_CONFIG_HOME", xdg_config_home);
        }
        let output = list.arg("list").output().unwrap();
        assert_eq!(output.stdout, b"development\n", "{output:?}");
    }
}

#[test]
fn name_prompt_cancellation_discards_capture_without_creating_directories() {
    let fixture = Fixture::new();
    setup(&fixture);
    let mut command = command(&fixture, CAPTURE, "");
    command.env("NAME_STATUS", "2").arg("menu");
    let output = run_capture(&fixture, &mut command);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    assert!(fixture.path.join("name-opened").exists());
    assert!(!fixture.path.join(".config").exists());
    assert!(!fixture.path.join("notification").exists());
}

#[test]
fn invalid_draft_names_and_name_prompt_failures_do_not_save() {
    for (name, status, expected) in [
        ("\n".to_owned(), "0", "draft name"),
        ("bad name\n".to_owned(), "0", "draft name"),
        ("../escape\n".to_owned(), "0", "draft name"),
        ("name.yaml\n".to_owned(), "0", "draft name"),
        (" trimmed\n".to_owned(), "0", "draft name"),
        ("_hidden\n".to_owned(), "0", "draft name"),
        (format!("{}\n", "a".repeat(251)), "0", "maximum length"),
        ("name\nextra\n".to_owned(), "0", "single line"),
        ("".to_owned(), "1", "exit status: 1"),
    ] {
        let fixture = Fixture::new();
        setup(&fixture);
        let mut command = command(&fixture, CAPTURE, &name);
        command.env("NAME_STATUS", status).arg("menu");
        let output = run_capture(&fixture, &mut command);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{output:?}"
        );
        assert!(!fixture.path.join(".config").exists());
        let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
        assert!(notification.contains("sway-stage menu failed"));
    }
}

#[test]
fn explicit_relative_layouts_dir_receives_captures_regardless_of_config_bases() {
    for config_bases_are_usable in [true, false] {
        let fixture = Fixture::new();
        setup(&fixture);
        let layouts = fixture.path.join("layouts");
        fs::create_dir(&layouts).unwrap();
        fs::set_permissions(&layouts, fs::Permissions::from_mode(0o750)).unwrap();
        fs::write(layouts.join("writing.yaml"), WINDOW).unwrap();

        // The relative directory resolves against the working directory, the fixture root.
        let mut command = command(&fixture, CAPTURE, "development\n");
        command.args(["--layouts-dir", "layouts", "menu"]);
        if config_bases_are_usable {
            command.env("XDG_CONFIG_HOME", fixture.path.join("xdg"));
        } else {
            command
                .env("XDG_CONFIG_HOME", "relative")
                .env_remove("HOME");
        }
        let output = run_capture(&fixture, &mut command);
        assert!(output.status.success(), "{output:?}");

        assert_eq!(
            fs::read_to_string(fixture.path.join("options")).unwrap(),
            format!("Load: writing\n{CAPTURE}")
        );
        let path = layouts.join("development.yaml");
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("app_id: editor"), "{saved}");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&layouts).unwrap().permissions().mode() & 0o777,
            0o750
        );
        assert_eq!(fs::read_dir(&layouts).unwrap().count(), 2);
        assert!(!fixture.path.join("xdg").exists());
        assert!(!fixture.path.join(".config").exists());
        let output = fixture.run(&["--layouts-dir", "layouts", "list"], 0);
        assert_eq!(output.stdout, b"development\nwriting\n");
    }

    // A missing explicit directory is still a discovery error, raised before either prompt.
    let fixture = Fixture::new();
    setup(&fixture);
    let mut command = command(&fixture, CAPTURE, "development\n");
    command.args(["--layouts-dir", "missing", "menu"]);
    let output = run_capture(&fixture, &mut command);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("read directory"));
    assert!(!fixture.path.join("options").exists());
    assert!(!fixture.path.join("missing").exists());
}

#[test]
fn captured_placeholders_fail_check_and_both_loads_before_contacting_sway() {
    let fixture = Fixture::new();
    setup(&fixture);
    let mut capture_command = command(&fixture, CAPTURE, "development\n");
    capture_command.arg("menu");
    let output = run_capture(&fixture, &mut capture_command);
    assert!(output.status.success(), "{output:?}");
    let layouts = fixture.path.join(".config/sway-stage/layouts");
    let path = layouts.join("development.yaml");
    let saved = fs::read_to_string(&path).unwrap();
    assert!(saved.lines().any(|line| line.trim() == "command: null"));

    // SWAYSOCK is unset: reaching IPC would report that instead of the placeholder.
    let output = fixture.run(&["check", "development"], 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("capture placeholder"));
    let output = fixture.run(&["load", "development"], 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("capture placeholder"), "{stderr}");
    assert!(!stderr.contains("SWAYSOCK"), "{stderr}");

    let output = command(&fixture, "Load: development\n", "")
        .arg("menu")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("capture placeholder"), "{stderr}");
    assert!(!stderr.contains("SWAYSOCK"), "{stderr}");
    let log = fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
    assert!(log.contains("menu load"), "{log}");
    assert!(!log.contains("validated layout"), "{log}");

    // Failed validation neither repairs nor removes the captured layout.
    assert_eq!(fs::read_to_string(&path).unwrap(), saved);
    assert_eq!(fs::read_dir(&layouts).unwrap().count(), 1);
}

#[test]
fn save_collisions_and_directory_failures_preserve_files_and_the_launch_log() {
    for obstacle in [
        "development.yaml",
        "development.yml",
        "dangling layouts link",
    ] {
        let fixture = Fixture::new();
        setup(&fixture);
        let layouts = fixture.path.join(".config/sway-stage/layouts");
        if obstacle == "dangling layouts link" {
            // Discovery sees an absent default directory, but creating it must fail.
            fs::create_dir_all(layouts.parent().unwrap()).unwrap();
            symlink("absent", &layouts).unwrap();
        } else {
            fs::create_dir_all(&layouts).unwrap();
            fs::write(layouts.join(obstacle), "preserve me").unwrap();
        }

        let mut command = command(&fixture, CAPTURE, "development\n");
        command.arg("menu");
        let output = run_capture(&fixture, &mut command);
        assert_eq!(output.status.code(), Some(1), "{output:?}");

        let stderr = String::from_utf8_lossy(&output.stderr);
        if obstacle == "dangling layouts link" {
            assert!(stderr.contains("create layouts directory"), "{stderr}");
            assert!(
                fs::symlink_metadata(&layouts)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert!(!layouts.parent().unwrap().join("absent").exists());
        } else {
            assert!(stderr.contains("already exists"), "{stderr}");
            assert_eq!(
                fs::read_to_string(layouts.join(obstacle)).unwrap(),
                "preserve me"
            );
            assert_eq!(fs::read_dir(&layouts).unwrap().count(), 1);
        }
        let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
        assert!(notification.contains("sway-stage menu failed"));
        assert!(!notification.contains("command:"));
    }
}

#[test]
fn saved_layout_and_output_are_fixed_before_either_prompt() {
    let fixture = Fixture::new();
    setup(&fixture);
    let mut tree = capture_tree();
    let workspace = &mut tree["nodes"][0]["nodes"][0];
    workspace["name"] = json!("original-workspace");
    let mut first = workspace["nodes"][0].clone();
    first["focused"] = json!(false);
    first["id"] = json!(21);
    first["app_id"] = json!("first");
    first["rect"]["width"] = json!(640);
    workspace["nodes"][0]["rect"]["width"] = json!(1280);
    workspace["nodes"].as_array_mut().unwrap().insert(0, first);
    // Literal output names remain a single argument, even with spaces or shell characters.
    tree["nodes"][0]["name"] = json!("original output;$literal");
    // Another monitor has a different visible workspace; it must not supply the output.
    let other_workspace = sway_node(11, "workspace", 1920, vec![]);
    let mut other_output = sway_node(3, "output", 1920, vec![other_workspace]);
    other_output["name"] = json!("other-output");
    tree["nodes"]
        .as_array_mut()
        .unwrap()
        .insert(0, other_output);
    let mut command = command(&fixture, CAPTURE, "saved\n");
    command.arg("menu");
    let output = run_snapshot(&fixture, &mut command, tree);
    assert!(output.status.success(), "{output:?}");
    let draft =
        fs::read_to_string(fixture.path.join(".config/sway-stage/layouts/saved.yaml")).unwrap();
    assert!(draft.contains("focus: window2"), "{draft}");
    assert!(draft.contains("weight: 33.33"));
    assert!(draft.contains("weight: 66.67"));
    assert!(draft.contains("app_id: first"));
    assert!(draft.contains("app_id: editor"));
    assert_eq!(
        fs::read_to_string(fixture.path.join("outputs")).unwrap(),
        "original output;$literal\noriginal output;$literal\n"
    );
}

#[test]
fn missing_output_does_not_change_capture_or_cancellation() {
    for selection in [CAPTURE, ""] {
        let fixture = Fixture::new();
        setup(&fixture);
        let mut tree = capture_tree();
        tree["nodes"][0]["name"] = Value::Null;
        let mut command = command(&fixture, selection, "saved\n");
        command.arg("menu");
        if selection.is_empty() {
            command.env("MENU_STATUS", "2");
        }
        let output = run_snapshot(&fixture, &mut command, tree);
        assert!(output.status.success(), "{output:?}");
        let arguments = fs::read_to_string(fixture.path.join("arguments")).unwrap();
        assert!(!arguments.contains("--output="));
        if selection.is_empty() {
            assert!(!fixture.path.join(".config").exists());
        } else {
            let draft =
                fs::read_to_string(fixture.path.join(".config/sway-stage/layouts/saved.yaml"))
                    .unwrap();
            assert!(draft.contains("app_id: editor"));
        }
    }
}

#[test]
fn failed_snapshot_is_reported_only_for_capture_and_is_never_retried() {
    for selection in [CAPTURE, "", "Load: writing\n"] {
        let fixture = Fixture::new();
        setup(&fixture);
        fixture.file("writing.yaml", "invalid layout contents");
        let socket = fixture.path.join("failed.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut query, _) = listener.accept().unwrap();
            query
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            assert_eq!(read_ipc_message(&mut query).0, 4);
            // Valid JSON, invalid tree: the original decode error must survive the menu.
            write_ipc_message(&mut query, 4, &json!({}));
            assert_eq!(query.read(&mut [0]).unwrap(), 0);
            listener
        });
        let mut command = command(&fixture, selection, "unused\n");
        command
            .env("SWAYSOCK", &socket)
            .args(["--layouts-dir", ".", "menu"]);
        if selection.is_empty() {
            command.env("MENU_STATUS", "2");
        }
        let output = command.output().unwrap();
        let listener = server.join().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        if selection == CAPTURE {
            assert_eq!(output.status.code(), Some(1));
            assert!(stderr.contains("capture: get tree:"), "{stderr}");
            assert_log_preserved(&fixture);
        } else if selection.is_empty() {
            assert!(output.status.success(), "{output:?}");
            assert!(stderr.is_empty());
            assert_log_preserved(&fixture);
        } else {
            assert_eq!(output.status.code(), Some(1));
            assert!(!stderr.contains("get tree:"), "{stderr}");
            assert!(stderr.contains("writing.yaml"), "{stderr}");
            let log =
                fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
            assert!(log.contains("validation"));
        }
        assert!(!fixture.path.join("name-opened").exists());
        assert!(!fixture.path.join(".config").exists());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn empty_or_unsupported_snapshot_allows_loading_with_fresh_observation() {
    for initial_layout in ["empty", "tabbed"] {
        let fixture = Fixture::new();
        setup(&fixture);
        fixture.file("writing.yaml", WINDOW);
        let mut initial = capture_tree();
        if initial_layout == "empty" {
            let workspace = &mut initial["nodes"][0]["nodes"][0];
            workspace["nodes"] = json!([]);
            workspace["focused"] = json!(true);
        } else {
            initial["nodes"][0]["nodes"][0]["layout"] = json!("tabbed");
        }
        let socket = fixture.path.join("load.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let directory = fixture.path.clone();
        let server = thread::spawn(move || {
            let accept = || {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
            };
            let mut query = accept();
            assert!(!directory.join("action-pid").exists());
            assert_eq!(read_ipc_message(&mut query).0, 4);
            fs::write(directory.join("snapshot-ready"), "").unwrap();
            write_ipc_message(&mut query, 4, &initial);
            assert_eq!(query.read(&mut [0]).unwrap(), 0);

            // The loader must subscribe and select a different workspace after Fuzzel exits.
            let mut events = accept();
            let pid = fs::read_to_string(directory.join("action-pid")).unwrap();
            assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
            assert_eq!(read_ipc_message(&mut events).0, 2);
            write_ipc_message(&mut events, 2, &json!({"success": true}));
            let mut query = accept();
            assert_eq!(read_ipc_message(&mut query).0, 1);
            write_ipc_message(
                &mut query,
                1,
                &json!([{
                    "id": 88, "num": 8, "name": "later", "visible": true, "focused": true,
                    "urgent": false, "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080},
                    "output": "another-output"
                }]),
            );
            assert_eq!(read_ipc_message(&mut query).0, 4);
            let mut later = capture_tree();
            later["nodes"][0]["nodes"][0]["id"] = json!(88);
            write_ipc_message(&mut query, 4, &later);
            let (kind, token) = read_ipc_message(&mut query);
            assert_eq!(kind, 10);
            write_ipc_message(
                &mut events,
                (1 << 31) | 7,
                &json!({"first": false, "payload": String::from_utf8(token).unwrap()}),
            );
            write_ipc_message(&mut query, 10, &json!({"success": true}));
            assert_eq!(events.read(&mut [0]).unwrap(), 0);
        });
        let output = command(&fixture, "Load: writing\n", "")
            .env("SWAYSOCK", &socket)
            .env("REQUIRE_SNAPSHOT", "1")
            .args(["--layouts-dir", ".", "menu", "--infer-commands"])
            .output()
            .unwrap();
        server.join().unwrap();
        // Refusal on the later occupied workspace proves the initial empty tree wasn't reused.
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let log = fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
        assert!(log.contains("prelaunch workspace=88"), "{log}");
        assert!(!fixture.path.join("name-opened").exists());
        assert_eq!(
            fs::read_to_string(fixture.path.join("outputs")).unwrap(),
            "DP-1\n"
        );
    }
}
