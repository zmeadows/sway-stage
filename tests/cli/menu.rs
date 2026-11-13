use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

pub(super) fn prepare_menu_fixture(fixture: &CliFixture) {
    fixture.file(
        "fuzzel",
        r#"#!/bin/sh
printf '%s\n' "$@" >> "$MENU_DIRECTORY/arguments"
prompt=''
output=''
keep_open=0
no_sort=0
return_index=0
width=0
placeholder_value=0
for argument in "$@"; do
    if test "$placeholder_value" = 1; then
        placeholder_value=0
        continue
    fi
    case "$argument" in
        --prompt=*|--prompt-only=*) prompt="$argument" ;;
        --output=*) output="${argument#--output=}" ;;
        --dmenu) ;;
        --no-exit-on-keyboard-focus-loss) keep_open=1 ;;
        --no-sort) no_sort=1 ;;
        --index) return_index=1 ;;
        --width=80) width=80 ;;
        --placeholder) placeholder_value=1 ;;
        *) exit 92 ;;
    esac
done
test "$keep_open" = 1 || exit 95
printf '%s\n' "$output" >> "$MENU_DIRECTORY/outputs"
case "$prompt" in
  '--prompt-only=Capture layout as: '|'--prompt-only=Capture profile as: ')
    test "$width" = 80 || exit 96
    test "$no_sort" = 0 || exit 96
    test -f "$MENU_DIRECTORY/snapshot-ready" || exit 91
    test "$(/bin/cat "$MENU_DIRECTORY/state/sway-stage/last-run.log")" = 'previous launch' || exit 93
    : > "$MENU_DIRECTORY/name-opened"
    if test -f "$MENU_DIRECTORY/retry.desktop"; then
        : > "$MENU_DIRECTORY/retry.desktop"
    fi
    attempt=1
    if test -f "$MENU_DIRECTORY/name-attempt"; then
        read -r attempt < "$MENU_DIRECTORY/name-attempt"
        attempt=$((attempt + 1))
    fi
    printf '%s\n' "$attempt" > "$MENU_DIRECTORY/name-attempt"
    if test "$attempt" = 1; then
        printf '%b' "$NAME_OUTPUT"
        exit "$NAME_STATUS"
    fi
    if test -f "$MENU_DIRECTORY/name-$attempt"; then
        /bin/cat "$MENU_DIRECTORY/name-$attempt"
        exit 0
    fi
    exit 2
    ;;
  '--prompt=Launch in: ')
    test "$width" = 0 || exit 96
    test "$return_index" = 1 || exit 96
    test "$no_sort" = 1 || exit 96
    test "$(/bin/cat "$MENU_DIRECTORY/state/sway-stage/last-run.log")" = 'previous launch' || exit 93
    /bin/cat > "$MENU_DIRECTORY/directories"
    if test -n "$REMOVE_DIRECTORY"; then
        /bin/rmdir "$REMOVE_DIRECTORY" || exit 97
    fi
    printf '%b' "$DIRECTORY_OUTPUT"
    exit "$DIRECTORY_STATUS"
    ;;
  '--prompt=Sway Stage: ')
    test "$return_index" = 1 || exit 96
    test "$width" = 0 || exit 96
    test "$no_sort" = 1 || exit 96
    if test "$REQUIRE_SNAPSHOT" = 1; then
        test -f "$MENU_DIRECTORY/snapshot-ready" || exit 94
    fi
    test "$(/bin/cat "$MENU_DIRECTORY/state/sway-stage/last-run.log")" = 'previous launch' || exit 93
    /bin/cat > "$MENU_DIRECTORY/options"
    if test -n "$REPLACE_LAYOUT"; then
        /bin/cp "$MENU_DIRECTORY/replacement" "$REPLACE_LAYOUT" || exit 98
    fi
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
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$MENU_DIRECTORY/notification\"\nexit \"$NOTIFY_STATUS\"\n",
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

pub(super) fn menu_command(
    fixture: &CliFixture,
    selection_output: &str,
    name_output: &str,
) -> Command {
    let mut menu_command = fixture.command();
    menu_command
        .env("PATH", &fixture.path)
        .env("XDG_STATE_HOME", fixture.path.join("state"))
        .env("MENU_DIRECTORY", &fixture.path)
        .env("MENU_OUTPUT", selection_output)
        .env("MENU_STATUS", "0")
        .env("NAME_OUTPUT", name_output)
        .env("NAME_STATUS", "0")
        .env("DIRECTORY_OUTPUT", "1\n")
        .env("DIRECTORY_STATUS", "0")
        .env_remove("REMOVE_DIRECTORY")
        .env_remove("REPLACE_LAYOUT")
        .env("NOTIFY_STATUS", "0");
    menu_command
}

fn assert_log_preserved(fixture: &CliFixture) {
    assert_eq!(
        fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap(),
        "previous launch\n"
    );
}

fn assert_preload_notification(fixture: &CliFixture, expected_body: &str) {
    let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
    assert!(
        notification.ends_with(&format!("\n{expected_body}\n")),
        "{notification}"
    );
    assert!(!notification.contains("last-run.log"), "{notification}");
    assert!(
        !notification.contains(fixture.path.to_str().unwrap()),
        "{notification}"
    );
    assert_eq!(notification.matches("--app-name=sway-stage").count(), 1);
    assert_log_preserved(fixture);
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
fn cancellation_lists_validated_labels_in_name_order_without_side_effects() {
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    fixture.file("writing.yaml", "malformed: [");
    fixture.file(
        "weighted.yaml",
        &SINGLE_WINDOW_YAML.replace("{id:", "{weight: 1, id:"),
    );
    fixture.file("bad name.yaml", SINGLE_WINDOW_YAML);
    fixture.file("duplicate.yaml", SINGLE_WINDOW_YAML);
    fixture.file("duplicate.yml", SINGLE_WINDOW_YAML);
    fixture.file("browsing.yml", SINGLE_WINDOW_YAML);
    fixture.file("development.yaml", SINGLE_WINDOW_YAML);
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
        "Layout: browsing\nLayout: development\n[invalid] Layout: weighted\n[invalid] Layout: writing\n"
    );
    assert_log_preserved(&fixture);
    assert!(!fixture.path.join(".config").exists());
    assert!(!fixture.path.join("notification").exists());
}

#[test]
fn only_filename_problems_report_no_layouts_without_opening_fuzzel() {
    for filenames in [vec!["bad name.yaml"], vec!["same.yaml", "same.yml"]] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        for name in filenames {
            fixture.file(name, SINGLE_WINDOW_YAML);
        }
        let output = run_on_empty_workspace(
            &fixture,
            menu_command(&fixture, "0\n", "").args(["--layouts-dir", ".", "menu"]),
        );
        assert_eq!(output.status.code(), Some(1));
        assert!(!fixture.path.join("options").exists());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("warning:"));
        assert_preload_notification(
            &fixture,
            "No saved layouts found. Add a YAML file to your layouts directory.",
        );
        assert_log_preserved(&fixture);
    }
}

