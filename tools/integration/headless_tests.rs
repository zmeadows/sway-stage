use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;

use serde_json::json;

use super::*;
use crate::fixtures::{
    PREEXISTING_WINDOW_ID, TARGET_WORKSPACE_ID, UNUSED_PID_BASE, desktop, node_mut, parse_tree,
    rect, wayland_window,
};

const ROOT: &str = "/tmp/sway-stage-headless-1-2";
const SEARCH_PATH: &str = "/usr/local/bin:/usr/bin";

/// Paths only; nothing is created on disk.
fn session_directory() -> SessionDirectory {
    SessionDirectory {
        root: PathBuf::from(ROOT),
    }
}

fn variable_names(environment: &Environment) -> Vec<&str> {
    environment.iter().map(|(name, _)| *name).collect()
}

fn variable<'a>(environment: &'a Environment, name: &str) -> &'a OsStr {
    environment
        .iter()
        .find(|(variable_name, _)| *variable_name == name)
        .map(|(_, value)| value.as_os_str())
        .expect("a variable in the environment")
}

const SHARED_VARIABLES: [&str; 11] = [
    "PATH",
    "LANG",
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "TMPDIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "SWAYSOCK",
];

#[test]
fn environments_contain_only_path_and_private_session_locations() {
    let directory = session_directory();
    let search_path = OsStr::new(SEARCH_PATH);
    let compositor = compositor_environment(&directory, search_path);
    let client = client_environment(&directory, search_path, OsStr::new("wayland-1"));

    let mut expected_compositor_names = SHARED_VARIABLES.to_vec();
    expected_compositor_names.extend([
        "WLR_BACKENDS",
        "WLR_HEADLESS_OUTPUTS",
        "WLR_LIBINPUT_NO_DEVICES",
    ]);
    assert_eq!(variable_names(&compositor), expected_compositor_names);
    let mut expected_client_names = SHARED_VARIABLES.to_vec();
    expected_client_names.push("WAYLAND_DISPLAY");
    assert_eq!(variable_names(&client), expected_client_names);

    assert_eq!(variable(&compositor, "WLR_BACKENDS"), "headless");
    assert_eq!(variable(&compositor, "WLR_HEADLESS_OUTPUTS"), "1");
    assert_eq!(variable(&client, "WAYLAND_DISPLAY"), "wayland-1");

    for environment in [&compositor, &client] {
        // Renderer selection stays automatic; real desktop connections are never named.
        for absent in [
            "WAYLAND_SOCKET",
            "DISPLAY",
            "I3SOCK",
            "XAUTHORITY",
            "WLR_RENDERER",
        ] {
            assert!(!variable_names(environment).contains(&absent), "{absent}");
        }

        assert_eq!(variable(environment, "PATH"), SEARCH_PATH);
        assert_eq!(variable(environment, "LANG"), "C.UTF-8");
        assert_eq!(
            variable(environment, "SWAYSOCK"),
            "/tmp/sway-stage-headless-1-2/runtime/sway-ipc.sock"
        );
        assert_eq!(
            variable(environment, "DBUS_SESSION_BUS_ADDRESS"),
            "unix:path=/tmp/sway-stage-headless-1-2/runtime/no-session-bus"
        );
        for private_location in [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_RUNTIME_DIR",
            "TMPDIR",
        ] {
            let path = Path::new(variable(environment, private_location));
            assert!(path.starts_with(ROOT), "{private_location}: {path:?}");
        }
    }
}

#[test]
fn configuration_disables_extras_and_sets_only_the_virtual_output() {
    let configuration = sway_configuration();

    assert_eq!(
        configuration.lines().collect::<Vec<_>>(),
        [
            "swaynag_command -",
            "xwayland disable",
            "default_border none",
            "default_floating_border none",
            "focus_follows_mouse no",
            "output HEADLESS-1 mode --custom 2560x1440 position 0 0",
        ]
    );
}

fn output(name: &str, rect: Rect) -> Output {
    Output {
        name: name.into(),
        active: true,
        rect,
        current_workspace: Some("1".into()),
    }
}

#[test]
fn only_the_configured_headless_output_is_ready() {
    verify_virtual_output(&[output(OUTPUT_NAME, VIRTUAL_OUTPUT_RECT)]).unwrap();

    assert!(verify_virtual_output(&[]).unwrap_err().contains("found []"));
    let two_outputs = [
        output(OUTPUT_NAME, VIRTUAL_OUTPUT_RECT),
        output("HEADLESS-2", VIRTUAL_OUTPUT_RECT),
    ];
    assert!(
        verify_virtual_output(&two_outputs)
            .unwrap_err()
            .contains("HEADLESS-2")
    );

    // A physical output name means a different compositor answered.
    let real_output = [output("DP-1", VIRTUAL_OUTPUT_RECT)];
    assert!(
        verify_virtual_output(&real_output)
            .unwrap_err()
            .contains("DP-1")
    );

    let mut inactive = output(OUTPUT_NAME, VIRTUAL_OUTPUT_RECT);
    inactive.active = false;
    assert!(
        verify_virtual_output(&[inactive])
            .unwrap_err()
            .contains("not active")
    );

    // The headless backend's default mode, before the configuration applies.
    let default_mode = Rect {
        x: 0,
        y: 0,
        width: 1280,
        height: 720,
    };
    assert!(
        verify_virtual_output(&[output(OUTPUT_NAME, default_mode)])
            .unwrap_err()
            .contains("width: 1280")
    );

    let mut without_workspace = output(OUTPUT_NAME, VIRTUAL_OUTPUT_RECT);
    without_workspace.current_workspace = None;
    assert!(
        verify_virtual_output(&[without_workspace])
            .unwrap_err()
            .contains("no workspace")
    );
}

