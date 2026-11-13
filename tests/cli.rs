use serde_json::{Value, json};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

const SINGLE_WINDOW_YAML: &str =
    "version: 1\nlayout: {id: t, command: [does-not-need-to-exist], match: {app_id: t}}\n";
// Failure advice that is appropriate only after this invocation started an application.
const PARTIAL_RUN_ADVICE: &str = "inspect windows before retrying";

struct CliFixture {
    path: PathBuf,
}

impl CliFixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "sway-stage-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();

        Self { path }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sway-stage"));
        command
            .current_dir(&self.path)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env("HOME", &self.path)
            .env_remove("SWAYSOCK")
            .env_remove("I3SOCK")
            .env("PATH", "");
        command
    }

    fn file(&self, name: &str, contents: &str) {
        fs::write(self.path.join(name), contents).unwrap();
    }

    fn run(&self, arguments: &[&str], expected_exit_code: i32) -> Output {
        let output = self.command().args(arguments).output().unwrap();
        assert_eq!(output.status.code(), Some(expected_exit_code), "{output:?}");
        output
    }
}

impl Drop for CliFixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}

#[test]
fn help_version_and_usage_do_not_need_configuration() {
    let fixture = CliFixture::new();
    for arguments in [
        vec![],
        vec!["--help"],
        vec!["check", "--help"],
        vec!["profile", "--help"],
        vec!["profile", "check", "--help"],
        vec!["profile", "load", "--help"],
        vec!["load", "--help"],
        vec!["capture", "--help"],
        vec!["menu", "--help"],
        vec!["--version"],
    ] {
        let output = fixture
            .command()
            .env_remove("HOME")
            .args(arguments)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        assert!(!output.stdout.is_empty());
    }
    for arguments in [
        vec!["check"],
        vec!["profile"],
        vec!["profile", "check"],
        vec!["profile", "load"],
        vec!["profile", "load", "work", "--cwd", "."],
        vec!["profile", "check", "work", "--cwd", "."],
        vec!["--profiles-dir", "", "profile", "check", "work"],
        vec!["load"],
        vec!["menu", "extra"],
        vec!["capture", "extra"],
        vec!["capture", "--infer-commands"],
        vec!["menu", "--infer-commands"],
        vec!["--bogus"],
        vec!["list", "extra"],
    ] {
        let output = fixture.run(&arguments, 2);
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn list_is_lexical_immediate_regular_files_only_and_does_not_parse() {
    let fixture = CliFixture::new();
    fixture.file("z.yaml", "not a layout");
    fixture.file("a.yml", "malformed: [");
    fixture.file("A-1.yaml", "");
    fixture.file("ignored.YAML", "");
    fixture.file("ignored.txt", "");
    fs::create_dir(fixture.path.join("directory.yaml")).unwrap();
    fs::create_dir(fixture.path.join("nested")).unwrap();
    fixture.file("nested/hidden.yaml", SINGLE_WINDOW_YAML);
    std::os::unix::fs::symlink(fixture.path.join("z.yaml"), fixture.path.join("link.yaml"))
        .unwrap();
    let output = fixture.run(&["--layouts-dir", ".", "list"], 0);

    assert_eq!(output.stdout, b"A-1\na\nz\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn discovery_skips_invalid_filenames_and_reports_only_eligible_files() {
    use std::os::unix::ffi::OsStringExt;
    let fixture = CliFixture::new();
    fixture.file("z.yaml", SINGLE_WINDOW_YAML);
    fixture.file("a.yml", SINGLE_WINDOW_YAML);
    let invalid_names = [
        "bad name.yaml",
        "_bad.yml",
        "bad.name.yaml",
        "é.yaml",
        ".yaml",
    ];
    for name in invalid_names {
        fixture.file(name, "");
    }
    fs::write(
        fixture
            .path
            .join(OsString::from_vec(b"bad\xff.yaml".to_vec())),
        "",
    )
    .unwrap();
    fixture.file("ignored name.txt", "");
    fs::create_dir(fixture.path.join("ignored directory.yaml")).unwrap();
    std::os::unix::fs::symlink("missing", fixture.path.join("ignored link.yml")).unwrap();

    let output = fixture.run(&["list", "--layouts-dir", "."], 0);
    assert_eq!(output.stdout, b"a\nz\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.lines().count(), invalid_names.len() + 1, "{stderr}");
    for name in invalid_names {
        assert!(stderr.contains(name), "{stderr}");
    }
    assert!(stderr.contains("bad\\xFF.yaml"), "{stderr}");
    assert!(!stderr.contains("ignored"), "{stderr}");
    assert!(
        stderr
            .lines()
            .all(|line| line.starts_with("sway-stage: warning: skipping invalid layout filename"))
    );

    let repeated = fixture.run(&["list", "--layouts-dir", "."], 0);
    assert_eq!(output.stdout, repeated.stdout);
    assert_eq!(output.stderr, repeated.stderr);
}

#[test]
fn duplicate_stems_are_excluded_and_explicit_resolution_reports_ambiguity() {
    // Reverse creation order must produce exactly the same output and diagnostics.
    let mut first_listing = None;
    for filenames in [["same.yml", "same.yaml"], ["same.yaml", "same.yml"]] {
        let fixture = CliFixture::new();
        for name in filenames {
            fixture.file(name, SINGLE_WINDOW_YAML);
        }
        fixture.file("z.yaml", SINGLE_WINDOW_YAML);
        fixture.file("a.yml", SINGLE_WINDOW_YAML);
        fixture.file("bad name.yaml", "");
        let output = fixture.run(&["list", "--layouts-dir", "."], 0);
        assert_eq!(output.stdout, b"a\nz\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("ambiguous layout name \"same\": \"same.yaml\" and \"same.yml\""),
            "{stderr}"
        );
        assert_eq!(stderr.lines().count(), 2);
        if let Some((stdout, stderr)) = &first_listing {
            assert_eq!(&output.stdout, stdout);
            assert_eq!(&output.stderr, stderr);
        } else {
            first_listing = Some((output.stdout, output.stderr));
        }

        for command in ["check", "load"] {
            let output = fixture.run(&["--layouts-dir", ".", command, "same"], 1);
            assert!(output.stdout.is_empty());
            let stderr = String::from_utf8_lossy(&output.stderr);
            for expected in ["ambiguous layout name", "same.yaml", "same.yml"] {
                assert!(stderr.contains(expected), "{stderr}");
            }
            assert!(!stderr.contains("not found"), "{stderr}");
            assert!(!stderr.contains("SWAYSOCK"), "{stderr}");
            assert!(!stderr.contains(PARTIAL_RUN_ADVICE), "{stderr}");
        }
        let log =
            fs::read_to_string(fixture.path.join(".local/state/sway-stage/last-run.log")).unwrap();
        assert!(!log.contains("spawned pid="), "{log}");

        let checked = fixture.run(&["--layouts-dir", ".", "check", "a"], 0);
        assert_eq!(checked.stdout, b"a: valid\n");
        assert!(checked.stderr.is_empty());
        // Reaching socket validation establishes successful resolution without live IPC.
        let loaded = fixture.run(&["--layouts-dir", ".", "load", "z"], 1);
        let stderr = String::from_utf8_lossy(&loaded.stderr);
        assert!(stderr.contains("SWAYSOCK is unset"), "{stderr}");
        assert!(!stderr.contains("ambiguous"), "{stderr}");
    }
}

#[test]
fn only_invalid_or_ambiguous_filenames_list_as_empty() {
    for filenames in [
        vec!["bad name.yaml"],
        vec!["same.yml", "same.yaml"],
        vec!["bad name.yaml", "same.yml", "same.yaml"],
    ] {
        let fixture = CliFixture::new();
        for name in filenames {
            fixture.file(name, SINGLE_WINDOW_YAML);
        }
        let output = fixture.run(&["--layouts-dir", ".", "list"], 0);
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
        let output = fixture.run(&["--layouts-dir", ".", "check", "missing"], 1);
        assert!(String::from_utf8_lossy(&output.stderr).contains("not found"));
    }
}

#[test]
fn filesystem_failures_remain_fatal_for_discovery_and_resolution() {
    let fixture = CliFixture::new();
    fixture.file("not-directory", "");
    std::os::unix::fs::symlink("loop", fixture.path.join("loop")).unwrap();
    for directory in ["not-directory", "loop", "missing"] {
        for arguments in [vec!["list"], vec!["check", "good"], vec!["load", "good"]] {
            let mut command_arguments = vec!["--layouts-dir", directory];
            command_arguments.extend(arguments);
            let output = fixture.run(&command_arguments, 1);
            assert!(output.stdout.is_empty());
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("read directory"), "{stderr}");
            assert!(!stderr.contains("not found in"), "{stderr}");
        }
    }
}

#[test]
fn default_directories_and_overrides_follow_xdg_without_creating_directories() {
    let fixture = CliFixture::new();

    assert!(fixture.run(&["list"], 0).stdout.is_empty());
    assert!(!fixture.path.join(".config").exists());
    fixture.run(&["check", "missing"], 1);
    fixture.run(&["--layouts-dir", "missing", "list"], 1);
    fixture.run(&["--layouts-dir", "", "list"], 2);
    fs::create_dir_all(fixture.path.join(".config/sway-stage/layouts")).unwrap();
    fixture.file(".config/sway-stage/layouts/home.yaml", SINGLE_WINDOW_YAML);
    assert_eq!(fixture.run(&["list"], 0).stdout, b"home\n");
    let empty_xdg = fixture
        .command()
        .env("XDG_CONFIG_HOME", "")
        .arg("list")
        .output()
        .unwrap();
    assert_eq!(empty_xdg.status.code(), Some(0));
    assert_eq!(empty_xdg.stdout, b"home\n");
    fs::create_dir_all(fixture.path.join("xdg/sway-stage/layouts")).unwrap();
    fixture.file("xdg/sway-stage/layouts/xdg.yml", SINGLE_WINDOW_YAML);
    let output = fixture
        .command()
        .env("XDG_CONFIG_HOME", fixture.path.join("xdg"))
        .arg("list")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"xdg\n");
    let output = fixture
        .command()
        .env("XDG_CONFIG_HOME", "relative")
        .arg("list")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("must be absolute"));
    let output = fixture
        .command()
        .env("HOME", "relative")
        .arg("list")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let output = fixture
        .command()
        .env_remove("HOME")
        .arg("list")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    // Explicit paths take precedence over even invalid environment bases.
    let output = fixture
        .command()
        .env("XDG_CONFIG_HOME", "relative")
        .env_remove("HOME")
        .args(["--layouts-dir", ".", "list"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    fixture.file("file", "");
    fixture.run(&["--layouts-dir", "file", "list"], 1);
    fixture.file(".config/sway-stage/layouts/file.yaml", SINGLE_WINDOW_YAML);
    fs::remove_dir_all(fixture.path.join(".config/sway-stage/layouts")).unwrap();
    fixture.file(".config/sway-stage/layouts", "not a directory");
    fixture.run(&["list"], 1);
}

#[test]
fn check_validates_without_compositor_tools_or_application_probes() {
    let fixture = CliFixture::new();
    fixture.file("good.yaml", SINGLE_WINDOW_YAML);
    fixture.file(
        "draft.yml",
        "version: 1\nlayout: {id: t, command: null, match: null}",
    );
    let output = fixture.run(&["check", "good", "--layouts-dir", "."], 0);

    assert_eq!(output.stdout, b"good: valid\n");
    assert!(output.stderr.is_empty());
    let output = fixture.run(&["check", "draft", "--layouts-dir", "."], 1);
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    for expected in [
        "check:",
        "draft.yml",
        "command",
        "fill in",
        "line",
        "column",
    ] {
        assert!(stderr.contains(expected), "{stderr}");
    }
    fixture.run(&["check", "missing", "--layouts-dir", "."], 1);
    fixture.run(&["check", "../good", "--layouts-dir", "."], 1);
    assert_eq!(fs::read_dir(&fixture.path).unwrap().count(), 2);
}

#[test]
fn root_weights_fail_shared_validation_before_loader_ipc() {
    let fixture = CliFixture::new();
    for weight in [1, 3] {
        fixture.file(
            "weighted.yaml",
            &SINGLE_WINDOW_YAML.replace("{id:", &format!("{{weight: {weight}, id:")),
        );

        for command in ["check", "load"] {
            let output = fixture.run(&["--layouts-dir", ".", command, "weighted"], 1);
            assert!(output.stdout.is_empty());
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("weighted.yaml: layout.weight"), "{stderr}");
            assert!(stderr.contains("it has no siblings"), "{stderr}");
            assert!(!stderr.contains("SWAYSOCK"), "{stderr}");
            assert!(!stderr.contains(PARTIAL_RUN_ADVICE), "{stderr}");
        }
    }
}

#[test]
fn unsupported_window_timeout_fails_validation_before_ipc() {
    let fixture = CliFixture::new();
    fixture.file(
        "slow.yaml",
        &format!("{SINGLE_WINDOW_YAML}window_timeout_ms: {}\n", u64::MAX),
    );
    let output = fixture.run(&["check", "slow", "--layouts-dir", "."], 1);

    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("slow.yaml: window_timeout_ms"), "{stderr}");
    assert!(stderr.contains("from 1 to 300000"), "{stderr}");

    // Load stops at the same validation stage instead of reaching socket discovery.
    let output = fixture.run(&["--layouts-dir", ".", "load", "slow"], 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("validation: "), "{stderr}");
    assert!(stderr.contains("window_timeout_ms"), "{stderr}");
    assert!(!stderr.contains("SWAYSOCK"), "{stderr}");
    assert!(!stderr.contains(PARTIAL_RUN_ADVICE), "{stderr}");
}

