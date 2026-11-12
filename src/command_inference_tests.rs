use super::*;

fn read_desktop_launcher(reader: impl Read, pid: i32) -> LauncherEvidence {
    let Some(bytes) = read_environment(reader) else {
        return LauncherEvidence::Unresolved;
    };
    let Some(environment) = LauncherEnvironment::parse(&bytes) else {
        return LauncherEvidence::Unresolved;
    };
    desktop_launcher(&environment, pid)
}

struct DesktopFixture {
    directory_path: std::path::PathBuf,
}

impl DesktopFixture {
    fn new() -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "sway-stage-launcher-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();

        std::fs::write(
            path.join("private launcher.desktop"),
            "[Desktop Entry]\nType=Application\n",
        )
        .unwrap();
        Self {
            directory_path: path,
        }
    }

    fn path(&self) -> std::path::PathBuf {
        self.directory_path.join("private launcher.desktop")
    }

    fn environment(&self, pid: &str) -> Vec<u8> {
        format!(
            "GIO_LAUNCHED_DESKTOP_FILE={}\0GIO_LAUNCHED_DESKTOP_FILE_PID={pid}\0",
            self.path().display()
        )
        .into_bytes()
    }
}

impl Drop for DesktopFixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory_path).unwrap();
    }
}

struct AppImageFixture {
    directory: DesktopFixture,
    image: PathBuf,
    appdir: PathBuf,
    executable: PathBuf,
}