#[test]
fn only_numbered_wayland_display_sockets_are_recognized() {
    for accepted in ["wayland-0", "wayland-1", "wayland-12"] {
        assert!(is_wayland_socket_name(OsStr::new(accepted)), "{accepted}");
    }
    for rejected in [
        "wayland-1.lock",
        "wayland-",
        "wayland-x",
        "wayland",
        "sway-ipc.sock",
        "no-session-bus",
    ] {
        assert!(!is_wayland_socket_name(OsStr::new(rejected)), "{rejected}");
    }
}

#[test]
fn session_socket_paths_must_fit_a_unix_socket_address() {
    // `/runtime/sway-ipc.sock` adds 22 bytes to the root.
    let longest_root = format!("/{}", "a".repeat(84));
    let fitting = SessionDirectory {
        root: PathBuf::from(&longest_root),
    };
    assert_eq!(
        fitting.ipc_socket().as_os_str().len(),
        MAX_SOCKET_PATH_BYTES
    );
    fitting.require_socket_paths_fit().unwrap();

    let too_long = SessionDirectory {
        root: PathBuf::from(format!("{longest_root}a")),
    };
    assert!(
        too_long
            .require_socket_paths_fit()
            .unwrap_err()
            .contains("TMPDIR")
    );
}

#[test]
fn session_clients_include_every_tiled_and_floating_view() {
    let mut tree = desktop(
        "splith",
        vec![
            wayland_window(20, "left", rect(0, 0, 960, 1080)),
            wayland_window(21, "right", rect(960, 0, 960, 1080)),
        ],
        20,
    );
    node_mut(&mut tree, TARGET_WORKSPACE_ID)["floating_nodes"] =
        json!([wayland_window(22, "floating", rect(10, 10, 100, 100))]);

    let expected: BTreeSet<i32> = [PREEXISTING_WINDOW_ID, 20, 21, 22]
        .into_iter()
        .map(|container_id| i32::try_from(UNUSED_PID_BASE + container_id).unwrap())
        .collect();
    assert_eq!(session_client_pids(&parse_tree(tree)), expected);
}

#[test]
fn the_runner_accepts_only_a_session_directory_ipc_socket() {
    assert!(is_session_ipc_socket(&session_directory().ipc_socket()));

    for refused in [
        // A desktop session's own socket.
        "/run/user/1000/sway-ipc.1000.4242.sock",
        // The right file and runtime names, but not inside a session directory.
        "/tmp/elsewhere/runtime/sway-ipc.sock",
        "/runtime/sway-ipc.sock",
        "/tmp/sway-stage-headless-1-2/sway-ipc.sock",
        "/tmp/sway-stage-headless-1-2/runtime/other.sock",
        "/tmp/sway-stage-headless-1-2/runtime/../runtime/sway-ipc.sock",
        "sway-stage-headless-1-2/runtime/sway-ipc.sock",
        "",
    ] {
        assert!(!is_session_ipc_socket(Path::new(refused)), "{refused:?}");
    }
}

#[test]
fn the_final_tree_is_saved_best_effort_and_names_the_session_clients() {
    let directory =
        env::temp_dir().join(format!("sway-stage-final-tree-test-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let final_tree_path = directory.join("final-tree.json");
    let tree_json = desktop(
        "splith",
        vec![wayland_window(20, "owned", rect(0, 0, 960, 1080))],
        20,
    )
    .to_string();
    let expected: BTreeSet<i32> = [PREEXISTING_WINDOW_ID, 20]
        .into_iter()
        .map(|container_id| i32::try_from(UNUSED_PID_BASE + container_id).unwrap())
        .collect();

    assert_eq!(
        record_final_tree(&tree_json, &final_tree_path).unwrap(),
        expected
    );
    assert_eq!(fs::read_to_string(&final_tree_path).unwrap(), tree_json);
    let final_tree_mode = fs::metadata(&final_tree_path).unwrap().permissions().mode();
    assert_eq!(final_tree_mode & 0o777, 0o600);

    // The file now exists, so this save fails; the clients are still recorded.
    assert_eq!(
        record_final_tree(&tree_json, &final_tree_path).unwrap(),
        expected
    );

    // An unparsable reply is still kept for diagnosis, but cannot name the clients.
    let invalid_tree_path = directory.join("invalid-tree.json");
    let error = record_final_tree("not a tree", &invalid_tree_path).unwrap_err();
    assert!(error.contains("get_tree"), "{error}");
    assert_eq!(
        fs::read_to_string(&invalid_tree_path).unwrap(),
        "not a tree"
    );

    fs::remove_dir_all(directory).unwrap();
}