#[test]
fn all_valid_layouts_keep_distinct_labels_and_cancel_without_notification() {
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    for name in ["z.yaml", "a.yml"] {
        fixture.file(name, SINGLE_WINDOW_YAML);
    }
    let output = run_on_empty_workspace(
        &fixture,
        menu_command(&fixture, "", "")
            .env("MENU_STATUS", "2")
            .args(["--layouts-dir", ".", "menu"]),
    );
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(fixture.path.join("options")).unwrap(),
        "Layout: a\nLayout: z\n"
    );
    assert!(!fixture.path.join("notification").exists());
    assert_log_preserved(&fixture);
}

#[test]
fn invalid_contents_are_marked_and_refused_before_loading() {
    let invalid_layouts = [
        "malformed: [".to_owned(),
        format!("{SINGLE_WINDOW_YAML}focus: missing\n"),
        SINGLE_WINDOW_YAML.replace("[does-not-need-to-exist]", "null"),
        format!("{SINGLE_WINDOW_YAML}cwd: /does-not-exist-sway-stage\n"),
        format!("{SINGLE_WINDOW_YAML}unexpected: true\n"),
    ];
    for contents in invalid_layouts {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        fixture.file("a-invalid.yaml", &contents);
        fixture.file("z-valid.yaml", SINGLE_WINDOW_YAML);
        let output = run_on_empty_workspace(
            &fixture,
            menu_command(&fixture, "0\n", "").args(["--layouts-dir", ".", "menu"]),
        );
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert_eq!(
            fs::read_to_string(fixture.path.join("options")).unwrap(),
            "[invalid] Layout: a-invalid\nLayout: z-valid\n"
        );
        let explanation =
            "Layout 'a-invalid' is invalid. Run sway-stage check a-invalid for details.";
        assert!(String::from_utf8_lossy(&output.stderr).contains(explanation));
        assert_preload_notification(&fixture, explanation);
        assert_eq!(
            fs::read_to_string(fixture.path.join("a-invalid.yaml")).unwrap(),
            contents
        );
    }
}

#[test]
fn all_invalid_layouts_report_check_guidance_without_opening_fuzzel() {
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    fixture.file("syntax.yaml", "malformed: [");
    fixture.file(
        "draft.yaml",
        &SINGLE_WINDOW_YAML.replace("[does-not-need-to-exist]", "null"),
    );
    let output = run_on_empty_workspace(
        &fixture,
        menu_command(&fixture, "0\n", "").args(["--layouts-dir", ".", "menu"]),
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(!fixture.path.join("arguments").exists());
    let explanation = "No valid layouts found. Run sway-stage check NAME to inspect a layout.";
    assert!(String::from_utf8_lossy(&output.stderr).contains(explanation));
    assert_preload_notification(&fixture, explanation);
}

#[test]
fn changed_layout_is_validated_again_after_selection_before_loader_ipc() {
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    fixture.file("writing.yaml", SINGLE_WINDOW_YAML);
    fixture.file("replacement", "malformed: [");
    let output = run_on_empty_workspace(
        &fixture,
        menu_command(&fixture, "0\n", "")
            .env("REPLACE_LAYOUT", fixture.path.join("writing.yaml"))
            .args(["--layouts-dir", ".", "menu"]),
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(
        fs::read_to_string(fixture.path.join("options")).unwrap(),
        "Layout: writing\n"
    );
    let log = fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
    assert_log_preserved(&fixture);
    assert!(String::from_utf8_lossy(&output.stderr).contains("sway-stage check writing"));
    assert!(!log.contains("validated layout"), "{log}");
    assert!(!log.contains("subscribe"), "{log}");
}

#[test]
fn selections_and_subprocess_errors_notify_without_starting_launch_diagnostics() {
    for (response, status, expected) in [
        ("Load: writing\n", "0", "not offered"),
        ("[invalid] writing\n", "0", "not offered"),
        ("unknown\n", "0", "not offered"),
        ("-1\n", "0", "not offered"),
        ("999999999999999999999999\n", "0", "not offered"),
        ("1\n", "0", "not offered"),
        ("Capture current layout\n", "0", "not offered"),
        ("Capture current layout…\n", "0", "not offered"),
        (" writing\n", "0", "not offered"),
        ("", "0", "newline-terminated"),
        ("writing", "0", "newline-terminated"),
        ("writing\nextra\n", "0", "single line"),
        ("writing\r\n", "0", "control characters"),
        ("bad\\000text\n", "0", "control characters"),
        ("\\377\n", "0", "not UTF-8"),
        ("writing\n", "2", "unexpected output while canceling"),
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
        assert_preload_notification(
            &fixture,
            "Fuzzel did not return a valid choice. Check its configuration.",
        );
        assert!(!fixture.path.join(".config").exists());
    }

    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    fs::remove_file(fixture.path.join("fuzzel")).unwrap();
    let output = run_capture(&fixture, menu_command(&fixture, "0\n", "").arg("menu"));

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("start Fuzzel"));
    assert_preload_notification(
        &fixture,
        "Cannot start Fuzzel. Check that Fuzzel 1.11+ is installed and executable.",
    );
}

#[test]
fn exact_load_selection_uses_existing_validation_and_launch_diagnostics() {
    for infer_commands in [false, true] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        fixture.file("writing.yaml", SINGLE_WINDOW_YAML);
        // Invalid unselected layouts do not prevent a valid selection from loading.
        fixture.file("broken.yaml", "malformed: [");
        let mut menu_command = menu_command(&fixture, "1\n", "");
        menu_command.args(["--layouts-dir", ".", "menu"]);
        if !infer_commands {
            menu_command.arg("--no-infer-commands");
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
        assert!(
            notification
                .contains("Unable to communicate with Sway.\n\nSee last-run.log for details.")
        );
        assert!(!fixture.path.join("name-opened").exists());
    }
}

#[test]
fn initial_snapshot_failure_never_opens_fuzzel() {
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    let output = menu_command(&fixture, "0\n", "unused\n")
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
fn captures_save_complete_private_layouts_to_xdg_or_home_with_default_or_disabled_inference() {
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
        let mut menu_command = menu_command(&fixture, "0\n", "development\n");
        menu_command
            .arg("menu")
            .env("NOTIFY_STATUS", notification_status);
        if let Some(xdg_config_home) = &xdg_config_home {
            menu_command.env("XDG_CONFIG_HOME", xdg_config_home);
        }
        if !infer_commands {
            menu_command.arg("--no-infer-commands");
        }
        let output = run_capture(&fixture, &mut menu_command);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_to_string(fixture.path.join("options")).unwrap(),
            "Capture current workspace…\n"
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
                "--dmenu\n--no-exit-on-keyboard-focus-loss\n--no-sort\n--index\n--output=DP-1\n--prompt=Sway Stage: \n",
                "--dmenu\n--no-exit-on-keyboard-focus-loss\n--width=80\n--output=DP-1\n--prompt-only=Capture layout as: \n",
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
        assert!(notification.ends_with("\nSaved draft 'development'.\n"));
        assert!(!notification.contains("command:"));
        assert!(!notification.contains("Commands inferred"));
        assert!(!notification.contains("Review"));
        let executable = std::env::current_exe().unwrap();
        assert!(!notification.contains(executable.to_str().unwrap()));

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
    let mut menu_command = menu_command(&fixture, "0\n", "");
    menu_command.env("NAME_STATUS", "2").arg("menu");
    let output = run_capture(&fixture, &mut menu_command);

    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    assert!(fixture.path.join("name-opened").exists());
    assert!(!fixture.path.join(".config").exists());
    assert!(!fixture.path.join("notification").exists());
}

#[test]
fn invalid_draft_names_retry_and_can_be_canceled_without_saving() {
    for (name, expected) in [
        ("".to_owned(), "(empty name) is invalid"),
        ("bad name".to_owned(), "bad name is invalid"),
        ("../escape".to_owned(), "../escape is invalid"),
        ("name.yaml".to_owned(), "name.yaml is invalid"),
        (" trimmed".to_owned(), " trimmed is invalid"),
        ("_hidden".to_owned(), "_hidden is invalid"),
        ("a".repeat(251), "maximum length is 250 ASCII characters"),
    ] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        let mut menu_command = menu_command(&fixture, "0\n", &format!("{name}\n"));
        menu_command.arg("menu");
        let output = run_capture(&fixture, &mut menu_command);
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let arguments = fs::read_to_string(fixture.path.join("arguments")).unwrap();
        assert!(arguments.contains(expected), "{arguments}");
        assert!(arguments.contains("--placeholder\n"), "{arguments}");
        assert!(!arguments.contains("--mesg"));
        assert!(!fixture.path.join(".config").exists());
        assert!(!fixture.path.join("notification").exists());
        assert_log_preserved(&fixture);
    }
}

#[test]
fn name_prompt_operational_failures_do_not_retry() {
    for (name, status, expected) in [
        ("name\nextra\n", "0", "single line"),
        ("name\t\n", "0", "control characters"),
        ("", "1", "exit status: 1"),
    ] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        let mut menu_command = menu_command(&fixture, "0\n", name);
        menu_command.env("NAME_STATUS", status).arg("menu");
        let output = run_capture(&fixture, &mut menu_command);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{output:?}"
        );
        assert_eq!(
            fs::read_to_string(fixture.path.join("name-attempt")).unwrap(),
            "1\n"
        );
        assert!(!fixture.path.join(".config").exists());
        assert_log_preserved(&fixture);
    }
}