impl AppImageFixture {
    fn new(name: &str) -> Self {
        let directory = DesktopFixture::new();
        let image = directory.directory_path.join(name);
        let appdir = directory.directory_path.join(".mount_example123");
        std::fs::create_dir(&appdir).unwrap();
        let executable = appdir.join("application");
        for path in [&image, &executable] {
            // Production checks filesystem properties, not the package's binary format.
            std::fs::write(path, b"fixture executable").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self {
            directory,
            image,
            appdir,
            executable,
        }
    }

    fn environment(&self) -> Vec<u8> {
        format!(
            "APPIMAGE={}\0APPDIR={}\0ARGV0={}\0",
            self.image.display(),
            self.appdir.display(),
            self.image.display()
        )
        .into_bytes()
    }
}

fn environment_with_field(environment: &[u8], key: &str, value: Option<&str>) -> Vec<u8> {
    let mut entries: Vec<&str> = std::str::from_utf8(environment)
        .unwrap()
        .split_terminator('\0')
        .filter(|entry| entry.split_once('=').unwrap().0 != key)
        .collect();
    let replacement = value.map(|value| format!("{key}={value}"));
    if let Some(entry) = &replacement {
        entries.push(entry);
    }
    format!("{}\0", entries.join("\0")).into_bytes()
}

fn package_inference(environment: &[u8], executable: Option<PathBuf>) -> Option<Vec<String>> {
    infer_host_command(
        101,
        Ok(environment),
        || executable,
        || panic!("recognized package must not fall back to internal argv"),
    )
}

#[test]
fn appimage_inference_preserves_the_persistent_path_as_one_argument() {
    for name in [
        "Example.AppImage",
        "application without extension",
        "雪 app",
    ] {
        let fixture = AppImageFixture::new(name);
        let expected = Some(vec![fixture.image.to_str().unwrap().to_owned()]);
        for invocation in [None, Some(name), Some(&format!("./{name}"))] {
            let mut environment =
                environment_with_field(&fixture.environment(), "ARGV0", invocation);
            environment.extend_from_slice(b"SECRET=private\0BINARY=\xff\0");
            assert_eq!(
                package_inference(&environment, Some(fixture.executable.clone())),
                expected
            );
        }
        assert_eq!(
            package_inference(&fixture.environment(), Some(fixture.executable.clone())),
            expected
        );
    }
}

#[test]
fn appimage_requires_a_usable_host_file_and_mounted_process_evidence() {
    let fixture = AppImageFixture::new("application");
    let environment = fixture.environment();
    for key in ["APPIMAGE", "APPDIR"] {
        for invalid in [None, Some(""), Some("relative"), Some("/missing/package")] {
            assert_eq!(
                package_inference(
                    &environment_with_field(&environment, key, invalid),
                    Some(fixture.executable.clone())
                ),
                None
            );
        }
    }
    for (name, mode, contents) in [("nonexecutable", 0o644, "contents"), ("empty", 0o755, "")] {
        let path = fixture.directory.directory_path.join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let environment = environment_with_field(&environment, "ARGV0", None);
        assert_eq!(
            package_inference(
                &environment_with_field(&environment, "APPIMAGE", path.to_str()),
                Some(fixture.executable.clone())
            ),
            None
        );
    }
    for path in [&fixture.appdir, &fixture.executable] {
        let environment = environment_with_field(&environment, "ARGV0", None);
        assert_eq!(
            package_inference(
                &environment_with_field(&environment, "APPIMAGE", path.to_str()),
                Some(fixture.executable.clone())
            ),
            None
        );
    }
    let symlink = fixture.directory.directory_path.join("symlink");
    std::os::unix::fs::symlink(&fixture.image, &symlink).unwrap();

    assert_eq!(
        package_inference(
            &environment_with_field(&environment, "APPIMAGE", symlink.to_str()),
            Some(fixture.executable.clone())
        ),
        None
    );
    for executable in [
        None,
        Some(fixture.image.clone()), // Unrelated descendant inheriting the environment.
        Some(fixture.appdir.clone()),
        Some(fixture.appdir.with_extension("other").join("application")),
        Some(fixture.appdir.join("vanished")),
        Some(PathBuf::from("relative")),
    ] {
        assert_eq!(package_inference(&environment, executable), None);
    }
    assert_eq!(
        package_inference(
            &environment_with_field(&environment, "APPDIR", fixture.image.to_str()),
            Some(fixture.executable.clone())
        ),
        None
    );
}

#[test]
fn appimage_rejects_temporary_host_paths_and_ambiguous_invocation_names() {
    let fixture = AppImageFixture::new("application");
    for invocation in [
        "",
        "other-name",
        "/other/application",
        "../application",
        "directory/application",
        "application/",
        "./application/.",
        "././application",
    ] {
        assert_eq!(
            package_inference(
                &environment_with_field(&fixture.environment(), "ARGV0", Some(invocation)),
                Some(fixture.executable.clone())
            ),
            None
        );
    }
    // A real symlink with a different basename still cannot select the same AppRun behavior.
    let invocation = fixture.directory.directory_path.join("another-binary");
    std::os::unix::fs::symlink(&fixture.image, &invocation).unwrap();

    assert_eq!(
        package_inference(
            &environment_with_field(&fixture.environment(), "ARGV0", invocation.to_str()),
            Some(fixture.executable.clone())
        ),
        None
    );
    let nested_mount = fixture.directory.directory_path.join(".mount_outer");
    std::fs::create_dir(&nested_mount).unwrap();
    let temporary_image = nested_mount.join("inner-image");
    std::fs::copy(&fixture.image, &temporary_image).unwrap();
    let environment = environment_with_field(&fixture.environment(), "ARGV0", None);
    assert_eq!(
        package_inference(
            &environment_with_field(&environment, "APPIMAGE", temporary_image.to_str()),
            Some(fixture.executable.clone())
        ),
        None
    );
}

#[test]
fn appimage_environment_fields_fail_closed_when_malformed_or_incomplete() {
    let fixture = AppImageFixture::new("application");
    let environment = fixture.environment();
    let executable = fixture.executable.clone();
    for key in ["APPIMAGE", "APPDIR", "ARGV0"] {
        let mut invalid_utf8 = environment_with_field(&environment, key, None);
        invalid_utf8.extend_from_slice(format!("{key}=").as_bytes());
        invalid_utf8.extend_from_slice(b"\xff\0");
        assert_eq!(
            package_inference(&invalid_utf8, Some(executable.clone())),
            None
        );

        let mut missing_separator = environment_with_field(&environment, key, None);
        missing_separator.extend_from_slice(format!("{key}\0").as_bytes());
        assert_eq!(
            package_inference(&missing_separator, Some(executable.clone())),
            None
        );

        let original_value = std::str::from_utf8(&environment)
            .unwrap()
            .split_terminator('\0')
            .filter_map(|entry| entry.split_once('='))
            .find(|(entry_key, _)| *entry_key == key)
            .unwrap()
            .1;
        for duplicate in [original_value, "different", ""] {
            let mut duplicated = environment.clone();
            duplicated.extend_from_slice(format!("{key}={duplicate}\0").as_bytes());
            assert_eq!(
                package_inference(&duplicated, Some(executable.clone())),
                None
            );
        }
    }
    let mut truncated = environment.clone();
    truncated.pop();

    assert_eq!(
        package_inference(&truncated, Some(executable.clone())),
        None
    );
    let mut unrelated_truncation = environment.clone();
    unrelated_truncation.extend_from_slice(b"SECRET=truncated");
    assert_eq!(
        package_inference(&unrelated_truncation, Some(executable.clone())),
        None
    );

    let mut maximum = environment;
    maximum.extend_from_slice(b"SECRET=");
    maximum.resize(MAX_ENVIRON_BYTES, b'x');
    maximum[MAX_ENVIRON_BYTES - 1] = 0;
    assert!(package_inference(&maximum, Some(executable.clone())).is_some());
    maximum.push(0);
    assert_eq!(package_inference(&maximum, Some(executable)), None);
}

#[test]
fn launcher_precedence_preserves_appimage_inference_and_snap_exclusion() {
    let fixture = AppImageFixture::new("application");
    let appimage = fixture.environment();
    let snap = b"SNAP=unsupported\0".to_vec();
    let mut unsupported = appimage.clone();
    unsupported.extend_from_slice(&snap);

    assert_eq!(package_inference(&unsupported, None), None);

    for package_environment in [&appimage, &snap, &unsupported] {
        let executable = fixture.executable.clone();
        // Flatpak's strict result wins without inspecting any host metadata.
        for (metadata, expected) in [
            (
                "[Application]\nname=org.example.App",
                Some(vec![
                    "flatpak".into(),
                    "run".into(),
                    "org.example.App".into(),
                ]),
            ),
            ("[Application]\nname=invalid", None),
        ] {
            assert_eq!(
                infer_process_command(Ok(metadata.as_bytes()), || {
                    package_inference(package_environment, Some(executable.clone()))
                }),
                expected
            );
        }
        let mut desktop = fixture.directory.environment("101");
        desktop.extend_from_slice(package_environment);
        assert_eq!(
            package_inference(&desktop, None),
            Some(vec![
                "gio".into(),
                "launch".into(),
                fixture.directory.path().to_str().unwrap().into()
            ])
        );
        // A known PID-only or unusable GLib launch must remain unresolved even when
        // package metadata could otherwise produce a conventional host command.
        for glib in [
            b"GIO_LAUNCHED_DESKTOP_FILE_PID=101\0".as_slice(),
            b"GIO_LAUNCHED_DESKTOP_FILE=/missing.desktop\0GIO_LAUNCHED_DESKTOP_FILE_PID=101\0",
        ] {
            let mut desktop = glib.to_vec();
            desktop.extend_from_slice(package_environment);
            assert_eq!(package_inference(&desktop, Some(executable.clone())), None);
        }
    }
    let mut inherited = fixture.directory.environment("102");
    inherited.extend_from_slice(&appimage);
    assert_eq!(
        package_inference(&inherited, Some(fixture.executable.clone())),
        Some(vec![fixture.image.to_str().unwrap().into()])
    );
    let mut inherited = fixture.directory.environment("102");
    inherited.extend_from_slice(&snap);
    assert_eq!(package_inference(&inherited, None), None);
}

#[test]
fn each_snap_marker_excludes_argv_and_appimage_regardless_of_value() {
    let fixture = AppImageFixture::new("application");
    for key in [
        "SNAP",
        "SNAP_NAME",
        "SNAP_INSTANCE_NAME",
        "SNAP_APP_NAME",
        "SNAP_INSTANCE_KEY",
    ] {
        for suffix in [
            b"=example\0".as_slice(),
            b"=\0",
            b"=\xff\0",
            b"\0",
            b"=one\0SNAP=two\0",
        ] {
            let mut marker = key.as_bytes().to_vec();
            marker.extend_from_slice(suffix);
            for mut environment in [Vec::new(), fixture.environment()] {
                environment.extend_from_slice(&marker);
                assert_eq!(
                    infer_host_command(
                        101,
                        Ok(environment.as_slice()),
                        || panic!("unsupported Snap needs no executable inspection"),
                        || panic!("unsupported Snap must not use argv"),
                    ),
                    None
                );
            }
        }
    }
}

#[test]
fn absent_package_identity_allows_ordinary_argv_without_executable_inspection() {
    for environment in [b"".as_slice(), b"ARGV0=custom-shell\0", b"UNRELATED=\xff\0"] {
        assert_eq!(
            infer_host_command(
                101,
                Ok(environment),
                || panic!("ordinary argv needs no package executable evidence"),
                || read_command_line(b"host-app\0two words\0\0".as_slice())
            ),
            Some(vec!["host-app".into(), "two words".into(), "".into()])
        );
    }
}

#[test]
fn verified_desktop_launcher_precedes_argv_and_preserves_path_boundaries() {
    let fixture = DesktopFixture::new();
    let mut environment = fixture.environment("101");
    environment.extend_from_slice(b"SECRET=must not escape\0BINARY=\xff\xfe\0OTHER=a=b\0");
    let expected = vec![
        "gio".into(),
        "launch".into(),
        fixture.path().to_str().unwrap().into(),
    ];

    assert_eq!(
        infer_host_command(
            101,
            Ok(environment.as_slice()),
            || panic!("no package evidence"),
            || panic!("must prefer launcher")
        ),
        Some(expected)
    );
}

#[test]
fn absent_and_inherited_launcher_evidence_allow_literal_argv_fallback() {
    let fixture = DesktopFixture::new();
    for environment in [
        Vec::new(),
        b"UNRELATED=\xff\0UNRELATED=duplicate ignored\0".to_vec(),
        fixture.environment("102"),
        b"GIO_LAUNCHED_DESKTOP_FILE_PID=102\0".to_vec(),
        b"GIO_LAUNCHED_DESKTOP_FILE=\xff\0GIO_LAUNCHED_DESKTOP_FILE_PID=102\0".to_vec(),
    ] {
        assert_eq!(
            read_desktop_launcher(environment.as_slice(), 101),
            LauncherEvidence::Absent
        );
        assert_eq!(
            infer_host_command(
                101,
                Ok(environment.as_slice()),
                || panic!("no package evidence"),
                || read_command_line(b"host\0two words\0\0".as_slice())
            ),
            Some(vec!["host".into(), "two words".into(), "".into()])
        );
    }
}

#[test]
fn glib_284_replay_with_only_matching_pid_requires_manual_command_editing() {
    // gio 2.84.4 loses the desktop filename. A PID alone cannot reconstruct its launcher,
    // even though replay successfully started an application with readable argv.
    let environment = b"GIO_LAUNCHED_DESKTOP_FILE_PID=101\0";

    assert_eq!(
        infer_host_command(
            101,
            Ok(environment.as_slice()),
            || panic!("no package evidence"),
            || panic!("PID-only launcher metadata must not fall back to argv")
        ),
        None
    );
}

#[test]
fn recognized_but_unusable_launcher_evidence_suppresses_argv() {
    let fixture = DesktopFixture::new();
    let mut invalid = Vec::new();
    for pid in [
        "",
        "0",
        "-1",
        "+101",
        " 101",
        "101 ",
        "101x",
        "99999999999999999999",
    ] {
        invalid.push(fixture.environment(pid));
    }
    for path in ["", "relative.desktop", "/missing/sway-stage.desktop"] {
        invalid.push(
            format!("GIO_LAUNCHED_DESKTOP_FILE={path}\0GIO_LAUNCHED_DESKTOP_FILE_PID=101\0")
                .into_bytes(),
        );
    }
    for name in [
        "directory.desktop",
        "empty.desktop",
        "wrong.txt",
        "broken.desktop",
    ] {
        let path = fixture.directory_path.join(name);
        match name {
            "directory.desktop" => std::fs::create_dir(&path).unwrap(),
            "broken.desktop" => std::os::unix::fs::symlink("missing", &path).unwrap(),
            "empty.desktop" => std::fs::write(&path, "").unwrap(),
            _ => std::fs::write(&path, "[Desktop Entry]").unwrap(),
        }
        invalid.push(
            format!(
                "GIO_LAUNCHED_DESKTOP_FILE={}\0GIO_LAUNCHED_DESKTOP_FILE_PID=101\0",
                path.display()
            )
            .into_bytes(),
        );
    }
    invalid.extend([
        b"GIO_LAUNCHED_DESKTOP_FILE=/missing.desktop\0".to_vec(),
        b"GIO_LAUNCHED_DESKTOP_FILE_PID=101\0".to_vec(),
        b"GIO_LAUNCHED_DESKTOP_FILE=\xff\0GIO_LAUNCHED_DESKTOP_FILE_PID=101\0".to_vec(),
        b"GIO_LAUNCHED_DESKTOP_FILE=x\0GIO_LAUNCHED_DESKTOP_FILE_PID=\xff\0".to_vec(),
        b"GIO_LAUNCHED_DESKTOP_FILE\0".to_vec(),
        b"GIO_LAUNCHED_DESKTOP_FILE_PID\0".to_vec(),
    ]);
    for key in ["GIO_LAUNCHED_DESKTOP_FILE", "GIO_LAUNCHED_DESKTOP_FILE_PID"] {
        for value in ["101", "102", ""] {
            let mut duplicate = fixture.environment("101");
            duplicate.extend_from_slice(format!("{key}={value}\0").as_bytes());
            invalid.push(duplicate);
        }
    }
    let mut identical_fields = fixture.environment("101");
    identical_fields.extend_from_slice(&fixture.environment("101"));
    invalid.push(identical_fields);
    let mut conflicting_inherited = fixture.environment("102");
    conflicting_inherited.extend_from_slice(b"GIO_LAUNCHED_DESKTOP_FILE_PID=101\0");
    invalid.push(conflicting_inherited);
    for environment in invalid {
        assert_eq!(
            read_desktop_launcher(environment.as_slice(), 101),
            LauncherEvidence::Unresolved
        );
        assert!(
            infer_host_command(
                101,
                Ok(environment.as_slice()),
                || panic!("no package evidence"),
                || panic!("unsafe argv fallback")
            )
            .is_none()
        );
    }
}

#[test]
fn environment_reads_fail_closed_without_exposing_sensitive_errors() {
    struct SensitiveFailure;
    impl Read for SensitiveFailure {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SECRET=private",
            ))
        }
    }

    assert_eq!(
        read_desktop_launcher(SensitiveFailure, 101),
        LauncherEvidence::Unresolved
    );
    for kind in [io::ErrorKind::NotFound, io::ErrorKind::PermissionDenied] {
        let unavailable: io::Result<&[u8]> = Err(io::Error::new(kind, "SECRET=private"));
        assert_eq!(
            infer_host_command(
                101,
                unavailable,
                || panic!("unavailable environment"),
                || panic!("uncertain environment")
            ),
            None
        );
    }
    let fixture = DesktopFixture::new();
    let mut truncated = fixture.environment("101");
    truncated.pop();
    assert_eq!(
        read_desktop_launcher(truncated.as_slice(), 101),
        LauncherEvidence::Unresolved
    );
    // A valid prefix does not excuse a truncated unrelated final field.
    let mut incomplete = fixture.environment("101");
    incomplete.extend_from_slice(b"SECRET=truncated");
    assert_eq!(
        read_desktop_launcher(incomplete.as_slice(), 101),
        LauncherEvidence::Unresolved
    );

    let mut maximum = b"UNRELATED=".to_vec();
    maximum.resize(MAX_ENVIRON_BYTES, b'x');
    maximum[MAX_ENVIRON_BYTES - 1] = 0;
    assert_eq!(
        read_desktop_launcher(maximum.as_slice(), 101),
        LauncherEvidence::Absent
    );
    maximum.push(0);
    assert_eq!(
        read_desktop_launcher(maximum.as_slice(), 101),
        LauncherEvidence::Unresolved
    );
    let mut oversized_launcher = fixture.environment("101");
    oversized_launcher.resize(MAX_ENVIRON_BYTES + 1, 0);
    assert_eq!(
        read_desktop_launcher(oversized_launcher.as_slice(), 101),
        LauncherEvidence::Unresolved
    );
    let mut endless = io::repeat(b'x').take(MAX_ENVIRON_BYTES as u64 + 10);
    assert_eq!(
        read_desktop_launcher(&mut endless, 101),
        LauncherEvidence::Unresolved
    );
    assert_eq!(endless.limit(), 9);
}

