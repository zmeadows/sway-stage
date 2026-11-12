use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

const CAPTURE: &str = "Capture current layout…\n";

fn prepare_menu_fixture(fixture: &CliFixture) {
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
  '--prompt=Directory: ')
    test "$no_sort" = 1 || exit 96
    test "$(/bin/cat "$MENU_DIRECTORY/state/sway-stage/last-run.log")" = 'previous launch' || exit 93
    /bin/cat > "$MENU_DIRECTORY/directories"
    if test -n "$REMOVE_DIRECTORY"; then
        /bin/rmdir "$REMOVE_DIRECTORY" || exit 97
    fi
    printf '%b' "$DIRECTORY_OUTPUT"
    exit "$DIRECTORY_STATUS"
    ;;
  '--prompt=⊞ ')
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

fn menu_command(fixture: &CliFixture, action_output: &str, name_output: &str) -> Command {
    let mut menu_command = fixture.command();
    menu_command
        .env("PATH", &fixture.path)
        .env("XDG_STATE_HOME", fixture.path.join("state"))
        .env("MENU_DIRECTORY", &fixture.path)
        .env("MENU_OUTPUT", action_output)
        .env("MENU_STATUS", "0")
        .env("NAME_OUTPUT", name_output)
        .env("NAME_STATUS", "0")
        .env("DIRECTORY_OUTPUT", "gon\n")
        .env("DIRECTORY_STATUS", "0")
        .env_remove("REMOVE_DIRECTORY")
        .env("NOTIFY_STATUS", "0");
    menu_command
}

fn assert_log_preserved(fixture: &CliFixture) {
    assert_eq!(
        fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap(),
        "previous launch\n"
    );
}

pub(super) fn capture_tree() -> Value {
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

fn run_capture(fixture: &CliFixture, menu_command: &mut Command) -> Output {
    run_with_recorded_snapshot(fixture, menu_command, capture_tree())
}

fn empty_tree() -> Value {
    let mut tree = capture_tree();
    let workspace = &mut tree["nodes"][0]["nodes"][0];
    workspace["nodes"] = json!([]);
    workspace["focused"] = json!(true);
    tree
}

fn run_on_empty_workspace(fixture: &CliFixture, menu_command: &mut Command) -> Output {
    // Close the simulated compositor after eligibility, so a valid Load reaches its
    // ordinary connection failure without any possibility of contacting real Sway.
    let socket = fixture.path.join("empty.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let directory = fixture.path.clone();
    let server = thread::spawn(move || {
        let (mut query, _) = listener.accept().unwrap();
        drop(listener);
        query
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert!(!directory.join("action-pid").exists());
        assert_eq!(read_ipc_message(&mut query).0, 4);
        fs::write(directory.join("snapshot-ready"), "").unwrap();
        write_ipc_message(&mut query, 4, &empty_tree());
        assert_eq!(query.read(&mut [0]).unwrap(), 0);
    });
    let output = menu_command
        .env("SWAYSOCK", socket)
        .env("REQUIRE_SNAPSHOT", "1")
        .output()
        .unwrap();
    server.join().unwrap();
    output
}

fn run_with_recorded_snapshot(
    fixture: &CliFixture,
    menu_command: &mut Command,
    tree: Value,
) -> Output {
    let socket = fixture.path.join("fixture.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = serve_capture(listener, tree, Some(fixture.path.clone()));
    let output = menu_command
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
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    fixture.file("writing.yaml", "malformed: [");
    fixture.file("browsing.yml", "not executable YAML");
    fixture.file("development.yaml", "");
    let output = run_on_empty_workspace(
        &fixture,
        menu_command(&fixture, "", "")
            .env("MENU_STATUS", "2")
            .args(["--layouts-dir", ".", "menu"]),
    );

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(fixture.path.join("options")).unwrap(),
        "Load: browsing\nLoad: development\nLoad: writing\n"
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
        (CAPTURE, "0", "not offered"),
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
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        fixture.file("writing.yaml", SINGLE_WINDOW_YAML);
        let output = run_on_empty_workspace(
            &fixture,
            menu_command(&fixture, response, "")
                .env("MENU_STATUS", status)
                .args(["menu", "--layouts-dir", "."]),
        );
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{stderr}");
        let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
        assert!(notification.contains("sway-stage menu failed"));
        assert_log_preserved(&fixture);
        assert!(!fixture.path.join(".config").exists());
    }

    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    fs::remove_file(fixture.path.join("fuzzel")).unwrap();
    let output = run_capture(&fixture, menu_command(&fixture, "", "").arg("menu"));

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("start Fuzzel"));
    assert_log_preserved(&fixture);
}

#[test]
fn exact_load_selection_uses_existing_validation_and_launch_diagnostics() {
    for infer_commands in [false, true] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        fixture.file("writing.yaml", SINGLE_WINDOW_YAML);
        // Unselected layouts must not be parsed.
        fixture.file("broken.yaml", "malformed: [");
        let mut menu_command = menu_command(&fixture, "Load: writing\n", "");
        menu_command.args(["--layouts-dir", ".", "menu"]);
        if infer_commands {
            menu_command.arg("--infer-commands");
        }
        let output = run_on_empty_workspace(&fixture, &mut menu_command);
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("connect"));
        let log = fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
        assert!(log.contains("menu load"));
        assert!(log.contains("validated layout"));
        assert!(log.contains("connect"));
        assert!(!log.contains("previous launch"));
        let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
        assert!(notification.contains("sway-stage menu failed"));
        assert!(!fixture.path.join("name-opened").exists());
    }
}