#[test]
fn repeated_name_retries_save_the_original_snapshot() {
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    let layouts = fixture.path.join("layouts");
    fs::create_dir(&layouts).unwrap();
    fs::write(layouts.join("development.yml"), "original").unwrap();
    fixture.file("name-2", "development\n");
    fixture.file("name-3", "work.yaml\n");
    fixture.file("name-4", "accepted\n");
    let mut menu_command = menu_command(&fixture, "0\n", "My Workspace\n");
    menu_command.args(["--layouts-dir", "layouts", "menu"]);
    // Infer a private launcher before the name prompt empties it. Repeating inference
    // during a name retry would leave a null command instead of the original launcher.
    fixture.file("retry.desktop", "[Desktop Entry]\nExec=unused\n");
    let mut process = Command::new("/bin/sh")
        .args([
            "-c",
            "export GIO_LAUNCHED_DESKTOP_FILE_PID=$$; exec /bin/cat",
        ])
        .env(
            "GIO_LAUNCHED_DESKTOP_FILE",
            fixture.path.join("retry.desktop"),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let process_id = process.id();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let pid_marker = format!("GIO_LAUNCHED_DESKTOP_FILE_PID={process_id}");
    let ready = loop {
        let environment = fs::read(format!("/proc/{process_id}/environ")).unwrap();
        if environment
            .split(|byte| *byte == 0)
            .any(|entry| entry == pid_marker.as_bytes())
        {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(5));
    };
    let mut tree = capture_tree();
    tree["nodes"][0]["nodes"][0]["nodes"][0]["pid"] = json!(process_id);
    let workspace = &mut tree["nodes"][0]["nodes"][0];
    let mut first = workspace["nodes"][0].clone();
    first["id"] = json!(21);
    first["focused"] = json!(false);
    workspace["nodes"].as_array_mut().unwrap().insert(0, first);
    workspace["layout"] = json!("tabbed");
    workspace["focus"] = json!([20, 21]);

    // The fake compositor accepts exactly one GET_TREE and closes after the snapshot.
    let output = run_with_recorded_snapshot(&fixture, &mut menu_command, tree);
    // EOF terminates only this test-owned process; no desktop application is involved.
    drop(process.stdin.take());
    assert!(process.wait().unwrap().success());
    assert!(ready, "test process did not publish launcher metadata");
    assert!(output.status.success(), "{output:?}");
    let saved = fs::read_to_string(layouts.join("accepted.yaml")).unwrap();
    assert!(saved.contains("active: 2"), "{saved}");
    assert!(saved.contains("focus: window2"), "{saved}");
    assert!(saved.contains("app_id: editor"), "{saved}");
    assert!(
        !saved.lines().any(|line| line.trim() == "command: null"),
        "{saved}"
    );
    assert!(saved.contains("Review recovered commands"), "{saved}");
    assert!(saved.contains("command: [gio, launch,"), "{saved}");
    assert_eq!(fs::read(fixture.path.join("retry.desktop")).unwrap(), b"");
    assert_eq!(
        fs::read_to_string(layouts.join("development.yml")).unwrap(),
        "original"
    );
    let arguments = fs::read_to_string(fixture.path.join("arguments")).unwrap();
    assert_eq!(
        arguments
            .matches("--prompt-only=Capture layout as: ")
            .count(),
        4
    );
    assert_eq!(arguments.matches("--width=80\n").count(), 4);
    assert_eq!(arguments.matches("--placeholder\n").count(), 3);
    assert!(arguments.contains("--placeholder\nMy Workspace is invalid — use [A-Za-z0-9][A-Za-z0-9_-]* without spaces or an extension\n"));
    assert!(arguments.contains("--placeholder\ndevelopment already exists — enter another name\n"));
    assert!(arguments.contains("--placeholder\nwork.yaml is invalid — use [A-Za-z0-9][A-Za-z0-9_-]* without spaces or an extension\n"));
    assert!(!arguments.contains("--mesg"));
    assert_eq!(fs::read_dir(&layouts).unwrap().count(), 2);
    assert_log_preserved(&fixture);
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
        let mut menu_command = menu_command(&fixture, "0\n", "development\n");
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
            "Capture current workspace…\n"
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
        menu_command(&fixture, "0\n", "development\n").args(["--layouts-dir", "missing", "menu"]),
    );

    assert!(output.status.success(), "{output:?}");
    assert!(fixture.path.join("missing/development.yaml").exists());
}