#[test]
fn flatpak_evidence_precedes_even_valid_or_inherited_desktop_metadata() {
    let fixture = DesktopFixture::new();
    for pid in ["101", "102"] {
        for (metadata, expected) in [
            (
                "[Application]\nname=org.example.App",
                Some(vec![
                    "flatpak".into(),
                    "run".into(),
                    "org.example.App".into(),
                ]),
            ),
            ("[Application]\nname=partial", None),
            ("[Runtime]\nname=org.example.Runtime", None),
        ] {
            let environment = fixture.environment(pid);
            assert_eq!(
                infer_process_command(Ok(metadata.as_bytes()), || {
                    infer_host_command(
                        101,
                        Ok(environment.as_slice()),
                        || panic!("no package evidence"),
                        || panic!("Flatpak must not consult argv"),
                    )
                }),
                expected
            );
        }
    }
    let missing: io::Result<&[u8]> = Err(io::ErrorKind::NotFound.into());

    assert!(
        infer_process_command(missing, || {
            infer_host_command(
                101,
                Ok(fixture.environment("101").as_slice()),
                || panic!("no package evidence"),
                || None,
            )
        })
        .is_some()
    );
}

#[test]
fn command_line_reader_preserves_literal_arguments_and_empty_arguments() {
    let arguments = [
        "foot",
        "--app-id",
        "two words",
        "'quotes'\"",
        "雪🦀",
        "#secret: [x]",
        "*alias",
        "true",
        "null",
        "line\nbreak",
        "",
        "",
    ];
    let bytes = arguments.join("\0") + "\0";

    assert_eq!(read_command_line(bytes.as_bytes()).unwrap(), arguments);
    assert_eq!(read_command_line(&b"foot\0"[..]).unwrap(), ["foot"]);
}