#[test]
fn invalid_and_missing_layout_names_fail_before_ipc_for_check_and_load() {
    let fixture = CliFixture::new();
    fixture.file("good.yaml", SINGLE_WINDOW_YAML);
    for command in ["check", "load"] {
        for (name, expected) in [
            (
                "../good",
                "invalid layout name \"../good\"; use [A-Za-z0-9]",
            ),
            ("missing", "layout \"missing\" not found in .; use list"),
        ] {
            let output = fixture.run(&["--layouts-dir", ".", command, name], 1);
            assert!(output.stdout.is_empty());
            // The primary error is the final line, after any secondary diagnostics.
            let stderr = String::from_utf8_lossy(&output.stderr);
            let primary = stderr.lines().last().unwrap_or_default();
            assert!(
                primary.starts_with(&format!("sway-stage: {command}")),
                "{stderr}"
            );
            assert!(primary.contains(expected), "{stderr}");
            assert!(!stderr.contains("SWAYSOCK"), "{stderr}");
            assert!(!stderr.contains(PARTIAL_RUN_ADVICE), "{stderr}");
        }
    }
}

#[test]
fn list_and_check_never_contact_sway_log_or_notify_even_on_failure() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = CliFixture::new();
    fixture.file("good.yaml", SINGLE_WINDOW_YAML);
    fixture.file("bad.yaml", "version: 2\n");
    fixture.file(
        "notify-send",
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$NOTIFY_RECORD\"\n",
    );
    fs::set_permissions(
        fixture.path.join("notify-send"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    // Not a socket: any connection attempt fails, so a successful list/check never tried.
    fixture.file("not-a-socket", "");
    let run = |arguments: &[&str]| {
        fixture
            .command()
            .env("SWAYSOCK", fixture.path.join("not-a-socket"))
            .env("XDG_STATE_HOME", fixture.path.join("state"))
            .env("PATH", &fixture.path)
            .env("NOTIFY_RECORD", fixture.path.join("notification"))
            .args(arguments)
            .output()
            .unwrap()
    };
    for (arguments, expected_exit_code) in [
        (&["--layouts-dir", ".", "list"][..], 0),
        (&["--layouts-dir", "absent", "list"], 1),
        (&["--layouts-dir", ".", "check", "good"], 0),
        (&["--layouts-dir", ".", "check", "bad"], 1),
        (&["--layouts-dir", ".", "check", "missing"], 1),
        (&["--layouts-dir", ".", "check", "../good"], 1),
    ] {
        let output = run(arguments);
        assert_eq!(
            output.status.code(),
            Some(expected_exit_code),
            "{arguments:?}: {output:?}"
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("SWAYSOCK"));
    }

    assert!(!fixture.path.join("state").exists());
    assert!(!fixture.path.join(".local").exists());
    assert!(!fixture.path.join("notification").exists());

    // Control: in the same environment, load does reach the socket, log and notifier.
    let output = run(&["--layouts-dir", ".", "load", "good"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("connect to SWAYSOCK"), "{stderr}");
    assert!(!stderr.contains(PARTIAL_RUN_ADVICE), "{stderr}");
    assert!(fixture.path.join("state/sway-stage/last-run.log").exists());
    let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
    assert!(
        notification.contains("Unable to communicate with Sway."),
        "{notification}"
    );
}

#[test]
fn check_validates_absolute_cwd_and_rejects_bad_files() {
    let fixture = CliFixture::new();
    fs::create_dir_all(fixture.path.join("layouts/work")).unwrap();
    fixture.file(
        "layouts/relative.yaml",
        &SINGLE_WINDOW_YAML.replace("match:", "cwd: work, match:"),
    );
    fixture.run(&["--layouts-dir", "layouts", "check", "relative"], 1);
    fixture.file(
        "layouts/absolute.yaml",
        &SINGLE_WINDOW_YAML.replace(
            "match:",
            &format!("cwd: '{}', match:", fixture.path.display()),
        ),
    );
    fixture.run(&["--layouts-dir", "layouts", "check", "absolute"], 0);
    fixture.file(
        "layouts/missing.yaml",
        &SINGLE_WINDOW_YAML.replace("match:", "cwd: absent, match:"),
    );
    fixture.run(&["--layouts-dir", "layouts", "check", "missing"], 1);
    for (name, cwd, expected_exit_code) in [
        ("default-relative", "work".to_owned(), 1),
        (
            "default-absolute",
            format!("'{}'", fixture.path.display()),
            0,
        ),
        ("default-missing", "absent".to_owned(), 1),
        ("default-empty", "''".to_owned(), 1),
        ("default-file", "relative.yaml".to_owned(), 1),
        ("default-null", "null".to_owned(), 1),
    ] {
        fixture.file(
            &format!("layouts/{name}.yaml"),
            &format!("{SINGLE_WINDOW_YAML}\ncwd: {cwd}\n"),
        );
        let output = fixture.run(
            &["--layouts-dir", "layouts", "check", name],
            expected_exit_code,
        );
        if expected_exit_code == 1 {
            assert!(String::from_utf8_lossy(&output.stderr).contains("cwd"));
        }
    }
    fs::write(fixture.path.join("invalid.yaml"), b"\xff").unwrap();
    let output = fixture.run(&["--layouts-dir", ".", "check", "invalid"], 1);

    assert!(String::from_utf8_lossy(&output.stderr).contains("UTF-8"));
    fixture.file("big.yaml", &"x".repeat(1024 * 1024 + 10));
    let output = fixture.run(&["--layouts-dir", ".", "check", "big"], 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("1 MiB"));
}

#[test]
fn load_requires_ipc_for_all_layouts_and_writes_private_last_run_log() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = CliFixture::new();
    fixture.file("nested.yaml", "version: 1\nlayout: {group: splith, children: [{id: a, command: [not-installed], match: {app_id: a}}, {id: b, command: [not-installed], match: {app_id: b}}]}\n");
    fixture.file("good.yaml", SINGLE_WINDOW_YAML);
    let log = fixture.path.join(".local/state/sway-stage/last-run.log");
    for name in ["nested", "good"] {
        let expected = "SWAYSOCK is unset";
        let output = fixture.run(&["--layouts-dir", ".", "load", name], 1);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{stderr}");
        assert!(stderr.contains("log:"), "{stderr}");
        // Nothing was launched, so there is no partial layout to inspect.
        assert!(!stderr.contains(PARTIAL_RUN_ADVICE), "{stderr}");
        assert!(output.stdout.is_empty());
        let contents = fs::read_to_string(&log).unwrap();
        assert!(contents.contains(expected), "{contents}");
        assert!(!contents.contains("not-installed"));
        assert_eq!(
            fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!fixture.path.join("launches").exists());
    }

    assert!(
        !fs::read_to_string(&log)
            .unwrap()
            .contains("nested layout execution")
    );
    // A previous log cannot redirect this run into a different file.
    fixture.file("private-target", "unchanged");
    fs::remove_file(&log).unwrap();
    std::os::unix::fs::symlink(fixture.path.join("private-target"), &log).unwrap();
    fixture.run(&["--layouts-dir", ".", "load", "missing"], 1);
    assert_eq!(
        fs::read_to_string(fixture.path.join("private-target")).unwrap(),
        "unchanged"
    );
    assert!(!fs::symlink_metadata(&log).unwrap().file_type().is_symlink());
}

#[test]
fn load_errors_notify_with_literal_arguments_and_logging_failure_stays_secondary() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = CliFixture::new();
    fixture.file("good.yaml", SINGLE_WINDOW_YAML);
    fixture.file(
        "notify-send",
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$NOTIFY_RECORD\"\n",
    );
    fs::set_permissions(
        fixture.path.join("notify-send"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let output = fixture
        .command()
        .env("XDG_STATE_HOME", fixture.path.join("state"))
        .env("PATH", &fixture.path)
        .env("NOTIFY_RECORD", fixture.path.join("notification"))
        .args(["--layouts-dir", ".", "load", "good"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
    assert!(
        notification.contains("--app-name=sway-stage\n--urgency=critical\n"),
        "{notification}"
    );
    assert!(
        notification.contains("\n--\nsway-stage load failed\n"),
        "{notification}"
    );
    assert!(
        notification.contains("Unable to communicate with Sway.\n\nSee last-run.log for details.")
    );
    assert!(!notification.contains("SWAYSOCK"));
    assert!(!notification.contains(fixture.path.to_str().unwrap()));
    let log = fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap();
    assert!(log.contains("SWAYSOCK is unset"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("SWAYSOCK is unset"));
    assert!(!notification.contains("does-not-need-to-exist"));
    assert_eq!(notification.matches("--app-name=sway-stage").count(), 1);
    assert!(fixture.path.join("state/sway-stage/last-run.log").exists());

    fs::remove_file(fixture.path.join("notification")).unwrap();
    let output = fixture
        .command()
        .env("XDG_STATE_HOME", "relative")
        .env("PATH", &fixture.path)
        .env("NOTIFY_RECORD", fixture.path.join("notification"))
        .args(["--layouts-dir", ".", "load", "good"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
    assert!(notification.contains("Unable to communicate with Sway."));
    assert!(!notification.contains("last-run.log"));
    assert_eq!(notification.matches("--app-name=sway-stage").count(), 1);

    let output = fixture
        .command()
        .env("XDG_STATE_HOME", "relative")
        .args(["--layouts-dir", ".", "load", "good"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("logging unavailable"), "{stderr}");
    assert!(stderr.contains("SWAYSOCK is unset"), "{stderr}");
    assert!(stderr.contains("notification unavailable"), "{stderr}");
}

#[test]
fn notification_helper_has_a_bounded_wait() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = CliFixture::new();
    fixture.file("good.yaml", SINGLE_WINDOW_YAML);
    fixture.file("notify-send", "#!/bin/sh\nexec /bin/sleep 10\n");
    fs::set_permissions(
        fixture.path.join("notify-send"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let start = std::time::Instant::now();
    let output = fixture
        .command()
        .env("PATH", &fixture.path)
        .args(["--layouts-dir", ".", "load", "good"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("two-second deadline"), "{stderr}");
    assert!(stderr.contains("SWAYSOCK is unset"), "{stderr}");
}

fn read_ipc_message(stream: &mut UnixStream) -> (u32, Vec<u8>) {
    let mut header = [0; 14];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(&header[..6], b"i3-ipc");
    let length = u32::from_ne_bytes(header[6..10].try_into().unwrap());
    let kind = u32::from_ne_bytes(header[10..].try_into().unwrap());
    let mut payload = vec![0; length as usize];
    stream.read_exact(&mut payload).unwrap();
    (kind, payload)
}

fn write_ipc_message(stream: &mut UnixStream, kind: u32, payload: &Value) {
    let payload = serde_json::to_vec(payload).unwrap();
    let mut message = b"i3-ipc".to_vec();
    message.extend((payload.len() as u32).to_ne_bytes());
    message.extend(kind.to_ne_bytes());
    message.extend(payload);
    stream.write_all(&message).unwrap();
}

/// A container as Sway's GET_TREE describes it, with every field swayipc requires.
fn sway_node(id: i64, kind: &str, width: i32, nodes: Vec<Value>) -> Value {
    let rect = json!({"x": 0, "y": 0, "width": width, "height": 1080});
    json!({
        "id": id, "type": kind, "name": "1", "border": "none", "current_border_width": 0,
        "layout": "splith", "orientation": "horizontal",
        "rect": rect, "window_rect": rect, "deco_rect": rect, "geometry": rect,
        "urgent": false, "focused": false, "focus": [],
        "nodes": nodes, "floating_nodes": [], "sticky": false,
        // Sway reports 1 on every workspace, independently of window fullscreen state.
        "fullscreen_mode": if kind == "workspace" { 1 } else { 0 }
    })
}

/// Answers exactly one GET_TREE, refusing all other IPC requests and connections.
fn serve_capture(
    listener: UnixListener,
    tree: Value,
    menu_directory: Option<PathBuf>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let accept = || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
        };
        let mut query = accept();
        if let Some(directory) = &menu_directory {
            assert!(
                !directory.join("action-pid").exists(),
                "snapshot precedes Fuzzel"
            );
            assert!(!directory.join("name-opened").exists());
            assert_eq!(
                fs::read_to_string(directory.join("state/sway-stage/last-run.log")).unwrap(),
                "previous launch\n"
            );
        }
        assert_eq!(read_ipc_message(&mut query).0, 4);
        if let Some(directory) = &menu_directory {
            fs::write(directory.join("snapshot-ready"), "").unwrap();
        }
        write_ipc_message(&mut query, 4, &tree);
        assert_eq!(query.read(&mut [0]).unwrap(), 0);
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    })
}

#[test]
fn capture_prints_only_a_draft_that_check_accepts_once_filled_in() {
    let fixture = CliFixture::new();
    let socket = fixture.path.join("sway.sock");
    let mut editor = sway_node(20, "con", 1280, vec![]);
    editor["shell"] = json!("xdg_shell");
    editor["app_id"] = json!("dev:editor");
    editor["focused"] = json!(true);
    // A view without an app ID or class gets a `match: null` placeholder.
    let mut unidentified = sway_node(21, "con", 640, vec![]);
    unidentified["shell"] = json!("xdg_shell");
    let workspace = sway_node(10, "workspace", 1920, vec![editor, unidentified]);
    let output = sway_node(2, "output", 1920, vec![workspace]);
    let tree = sway_node(1, "root", 1920, vec![output]);
    let mut drafts = Vec::new();
    for infer_commands in [false, true] {
        // Separate fixture socket names avoid rebinding a retained filesystem entry.
        let socket = socket.with_extension(if infer_commands {
            "inferred.sock"
        } else {
            "sock"
        });
        let listener = UnixListener::bind(&socket).unwrap();
        let server = serve_capture(listener, tree.clone(), None);
        let mut command = fixture.command();
        command.env("SWAYSOCK", &socket).arg("capture");
        if !infer_commands {
            command.arg("--no-infer-commands");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        server.join().unwrap();
        let summary = String::from_utf8(output.stderr).unwrap();
        assert!(summary.contains("capture completed."), "{summary}");
        let expected = if infer_commands {
            "Commands inferred: 0; unresolved: 2. Manual editing required."
        } else {
            "Command inference disabled; unresolved commands: 2. Manual editing required."
        };
        assert!(summary.contains(expected), "{summary}");
        assert_eq!(
            summary.contains("Command inference disabled"),
            !infer_commands
        );
        assert!(!summary.contains(".yaml"));
        drafts.push(String::from_utf8(output.stdout).unwrap());
    }
    // Missing PIDs retain placeholders; only the inference review comment is added.
    let without_comments = |draft: &str| {
        draft
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
    };

    assert_eq!(without_comments(&drafts[0]), without_comments(&drafts[1]));
    assert!(drafts[1].contains("Review recovered commands before executing or sharing"));
    // Capture writes no launch log and needs no layout directory.
    assert!(!fixture.path.join(".local").exists());
    assert!(!fixture.path.join(".config").exists());

    let draft = drafts.remove(0);
    assert!(draft.starts_with("# Layout draft from `sway-stage capture`."));
    assert!(
        draft.contains("version: 1\nfocus: window1\nlayout:\n  group: splith\n"),
        "{draft}"
    );
    let placeholders = |placeholder: &str| {
        draft
            .lines()
            .filter(|line| line.trim() == placeholder)
            .count()
    };
    assert_eq!(placeholders("command: null"), 2);
    assert_eq!(placeholders("match: null"), 1);

    fixture.file("draft.yaml", &draft);
    let output = fixture.run(&["--layouts-dir", ".", "check", "draft"], 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("capture placeholder"));
    let edited = draft
        .replace("command: null", "command: [foot]")
        .replace("match: null", "match: {app_id: unidentified}");
    fixture.file("draft.yaml", &edited);
    let output = fixture.run(&["--layouts-dir", ".", "check", "draft"], 0);
    assert_eq!(output.stdout, b"draft: valid\n");
}

#[test]
fn unsupported_capture_emits_no_partial_yaml_and_preserves_existing_log() {
    let fixture = CliFixture::new();
    fs::create_dir_all(fixture.path.join("state/sway-stage")).unwrap();
    fixture.file("state/sway-stage/last-run.log", "previous launch\n");
    let mut empty = sway_node(10, "workspace", 1920, vec![]);
    empty["focused"] = json!(true);
    let tree = sway_node(
        1,
        "root",
        1920,
        vec![sway_node(2, "output", 1920, vec![empty])],
    );
    let socket = fixture.path.join("capture.sock");
    let server = serve_capture(UnixListener::bind(&socket).unwrap(), tree, None);
    let output = fixture
        .command()
        .env("SWAYSOCK", &socket)
        .env("XDG_STATE_HOME", fixture.path.join("state"))
        .arg("capture")
        .output()
        .unwrap();
    server.join().unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("empty; there is nothing to capture"));
    assert_eq!(
        fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap(),
        "previous launch\n"
    );
}

#[test]
fn invalid_tab_selection_emits_no_yaml_and_preserves_existing_log() {
    let fixture = CliFixture::new();
    fs::create_dir_all(fixture.path.join("state/sway-stage")).unwrap();
    fixture.file("state/sway-stage/last-run.log", "previous launch\n");
    let mut tree = menu::capture_tree();
    let workspace = &mut tree["nodes"][0]["nodes"][0];
    let mut second = workspace["nodes"][0].clone();
    second["id"] = json!(21);
    second["focused"] = json!(false);
    workspace["nodes"].as_array_mut().unwrap().push(second);
    workspace["layout"] = json!("tabbed");
    workspace["focus"] = json!([999]);
    let socket = fixture.path.join("capture.sock");
    let server = serve_capture(UnixListener::bind(&socket).unwrap(), tree, None);
    let output = fixture
        .command()
        .env("SWAYSOCK", &socket)
        .env("XDG_STATE_HOME", fixture.path.join("state"))
        .arg("capture")
        .output()
        .unwrap();
    server.join().unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("child-focus history"));
    assert_eq!(
        fs::read_to_string(fixture.path.join("state/sway-stage/last-run.log")).unwrap(),
        "previous launch\n"
    );
}

#[test]
fn capture_failures_notify_without_touching_the_launch_log() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = CliFixture::new();
    fixture.file(
        "notify-send",
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$NOTIFY_RECORD\"\n",
    );
    fs::set_permissions(
        fixture.path.join("notify-send"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::create_dir_all(fixture.path.join("state/sway-stage")).unwrap();
    let log = fixture.path.join("state/sway-stage/last-run.log");
    fs::write(&log, "previous launch\n").unwrap();
    // Not a socket: connecting fails.
    fixture.file("not-a-socket", "");

    for (sway_socket, expected) in [
        (None, "SWAYSOCK is unset"),
        (Some("not-a-socket"), "connect to SWAYSOCK"),
    ] {
        let mut command = fixture.command();
        command
            .env("XDG_STATE_HOME", fixture.path.join("state"))
            // Capture never resolves layouts, so unusable discovery bases are irrelevant.
            .env("XDG_CONFIG_HOME", "relative")
            .env_remove("HOME")
            .env("PATH", &fixture.path)
            .env("NOTIFY_RECORD", fixture.path.join("notification"))
            .arg("capture");
        if let Some(path) = sway_socket {
            command.env("SWAYSOCK", fixture.path.join(path));
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty());

        let stderr = String::from_utf8_lossy(&output.stderr);
        let primary = stderr.lines().last().unwrap_or_default();
        assert!(primary.starts_with("sway-stage: capture: "), "{stderr}");
        assert!(primary.contains(expected), "{stderr}");
        assert!(!stderr.contains("log:"), "{stderr}");
        let notification = fs::read_to_string(fixture.path.join("notification")).unwrap();
        assert!(
            notification.contains("\n--\nsway-stage capture failed\n"),
            "{notification}"
        );
        assert!(
            notification.contains("Unable to communicate with Sway."),
            "{notification}"
        );
        assert!(!notification.contains("last-run.log"), "{notification}");
        assert_eq!(fs::read_to_string(&log).unwrap(), "previous launch\n");
        assert_eq!(
            fs::read_dir(fixture.path.join("state/sway-stage"))
                .unwrap()
                .count(),
            1
        );
    }
}

#[path = "cli/menu.rs"]
mod menu;

#[test]
fn standalone_inferred_summary_follows_successful_output_and_omits_arguments() {
    for output_fails in [false, true] {
        let fixture = CliFixture::new();
        let socket = fixture.path.join("capture.sock");
        let server = serve_capture(
            UnixListener::bind(&socket).unwrap(),
            menu::capture_tree(),
            None,
        );
        let mut command = fixture.command();
        command.env("SWAYSOCK", &socket).args(["capture"]);
        if output_fails {
            command.stdout(
                fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/full")
                    .unwrap(),
            );
        }
        let output = command.output().unwrap();
        server.join().unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        if output_fails {
            assert_eq!(output.status.code(), Some(1));
            assert!(stderr.contains("stdout:"));
            assert!(!stderr.contains("capture completed"));
        } else {
            assert!(output.status.success(), "{stderr}");
            assert_eq!(
                stderr,
                "sway-stage: capture completed. Commands inferred: 1; unresolved: 0. Review best-effort commands before use.\n"
            );
            let yaml = String::from_utf8(output.stdout).unwrap();
            assert!(!yaml.contains("capture completed"));
            fixture.file("draft.yaml", &yaml);
            fixture.run(&["--layouts-dir", ".", "check", "draft"], 0);
        }
        assert!(!stderr.contains(std::env::current_exe().unwrap().to_str().unwrap()));
        assert!(!fixture.path.join(".local").exists());
    }
}

#[test]
fn check_accepts_all_groups_and_load_requires_sway() {
    let fixture = CliFixture::new();
    for group in ["tabbed", "stacked"] {
        fixture.file("group.yaml", &format!(
            "version: 1\nlayout: {{group: {group}, children: [{{id: a, command: [not-installed], match: {{app_id: a}}}}]}}\n"
        ));
        let output = fixture.run(&["--layouts-dir", ".", "check", "group"], 0);
        assert_eq!(output.stdout, b"group: valid\n");
        assert!(output.stderr.is_empty());
        let output = fixture.run(&["--layouts-dir", ".", "load", "group"], 1);
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("SWAYSOCK"), "{error}");
    }
}

#[test]
fn home_expansion_and_yaml_directories_are_validated_independently() {
    use std::os::unix::fs::symlink;
    let fixture = CliFixture::new();
    fs::create_dir(fixture.path.join("project")).unwrap();
    symlink(fixture.path.join("project"), fixture.path.join("link")).unwrap();
    fixture.file("file", "ordinary file");
    for directory in [
        "~/project",
        "~//project",
        "~/link",
        "~/",
        fixture.path.to_str().unwrap(),
    ] {
        fixture.file(
            "home.yaml",
            &format!("{SINGLE_WINDOW_YAML}cwd: '{directory}'\n"),
        );
        fixture.run(&["--layouts-dir", ".", "check", "home"], 0);
        fixture.file(
            "home.yaml",
            &SINGLE_WINDOW_YAML.replace("match:", &format!("cwd: '{directory}', match:")),
        );
        fixture.run(&["--layouts-dir", ".", "check", "home"], 0);
    }
    fixture.file(
        "home.yaml",
        &format!("{SINGLE_WINDOW_YAML}cwd: ~/project\n"),
    );
    for home in [
        None,
        Some(""),
        Some("relative"),
        Some("/does-not-exist/sway-stage-home"),
        Some("/dev/null"),
    ] {
        let mut command = fixture.command();
        command.env_remove("HOME");
        if let Some(home) = home {
            command.env("HOME", home);
        }
        let output = command
            .args(["--layouts-dir", ".", "check", "home"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("cwd"), "{error}");
        if home.is_none() || home == Some("") || home == Some("relative") {
            assert!(error.contains("HOME"), "{error}");
        }
    }
    for directory in [
        ".",
        "../project",
        "project",
        "$HOME/project",
        "~other/project",
        "~",
        "*.txt",
        "$(pwd)",
        "",
        "~/absent",
        "~/file",
    ] {
        for top_level in [false, true] {
            let yaml = if top_level {
                format!(
                    "{}cwd: '{directory}'\n",
                    SINGLE_WINDOW_YAML.replace("match:", "cwd: /tmp, match:")
                )
            } else {
                format!(
                    "{}cwd: /tmp\n",
                    SINGLE_WINDOW_YAML.replace("match:", &format!("cwd: '{directory}', match:"))
                )
            };
            fixture.file("bad.yaml", &yaml);
            let output = fixture.run(&["--layouts-dir", ".", "check", "bad"], 1);
            assert!(String::from_utf8_lossy(&output.stderr).contains("cwd"));
        }
    }
}

#[test]
fn load_directory_override_is_validated_before_ipc_and_belongs_only_to_load() {
    let fixture = CliFixture::new();
    fs::create_dir_all(fixture.path.join("layouts")).unwrap();
    fs::create_dir(fixture.path.join("project")).unwrap();
    fixture.file(
        "layouts/good.yaml",
        &format!("{SINGLE_WINDOW_YAML}cwd: /tmp\ncwd_select: true\n"),
    );
    for directory in [
        ".",
        "project",
        "../",
        "~/project",
        fixture.path.to_str().unwrap(),
    ] {
        let output = fixture.run(
            &[
                "--layouts-dir",
                "layouts",
                "load",
                "good",
                "--cwd",
                directory,
            ],
            1,
        );
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("SWAYSOCK is unset"), "{error}");
    }
    // The invoking directory contains project; the YAML directory does not.
    for directory in ["absent", "layouts/good.yaml", "~/absent"] {
        let output = fixture.run(
            &[
                "--layouts-dir",
                "layouts",
                "load",
                "good",
                "--cwd",
                directory,
            ],
            1,
        );
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("--cwd:"), "{error}");
        assert!(!error.contains("SWAYSOCK"), "{error}");
    }
    fixture.file(
        "layouts/bad.yaml",
        &format!("{SINGLE_WINDOW_YAML}cwd: absent\n"),
    );
    let output = fixture.run(
        &["--layouts-dir", "layouts", "load", "bad", "--cwd", "."],
        1,
    );

    assert!(String::from_utf8_lossy(&output.stderr).contains("cwd:"));
    for arguments in [
        vec!["check", "good", "--cwd", "."],
        vec!["capture", "--cwd", "."],
        vec!["menu", "--cwd", "."],
        vec!["load", "good", "--cwd"],
        vec!["load", "good", "--cwd", ""],
    ] {
        fixture.run(&arguments, 2);
    }
    let help = fixture.run(&["load", "--help"], 0);
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--cwd <PATH>"));
    assert!(help.contains("invoking directory"));
    let output = fixture.run(&["--layouts-dir", "layouts", "load", "good"], 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("SWAYSOCK is unset"));
}

#[path = "cli/profile.rs"]
mod profile;

#[path = "cli/profile_load.rs"]
mod profile_load;