#[test]
fn captured_placeholders_fail_check_and_both_loads_before_loader_ipc() {
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    let mut capture_command = menu_command(&fixture, "0\n", "development\n");
    capture_command.args(["menu", "--no-infer-commands"]);
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

    fixture.file("state/sway-stage/last-run.log", "previous launch\n");
    fs::remove_file(fixture.path.join("notification")).unwrap();
    let capture_arguments = fs::read_to_string(fixture.path.join("arguments")).unwrap();
    fs::remove_file(fixture.path.join("action-pid")).unwrap();
    let output = run_on_empty_workspace(&fixture, menu_command(&fixture, "0\n", "").arg("menu"));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("No valid layouts found"), "{stderr}");
    assert_eq!(
        fs::read_to_string(fixture.path.join("arguments")).unwrap(),
        capture_arguments
    );
    assert_preload_notification(
        &fixture,
        "No valid layouts found. Run sway-stage check NAME to inspect a layout.",
    );

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

        let mut menu_command = menu_command(&fixture, "0\n", "development\n");
        menu_command.arg("menu");
        let output = run_capture(&fixture, &mut menu_command);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if obstacle == "dangling layouts link" {
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            assert_eq!(
                fs::read_to_string(fixture.path.join("name-attempt")).unwrap(),
                "1\n"
            );
            assert!(stderr.contains("create layouts directory"), "{stderr}");
            assert!(
                fs::symlink_metadata(&layouts)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert!(!layouts.parent().unwrap().join("absent").exists());
        } else {
            assert!(output.status.success(), "{output:?}");
            assert!(stderr.is_empty(), "{stderr}");
            let arguments = fs::read_to_string(fixture.path.join("arguments")).unwrap();
            assert!(arguments.contains("development already exists — enter another name"));
            assert_eq!(
                fs::read_to_string(layouts.join(obstacle)).unwrap(),
                "preserve me"
            );
            assert_eq!(fs::read_dir(&layouts).unwrap().count(), 1);
        }
        if obstacle == "dangling layouts link" {
            let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
            assert!(notification.contains("sway-stage menu failed"));
            assert!(!notification.contains("command:"));
            assert_preload_notification(
                &fixture,
                "Could not save the captured layout. Check the layouts directory, permissions, and available storage.",
            );
        } else {
            assert!(!fixture.path.join("notification").exists());
        }
        assert_log_preserved(&fixture);
    }
}

#[test]
fn saved_layout_and_output_are_fixed_before_the_capture_prompt() {
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
    let mut menu_command = menu_command(&fixture, "0\n", "saved\n");
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
    for name in ["saved\n", ""] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        let mut tree = capture_tree();
        tree["nodes"][0]["name"] = Value::Null;
        let mut menu_command = menu_command(&fixture, "0\n", name);
        menu_command.arg("menu");
        if name.is_empty() {
            menu_command.env("NAME_STATUS", "2");
        }
        let output = run_with_recorded_snapshot(&fixture, &mut menu_command, tree);
        assert!(output.status.success(), "{output:?}");
        let arguments = fs::read_to_string(fixture.path.join("arguments")).unwrap();
        assert!(!arguments.contains("--output="));
        if name.is_empty() {
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
fn failed_snapshot_without_profiles_prevents_selection_and_is_never_retried() {
    for selection in ["", "writing\n", "valid\n"] {
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
            menu_command.env("MENU_STATUS", "2").env("MENU_OUTPUT", "");
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
        let output = menu_command(&fixture, "0\n", "")
            .env("SWAYSOCK", &socket)
            .env("REQUIRE_SNAPSHOT", "1")
            .args(["--layouts-dir", ".", "menu"])
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
fn partial_inference_keeps_unresolved_commands_and_the_short_save_notification() {
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
        menu_command(&fixture, "0\n", "partial\n").args(["--layouts-dir", ".", "menu"]),
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
    assert!(notification.ends_with("\nSaved draft 'partial'.\n"));
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

    for absolute_path in [false, true] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        project_layout(&fixture, true);
        let root = fixture.path.join("projects");
        let configured_path = if absolute_path {
            root.to_str().unwrap()
        } else {
            "~/projects"
        };
        fixture.file(
            "writing.yaml",
            &format!("{SINGLE_WINDOW_YAML}cwd: '{configured_path}'\ncwd_select: true\n"),
        );
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
            menu_command(&fixture, "0\n", "")
                .env("DIRECTORY_OUTPUT", "")
                .env("DIRECTORY_STATUS", "2")
                .args(["--layouts-dir", ".", "menu"]),
        );

        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_to_string(fixture.path.join("directories")).unwrap(),
            format!("Base directory ({configured_path})\nAlpha\ngon\nlink\ntwo words\nzebra\n")
        );
        assert_eq!(
            fs::read_to_string(fixture.path.join("outputs")).unwrap(),
            "DP-1\nDP-1\n"
        );
    }
}

#[test]
fn directory_cancellation_and_errors_preserve_the_launch_log() {
    for (response, status, expected) in [
        ("", "2", ""),
        ("unknown\n", "0", "not offered"),
        ("../gon\n", "0", "not offered"),
        (" gon\n", "0", "not offered"),
        ("-1\n", "0", "not offered"),
        ("2\n", "0", "not offered"),
        ("Base directory (~/projects)\n", "0", "not offered"),
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
            menu_command(&fixture, "0\n", "")
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
        if expected.is_empty() {
            assert!(output.stderr.is_empty());
            assert!(!fixture.path.join("notification").exists());
        }
    }
}

#[test]
fn disabled_directory_selection_uses_the_single_prompt_loading_path() {
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    project_layout(&fixture, false);
    let output = run_on_empty_workspace(
        &fixture,
        menu_command(&fixture, "0\n", "").args(["--layouts-dir", ".", "menu"]),
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("connect"));
    assert!(!fixture.path.join("directories").exists());
    let log = fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
    assert!(log.contains("validated layout"));
}

#[test]
fn root_without_eligible_children_still_opens_the_picker_and_can_cancel() {
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    project_layout(&fixture, true);
    fs::remove_dir(fixture.path.join("projects/gon")).unwrap();
    fs::create_dir(fixture.path.join("projects/.hidden")).unwrap();
    fixture.file("projects/file", "ordinary file");

    let output = run_on_empty_workspace(
        &fixture,
        menu_command(&fixture, "0\n", "")
            .env("DIRECTORY_OUTPUT", "")
            .env("DIRECTORY_STATUS", "2")
            .args(["--layouts-dir", ".", "menu"]),
    );
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(fixture.path.join("directories")).unwrap(),
        "Base directory (~/projects)\n"
    );
    assert_log_preserved(&fixture);
    assert!(!fixture.path.join("notification").exists());
}