#[test]
fn command_line_reader_rejects_unusable_or_incomplete_data_and_bounds_reads() {
    for bytes in [
        &b""[..],
        &b"\0"[..],
        &b"\0argument\0"[..],
        &b"foot"[..],
        &b"foot\0unfinished"[..],
        &b"foot\0\xff\0"[..],
        &b"\xff\0"[..],
    ] {
        assert!(read_command_line(bytes).is_none());
    }

    let mut maximum = vec![b'x'; MAX_CMDLINE_BYTES];
    maximum[MAX_CMDLINE_BYTES - 1] = 0;

    assert!(read_command_line(maximum.as_slice()).is_some());
    maximum.push(0);
    assert!(read_command_line(maximum.as_slice()).is_none());

    let mut endless = std::io::repeat(b'x').take(MAX_CMDLINE_BYTES as u64 + 10);
    assert!(read_command_line(&mut endless).is_none());
    // The bounded reader stops even if the source has no EOF.
    assert_eq!(endless.limit(), 9);
}

#[test]
fn process_read_failures_are_silent_inference_misses() {
    struct Unavailable(std::io::ErrorKind);
    impl Read for Unavailable {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                self.0,
                "sensitive argument must not escape",
            ))
        }
    }
    for kind in [
        std::io::ErrorKind::NotFound,
        std::io::ErrorKind::PermissionDenied,
    ] {
        assert!(read_command_line(Unavailable(kind)).is_none());
    }
    for pid in [i32::MIN, -1, 0] {
        assert!(read_process_command(pid).is_none());
    }
}

