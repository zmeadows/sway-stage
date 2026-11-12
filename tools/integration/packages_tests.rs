use super::*;
use crate::fixtures::{
    TARGET_WORKSPACE_ID, container_ids, desktop, node_mut, parse_tree, rect, uncertain_message,
    wayland_window, window_event, xwayland_window,
};
use crate::ownership::{AcceptedWindows, verify_ownership};
use std::os::unix::fs::{PermissionsExt, symlink};

fn image_directory() -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("sway-stage-package-tests-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    directory
}

#[test]
fn configured_absence_skips_but_invalid_image_fails_without_a_launcher_probe() {
    assert_eq!(SCENARIOS, ["appimage-inference", "flatpak-inference"]);
    assert!(
        Package::configured(SCENARIOS[0], None, None)
            .unwrap()
            .is_none()
    );
    assert!(
        Package::configured(SCENARIOS[1], None, None)
            .unwrap()
            .is_none()
    );
    assert!(
        Package::configured(
            SCENARIOS[0],
            Some(Path::new("/does-not-exist.AppImage")),
            None
        )
        .is_err()
    );
    assert!(Package::configured(SCENARIOS[1], None, Some("--bad-option")).is_err());
}

#[test]
fn appimage_normalization_resolves_aliases_and_rejects_nonimages() {
    let directory = image_directory();
    let image = directory.join("real image.AppImage");
    let alias = directory.join("another-name");
    let mut bytes = vec![0; 64];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[8..11].copy_from_slice(b"AI\x02");
    fs::write(&image, &bytes).unwrap();

    assert!(normalize_appimage(&image).is_err());
    fs::set_permissions(&image, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&image, &alias).unwrap();
    assert_eq!(
        normalize_appimage(&alias).unwrap(),
        image.canonicalize().unwrap()
    );
    // Build a relative path through the filesystem root without changing the test cwd.
    let mut relative_image = PathBuf::new();
    for _ in std::env::current_dir().unwrap().ancestors().skip(1) {
        relative_image.push("..");
    }
    relative_image.push(image.strip_prefix("/").unwrap());
    assert_eq!(
        normalize_appimage(&relative_image).unwrap(),
        image.canonicalize().unwrap()
    );
    assert!(normalize_appimage(&directory).is_err());
    bytes[8] = 0;
    fs::write(&image, bytes).unwrap();
    assert!(normalize_appimage(&alias).is_err());
    fs::write(&image, b"#!/bin/sh\n").unwrap();
    assert!(normalize_appimage(&image).is_err());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn flatpak_cli_identity_is_a_complete_id_not_an_option_or_reference() {
    for valid in [
        "org.gnome.Calculator",
        "com.example.Some-App",
        "a.b.c",
        "org.example.-App",
        "org.example.-",
        "_org.example_2._App9",
        "org.example.subdomain.App",
        "org.example.Some--App",
    ] {
        validate_flatpak_id(valid).unwrap();
    }
    for invalid in [
        "",
        "app",
        "org.App",
        "org..App",
        ".org.example.App",
        "org.example.App.",
        "org.example..App",
        "org-example.test.App",
        "org.example-site.App",
        "org.example.Sub-App.More",
        "-org.example.App",
        "org.-example.App",
        "1org.example.App",
        "org.1example.App",
        "org.example.é",
        "org.example.App+",
        "org.example.App ",
        "org.example.App\t",
        "org.example.App\0",
        "org.example.1app",
        "--user",
        "org.example.App/stable",
        "org.example.App\n",
    ] {
        assert!(validate_flatpak_id(invalid).is_err(), "{invalid:?}");
    }
}

#[test]
fn flatpak_cli_identity_enforces_the_total_length_boundary() {
    let longest_id = format!("a.b.{}", "c".repeat(251));

    assert_eq!(longest_id.len(), 255);
    validate_flatpak_id(&longest_id).unwrap();
    assert!(validate_flatpak_id(&format!("{longest_id}c")).is_err());

    // The limit applies to the entire ID, with no separate component length limit.
    let long_domain = format!("{}.b.c", "a".repeat(251));
    validate_flatpak_id(&long_domain).unwrap();
    assert!(validate_flatpak_id(&format!("a{long_domain}")).is_err());
}

fn flatpak_metadata() -> String {
    "[Application]\nname=org.example.App\n[Instance]\ninstance-id=123\narch=aarch64\nbranch=beta\n"
        .into()
}

#[test]
fn flatpak_expected_command_comes_from_the_running_instance_table() {
    let fields = flatpak_fields(&flatpak_metadata()).unwrap();
    let rows = "999 org.other.App x86_64 stable 10 11\n123 org.example.App aarch64 beta 42 43\n";

    assert_eq!(
        verify_flatpak_instance("org.example.App", &fields, rows)
            .unwrap()
            .expected_command,
        [
            "flatpak",
            "run",
            "--branch=beta",
            "--arch=aarch64",
            "org.example.App"
        ]
    );
    for invalid_reply in [
        "",
        "123 org.other.App aarch64 beta 42 43",
        "123 org.example.App x86_64 beta 42 43",
        "123 org.example.App aarch64 stable 42 43",
        "123 org.example.App aarch64 beta 0 43",
        "123 org.example.App aarch64 beta 42",
        "123 org.example.App aarch64 beta 42 43\n123 org.example.App aarch64 beta 42 43",
    ] {
        assert!(
            verify_flatpak_instance("org.example.App", &fields, invalid_reply).is_err(),
            "{invalid_reply}"
        );
    }
}

#[test]
fn missing_duplicate_and_contradictory_sandbox_metadata_fail() {
    for invalid_reply in [
        flatpak_metadata().replace("arch=aarch64\n", ""),
        flatpak_metadata().replace("name=org.example.App", "name=org.other.App"),
        flatpak_metadata().replace("instance-id=123", "instance-id=456"),
    ] {
        let fields = flatpak_fields(&invalid_reply).unwrap();
        assert!(
            verify_flatpak_instance(
                "org.example.App",
                &fields,
                "123 org.example.App aarch64 beta 42 43"
            )
            .is_err()
        );
    }
    for invalid_reply in [
        format!(
            "{}[Runtime]\nname=org.example.Runtime\n",
            flatpak_metadata()
        ),
        format!("{}branch=stable\n", flatpak_metadata()),
        format!("{}[Instance]\n", flatpak_metadata()),
        "name=org.example.App".into(),
    ] {
        assert!(flatpak_fields(&invalid_reply).is_err());
    }
}

#[test]
fn appimage_metadata_needs_mount_and_executable_evidence() {
    let mounts = "31 20 0:55 / /tmp/.mount_App rw - fuse.AppImage AppImage rw\n";
    validate_image_mount(
        Path::new("/apps/AppImage"),
        Path::new("/tmp/.mount_App"),
        Path::new("/tmp/.mount_App/usr/bin/app"),
        mounts,
    )
    .unwrap();
    for (image, appdir, exe, mounts) in [
        (
            "/apps/AppImage",
            "/tmp/.mount_App",
            "/usr/bin/unrelated",
            mounts,
        ),
        (
            "/tmp/.mount_App/image",
            "/tmp/.mount_App",
            "/tmp/.mount_App/app",
            mounts,
        ),
        (
            "/apps/AppImage",
            "/tmp/.mount_App",
            "/tmp/.mount_App",
            mounts,
        ),
        (
            "/apps/AppImage",
            "/tmp/.mount_App",
            "/tmp/.mount_App/app",
            "",
        ),
        (
            "/apps/AppImage",
            "/tmp/.mount_App",
            "/tmp/.mount_App/app",
            "31 20 0:55 / /tmp/.mount_App rw - ext4 /dev/sda rw",
        ),
    ] {
        assert!(
            validate_image_mount(Path::new(image), Path::new(appdir), Path::new(exe), mounts)
                .is_err()
        );
    }

    assert_eq!(
        decode_mount_path("/tmp/mount\\040with\\134slash").unwrap(),
        PathBuf::from("/tmp/mount with\\slash")
    );
    assert_eq!(
        environment_field(b"APPIMAGE=/apps/AppImage\0", "APPIMAGE").unwrap(),
        "/apps/AppImage"
    );
    for invalid_reply in [
        b"APPDIR=/tmp/mount\0".as_slice(),
        b"APPIMAGE=\0",
        b"APPIMAGE=a\0APPIMAGE=b\0",
        b"APPIMAGE=a",
    ] {
        assert!(environment_field(invalid_reply, "APPIMAGE").is_err());
    }
}

#[test]
fn appimage_revalidation_ignores_unrelated_mount_changes() {
    let image_mount = "31 20 0:55 / /tmp/.mount_App rw - fuse.AppImage AppImage rw";
    let evidence = |mounts: &str| {
        validate_image_mount(
            Path::new("/apps/AppImage"),
            Path::new("/tmp/.mount_App"),
            Path::new("/tmp/.mount_App/usr/bin/app"),
            mounts,
        )
        .map(|mount| PackageEvidence::AppImage {
            image: "/apps/AppImage".into(),
            appdir: "/tmp/.mount_App".into(),
            mount,
        })
    };
    let original = evidence(image_mount).unwrap();
    for unrelated in [
        "",
        "40 20 0:60 / /media/disk rw - ext4 /dev/sdb rw\n",
        "41 20 0:61 / /tmp/.mount_Other rw - fuse.AppImage Other rw\n",
        "42 31 0:62 / /tmp/.mount_App/share rw - tmpfs tmpfs rw\n",
    ] {
        assert!(evidence(&format!("{unrelated}{image_mount}\n")).unwrap() == original);
    }

    for changed in [
        image_mount.replace("31 20", "32 20"),
        image_mount.replace("0:55", "0:56"),
        image_mount.replace(" / /tmp", " /subdir /tmp"),
        image_mount.replace("AppImage rw", "DifferentImage rw"),
        image_mount.replace("rw -", "ro -"),
        image_mount.replace("fuse.AppImage", "squashfs"),
    ] {
        assert!(evidence(&changed).unwrap() != original, "{changed}");
    }
    for invalid in [
        String::new(),
        image_mount.replace("/tmp/.mount_App", "/tmp/.mount_Other"),
        format!("{image_mount}\n{image_mount}"),
        format!("{image_mount}\n50 31 0:55 / /tmp/.mount_App/usr rw - tmpfs tmpfs rw"),
        format!("{image_mount}\n50 31 0:55 / /tmp/.mount_App/usr/bin/app rw - tmpfs tmpfs rw"),
        image_mount.replace(" - ", " "),
        image_mount.replace("31 20", "bad 20"),
        image_mount.replace("0:55", "bad"),
        image_mount.replace("AppImage rw", "AppImage"),
        image_mount.replace("/tmp/.mount_App", "/tmp/.mount_App\\999"),
        image_mount.replace("fuse.AppImage", "ext4"),
    ] {
        assert!(evidence(&invalid).is_err(), "{invalid}");
    }
}

fn candidate_tree() -> TreeNode {
    parse_tree(desktop(
        "splith",
        vec![wayland_window(
            20,
            "real.application",
            rect(0, 0, 1920, 1080),
        )],
        20,
    ))
}

#[test]
fn discovery_needs_one_fresh_creation_and_discovers_the_real_identity() {
    let before = parse_tree(desktop("splith", Vec::new(), TARGET_WORKSPACE_ID));
    let preexisting = container_ids(&before);
    let tree = candidate_tree();
    let event = window_event("new", 20, Some("real.application"));
    let window = discover(&tree, &[event], &preexisting, TARGET_WORKSPACE_ID)
        .unwrap()
        .unwrap();

    assert_eq!(
        selector(window).unwrap(),
        Selector::AppId("real.application".into())
    );
    assert!(discover(&tree, &[], &preexisting, TARGET_WORKSPACE_ID).is_err());
    let x11 = parse_tree(xwayland_window(21, "ActualClass", rect(0, 0, 1920, 1080)));
    assert_eq!(
        selector(&x11).unwrap(),
        Selector::Class("ActualClass".into())
    );
    let mut ambiguous = x11;
    ambiguous.app_id = Some("unexpected".into());
    assert!(selector(&ambiguous).is_err());
}

#[test]
fn discovery_rejects_extra_unrelated_preexisting_or_foreign_windows() {
    let before = parse_tree(desktop("splith", Vec::new(), TARGET_WORKSPACE_ID));
    let preexisting = container_ids(&before);
    for events in [
        vec![window_event("new", 20, None), window_event("new", 21, None)],
        vec![window_event("new", 20, None), window_event("new", 20, None)],
        vec![window_event("new", 6, None)],
        vec![
            window_event("new", 20, None),
            window_event("focus", 6, None),
        ],
        vec![
            window_event("new", 20, None),
            window_event("close", 20, None),
        ],
    ] {
        assert!(
            discover(
                &candidate_tree(),
                &events,
                &preexisting,
                TARGET_WORKSPACE_ID
            )
            .is_err()
        );
    }
    let mut extra = desktop(
        "splith",
        vec![
            wayland_window(20, "real.application", rect(0, 0, 960, 1080)),
            wayland_window(21, "unrelated", rect(960, 0, 960, 1080)),
        ],
        20,
    );

    assert!(
        discover(
            &parse_tree(extra.clone()),
            &[window_event("new", 20, None)],
            &preexisting,
            TARGET_WORKSPACE_ID
        )
        .is_err()
    );
    node_mut(&mut extra, 21)["floating"] = serde_json::json!("user_on");
    assert!(
        discover(
            &parse_tree(extra),
            &[window_event("new", 20, None)],
            &preexisting,
            TARGET_WORKSPACE_ID
        )
        .is_err()
    );
    assert!(
        discover(
            &candidate_tree(),
            &[window_event("new", 20, None)],
            &preexisting,
            999
        )
        .is_err()
    );
}

#[test]
fn discovery_waits_for_metadata_but_does_not_accept_an_unknown_selector() {
    let before = parse_tree(desktop("splith", Vec::new(), TARGET_WORKSPACE_ID));
    let preexisting = container_ids(&before);
    let mut tree = desktop(
        "splith",
        vec![wayland_window(
            20,
            "real.application",
            rect(0, 0, 1920, 1080),
        )],
        20,
    );
    node_mut(&mut tree, 20)["app_id"] = serde_json::Value::Null;

    assert!(
        discover(
            &parse_tree(tree),
            &[window_event("new", 20, None)],
            &preexisting,
            TARGET_WORKSPACE_ID
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn process_handles_detect_disappearance_and_cannot_authorize_unrelated_ancestry() {
    let own = ProcessHandle::open(std::process::id() as i32).unwrap();
    own.require_present().unwrap();
    let wrong_start = ProcessHandle {
        directory: own.directory.try_clone().unwrap(),
        pid: own.pid,
        start: own.start + 1,
    };

    assert!(wrong_start.require_present().is_err());
    assert!(require_ancestry(&own, &wrong_start).is_err());
    assert!(ProcessHandle::open(5_000_000).is_err());
    let fields = format!(
        "{} (name with ) parentheses) S {} {}\n",
        own.pid,
        1,
        vec!["0"; 18].join(" ")
    );
    assert_eq!(parse_stat(&fields, own.pid).unwrap(), (1, 0));
    assert!(parse_stat(&fields.replace(") S ", ") Z "), own.pid).is_err());
    assert!(parse_stat(&fields, own.pid + 1).is_err());
}

#[test]
fn ordinary_tree_ownership_still_rejects_unaccepted_package_windows() {
    let tree = candidate_tree();
    let scenario = captured_scenario(SCENARIOS[0], Selector::AppId("real.application".into()));
    let error = verify_ownership(
        &tree,
        TARGET_WORKSPACE_ID,
        &scenario.layout,
        &HashSet::new(),
        &AcceptedWindows::new(),
    )
    .unwrap_err();

    assert!(uncertain_message::<()>(Err(error)).contains("ownership"));
}

#[test]
fn package_registration_adds_two_cases_without_replacing_existing_assertions() {
    use crate::fixtures::{FOOT, RUN_ID, SLEEP};
    let standard = crate::scenarios::standard_scenarios(RUN_ID, FOOT, SLEEP);
    let extended = crate::scenarios::extended_scenarios(RUN_ID, FOOT, SLEEP);

    assert_eq!(standard.len(), 17);
    assert_eq!(extended.len() + SCENARIOS.len(), 8);
    assert!(
        extended
            .iter()
            .any(|scenario| scenario.name == "desktop-launcher-roundtrip")
    );
    assert!(
        extended
            .iter()
            .any(|scenario| scenario.name == "unresolved-launcher")
    );
}

#[test]
fn retained_process_and_matching_selector_cannot_replace_package_evidence() {
    let pid = std::process::id() as i32;
    let root = ProcessHandle::open(pid).unwrap();
    let process = ProcessHandle::open(pid).unwrap();
    let executable = fs::read_link(process.path().join("exe")).unwrap();
    let executable_identity = file_identity(&process.path().join("exe")).unwrap();
    let launch = Launch {
        root,
        package: Package::AppImage(PathBuf::from("/missing-test-image.AppImage")),
        image_identity: None,
    };
    let mut ownership = Ownership {
        process,
        executable,
        executable_identity,
        container_id: 20,
        selector: Selector::AppId("real.application".into()),
        evidence: None,
        expected_command: Vec::new(),
    };
    let mut tree = candidate_tree();
    let window = &mut tree.nodes[1].nodes[1].nodes[0];
    window.pid = Some(pid);

    assert!(ownership.verify(&launch, window).is_err());
    ownership.executable_identity.inode ^= 1;
    assert!(
        ownership
            .require_process_identity()
            .unwrap_err()
            .contains("executable changed")
    );
    ownership.executable_identity.inode ^= 1;
    ownership.require_process_identity().unwrap();
    window.id = 21;
    assert!(
        ownership
            .verify(&launch, window)
            .unwrap_err()
            .contains("container/process/selector changed")
    );
    window.id = 20;
    window.pid = Some(pid + 1);
    assert!(ownership.verify(&launch, window).is_err());
}

#[test]
fn retained_process_observation_refuses_replacement_and_disappearance() {
    let directory =
        std::env::temp_dir().join(format!("sway-stage-package-process-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let stat = format!("42 (fixture) S 1 {} 100\n", vec!["0"; 17].join(" "));
    fs::write(directory.join("stat"), &stat).unwrap();
    let handle = ProcessHandle {
        directory: File::open(&directory).unwrap(),
        pid: 42,
        start: 100,
    };
    handle.require_present().unwrap();
    fs::write(directory.join("stat"), stat.replace("100\n", "101\n")).unwrap();

    assert!(
        handle
            .require_present()
            .unwrap_err()
            .contains("identity changed")
    );
    fs::remove_file(directory.join("stat")).unwrap();
    assert!(handle.require_present().is_err());
    fs::remove_dir(directory).unwrap();
}