#[test]
fn disappearing_child_selection_never_reaches_loader_ipc() {
    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    project_layout(&fixture, true);
    let output = run_on_empty_workspace(
        &fixture,
        menu_command(&fixture, "0\n", "")
            .env("REMOVE_DIRECTORY", fixture.path.join("projects/gon"))
            .args(["--layouts-dir", ".", "menu"]),
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("inspect") && stderr.contains("projects/gon"),
        "{stderr}"
    );
    assert!(!stderr.contains("SWAYSOCK"), "{stderr}");
    let log = fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
    assert!(!log.contains("subscribe"));
}

#[test]
fn successful_menu_load_is_silent_and_preserves_window_directory_precedence() {
    for (directory_index, selected_subdirectory, window_override) in [
        ("0", None, false),
        ("0", None, true),
        ("1", Some("gon"), false),
        ("1", Some("gon"), true),
        ("1", Some("Base directory (~projects)"), false),
    ] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        project_layout(&fixture, true);
        if let Some(child_name) = selected_subdirectory {
            fs::create_dir_all(fixture.path.join("projects").join(child_name)).unwrap();
        } else {
            fs::remove_dir(fixture.path.join("projects/gon")).unwrap();
        }
        fixture.file("bad name.yaml", SINGLE_WINDOW_YAML);
        fixture.file("duplicate.yaml", SINGLE_WINDOW_YAML);
        fixture.file("duplicate.yml", SINGLE_WINDOW_YAML);
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
            let mut populated = capture_tree();
            populated["nodes"][0]["nodes"][0]["nodes"][0]["app_id"] = json!("t");
            // Initial/prelaunch emptiness, two acceptance observations, then the
            // final focus command's before/after observations. All windows are recorded.
            for snapshot_index in 0..6 {
                if snapshot_index == 5 {
                    let mut command = accept();
                    let (kind, payload) = read_ipc_message(&mut command);
                    assert_eq!(kind, 0);
                    assert_eq!(String::from_utf8(payload).unwrap(), "[con_id=20] focus");
                    write_ipc_message(&mut command, 0, &json!([{"success": true}]));
                }

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
                let tree = if snapshot_index < 2 {
                    &empty
                } else {
                    &populated
                };
                write_ipc_message(&mut query, 4, tree);
                let (kind, token) = read_ipc_message(&mut query);
                assert_eq!(kind, 10);
                write_ipc_message(
                    &mut events,
                    (1 << 31) | 7,
                    &json!({"first": false, "payload": String::from_utf8(token).unwrap()}),
                );
                write_ipc_message(&mut query, 10, &json!({"success": true}));
            }
            assert_eq!(events.read(&mut [0]).unwrap(), 0);
        });
        let output = menu_command(&fixture, "0\n", "")
            .env("DIRECTORY_OUTPUT", format!("{directory_index}\n"))
            .env("SWAYSOCK", &socket)
            .args(["--layouts-dir", ".", "menu"])
            .output()
            .unwrap();
        server.join().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(
            output.stdout.is_empty() && output.stderr.is_empty(),
            "{output:?}"
        );
        assert!(!fixture.path.join("notification").exists());
        let log = fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
        assert!(log.contains("final focus verified"), "{log}");
        let expected = if window_override {
            fixture.path.join("override")
        } else {
            let root = fixture.path.join("projects");
            match selected_subdirectory {
                Some(name) => root.join(name),
                None => root,
            }
        };
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
    let output = run_on_empty_workspace(&fixture, menu_command(&fixture, "0\n", "").arg("menu"));

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no layouts available"));
    assert!(!fixture.path.join("arguments").exists());
    assert_preload_notification(
        &fixture,
        "No saved layouts found. Add a YAML file to your layouts directory.",
    );
}

#[test]
fn capture_action_skips_layout_discovery() {
    for (name, status) in [("", "2"), ("saved\n", "0")] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        // Ambiguous layouts are irrelevant to capturing a new draft.
        fixture.file("writing.yaml", "malformed: [");
        fixture.file("writing.yml", "");
        let output = run_capture(
            &fixture,
            menu_command(&fixture, "0\n", name)
                .env("NAME_STATUS", status)
                .args(["--layouts-dir", ".", "menu"]),
        );
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_to_string(fixture.path.join("options")).unwrap(),
            "Capture current workspace…\n"
        );
        assert!(fixture.path.join("name-opened").exists());
        assert_eq!(fixture.path.join("saved.yaml").exists(), status == "0");
        assert_eq!(
            fs::read_to_string(fixture.path.join("arguments")).unwrap(),
            concat!(
                "--dmenu\n--no-exit-on-keyboard-focus-loss\n--no-sort\n--index\n--output=DP-1\n--prompt=Sway Stage: \n",
                "--dmenu\n--no-exit-on-keyboard-focus-loss\n--width=80\n--output=DP-1\n--prompt-only=Capture layout as: \n",
            )
        );
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
        ("tab selection", "child-focus history"),
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
            "geometry" | "tab selection" => {
                let mut second = workspace["nodes"][0].clone();
                second["id"] = json!(21);
                second["focused"] = json!(false);
                second["rect"]["width"] = json!(0);
                workspace["nodes"].as_array_mut().unwrap().push(second);
                if arrangement == "tab selection" {
                    workspace["layout"] = json!("tabbed");
                    workspace["focus"] = json!([]);
                }
            }
            _ => unreachable!(),
        }
        let output = run_with_recorded_snapshot(
            &fixture,
            menu_command(&fixture, "0\n", "unused\n").args(["--layouts-dir", ".", "menu"]),
            tree,
        );
        assert_eq!(output.status.code(), Some(1), "{arrangement}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(reason),
            "{output:?}"
        );
        assert!(!fixture.path.join("arguments").exists());
        assert!(output.stdout.is_empty());
        assert!(!fixture.path.join("unused.yaml").exists());
        assert!(!fs::read_dir(&fixture.path).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".partial")
        }));
        assert_preload_notification(
            &fixture,
            "Cannot capture this workspace. Check floating or fullscreen windows, containers, and window focus.",
        );
    }
}