#[test]
fn flatpak_metadata_builds_host_commands_from_application_and_instance_fields() {
    let application = "# generated by Flatpak\n[Application]\nname=com.mastermindzh.tidal-hifi\n\
                       runtime=org.freedesktop.Platform/x86_64/24.08\n";

    assert_eq!(
        flatpak_command(application).unwrap(),
        ["flatpak", "run", "com.mastermindzh.tidal-hifi"]
    );
    let instance = format!(
        "{application}[Instance]\nbranch=beta\narch=x86_64\n\
         extra-args=--filesystem=host;--env=EXAMPLE=value;\n"
    );
    assert_eq!(
        flatpak_command(&instance).unwrap(),
        [
            "flatpak",
            "run",
            "--branch=beta",
            "--arch=x86_64",
            "com.mastermindzh.tidal-hifi"
        ]
    );
    for (field, option) in [
        ("branch=stable", "--branch=stable"),
        ("arch=aarch64", "--arch=aarch64"),
    ] {
        assert_eq!(
            flatpak_command(&format!("{application}[Instance]\n{field}\n")).unwrap(),
            ["flatpak", "run", option, "com.mastermindzh.tidal-hifi"]
        );
    }
    // Old or unrelated fields must not be confused with the current [Instance] fields.
    assert_eq!(
        flatpak_command(&format!("{application}branch=ignored\narch=ignored\n")).unwrap(),
        ["flatpak", "run", "com.mastermindzh.tidal-hifi"]
    );
}

