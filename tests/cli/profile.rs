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
fn offline_profile_commands_never_connect_launch_notify_or_change_logs_even_on_failure() {
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

    for (command, name, expected_exit_code, expected_stdout) in [
        ("check", Some("work"), 0, "work: valid\n"),
        ("check", Some("bad"), 1, ""),
        ("check", Some("missing-layout"), 1, ""),
        ("check", Some("invalid-layout"), 1, ""),
        ("check", Some("absent"), 1, ""),
        ("check", Some("../work"), 1, ""),
        (
            "show",
            Some("work"),
            0,
            "work:\n  3 -> development\n  1 -> browser\n",
        ),
        (
            "show",
            Some("missing-layout"),
            0,
            "missing-layout:\n  3 -> development\n  1 -> missing\n",
        ),
        (
            "show",
            Some("invalid-layout"),
            0,
            "invalid-layout:\n  3 -> development\n  1 -> invalid\n",
        ),
        ("show", Some("bad"), 1, ""),
        ("show", Some("absent"), 1, ""),
        (
            "list",
            None,
            0,
            "bad\nbrowser\ndevelopment\ninvalid\ninvalid-layout\nmissing-layout\nwork\n",
        ),
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
                command,
            ])
            .args(name)
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
            assert_eq!(output.stdout, expected_stdout.as_bytes());
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
    fixture.file(
        "work.yaml",
        &PROFILE.replace("workspace: 1", "workspace: 0"),
    );
    let output = fixture.run(&arguments, 0);
    assert_eq!(output.stdout, b"work: valid\n");
    assert!(output.stderr.is_empty());
    for target in [
        "'1'",
        "'1:code'",
        "'0'",
        "'0:code'",
        "-1",
        "'next'",
        "'music;exit'",
        "[]",
        "false",
    ] {
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

#[test]
fn profile_list_is_sorted_read_only_discovery_with_recoverable_warnings() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let fixture = CliFixture::new();
    fixture.file("z.yaml", "not a profile");
    fixture.file("a.yml", "malformed: [");
    fixture.file("A-1.yaml", "");
    fixture.file("duplicate.yaml", "");
    fixture.file("duplicate.yml", "");
    for filename in ["bad name.yaml", "_bad.yml", "é.yaml"] {
        fixture.file(filename, "");
    }
    fs::write(
        fixture
            .path
            .join(OsString::from_vec(b"bad\xff.yaml".to_vec())),
        "",
    )
    .unwrap();
    fixture.file("ignored.YAML", "");
    fixture.file("ignored.txt", "");
    fs::create_dir(fixture.path.join("directory.yaml")).unwrap();
    fs::create_dir(fixture.path.join("nested")).unwrap();
    fixture.file("nested/hidden.yaml", PROFILE);
    symlink("z.yaml", fixture.path.join("link.yaml")).unwrap();
    symlink("absent", fixture.path.join("dangling.yml")).unwrap();

    let arguments = [
        "profile",
        "list",
        "--profiles-dir",
        ".",
        "--layouts-dir",
        "missing",
    ];
    let output = fixture.run(&arguments, 0);
    assert_eq!(output.stdout, b"A-1\na\nz\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.lines().count(), 5, "{stderr}");
    for expected in [
        "skipping invalid profile filename",
        "bad name.yaml",
        "_bad.yml",
        "é.yaml",
        "bad\\xFF.yaml",
        "ambiguous profile name",
        "duplicate.yaml",
        "duplicate.yml",
    ] {
        assert!(stderr.contains(expected), "{stderr}");
    }
    assert!(
        stderr
            .lines()
            .all(|line| line.starts_with("sway-stage: warning: "))
    );
    let repeated = fixture.run(&arguments, 0);
    assert_eq!(output.stdout, repeated.stdout);
    assert_eq!(output.stderr, repeated.stderr);
}

#[test]
fn profile_list_respects_default_and_explicit_directories_and_empty_results() {
    let fixture = CliFixture::new();
    for arguments in [
        vec!["profile", "list"],
        vec!["profile", "list", "--profiles-dir", "."],
    ] {
        let output = fixture.run(&arguments, 0);
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
    assert!(!fixture.path.join(".config").exists());
    fixture.file("file", "");
    for directory in ["missing", "file"] {
        let output = fixture.run(&["--profiles-dir", directory, "profile", "list"], 1);
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .starts_with("sway-stage: profile list: read directory")
        );
    }
    assert!(!fixture.path.join("missing").exists());

    for (base, name) in [(".config", "home"), ("xdg", "xdg")] {
        fs::create_dir_all(fixture.path.join(base).join("sway-stage/profiles")).unwrap();
        fixture.file(&format!("{base}/sway-stage/profiles/{name}.yaml"), "");
    }
    let output = fixture.run(&["profile", "list"], 0);
    assert_eq!(output.stdout, b"home\n");
    assert!(output.stderr.is_empty());
    let output = fixture
        .command()
        .env("XDG_CONFIG_HOME", fixture.path.join("xdg"))
        .args(["profile", "list"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"xdg\n");
    assert!(output.stderr.is_empty());
    let output = fixture.run(&["--profiles-dir", ".", "profile", "list"], 0);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn profile_show_preserves_mapping_order_and_does_not_validate_layout_files() {
    let fixture = CliFixture::new();
    fixture.file("work.yml", "version: 1\nworkspaces:\n  - {workspace: 3, layout: missing}\n  - {workspace: music, layout: invalid}\n  - {workspace: 1, layout: missing}\n  - {workspace: 0, layout: missing}\n  - {workspace: 'music player', layout: invalid}\n");
    fixture.file("invalid.yaml", "malformed: [");
    for directory in ["missing", ".", "invalid.yaml"] {
        let output = fixture.run(
            &[
                "--profiles-dir",
                ".",
                "--layouts-dir",
                directory,
                "profile",
                "show",
                "work",
            ],
            0,
        );
        assert_eq!(output.stdout, b"work:\n  3 -> missing\n  \"music\" -> invalid\n  1 -> missing\n  0 -> missing\n  \"music player\" -> invalid\n");
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn profile_show_refuses_missing_ambiguous_and_malformed_profiles() {
    let fixture = CliFixture::new();
    fixture.file("duplicate.yaml", PROFILE);
    fixture.file("duplicate.yml", PROFILE);
    for (name, expected) in [
        ("missing", "not found"),
        ("../work", "invalid profile name"),
        ("duplicate", "ambiguous profile name"),
    ] {
        let output = fixture.run(&["--profiles-dir", ".", "profile", "show", name], 1);
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.starts_with("sway-stage: profile show:"), "{stderr}");
        assert!(stderr.contains(expected), "{stderr}");
    }
    for (yaml, expected) in [
        ("version: 1\nworkspaces: [".to_owned(), "workspaces"),
        (PROFILE.replace("version: 1", "version: 2"), "version"),
        (
            PROFILE.replace("workspace: 1", "workspace: 3"),
            "duplicate workspace number",
        ),
        (
            PROFILE.replace("workspace: 1", "workspace: '1'"),
            "workspaces[1].workspace",
        ),
        (
            PROFILE.replace("layout: browser", "layout: ../browser"),
            "workspaces[1].layout",
        ),
        (format!("{PROFILE}unknown: true\n"), "unknown field"),
    ] {
        fixture.file("bad.yaml", &yaml);
        let output = fixture.run(&["--profiles-dir", ".", "profile", "show", "bad"], 1);
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        for expected in ["sway-stage: profile show:", "bad.yaml", expected] {
            assert!(stderr.contains(expected), "{stderr}");
        }
    }
}

#[test]
fn profile_show_displays_only_configured_output_expectations() {
    let fixture = CliFixture::new();
    fixture.file(
        "work.yaml",
        &PROFILE
            .replace(
                "layout: development",
                "layout: development\n    output: 'Display 1; $[]'",
            )
            .replace("workspace: 1", "workspace: music"),
    );
    let output = fixture.run(&["--profiles-dir", ".", "profile", "show", "work"], 0);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "work:\n  3 -> development (output: \"Display 1; $[]\")\n  \"music\" -> browser\n"
    );
    assert!(output.stderr.is_empty());
}

/// Layouts and profiles share one discovery contract; given the same directory, their
/// listings, warnings, resolution errors, and directory failures differ only by noun.
#[test]
fn layout_and_profile_discovery_agree_on_equivalent_directories() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let fixture = CliFixture::new();
    fs::create_dir(fixture.path.join("configs")).unwrap();
    // Create in reverse lexical order so sorted output cannot come from creation order.
    for filename in [
        "z.yaml",
        "é.yaml",
        "same.yml",
        "same.yaml",
        "bad name.yaml",
        "a.yml",
        "_bad.yml",
        "A-1.yaml",
        "ignored.YAML",
        "ignored.txt",
    ] {
        fixture.file(&format!("configs/{filename}"), "");
    }
    fs::write(
        fixture
            .path
            .join("configs")
            .join(OsString::from_vec(b"bad\xff.yaml".to_vec())),
        "",
    )
    .unwrap();
    fs::create_dir(fixture.path.join("configs/directory.yaml")).unwrap();
    fs::create_dir(fixture.path.join("configs/nested")).unwrap();
    fixture.file("configs/nested/hidden.yaml", "");
    symlink("z.yaml", fixture.path.join("configs/link.yaml")).unwrap();
    symlink("absent", fixture.path.join("configs/dangling.yml")).unwrap();

    let layout_list = fixture.run(&["--layouts-dir", "configs", "list"], 0);
    let profile_list = fixture.run(&["--profiles-dir", "configs", "profile", "list"], 0);
    assert_eq!(layout_list.stdout, b"A-1\na\nz\n");
    assert_eq!(profile_list.stdout, layout_list.stdout);

    // Invalid filenames in path order, then ambiguous names in lexical order.
    let filename_rule = "use [A-Za-z0-9][A-Za-z0-9_-]* with .yaml or .yml";
    let expected_layout_warnings = [
        format!("sway-stage: warning: skipping invalid layout filename \"_bad.yml\"; {filename_rule}"),
        format!(
            "sway-stage: warning: skipping invalid layout filename \"bad name.yaml\"; {filename_rule}"
        ),
        format!(
            "sway-stage: warning: skipping invalid layout filename \"bad\\xFF.yaml\"; {filename_rule}"
        ),
        format!("sway-stage: warning: skipping invalid layout filename \"é.yaml\"; {filename_rule}"),
        "sway-stage: warning: ambiguous layout name \"same\": \"same.yaml\" and \"same.yml\"; rename one file".into(),
    ]
    .map(|line| line + "\n")
    .concat();
    let layout_warnings = String::from_utf8(layout_list.stderr).unwrap();
    let profile_warnings = String::from_utf8(profile_list.stderr).unwrap();
    assert_eq!(layout_warnings, expected_layout_warnings);
    assert_eq!(
        profile_warnings,
        layout_warnings.replace(" layout ", " profile ")
    );

    // Explicit resolution: ambiguity, absence (including ineligible entries), and the name
    // grammar. Only the command prefix and noun differ; not-found hints stay command-specific.
    for name in [
        "same",
        "missing",
        "link",
        "dangling",
        "directory",
        "hidden",
        "ignored",
        "bad name",
        "_bad",
    ] {
        let layout_error = fixture.run(&["--layouts-dir", "configs", "check", name], 1);
        let profile_error = fixture.run(&["--profiles-dir", "configs", "profile", "show", name], 1);
        let layout_error = String::from_utf8(layout_error.stderr).unwrap();
        let profile_error = String::from_utf8(profile_error.stderr).unwrap();
        let layout_detail = layout_error
            .strip_prefix("sway-stage: check: ")
            .unwrap_or_else(|| panic!("{layout_error}"));
        let profile_detail = profile_error
            .strip_prefix("sway-stage: profile show: ")
            .unwrap_or_else(|| panic!("{profile_error}"));

        if layout_detail.contains("not found in configs") {
            assert_eq!(
                profile_detail,
                format!("profile {name:?} not found in configs\n")
            );
        } else {
            assert_eq!(profile_detail, layout_detail.replace("layout", "profile"));
        }
        let expected = match name {
            "same" => "ambiguous layout name \"same\": \"same.yaml\" and \"same.yml\"",
            "bad name" | "_bad" => "invalid layout name",
            _ => "not found in configs",
        };
        assert!(layout_detail.contains(expected), "{name}: {layout_detail}");
    }

    // A missing explicit directory is fatal for listing and resolution alike.
    for (layout_arguments, profile_arguments) in [
        (vec!["list"], vec!["profile", "list"]),
        (vec!["check", "a"], vec!["profile", "show", "a"]),
    ] {
        let layout_error = fixture.run(
            &[&["--layouts-dir", "missing"], layout_arguments.as_slice()].concat(),
            1,
        );
        let profile_error = fixture.run(
            &[&["--profiles-dir", "missing"], profile_arguments.as_slice()].concat(),
            1,
        );
        let layout_error = String::from_utf8(layout_error.stderr).unwrap();
        let profile_error = String::from_utf8(profile_error.stderr).unwrap();
        let (_, layout_detail) = layout_error.split_once(": read directory").unwrap();
        let (_, profile_detail) = profile_error.split_once(": read directory").unwrap();
        assert_eq!(profile_detail, layout_detail);
    }

    // Missing default directories are empty for listing but report absence on resolution.
    let layout_list = fixture.run(&["list"], 0);
    let profile_list = fixture.run(&["profile", "list"], 0);
    for output in [&layout_list, &profile_list] {
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
    let layout_error = fixture.run(&["check", "a"], 1);
    let profile_error = fixture.run(&["profile", "show", "a"], 1);
    assert!(String::from_utf8_lossy(&layout_error.stderr).contains("not found in"));
    assert!(String::from_utf8_lossy(&profile_error.stderr).contains("not found in"));
    assert!(!fixture.path.join(".config").exists());
}