fn add_menu_profile(fixture: &CliFixture, name: &str, contents: &str) {
    fs::create_dir_all(fixture.path.join("profiles")).unwrap();
    fixture.file(&format!("profiles/{name}"), contents);
}

const MENU_PROFILE: &str = "version: 1\nworkspaces: [{workspace: music, layout: shared}]\n";

#[test]
fn unified_actions_follow_workspace_eligibility_and_name_order() {
    for state in ["empty", "capturable", "unsupported", "failed"] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        fixture.file("zebra.yaml", SINGLE_WINDOW_YAML);
        fixture.file("shared.yaml", SINGLE_WINDOW_YAML);
        add_menu_profile(&fixture, "zebra.yaml", MENU_PROFILE);
        add_menu_profile(&fixture, "shared.yml", MENU_PROFILE);
        let mut tree = capture_tree();
        let expected = match state {
            "empty" => {
                tree = empty_tree();
                "Layout: shared\nLayout: zebra\nProfile: shared\nProfile: zebra\n"
            }
            "capturable" => "Capture current workspace…\nProfile: shared\nProfile: zebra\n",
            "unsupported" => {
                tree["nodes"][0]["nodes"][0]["nodes"][0]["fullscreen_mode"] = json!(1);
                "Profile: shared\nProfile: zebra\n"
            }
            "failed" => {
                tree = json!({});
                "Profile: shared\nProfile: zebra\n"
            }
            _ => unreachable!(),
        };
        let output = run_with_recorded_snapshot(
            &fixture,
            menu_command(&fixture, "", "")
                .env("MENU_STATUS", "2")
                .args(["--layouts-dir", ".", "--profiles-dir", "profiles", "menu"]),
            tree,
        );
        assert!(output.status.success(), "{state}: {output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        assert_eq!(
            fs::read_to_string(fixture.path.join("options")).unwrap(),
            expected
        );
        assert!(!fixture.path.join("notification").exists());
    }
}

#[test]
fn profile_discovery_marks_invalid_profiles_and_references_but_skips_bad_filenames() {
    use std::os::unix::ffi::OsStringExt;

    let fixture = CliFixture::new();
    prepare_menu_fixture(&fixture);
    fixture.file("shared.yaml", SINGLE_WINDOW_YAML);
    fixture.file("broken.yaml", "malformed: [");
    for (filename, contents) in [
        ("valid.yml", MENU_PROFILE),
        ("broken.yaml", "malformed: ["),
        (
            "reference.yaml",
            "version: 1\nworkspaces: [{workspace: 3, layout: broken}]\n",
        ),
        ("duplicate.yaml", MENU_PROFILE),
        ("duplicate.yml", MENU_PROFILE),
        ("bad name.yaml", MENU_PROFILE),
        ("_hidden.yaml", MENU_PROFILE),
        ("ignored.YAML", MENU_PROFILE),
    ] {
        add_menu_profile(&fixture, filename, contents);
    }
    let profiles = fixture.path.join("profiles");
    symlink(profiles.join("valid.yml"), profiles.join("link.yaml")).unwrap();
    fs::create_dir(profiles.join("nested.yaml")).unwrap();
    fs::write(
        profiles.join(OsString::from_vec(vec![0xff, b'.', b'y', b'm', b'l'])),
        MENU_PROFILE,
    )
    .unwrap();
    let output = run_on_empty_workspace(
        &fixture,
        menu_command(&fixture, "", "")
            .env("MENU_STATUS", "2")
            .args(["--layouts-dir", ".", "--profiles-dir", "profiles", "menu"]),
    );
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(
        fs::read_to_string(fixture.path.join("options")).unwrap(),
        "[invalid] Layout: broken\nLayout: shared\n[invalid] Profile: broken\n[invalid] Profile: reference\nProfile: valid\n"
    );
    assert_log_preserved(&fixture);
}

#[test]
fn invalid_profile_selections_preserve_logs_and_offer_profile_check_guidance() {
    for contents in [
        "malformed: [",
        "version: 1\nworkspaces: [{workspace: 3, layout: missing}]\n",
    ] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        fixture.file("shared.yaml", SINGLE_WINDOW_YAML);
        add_menu_profile(&fixture, "broken.yaml", contents);
        let output = run_on_empty_workspace(
            &fixture,
            menu_command(&fixture, "1\n", "").args([
                "--layouts-dir",
                ".",
                "--profiles-dir",
                "profiles",
                "menu",
            ]),
        );
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let explanation =
            "Profile 'broken' is invalid. Run sway-stage profile check broken for details.";
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(explanation),
            "{output:?}"
        );
        assert_preload_notification(&fixture, explanation);
    }
}

#[test]
fn changed_profile_or_referenced_layout_is_revalidated_before_loading() {
    for replaced_path in ["profiles/work.yaml", "shared.yaml"] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        fixture.file("shared.yaml", SINGLE_WINDOW_YAML);
        add_menu_profile(&fixture, "work.yaml", MENU_PROFILE);
        fixture.file("replacement", "malformed: [");
        let output = run_on_empty_workspace(
            &fixture,
            menu_command(&fixture, "1\n", "")
                .env("REPLACE_LAYOUT", fixture.path.join(replaced_path))
                .args(["--layouts-dir", ".", "--profiles-dir", "profiles", "menu"]),
        );
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("sway-stage profile check work"),
            "{output:?}"
        );
        assert_preload_notification(
            &fixture,
            "Profile 'work' is invalid. Run sway-stage profile check work for details.",
        );
    }
}

#[test]
fn profile_discovery_uses_xdg_home_and_explicit_directories_without_writes() {
    for base in [".config", "xdg", "profiles"] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        fixture.file("shared.yaml", SINGLE_WINDOW_YAML);
        let profile_directory = if base == "profiles" {
            "profiles".to_owned()
        } else {
            format!("{base}/sway-stage/profiles")
        };
        fs::create_dir_all(fixture.path.join(&profile_directory)).unwrap();
        fixture.file(&format!("{profile_directory}/work.yaml"), MENU_PROFILE);
        let mut process = menu_command(&fixture, "", "");
        process
            .env("MENU_STATUS", "2")
            .args(["--layouts-dir", ".", "menu"]);
        if base == "xdg" {
            process.env("XDG_CONFIG_HOME", fixture.path.join(base));
        }
        if base == "profiles" {
            process.args(["--profiles-dir", "profiles"]);
        }
        // No usable initial workspace: only profiles can be offered.
        let output = process.output().unwrap();
        assert!(output.status.success(), "{base}: {output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        assert_eq!(
            fs::read_to_string(fixture.path.join("options")).unwrap(),
            "Profile: work\n"
        );
        assert_log_preserved(&fixture);
        assert!(!fixture.path.join("notification").exists());
        assert_eq!(
            fs::read_dir(fixture.path.join(profile_directory))
                .unwrap()
                .count(),
            1
        );
    }
}