#[test]
fn flatpak_metadata_rejects_missing_ambiguous_and_malformed_identity() {
    for metadata in [
        "",
        "[Instance]\narch=x86_64",
        "[Runtime]\nname=org.example.Runtime",
        "[Application]\nruntime=org.example.Runtime/x86_64/stable",
        "[Application]\nname=",
        "[Application\nname=org.example.App",
        "name=org.example.App",
        "[Application]\nname=org.example.App\ninvalid line",
        "[Application]\nname=org.example.App\nname=org.example.Other",
        "[Application]\nname=org.example.App\n[Application]\nname=org.example.Other",
        "[Application]\nname=org.example.App\n[Runtime]\nname=org.example.Runtime",
        "[Application]\nname=org.example.App\0",
    ] {
        assert!(flatpak_command(metadata).is_none(), "{metadata:?}");
    }
    for application_id in [
        "org.example",
        "org..App",
        ".org.example.App",
        "org.example.App.",
        "org.1example.App",
        "org.example.1App",
        "org.ex-ample.App",
        "--command=sh",
        "org.example.App;sh",
        "org.example.$(sh)",
        "org.example.App/extra",
        "org.example.App --command=sh",
        "org.example.App\\sargument",
        "org.example.雪",
    ] {
        assert!(flatpak_command(&format!("[Application]\nname={application_id}")).is_none());
    }

    assert!(!valid_flatpak_id(&format!(
        "org.example.{}",
        "x".repeat(244)
    )));
    assert!(valid_flatpak_id(&format!(
        "org.example.{}",
        "x".repeat(243)
    )));
    assert!(valid_flatpak_id("org.example._App-2"));

    for field in ["branch", "arch"] {
        for invalid in ["", "--command=sh", "x/y", "x y", "x;sh", "x\\sy", "$(sh)"] {
            let metadata =
                format!("[Application]\nname=org.example.App\n[Instance]\n{field}={invalid}");
            assert!(flatpak_command(&metadata).is_none());
        }
        let duplicate =
            format!("[Application]\nname=org.example.App\n[Instance]\n{field}=a\n{field}=b");
        assert!(flatpak_command(&duplicate).is_none());
    }
}