#[test]
fn initial_snapshot_failure_never_opens_fuzzel() {
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    let output = menu_command(&fixture, CAPTURE, "unused\n")
        .arg("menu")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("pre-menu workspace snapshot: connect: SWAYSOCK is unset")
    );
    assert!(!fixture.path.join("arguments").exists());
    assert_log_preserved(&fixture);
}

#[test]
fn captures_save_complete_private_layouts_to_xdg_or_home_with_opt_in_inference() {
    for (base, infer_commands, notification_status) in [
        (None, false, "0"),
        (Some(""), false, "0"),
        (Some("xdg"), true, "1"),
    ] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
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
        let mut menu_command = menu_command(&fixture, CAPTURE, "development\n");
        menu_command
            .arg("menu")
            .env("NOTIFY_STATUS", notification_status);
        if let Some(xdg_config_home) = &xdg_config_home {
            menu_command.env("XDG_CONFIG_HOME", xdg_config_home);
        }
        if infer_commands {
            menu_command.arg("--infer-commands");
        }
        let output = run_capture(&fixture, &mut menu_command);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_to_string(fixture.path.join("options")).unwrap(),
            CAPTURE
        );
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
                "--output=DP-1\n--prompt=⊞ \n",
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
        if infer_commands {
            assert!(notification.contains("Commands inferred: 1; unresolved: 0."));
            assert!(notification.contains("Review best-effort commands before use."));
            let executable = std::env::current_exe().unwrap();
            assert!(!notification.contains(executable.to_str().unwrap()));
        } else {
            assert!(notification.contains("Command inference disabled; unresolved commands: 1."));
            assert!(notification.contains("Manual editing required."));
        }

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
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    let mut menu_command = menu_command(&fixture, CAPTURE, "");
    menu_command.env("NAME_STATUS", "2").arg("menu");
    let output = run_capture(&fixture, &mut menu_command);

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
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        let mut menu_command = menu_command(&fixture, CAPTURE, &name);
        menu_command.env("NAME_STATUS", status).arg("menu");
        let output = run_capture(&fixture, &mut menu_command);
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
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        let layouts = fixture.path.join("layouts");
        fs::create_dir(&layouts).unwrap();
        fs::set_permissions(&layouts, fs::Permissions::from_mode(0o750)).unwrap();
        fs::write(layouts.join("writing.yaml"), SINGLE_WINDOW_YAML).unwrap();

        // The relative directory resolves against the working directory, the fixture root.
        let mut menu_command = menu_command(&fixture, CAPTURE, "development\n");
        menu_command.args(["--layouts-dir", "layouts", "menu"]);
        if config_bases_are_usable {
            menu_command.env("XDG_CONFIG_HOME", fixture.path.join("xdg"));
        } else {
            menu_command
                .env("XDG_CONFIG_HOME", "relative")
                .env_remove("HOME");
        }
        let output = run_capture(&fixture, &mut menu_command);
        assert!(output.status.success(), "{output:?}");

        assert_eq!(
            fs::read_to_string(fixture.path.join("options")).unwrap(),
            CAPTURE
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

    // Capture availability and saving do not require discovering an existing directory.
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    let output = run_capture(
        &fixture,
        menu_command(&fixture, CAPTURE, "development\n").args(["--layouts-dir", "missing", "menu"]),
    );

    assert!(output.status.success(), "{output:?}");
    assert!(fixture.path.join("missing/development.yaml").exists());
}

