use super::{CliFixture, SINGLE_WINDOW_YAML};
use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;

const PROFILE: &str = "version: 1\nworkspaces:\n  - workspace: 3\n    layout: development\n  - workspace: 1\n    layout: browser\n";

#[test]
fn default_profile_directory_uses_home_and_xdg_without_creating_directories() {
    let fixture = CliFixture::new();
    let output = fixture.run(&["profile", "check", "work"], 1);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("profile \"work\" not found"), "{error}");
    assert!(error.contains(".config/sway-stage/profiles"), "{error}");
    assert!(!fixture.path.join(".config").exists());

    for base in [".config", "xdg"] {
        fs::create_dir_all(fixture.path.join(base).join("sway-stage/profiles")).unwrap();
        fs::create_dir_all(fixture.path.join(base).join("sway-stage/layouts")).unwrap();
        fixture.file(&format!("{base}/sway-stage/profiles/work.yml"), PROFILE);
        fixture.file(
            &format!("{base}/sway-stage/layouts/development.yaml"),
            SINGLE_WINDOW_YAML,
        );
        fixture.file(
            &format!("{base}/sway-stage/layouts/browser.yml"),
            SINGLE_WINDOW_YAML,
        );
    }

    for xdg in [None, Some(""), Some("xdg")] {
        let mut command = fixture.command();
        if let Some(xdg) = xdg {
            let value = if xdg.is_empty() {
                String::new()
            } else {
                fixture.path.join(xdg).to_str().unwrap().to_owned()
            };
            command.env("XDG_CONFIG_HOME", value);
        }
        let output = command.args(["profile", "check", "work"]).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"work: valid\n");
        assert!(output.stderr.is_empty());
    }

    // A nonempty XDG base takes precedence, even when HOME has a usable profile.
    let output = fixture
        .command()
        .env("XDG_CONFIG_HOME", fixture.path.join("absent"))
        .args(["profile", "check", "work"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!fixture.path.join("absent").exists());
}

#[test]
fn directory_overrides_are_independent_global_and_relative_to_the_invoking_directory() {
    let fixture = CliFixture::new();
    fs::create_dir(fixture.path.join("profiles")).unwrap();
    fs::create_dir(fixture.path.join("layouts")).unwrap();
    fixture.file("profiles/work.yaml", PROFILE);
    fixture.file("layouts/development.yaml", SINGLE_WINDOW_YAML);
    fixture.file("layouts/browser.yml", SINGLE_WINDOW_YAML);

    for arguments in [
        vec![
            "--profiles-dir",
            "profiles",
            "--layouts-dir",
            "layouts",
            "profile",
            "check",
            "work",
        ],
        vec![
            "profile",
            "--profiles-dir",
            "profiles",
            "check",
            "work",
            "--layouts-dir",
            "layouts",
        ],
        vec![
            "profile",
            "check",
            "work",
            "--profiles-dir",
            "profiles",
            "--layouts-dir",
            "layouts",
        ],
    ] {
        let output = fixture
            .command()
            .env("XDG_CONFIG_HOME", "relative")
            .env_remove("HOME")
            .args(arguments)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"work: valid\n");
        assert!(output.stderr.is_empty());
    }

    fs::create_dir_all(fixture.path.join(".config/sway-stage/profiles")).unwrap();
    fs::create_dir_all(fixture.path.join(".config/sway-stage/layouts")).unwrap();
    fixture.file(".config/sway-stage/profiles/work.yaml", PROFILE);
    fixture.file(
        ".config/sway-stage/layouts/development.yaml",
        SINGLE_WINDOW_YAML,
    );
    fixture.file(".config/sway-stage/layouts/browser.yml", SINGLE_WINDOW_YAML);
    // Each override can be used without the other.
    fixture.run(
        &["--profiles-dir", "profiles", "profile", "check", "work"],
        0,
    );
    fixture.run(&["--layouts-dir", "layouts", "profile", "check", "work"], 0);

    // Layout commands do not resolve the unused profiles directory.
    let output = fixture.run(
        &[
            "--profiles-dir",
            "missing",
            "--layouts-dir",
            "layouts",
            "check",
            "browser",
        ],
        0,
    );
    assert_eq!(output.stdout, b"browser: valid\n");
    fixture.run(
        &[
            "--profiles-dir",
            "missing",
            "--layouts-dir",
            "layouts",
            "list",
        ],
        0,
    );
}