#[test]
fn unusable_profile_directories_do_not_suppress_layouts_or_capture() {
    for directory in ["missing", "file", "loop"] {
        for populated in [false, true] {
            let fixture = CliFixture::new();
            prepare_menu_fixture(&fixture);
            fixture.file("shared.yaml", SINGLE_WINDOW_YAML);
            fixture.file("file", "not a directory");
            symlink("loop", fixture.path.join("loop")).unwrap();
            let output = run_with_recorded_snapshot(
                &fixture,
                menu_command(&fixture, "", "")
                    .env("MENU_STATUS", "2")
                    .args(["--layouts-dir", ".", "--profiles-dir", directory, "menu"]),
                if populated {
                    capture_tree()
                } else {
                    empty_tree()
                },
            );
            assert!(output.status.success(), "{output:?}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains(directory), "{stderr}");
            assert_eq!(
                fs::read_to_string(fixture.path.join("options")).unwrap(),
                if populated {
                    "Capture current workspace…\n"
                } else {
                    "Layout: shared\n"
                }
            );
            assert_log_preserved(&fixture);
            assert!(!fixture.path.join("notification").exists());
        }
    }
}

#[test]
fn no_usable_profiles_reports_diagnostics_without_opening_fuzzel() {
    for directory in ["missing", "profiles"] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        if directory == "profiles" {
            add_menu_profile(&fixture, "broken.yaml", "malformed: [");
        }
        let output = menu_command(&fixture, "0\n", "")
            .args(["--profiles-dir", directory, "menu"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(if directory == "missing" {
                "read directory"
            } else {
                "sway-stage profile check NAME"
            }),
            "{stderr}"
        );
        assert_log_preserved(&fixture);
        assert!(!fixture.path.join("arguments").exists());
        assert_eq!(
            fs::read_to_string(fixture.path.join("notification"))
                .unwrap()
                .matches("--app-name=sway-stage")
                .count(),
            1
        );
    }
}

#[test]
fn identical_layout_and_profile_names_dispatch_by_index() {
    for selection in ["0\n", "1\n"] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        fixture.file("shared.yaml", SINGLE_WINDOW_YAML);
        add_menu_profile(&fixture, "shared.yaml", MENU_PROFILE);
        let output = run_on_empty_workspace(
            &fixture,
            menu_command(&fixture, selection, "").args([
                "--layouts-dir",
                ".",
                "--profiles-dir",
                "profiles",
                "menu",
            ]),
        );
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let log = fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
        assert!(
            log.contains(if selection == "0\n" {
                "menu load"
            } else {
                "completed 0 workspaces"
            }),
            "{log}"
        );
        let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
        assert!(
            notification.contains(if selection == "0\n" {
                "sway-stage menu failed"
            } else {
                "sway-stage profile load failed"
            }),
            "{notification}"
        );
        assert_eq!(notification.matches("--app-name=sway-stage").count(), 1);
    }
}

#[test]
fn capture_selection_with_profiles_preserves_inference_flag_and_original_snapshot() {
    for infer_commands in [false, true] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        fixture.file("shared.yaml", SINGLE_WINDOW_YAML);
        add_menu_profile(&fixture, "work.yaml", MENU_PROFILE);
        let mut process = menu_command(&fixture, "0\n", "saved\n");
        process.args(["--layouts-dir", ".", "--profiles-dir", "profiles", "menu"]);
        if !infer_commands {
            process.arg("--no-infer-commands");
        }
        let output = run_capture(&fixture, &mut process);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_to_string(fixture.path.join("options")).unwrap(),
            "Capture current workspace…\nProfile: work\n"
        );
        let saved = fs::read_to_string(fixture.path.join("saved.yaml")).unwrap();
        assert_eq!(
            saved.lines().any(|line| line.trim() == "command: null"),
            !infer_commands
        );
        assert!(saved.contains("app_id: editor"));
        assert_log_preserved(&fixture);
    }
}

// Realistic ordinary workspace identities are required by profile capture. The older
// single-workspace fixtures deliberately omit these unrelated selection fields.
fn profile_capture_tree(focused_empty: bool) -> Value {
    let mut tree = capture_tree();
    let workspace = &mut tree["nodes"][0]["nodes"][0];
    workspace["name"] = json!("3:work");
    workspace["num"] = json!(3);
    workspace["focus"] = json!([20]);
    let mut named = workspace.clone();
    named["id"] = json!(30);
    named["name"] = json!("music player");
    named["num"] = json!(-1);
    named["focus"] = json!([40]);
    named["nodes"][0]["id"] = json!(40);
    named["nodes"][0]["focused"] = json!(false);
    tree["nodes"][0]["nodes"]
        .as_array_mut()
        .unwrap()
        .push(named);
    if focused_empty {
        tree["nodes"][0]["nodes"][0]["nodes"][0]["focused"] = json!(false);
        let mut empty = sway_node(50, "workspace", 1920, vec![]);
        empty["name"] = json!("1");
        empty["num"] = json!(1);
        empty["focused"] = json!(true);
        tree["nodes"][0]["nodes"]
            .as_array_mut()
            .unwrap()
            .push(empty);
    }
    tree
}

#[test]
fn profile_capture_eligibility_keeps_flat_order_and_independent_loading_choices() {
    for focused_empty in [false, true] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        fixture.file("shared.yaml", SINGLE_WINDOW_YAML);
        add_menu_profile(&fixture, "work.yaml", MENU_PROFILE);
        let output = run_with_recorded_snapshot(
            &fixture,
            menu_command(&fixture, "", "")
                .env("MENU_STATUS", "2")
                .args(["--layouts-dir", ".", "--profiles-dir", "profiles", "menu"]),
            profile_capture_tree(focused_empty),
        );
        assert!(output.status.success(), "{output:?}");
        let options = fs::read_to_string(fixture.path.join("options")).unwrap();
        assert_eq!(
            options,
            if focused_empty {
                "Capture all populated workspaces…\nLayout: shared\nProfile: work\n"
            } else {
                "Capture current workspace…\nCapture all populated workspaces…\nProfile: work\n"
            }
        );
        assert!(!fixture.path.join("notification").exists());
    }
}