#[test]
fn captured_placeholders_fail_check_and_both_loads_before_loader_ipc() {
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    let mut capture_command = menu_command(&fixture, CAPTURE, "development\n");
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

    fs::remove_file(fixture.path.join("action-pid")).unwrap();
    fixture.file("state/sway-stage/last-run.log", "previous launch\n");
    let output = run_on_empty_workspace(
        &fixture,
        menu_command(&fixture, "Load: development\n", "").arg("menu"),
    );
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
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        let layouts = fixture.path.join(".config/sway-stage/layouts");
        if obstacle == "dangling layouts link" {
            // Capture skips discovery, but creating this directory must fail.
            fs::create_dir_all(layouts.parent().unwrap()).unwrap();
            symlink("absent", &layouts).unwrap();
        } else {
            fs::create_dir_all(&layouts).unwrap();
            fs::write(layouts.join(obstacle), "preserve me").unwrap();
        }

        let mut menu_command = menu_command(&fixture, CAPTURE, "development\n");
        menu_command.arg("menu");
        let output = run_capture(&fixture, &mut menu_command);
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
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
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
    let mut menu_command = menu_command(&fixture, CAPTURE, "saved\n");
    menu_command.arg("menu");
    let output = run_with_recorded_snapshot(&fixture, &mut menu_command, tree);

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
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        let mut tree = capture_tree();
        tree["nodes"][0]["name"] = Value::Null;
        let mut menu_command = menu_command(&fixture, selection, "saved\n");
        menu_command.arg("menu");
        if selection.is_empty() {
            menu_command.env("MENU_STATUS", "2");
        }
        let output = run_with_recorded_snapshot(&fixture, &mut menu_command, tree);
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
fn failed_snapshot_prevents_all_selections_and_is_never_retried() {
    for selection in [CAPTURE, "", "Load: writing\n", "Load: valid\n"] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        fixture.file("writing.yaml", "invalid layout contents");
        fixture.file("valid.yaml", SINGLE_WINDOW_YAML);
        let socket = fixture.path.join("failed.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut query, _) = listener.accept().unwrap();
            query
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            assert_eq!(read_ipc_message(&mut query).0, 4);
            // Valid JSON, invalid tree: report the decode error before opening Fuzzel.
            write_ipc_message(&mut query, 4, &json!({}));
            assert_eq!(query.read(&mut [0]).unwrap(), 0);
            listener
        });
        let mut menu_command = menu_command(&fixture, selection, "unused\n");
        menu_command
            .env("SWAYSOCK", &socket)
            .args(["--layouts-dir", ".", "menu"]);
        if selection.is_empty() {
            menu_command.env("MENU_STATUS", "2");
        }
        let output = menu_command.output().unwrap();
        let listener = server.join().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1));
        assert!(
            stderr.contains("pre-menu workspace snapshot: get tree:"),
            "{stderr}"
        );
        assert_log_preserved(&fixture);
        assert!(!fixture.path.join("arguments").exists());
        assert!(!fixture.path.join("name-opened").exists());
        assert!(!fixture.path.join(".config").exists());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn menu_load_constrains_fresh_observation_to_original_workspace_identity() {
    for (initial_layout, final_id, final_name, select_directory) in [
        ("empty", 10, "original", false),
        ("tabbed", 10, "original", false),
        ("empty", 88, "later", false),
        ("empty", 88, "original", false),
        ("empty", 10, "original", true),
        ("empty", 88, "later", true),
        ("empty", 88, "original", true),
    ] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        fixture.file("writing.yaml", SINGLE_WINDOW_YAML);
        if select_directory {
            project_layout(&fixture, true);
        }
        let mut initial = empty_tree();
        initial["nodes"][0]["nodes"][0]["name"] = json!("original");
        if initial_layout == "tabbed" {
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

            // The loader subscribes after Fuzzel exits, then checks the saved identity
            // against the same synchronized observation that establishes its target.
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
                    "id": final_id, "num": 1, "name": final_name, "visible": true, "focused": true,
                    "urgent": false, "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080},
                    "output": "another-output"
                }]),
            );
            assert_eq!(read_ipc_message(&mut query).0, 4);
            let mut later = capture_tree();
            later["nodes"][0]["nodes"][0]["id"] = json!(final_id);
            later["nodes"][0]["nodes"][0]["name"] = json!(final_name);
            if final_id != 10 {
                let workspace = &mut later["nodes"][0]["nodes"][0];
                workspace["nodes"] = json!([]);
                workspace["focused"] = json!(true);
            }
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
        let output = menu_command(&fixture, "Load: writing\n", "")
            .env("SWAYSOCK", &socket)
            .env("REQUIRE_SNAPSHOT", "1")
            .args(["--layouts-dir", ".", "menu", "--infer-commands"])
            .output()
            .unwrap();
        server.join().unwrap();
        // Matching identity reaches ordinary occupancy validation; a changed identity
        // must refuse even with the same workspace name and number.
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let log = fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
        if final_id == 10 {
            assert!(log.contains("prelaunch workspace=10"), "{log}");
            assert!(log.contains("completely empty"), "{log}");
        } else {
            assert!(
                log.contains("differs from pre-menu workspace id=10"),
                "{log}"
            );
            assert!(!log.contains("prelaunch workspace="), "{log}");
        }
        assert!(!log.contains("launch leaf="), "{log}");
        assert!(!log.contains("command replies"), "{log}");
        let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
        assert!(notification.contains("sway-stage menu failed"));
        assert!(!fixture.path.join("name-opened").exists());
        assert_eq!(
            fs::read_to_string(fixture.path.join("outputs")).unwrap(),
            if select_directory {
                "DP-1\nDP-1\n"
            } else {
                "DP-1\n"
            }
        );
    }
}