#[test]
fn flatpak_metadata_reads_are_bounded_and_fail_closed() {
    let mut metadata = b"[Application]\nname=org.example.App\n#".to_vec();
    metadata.resize(MAX_FLATPAK_INFO_BYTES, b'x');

    assert!(read_flatpak_command(metadata.as_slice()).is_some());
    metadata.push(b'x');
    assert!(read_flatpak_command(metadata.as_slice()).is_none());
    assert!(read_flatpak_command(&b"[Application]\nname=org.example.\xff"[..]).is_none());

    let mut endless = io::repeat(b'x').take(MAX_FLATPAK_INFO_BYTES as u64 + 10);
    assert!(read_flatpak_command(&mut endless).is_none());
    assert_eq!(endless.limit(), 9);
    struct Unreadable;
    impl Read for Unreadable {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::PermissionDenied.into())
        }
    }
    assert!(read_flatpak_command(Unreadable).is_none());
    for kind in [
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::NotADirectory,
        io::ErrorKind::Other,
    ] {
        let metadata: io::Result<&[u8]> = Err(kind.into());
        assert!(
            infer_process_command(metadata, || panic!("uncertain identity must not read argv"))
                .is_none()
        );
    }
    let missing: io::Result<&[u8]> = Err(io::ErrorKind::NotFound.into());
    assert_eq!(
        infer_process_command(missing, || read_command_line(
            &b"host-app\0two words\0\0"[..]
        ))
        .unwrap(),
        ["host-app", "two words", ""]
    );
    for metadata in [
        "[Application]\nname=org.example.App",
        "[Application]\nname=",
        "[Runtime]\nname=org.example.Runtime",
    ] {
        // Even unusable metadata must suppress sandbox-internal /app argv.
        let _ = infer_process_command(Ok(metadata.as_bytes()), || {
            panic!("must not reuse /app argv")
        });
    }
}

fn stat_fixture(pid: &str, comm: &[u8], start_time: &str) -> Vec<u8> {
    let mut bytes = format!("{pid} (").into_bytes();
    bytes.extend_from_slice(comm);
    bytes.extend_from_slice(format!(") S {} {start_time} 0 0\n", ["0"; 18].join(" ")).as_bytes());
    bytes
}