#[test]
fn unusable_default_bases_and_explicit_profile_directories_are_errors() {
    let fixture = CliFixture::new();
    for (variable, value) in [
        ("XDG_CONFIG_HOME", "relative"),
        ("HOME", "relative"),
        ("HOME", ""),
    ] {
        let output = fixture
            .command()
            .env(variable, value)
            .args(["profile", "check", "work"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(variable),
            "{output:?}"
        );
    }
    let output = fixture
        .command()
        .env_remove("HOME")
        .args(["profile", "check", "work"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));

    fixture.file("file", "not a directory");
    symlink("loop", fixture.path.join("loop")).unwrap();
    for directory in ["missing", "file", "loop"] {
        let output = fixture.run(
            &["--profiles-dir", directory, "profile", "check", "work"],
            1,
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("read directory"));
    }
    assert!(!fixture.path.join("missing").exists());
}

#[test]
fn profile_discovery_uses_only_immediate_regular_yaml_files_and_rejects_ambiguity() {
    let fixture = CliFixture::new();
    fixture.file("work.yml", PROFILE);
    fixture.file("development.yaml", SINGLE_WINDOW_YAML);
    fixture.file("browser.yaml", SINGLE_WINDOW_YAML);
    fixture.file("bad name.yaml", "malformed: [");
    fixture.file("unrelated.yaml", "");
    fixture.file("unrelated.yml", "");
    fixture.file("uppercase.YAML", PROFILE);
    fs::create_dir(fixture.path.join("directory.yaml")).unwrap();
    fs::create_dir(fixture.path.join("nested")).unwrap();
    fixture.file("nested/hidden.yaml", PROFILE);
    symlink("work.yml", fixture.path.join("link.yaml")).unwrap();
    symlink("absent", fixture.path.join("dangling.yml")).unwrap();

    let output = fixture.run(
        &[
            "--profiles-dir",
            ".",
            "--layouts-dir",
            ".",
            "profile",
            "check",
            "work",
        ],
        0,
    );
    assert_eq!(output.stdout, b"work: valid\n");
    assert!(output.stderr.is_empty());
    for name in ["uppercase", "directory", "hidden", "link", "dangling"] {
        let output = fixture.run(&["--profiles-dir", ".", "profile", "check", name], 1);
        assert!(String::from_utf8_lossy(&output.stderr).contains("not found"));
    }
    for name in ["../work", "work.yml", "bad name", "_bad", "é"] {
        let output = fixture.run(&["--profiles-dir", ".", "profile", "check", name], 1);
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid profile name"));
    }

    fixture.file("work.yaml", PROFILE);
    let output = fixture.run(&["--profiles-dir", ".", "profile", "check", "work"], 1);
    let error = String::from_utf8_lossy(&output.stderr);
    for expected in ["ambiguous profile name", "work.yaml", "work.yml"] {
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn profile_errors_include_file_entry_and_yaml_location_before_any_sway_access() {
    let fixture = CliFixture::new();
    fixture.file("development.yaml", SINGLE_WINDOW_YAML);
    fixture.file("browser.yml", SINGLE_WINDOW_YAML);
    for (yaml, expected) in [
        (
            PROFILE.replace("workspace: 1", "workspace: 3"),
            "duplicate workspace number",
        ),
        (
            PROFILE.replace("version: 1", "version: 2"),
            "version: expected format version 1",
        ),
        (PROFILE.replace("version: 1", "version: '1'"), "version"),
        ("version: 1\nworkspaces: [unterminated".into(), "workspaces"),
        (
            PROFILE.replace("layout: browser", "layout: ../browser"),
            "workspaces[1].layout",
        ),
        (
            PROFILE.replace("layout: browser", "layout: missing"),
            "layout \"missing\" not found",
        ),
    ] {
        fixture.file("work.yaml", &yaml);
        let output = fixture.run(
            &[
                "--profiles-dir",
                ".",
                "--layouts-dir",
                ".",
                "profile",
                "check",
                "work",
            ],
            1,
        );
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.starts_with("sway-stage: profile check:"), "{error}");
        for expected in ["work.yaml", expected] {
            assert!(error.contains(expected), "{error}");
        }
        assert!(!error.contains("SWAYSOCK"), "{error}");
    }
    fixture.file("work.yaml", PROFILE);
    fixture.file(
        "browser.yml",
        "version: 1\nlayout: {id: t, command: null, match: {app_id: t}}\n",
    );
    let output = fixture.run(
        &[
            "--profiles-dir",
            ".",
            "--layouts-dir",
            ".",
            "profile",
            "check",
            "work",
        ],
        1,
    );
    let error = String::from_utf8_lossy(&output.stderr);
    for expected in [
        "work.yaml",
        "workspaces[1].layout",
        "browser.yml",
        "command",
        "line",
        "column",
    ] {
        assert!(error.contains(expected), "{error}");
    }
    fixture.file(
        "browser.yml",
        &format!("{SINGLE_WINDOW_YAML}cwd: /does-not-exist/sway-stage-profile-test\n"),
    );
    let output = fixture.run(
        &[
            "--profiles-dir",
            ".",
            "--layouts-dir",
            ".",
            "profile",
            "check",
            "work",
        ],
        1,
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("cwd:"));

    fixture.file("browser.yaml", SINGLE_WINDOW_YAML);
    let output = fixture.run(
        &[
            "--profiles-dir",
            ".",
            "--layouts-dir",
            ".",
            "profile",
            "check",
            "work",
        ],
        1,
    );
    let error = String::from_utf8_lossy(&output.stderr);
    for expected in ["ambiguous layout name", "browser.yaml", "browser.yml"] {
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn profile_check_never_connects_launches_notifies_or_changes_logs_even_on_failure() {
    let fixture = CliFixture::new();
    fixture.file("work.yaml", PROFILE);
    fixture.file("bad.yaml", &PROFILE.replace("workspace: 1", "workspace: 3"));
    fixture.file(
        "missing-layout.yaml",
        &PROFILE.replace("layout: browser", "layout: missing"),
    );
    fixture.file(
        "invalid-layout.yaml",
        &PROFILE.replace("layout: browser", "layout: invalid"),
    );
    fixture.file(
        "invalid.yml",
        "version: 1\nlayout: {id: t, command: [], match: {app_id: t}}\n",
    );
    fixture.file(
        "development.yaml",
        &SINGLE_WINDOW_YAML.replace("does-not-need-to-exist", "profile-app"),
    );
    fixture.file("browser.yml", SINGLE_WINDOW_YAML);
    for executable in ["notify-send", "profile-app"] {
        fixture.file(
            executable,
            "#!/bin/sh\nprintf 'unexpected invocation\\n' > \"$INVOCATION_RECORD\"\n",
        );
        fs::set_permissions(
            fixture.path.join(executable),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    fs::create_dir_all(fixture.path.join("state/sway-stage")).unwrap();
    let log_path = fixture.path.join("state/sway-stage/last-run.log");
    fs::write(&log_path, "previous launch\n").unwrap();
    let socket_path = fixture.path.join("offline.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    listener.set_nonblocking(true).unwrap();

    for (name, expected_exit_code) in [
        ("work", 0),
        ("bad", 1),
        ("missing-layout", 1),
        ("invalid-layout", 1),
        ("absent", 1),
        ("../work", 1),
    ] {
        let output = fixture
            .command()
            .env("SWAYSOCK", &socket_path)
            .env("I3SOCK", &socket_path)
            .env("PATH", &fixture.path)
            .env("INVOCATION_RECORD", fixture.path.join("invoked"))
            .env("XDG_STATE_HOME", fixture.path.join("state"))
            .args([
                "--profiles-dir",
                ".",
                "--layouts-dir",
                ".",
                "profile",
                "check",
                name,
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(expected_exit_code), "{output:?}");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(!fixture.path.join("invoked").exists());
        assert_eq!(fs::read_to_string(&log_path).unwrap(), "previous launch\n");
        assert_eq!(
            fs::read_dir(fixture.path.join("state/sway-stage"))
                .unwrap()
                .count(),
            1
        );
        if expected_exit_code == 0 {
            assert_eq!(output.stdout, b"work: valid\n");
            assert!(output.stderr.is_empty());
        } else {
            assert!(output.stdout.is_empty());
            assert!(!String::from_utf8_lossy(&output.stderr).contains("SWAYSOCK"));
        }
    }
}

#[test]
fn profile_check_accepts_exact_names_and_reports_invalid_target_locations_offline() {
    let fixture = CliFixture::new();
    fixture.file("development.yaml", SINGLE_WINDOW_YAML);
    fixture.file("browser.yml", SINGLE_WINDOW_YAML);
    let arguments = [
        "--profiles-dir",
        ".",
        "--layouts-dir",
        ".",
        "profile",
        "check",
        "work",
    ];
    fixture.file(
        "work.yaml",
        &PROFILE.replace("workspace: 1", "workspace: 'music player'"),
    );
    let output = fixture.run(&arguments, 0);
    assert_eq!(output.stdout, b"work: valid\n");
    assert!(output.stderr.is_empty());
    for target in ["'1'", "'1:code'", "'next'", "'music;exit'", "[]", "false"] {
        fixture.file(
            "work.yaml",
            &PROFILE.replace("workspace: 1", &format!("workspace: {target}")),
        );
        let output = fixture.run(&arguments, 1);
        let error = String::from_utf8_lossy(&output.stderr);
        for expected in ["work.yaml", "workspaces[1].workspace", "line 5", "column"] {
            assert!(error.contains(expected), "{error}");
        }
        assert!(!error.contains("SWAYSOCK"), "{error}");
    }
    assert!(!fixture.path.join(".local/state").exists());
}