#[test]
fn partial_inference_notification_identifies_the_saved_draft_and_unresolved_commands() {
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    let mut tree = capture_tree();
    let workspace = &mut tree["nodes"][0]["nodes"][0];
    let mut unresolved = workspace["nodes"][0].clone();
    unresolved["id"] = json!(21);
    unresolved["focused"] = json!(false);
    unresolved["pid"] = Value::Null;
    // Missing match information must not add to the unresolved command count.
    unresolved["app_id"] = Value::Null;
    workspace["nodes"].as_array_mut().unwrap().push(unresolved);
    let output = run_with_recorded_snapshot(
        &fixture,
        menu_command(&fixture, CAPTURE, "partial\n").args([
            "--layouts-dir",
            ".",
            "menu",
            "--infer-commands",
        ]),
        tree,
    );

    assert!(output.status.success(), "{output:?}");
    let path = fixture.path.join("partial.yaml");
    let draft = fs::read_to_string(&path).unwrap();
    assert_eq!(
        draft
            .lines()
            .filter(|line| line.trim() == "command: null")
            .count(),
        1
    );
    let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
    assert!(notification.contains("Commands inferred: 1; unresolved: 1. Manual editing required."));
    assert!(notification.contains("./partial.yaml"));
    assert!(!notification.contains(std::env::current_exe().unwrap().to_str().unwrap()));
}