#[test]
fn start_time_uses_the_pid_and_field_22_after_arbitrary_comm_bytes() {
    for comm in [b"ordinary".as_slice(), b"name ) ( space\n\xff"] {
        for ticks in [0, 123456, u64::MAX] {
            let stat = stat_fixture("101", comm, &ticks.to_string());
            assert_eq!(read_start_time(stat.as_slice(), 101), Some(ticks));
        }
    }
    for pid in ["0", "-1", "+101", "0101", "102", "99999999999999999999"] {
        assert!(read_start_time(stat_fixture(pid, b"app", "1").as_slice(), 101).is_none());
    }
    for ticks in ["", "-1", "+1", "not-a-number", "18446744073709551616"] {
        // No trailing fields: an absent start time must not consume a later field.
        let stat = format!("101 (app) S {} {ticks}\n", ["0"; 18].join(" "));
        assert!(read_start_time(stat.as_bytes(), 101).is_none());
    }
    for stat in [
        b"".as_slice(),
        b"101 app) S 0\n",
        b"101 (app S 0\n",
        b"101 (app) S\n",
    ] {
        assert!(read_start_time(stat, 101).is_none());
    }
    let mut truncated = stat_fixture("101", b"app", "123");
    truncated.pop();

    assert!(read_start_time(truncated.as_slice(), 101).is_none());
    let mut endless = io::repeat(b'x').take(MAX_STAT_BYTES as u64 + 10);
    assert!(read_start_time(&mut endless, 101).is_none());
    assert_eq!(endless.limit(), 9);
}

#[test]
fn inference_discards_commands_when_process_or_executable_identity_changes() {
    let original = ProcessIdentity {
        start_time_ticks: 123,
        executable: "/application".into(),
        executable_device: 1,
        executable_inode: 2,
    };
    let mut changed_start = original.clone();
    changed_start.start_time_ticks += 1;
    let mut changed_executable = original.clone();
    changed_executable.executable = "/replacement".into();
    let mut changed_device = original.clone();
    changed_device.executable_device += 1;
    let mut changed_inode = original.clone();
    changed_inode.executable_inode += 1;
    for after in [
        None,
        Some(changed_start),
        Some(changed_executable),
        Some(changed_device),
        Some(changed_inode),
    ] {
        let mut observations = [Some(original.clone()), after].into_iter();
        let command = with_stable_process(
            || observations.next().unwrap(),
            || Some(vec!["application".into(), "SECRET=private".into()]),
        );
        assert!(command.is_none());
    }
    let mut observations = [Some(original.clone()), Some(original)].into_iter();

    assert!(
        with_stable_process(|| observations.next().unwrap(), || Some(vec!["app".into()])).is_some()
    );
    assert!(
        with_stable_process(
            || None,
            || panic!("missing identity must not inspect metadata")
        )
        .is_none()
    );
}

#[test]
fn missing_or_malformed_process_observations_remain_silent() {
    let fixture = DesktopFixture::new();

    assert!(read_process_identity(&fixture.directory_path, 101).is_none());
    std::fs::write(fixture.directory_path.join("stat"), b"SECRET=private\n").unwrap();
    assert!(read_process_identity(&fixture.directory_path, 101).is_none());
    std::fs::write(
        fixture.directory_path.join("stat"),
        stat_fixture("101", b"app", "123"),
    )
    .unwrap();
    assert!(read_process_identity(&fixture.directory_path, 101).is_none());
    std::os::unix::fs::symlink(fixture.path(), fixture.directory_path.join("exe")).unwrap();
    assert!(read_process_identity(&fixture.directory_path, 101).is_some());
    std::fs::remove_file(fixture.path()).unwrap();
    assert!(read_process_identity(&fixture.directory_path, 101).is_none());
}

#[test]
fn deleted_appimage_executables_never_corroborate_an_inherited_launch() {
    let fixture = AppImageFixture::new("application");
    // Even a replacement with this literal name cannot stand in for a deleted executable.
    let deleted = fixture.appdir.join("application (deleted)");
    std::fs::write(&deleted, b"replacement").unwrap();

    assert!(package_inference(&fixture.environment(), Some(deleted)).is_none());
}

#[test]
fn a_temporarily_unavailable_desktop_launcher_never_falls_back_to_secret_argv() {
    let fixture = DesktopFixture::new();
    let environment = fixture.environment("101");
    let hidden = fixture.path().with_extension("unavailable");
    std::fs::rename(fixture.path(), &hidden).unwrap();

    assert!(
        infer_host_command(
            101,
            Ok(environment.as_slice()),
            || None,
            || panic!("SECRET argv must not be used")
        )
        .is_none()
    );
    std::fs::rename(hidden, fixture.path()).unwrap();
    assert!(matches!(
        read_desktop_launcher(environment.as_slice(), 101),
        LauncherEvidence::Command(_)
    ));
}
