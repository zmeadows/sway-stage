use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const WINDOW: &str =
    "version: 1\nlayout: {id: t, command: [does-not-need-to-exist], match: {app_id: t}}\n";

struct Fixture {
    path: PathBuf,
}

impl Fixture {
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

    fn run(&self, arguments: &[&str], status: i32) -> Output {
        let output = self.command().args(arguments).output().unwrap();
        assert_eq!(output.status.code(), Some(status), "{output:?}");
        output
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}

#[test]
fn help_version_and_usage_do_not_need_configuration() {
    let fixture = Fixture::new();
    for args in [
        vec![],
        vec!["--help"],
        vec!["check", "--help"],
        vec!["load", "--help"],
        vec!["--version"],
    ] {
        let output = fixture
            .command()
            .env_remove("HOME")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        assert!(!output.stdout.is_empty());
    }
    for args in [
        vec!["check"],
        vec!["load"],
        vec!["pick"],
        vec!["capture"],
        vec!["--bogus"],
        vec!["list", "extra"],
    ] {
        let output = fixture.run(&args, 2);
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn list_is_lexical_immediate_regular_files_only_and_does_not_parse() {
    let fixture = Fixture::new();
    fixture.file("z.yaml", "not a layout");
    fixture.file("a.yml", "malformed: [");
    fixture.file("A-1.yaml", "");
    fixture.file("ignored.YAML", "");
    fixture.file("ignored.txt", "");
    fs::create_dir(fixture.path.join("directory.yaml")).unwrap();
    fs::create_dir(fixture.path.join("nested")).unwrap();
    fixture.file("nested/hidden.yaml", WINDOW);
    std::os::unix::fs::symlink(fixture.path.join("z.yaml"), fixture.path.join("link.yaml"))
        .unwrap();
    let output = fixture.run(&["--layouts-dir", ".", "list"], 0);
    assert_eq!(output.stdout, b"A-1\na\nz\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn discovery_rejects_duplicates_invalid_names_and_non_utf8_names() {
    let fixture = Fixture::new();
    fixture.file("same.yaml", WINDOW);
    fixture.file("same.yml", WINDOW);
    let output = fixture.run(&["list", "--layouts-dir", "."], 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("duplicate layout name"));
    assert!(output.stdout.is_empty());
    fs::remove_file(fixture.path.join("same.yml")).unwrap();
    for name in [
        "bad name.yaml",
        "_bad.yml",
        "bad.name.yaml",
        "é.yaml",
        ".yaml",
    ] {
        fixture.file(name, "");
        let output = fixture.run(&["list", "--layouts-dir", "."], 1);
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid layout name"));
        fs::remove_file(fixture.path.join(name)).unwrap();
    }
    use std::os::unix::ffi::OsStringExt;
    fs::write(
        fixture
            .path
            .join(OsString::from_vec(b"bad\xff.yaml".to_vec())),
        "",
    )
    .unwrap();
    fixture.run(&["list", "--layouts-dir", "."], 1);
}

#[test]
fn default_directories_and_overrides_follow_xdg_without_creating_directories() {
    let fixture = Fixture::new();
    assert!(fixture.run(&["list"], 0).stdout.is_empty());
    assert!(!fixture.path.join(".config").exists());
    fixture.run(&["check", "missing"], 1);
    fixture.run(&["--layouts-dir", "missing", "list"], 1);
    fixture.run(&["--layouts-dir", "", "list"], 2);
    fs::create_dir_all(fixture.path.join(".config/sway-stage/layouts")).unwrap();
    fixture.file(".config/sway-stage/layouts/home.yaml", WINDOW);
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
    fixture.file("xdg/sway-stage/layouts/xdg.yml", WINDOW);
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
    fixture.file(".config/sway-stage/layouts/file.yaml", WINDOW);
    fs::remove_dir_all(fixture.path.join(".config/sway-stage/layouts")).unwrap();
    fixture.file(".config/sway-stage/layouts", "not a directory");
    fixture.run(&["list"], 1);
}

#[test]
fn check_validates_without_compositor_tools_or_application_probes() {
    let fixture = Fixture::new();
    fixture.file("good.yaml", WINDOW);
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
fn check_resolves_cwd_against_yaml_file_and_rejects_bad_files() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("layouts/work")).unwrap();
    fixture.file(
        "layouts/relative.yaml",
        &WINDOW.replace("match:", "cwd: work, match:"),
    );
    fixture.run(&["--layouts-dir", "layouts", "check", "relative"], 0);
    fixture.file(
        "layouts/absolute.yaml",
        &WINDOW.replace(
            "match:",
            &format!("cwd: '{}', match:", fixture.path.display()),
        ),
    );
    fixture.run(&["--layouts-dir", "layouts", "check", "absolute"], 0);
    fixture.file(
        "layouts/missing.yaml",
        &WINDOW.replace("match:", "cwd: absent, match:"),
    );
    fixture.run(&["--layouts-dir", "layouts", "check", "missing"], 1);
    fs::write(fixture.path.join("invalid.yaml"), b"\xff").unwrap();
    let output = fixture.run(&["--layouts-dir", ".", "check", "invalid"], 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("UTF-8"));
    fixture.file("big.yaml", &"x".repeat(1024 * 1024 + 10));
    let output = fixture.run(&["--layouts-dir", ".", "check", "big"], 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("1 MiB"));
}

#[test]
fn load_rejects_general_layouts_before_ipc_and_writes_private_last_run_log() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.file("nested.yaml", "version: 1\nlayout: {split: horizontal, children: [{id: a, command: [not-installed], match: {app_id: a}}, {id: b, command: [not-installed], match: {app_id: b}}]}\n");
    fixture.file("good.yaml", WINDOW);
    let log = fixture.path.join(".local/state/sway-stage/last-run.log");
    for expected in [
        "nested layout execution is not yet implemented",
        "SWAYSOCK is unset",
    ] {
        let name = if expected.starts_with("nested") {
            "nested"
        } else {
            "good"
        };
        let output = fixture.run(&["--layouts-dir", ".", "load", name], 1);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{stderr}");
        assert!(stderr.contains("log:"), "{stderr}");
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
    let fixture = Fixture::new();
    fixture.file("good.yaml", WINDOW);
    fixture.file(
        "notify-send",
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$NOTIFY_RECORD\"\n",
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
    assert!(notification.contains("SWAYSOCK is unset"));
    assert!(!notification.contains("does-not-need-to-exist"));
    assert!(fixture.path.join("state/sway-stage/last-run.log").exists());

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
    let fixture = Fixture::new();
    fixture.file("good.yaml", WINDOW);
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