// The selected path is deliberately distinct from both the root and the window override.
fn project_layout(fixture: &CliFixture, select: bool) {
    fs::create_dir_all(fixture.path.join("projects/gon")).unwrap();
    fixture.file(
        "writing.yaml",
        &format!("{SINGLE_WINDOW_YAML}cwd: ~/projects\ncwd_select: {select}\n"),
    );
}

#[test]
fn directory_prompt_lists_only_safe_immediate_directories_in_lexical_order() {
    use std::os::unix::ffi::OsStringExt;

    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    project_layout(&fixture, true);
    let root = fixture.path.join("projects");
    for name in [
        "zebra",
        "Alpha",
        ".hidden",
        "gon/grandchild",
        "two words",
        "bad\nname",
        "bad\tname",
    ] {
        fs::create_dir_all(root.join(name)).unwrap();
    }
    fs::create_dir(root.join(OsString::from_vec(vec![0xff]))).unwrap();
    fs::create_dir(fixture.path.join("external")).unwrap();
    symlink(fixture.path.join("external"), root.join("link")).unwrap();
    symlink("missing", root.join("broken")).unwrap();
    symlink("loop", root.join("loop")).unwrap();
    fixture.file("projects/file", "ordinary file");
    symlink("file", root.join("file-link")).unwrap();

    let output = run_on_empty_workspace(
        &fixture,
        menu_command(&fixture, "Load: writing\n", "")
            .env("DIRECTORY_OUTPUT", "")
            .env("DIRECTORY_STATUS", "2")
            .args(["--layouts-dir", ".", "menu"]),
    );

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        fs::read_to_string(fixture.path.join("directories")).unwrap(),
        "Alpha\ngon\nlink\ntwo words\nzebra\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.path.join("outputs")).unwrap(),
        "DP-1\nDP-1\n"
    );
}

#[test]
fn directory_cancellation_and_errors_preserve_the_launch_log() {
    for (response, status, expected) in [
        ("", "2", ""),
        ("unknown\n", "0", "not offered"),
        ("../gon\n", "0", "not offered"),
        (" gon\n", "0", "not offered"),
        ("gon", "0", "newline-terminated"),
        ("gon\nextra\n", "0", "single line"),
        ("gon\r\n", "0", "control characters"),
        ("\\377\n", "0", "not UTF-8"),
        ("gon\n", "2", "unexpected output while canceling"),
        ("", "1", "exit status: 1"),
    ] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        project_layout(&fixture, true);
        let output = run_on_empty_workspace(
            &fixture,
            menu_command(&fixture, "Load: writing\n", "")
                .env("DIRECTORY_OUTPUT", response)
                .env("DIRECTORY_STATUS", status)
                .args(["--layouts-dir", ".", "menu"]),
        );
        assert_eq!(output.status.success(), expected.is_empty(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{output:?}"
        );
        assert_log_preserved(&fixture);
        assert!(!fixture.path.join("name-opened").exists());
    }
}

#[test]
fn disabled_directory_selection_uses_the_single_prompt_loading_path() {
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    project_layout(&fixture, false);
    let output = run_on_empty_workspace(
        &fixture,
        menu_command(&fixture, "Load: writing\n", "").args(["--layouts-dir", ".", "menu"]),
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("connect"));
    assert!(!fixture.path.join("directories").exists());
    let log = fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
    assert!(log.contains("validated layout"));
}