#[test]
fn profile_capture_reuses_snapshot_orders_targets_and_respects_inference_and_directories() {
    for infer_commands in [false, true] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        let mut process = menu_command(&fixture, "1\n", "work\n");
        process.args(["--layouts-dir", "drafts", "--profiles-dir", "saved", "menu"]);
        if !infer_commands {
            process.arg("--no-infer-commands");
        }
        // serve_capture accepts exactly one GET_TREE and closes its listener. Any
        // post-menu snapshot query therefore fails instead of supplying another tree.
        let output =
            run_with_recorded_snapshot(&fixture, &mut process, profile_capture_tree(false));
        assert!(output.status.success(), "{output:?}");
        let profile: Value = serde_saphyr::from_str(
            &fs::read_to_string(fixture.path.join("saved/work.yaml")).unwrap(),
        )
        .unwrap();
        for (index, target) in [json!(3), json!("music player")].iter().enumerate() {
            let layout_name = format!("work-{:03}", index + 1);
            assert_eq!(profile["workspaces"][index]["workspace"], *target);
            assert_eq!(profile["workspaces"][index]["layout"], layout_name);
            let draft: Value = serde_saphyr::from_str(
                &fs::read_to_string(fixture.path.join(format!("drafts/{layout_name}.yaml")))
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(draft["layout"]["command"].is_array(), infer_commands);
            assert_eq!(draft["layout"]["match"]["app_id"], "editor");
        }
        let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
        assert!(notification.contains("Saved profile 'work' (2 workspaces)."));
        assert_eq!(notification.matches("--app-name=sway-stage").count(), 1);
        assert!(!fixture.path.join(".config").exists());
    }
}

#[test]
fn profile_capture_retries_only_invalid_names_and_preflight_conflicts() {
    for conflict in ["profiles/taken.yml", "layouts/taken-002.yml"] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        fs::create_dir(fixture.path.join("profiles")).unwrap();
        fs::create_dir(fixture.path.join("layouts")).unwrap();
        fixture.file(conflict, "original");
        fixture.file("name-2", &format!("{}\n", "a".repeat(247)));
        fixture.file("name-3", "taken\n");
        fixture.file("name-4", "saved\n");
        let output = run_with_recorded_snapshot(
            &fixture,
            menu_command(&fixture, "1\n", "bad.name\n").args([
                "--layouts-dir",
                "layouts",
                "--profiles-dir",
                "profiles",
                "menu",
            ]),
            profile_capture_tree(false),
        );
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_to_string(fixture.path.join(conflict)).unwrap(),
            "original"
        );
        assert!(fixture.path.join("profiles/saved.yaml").exists());
        assert!(!fixture.path.join("layouts/taken-001.yaml").exists());
        let arguments = fs::read_to_string(fixture.path.join("arguments")).unwrap();
        assert_eq!(
            arguments
                .matches("--prompt-only=Capture profile as: ")
                .count(),
            4
        );
        assert!(arguments.contains("at most 246"));
        assert!(arguments.contains("already exists — enter another name"));
    }
}

#[test]
fn profile_capture_cancellation_preserves_files_log_and_notifications() {
    for cancel_selector in [false, true] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        let mut process = menu_command(&fixture, if cancel_selector { "" } else { "1\n" }, "");
        process.args([
            "--layouts-dir",
            "layouts",
            "--profiles-dir",
            "profiles",
            "menu",
        ]);
        process.env(
            if cancel_selector {
                "MENU_STATUS"
            } else {
                "NAME_STATUS"
            },
            "2",
        );
        let output =
            run_with_recorded_snapshot(&fixture, &mut process, profile_capture_tree(false));
        assert!(output.status.success(), "{output:?}");
        assert!(!fixture.path.join("layouts").exists());
        assert!(!fixture.path.join("profiles").exists());
        assert!(!fixture.path.join("notification").exists());
    }
}

#[test]
fn unsupported_profile_capture_keeps_unrelated_actions_and_never_skips_workspaces() {
    for invalid in ["floating", "identity"] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        add_menu_profile(&fixture, "work.yaml", MENU_PROFILE);
        let mut tree = profile_capture_tree(false);
        let named = &mut tree["nodes"][0]["nodes"][1];
        if invalid == "floating" {
            named["floating_nodes"] = named["nodes"].take();
            named["nodes"] = json!([]);
        } else {
            named["num"] = Value::Null;
        }
        let output = run_with_recorded_snapshot(
            &fixture,
            menu_command(&fixture, "", "")
                .env("MENU_STATUS", "2")
                .args(["--profiles-dir", "profiles", "menu"]),
            tree,
        );
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_to_string(fixture.path.join("options")).unwrap(),
            "Capture current workspace…\n[invalid] Profile: work\n"
        );
        assert!(!fixture.path.join("name-opened").exists());
        assert!(!fixture.path.join("notification").exists());
    }
}

#[test]
fn profile_capture_operational_errors_stop_and_notification_errors_remain_secondary() {
    for fail_publication in [false, true] {
        let fixture = CliFixture::new();
        prepare_menu_fixture(&fixture);
        if fail_publication {
            fs::create_dir(fixture.path.join("profiles")).unwrap();
            fs::set_permissions(
                fixture.path.join("profiles"),
                fs::Permissions::from_mode(0o500),
            )
            .unwrap();
        }
        let output = run_with_recorded_snapshot(
            &fixture,
            menu_command(&fixture, "1\n", "work\n")
                .env("NOTIFY_STATUS", "1")
                .args([
                    "--layouts-dir",
                    "layouts",
                    "--profiles-dir",
                    "profiles",
                    "menu",
                ]),
            profile_capture_tree(false),
        );
        if fail_publication {
            fs::set_permissions(
                fixture.path.join("profiles"),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        assert_eq!(output.status.success(), !fail_publication, "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("notification unavailable"), "{stderr}");
        let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
        assert_eq!(notification.matches("--app-name=sway-stage").count(), 1);
        assert_eq!(notification.contains("Saved profile"), !fail_publication);
        if fail_publication {
            assert!(stderr.contains("no configuration published"), "{stderr}");
            assert!(stderr.contains("staging files may remain"), "{stderr}");
            assert!(!fixture.path.join("layouts/work-001.yaml").exists());
            assert!(fs::read_dir(fixture.path.join("layouts")).unwrap().count() > 0);
            assert!(!fixture.path.join("profiles/work.yaml").exists());
            assert_eq!(
                fs::read_to_string(fixture.path.join("name-attempt")).unwrap(),
                "1\n"
            );
        }
    }
}