#[test]
fn empty_root_and_disappearing_selection_never_reach_loader_ipc() {
    for remove_before_prompt in [true, false] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        project_layout(&fixture, true);
        let selected = fixture.path.join("projects/gon");
        let mut menu_command = menu_command(&fixture, "Load: writing\n", "");
        menu_command.args(["--layouts-dir", ".", "menu"]);
        if remove_before_prompt {
            fs::remove_dir(&selected).unwrap();
        } else {
            menu_command.env("REMOVE_DIRECTORY", &selected);
        }
        let output = run_on_empty_workspace(&fixture, &mut menu_command);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("SWAYSOCK"), "{stderr}");
        if remove_before_prompt {
            assert!(stderr.contains("no selectable directories"), "{stderr}");
            assert!(!fixture.path.join("directories").exists());
            assert_log_preserved(&fixture);
        } else {
            assert!(
                stderr.contains("inspect") && stderr.contains("projects/gon"),
                "{stderr}"
            );
            let log =
                fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
            assert!(!log.contains("subscribe"));
        }
    }
}

#[test]
fn directory_selection_reaches_child_launch_with_window_override_precedence() {
    for window_override in [false, true] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        project_layout(&fixture, true);
        fs::create_dir(fixture.path.join("override")).unwrap();
        fixture.file("app", "#!/bin/sh\nprintf '%s\n' \"$PWD\" > \"$RESULT\"\n");
        fs::set_permissions(fixture.path.join("app"), fs::Permissions::from_mode(0o700)).unwrap();
        let window_cwd = if window_override {
            "cwd: ~/override,"
        } else {
            ""
        };
        fixture.file(
            "writing.yaml",
            &format!(
                "version: 1
cwd: ~/projects
cwd_select: true
window_timeout_ms: 500
layout: {{id: t, command: ['{}'], {window_cwd} match: {{app_id: t}}, env: {{RESULT: '{}'}}}}
",
                fixture.path.join("app").display(),
                fixture.path.join("result").display()
            ),
        );

        let mut empty = capture_tree();
        empty["nodes"][0]["nodes"][0]["nodes"] = json!([]);
        empty["nodes"][0]["nodes"][0]["focused"] = json!(true);
        let socket = fixture.path.join("launch.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let accept = || {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
            };
            let mut query = accept();
            assert_eq!(read_ipc_message(&mut query).0, 4);
            write_ipc_message(&mut query, 4, &empty);
            assert_eq!(query.read(&mut [0]).unwrap(), 0);
            let mut events = accept();
            assert_eq!(read_ipc_message(&mut events).0, 2);
            write_ipc_message(&mut events, 2, &json!({"success": true}));
            // Initial and immediately-before-launch snapshots. Any layout mutation
            // instead of these read-only requests fails the fixture.
            for _ in 0..2 {
                let mut query = accept();
                assert_eq!(read_ipc_message(&mut query).0, 1);
                write_ipc_message(
                    &mut query,
                    1,
                    &json!([{
                        "id": 10, "num": 1, "name": "fixture", "visible": true, "focused": true,
                        "urgent": false, "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080},
                        "output": "DP-1"
                    }]),
                );
                assert_eq!(read_ipc_message(&mut query).0, 4);
                write_ipc_message(&mut query, 4, &empty);
                let (kind, token) = read_ipc_message(&mut query);
                assert_eq!(kind, 10);
                write_ipc_message(
                    &mut events,
                    (1 << 31) | 7,
                    &json!({"first": false, "payload": String::from_utf8(token).unwrap()}),
                );
                write_ipc_message(&mut query, 10, &json!({"success": true}));
            }
            // The harmless fixture child creates no window. Disconnect at observation
            // after launch; construction must stop without issuing any mutation.
            let mut query = accept();
            assert_eq!(read_ipc_message(&mut query).0, 1);
        });
        let output = menu_command(&fixture, "Load: writing\n", "")
            .env("SWAYSOCK", &socket)
            .args(["--layouts-dir", ".", "menu"])
            .output()
            .unwrap();
        server.join().unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let expected = fixture.path.join(if window_override {
            "override"
        } else {
            "projects/gon"
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while fs::read_to_string(fixture.path.join("result")).unwrap_or_default()
            != format!("{}\n", expected.display())
        {
            assert!(std::time::Instant::now() < deadline, "{output:?}");
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            fs::read_to_string(fixture.path.join("result")).unwrap(),
            format!("{}\n", expected.display())
        );
        assert_eq!(
            fs::read_to_string(fixture.path.join("outputs")).unwrap(),
            "DP-1\nDP-1\n"
        );
    }
}

#[test]
fn empty_workspace_without_layouts_reports_an_error_without_fuzzel() {
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    let output = run_on_empty_workspace(&fixture, menu_command(&fixture, "", "").arg("menu"));

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no layouts available"));
    assert!(!fixture.path.join("arguments").exists());
    assert!(fixture.path.join("notification").exists());
    assert_log_preserved(&fixture);
}

#[test]
fn capture_choices_skip_discovery_and_reject_unoffered_loads() {
    for (response, status) in [("", "2"), ("Load: writing\n", "0")] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        // Duplicate stems would fail discovery even though neither layout is needed.
        fixture.file("writing.yaml", "malformed: [");
        fixture.file("writing.yml", "");
        let output = run_capture(
            &fixture,
            menu_command(&fixture, response, "")
                .env("MENU_STATUS", status)
                .args(["--layouts-dir", ".", "menu"]),
        );
        assert_eq!(output.status.success(), status == "2", "{output:?}");
        assert_eq!(
            fs::read_to_string(fixture.path.join("options")).unwrap(),
            CAPTURE
        );
        if status == "0" {
            assert!(String::from_utf8_lossy(&output.stderr).contains("not offered"));
        }
        assert!(!fixture.path.join("name-opened").exists());
    }
}

#[test]
fn unrepresentable_workspaces_fail_before_fuzzel_with_capture_guidance() {
    for (arrangement, reason) in [
        ("floating", "tile or move"),
        ("fullscreen", "exit fullscreen"),
        ("empty container", "holds neither"),
        ("parent focus", "focus a window"),
        ("unsupported layout", "unsupported layout"),
        ("geometry", "unusable geometry"),
    ] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        fixture.file("writing.yaml", SINGLE_WINDOW_YAML);
        let mut tree = capture_tree();
        let workspace = &mut tree["nodes"][0]["nodes"][0];
        match arrangement {
            "floating" => {
                workspace["floating_nodes"] = workspace["nodes"].take();
                workspace["nodes"] = json!([]);
            }
            "fullscreen" => workspace["nodes"][0]["fullscreen_mode"] = json!(1),
            "empty container" => {
                workspace["nodes"] = json!([sway_node(21, "con", 1920, vec![])]);
                workspace["focused"] = json!(true);
            }
            "parent focus" => {
                workspace["nodes"][0]["focused"] = json!(false);
                workspace["focused"] = json!(true);
            }
            "unsupported layout" => workspace["layout"] = json!("none"),
            "geometry" => {
                let mut second = workspace["nodes"][0].clone();
                second["id"] = json!(21);
                second["focused"] = json!(false);
                second["rect"]["width"] = json!(0);
                workspace["nodes"].as_array_mut().unwrap().push(second);
            }
            _ => unreachable!(),
        }
        let output = run_with_recorded_snapshot(
            &fixture,
            menu_command(&fixture, CAPTURE, "unused\n").args([
                "--layouts-dir",
                ".",
                "menu",
                "--infer-commands",
            ]),
            tree,
        );
        assert_eq!(output.status.code(), Some(1), "{arrangement}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(reason),
            "{output:?}"
        );
        assert!(!fixture.path.join("arguments").exists());
        assert!(fixture.path.join("notification").exists());
        assert_log_preserved(&fixture);
    }
}
