use super::*;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Instant;

/// Each wait for a short-lived fixture process gives up after this.
const PROCESS_DEADLINE: Duration = Duration::from_secs(5);
const LONG_SUPERVISION: Duration = Duration::from_secs(30);

/// A private directory for one test's fixture application, which runs only `/bin/sh`
/// builtins and `sleep`, creates no windows, and always exits by itself.
struct ApplicationFixture {
    path: PathBuf,
}

impl ApplicationFixture {
    fn new(script_body: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = env::temp_dir().join(format!(
            "sway-stage-application-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        // The PID file is renamed into place, so a reader never sees it half written.
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > pid.partial && mv pid.partial pid\n{script_body}\n"
        );
        fs::write(path.join("app"), script).unwrap();
        fs::set_permissions(path.join("app"), fs::Permissions::from_mode(0o700)).unwrap();
        Self { path }
    }

    fn start(&self, supervision_limit: Duration) -> (LaunchedApplication, Diagnostics) {
        let mut diagnostics = Diagnostics::for_test(self.path.join("last-run.log"));
        let command =
            application_command(&["./app".into()], Some(&self.path), &BTreeMap::new()).unwrap();
        let application =
            LaunchedApplication::start(command, supervision_limit, &mut diagnostics).unwrap();
        (application, diagnostics)
    }

    /// The PID that the application itself wrote.
    fn recorded_pid(&self) -> u32 {
        let pid_path = self.path.join("pid");
        let text = wait_for(|| fs::read_to_string(&pid_path).ok());
        text.trim().parse().unwrap()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.path.join("last-run.log")).unwrap()
    }
}

impl Drop for ApplicationFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn wait_for<T>(mut observe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + PROCESS_DEADLINE;
    loop {
        if let Some(value) = observe() {
            return value;
        }
        assert!(Instant::now() < deadline, "fixture process wait timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

/// The state and parent from `/proc/PID/stat`, or `None` once the process is gone.
fn process_state_and_parent(pid: u32) -> Option<(char, u32)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is parenthesized and may itself contain spaces or parentheses.
    let after_name = &stat[stat.rfind(')')? + 2..];
    let mut fields = after_name.split(' ');
    let state = fields.next()?.chars().next()?;
    let parent_pid = fields.next()?.parse().ok()?;
    Some((state, parent_pid))
}

fn has_exited(pid: u32) -> bool {
    !matches!(process_state_and_parent(pid), Some((state, _)) if state != 'Z')
}

fn ancestors(pid: u32) -> Vec<u32> {
    let mut ancestor_pids = Vec::new();
    let mut current_pid = pid;
    while let Some((_, parent_pid)) = process_state_and_parent(current_pid) {
        if parent_pid == 0 {
            break;
        }
        ancestor_pids.push(parent_pid);
        current_pid = parent_pid;
    }
    ancestor_pids
}

fn record_exit_when_reported(
    application: &mut LaunchedApplication,
    diagnostics: &mut Diagnostics,
) -> Option<ExitStatus> {
    wait_for(|| {
        application.record_exit_if_reported(diagnostics).unwrap();
        match application.supervision {
            Supervision::Listening => None,
            Supervision::Exited(status) => Some(Some(status)),
            Supervision::EndedWithoutStatus => Some(None),
        }
    })
}

#[test]
fn command_keeps_arguments_cwd_and_overrides_but_removes_activation_context() {
    let cwd = env::temp_dir();
    let argv: Vec<String> = ["./bin/app", "", "two words", "$HOME", "; true"]
        .map(String::from)
        .into();
    let environment = BTreeMap::from([("MARKER".to_owned(), "child-value".to_owned())]);
    let command = application_command(&argv, Some(&cwd), &environment).unwrap();

    assert_eq!(command.get_program(), cwd.join("./bin/app"));
    let arguments: Vec<&OsStr> = command.get_args().collect();
    assert_eq!(arguments, ["", "two words", "$HOME", "; true"]);
    assert_eq!(command.get_current_dir(), Some(cwd.as_path()));
    let environment_changes: Vec<(&OsStr, Option<&OsStr>)> = command.get_envs().collect();
    assert_eq!(
        environment_changes,
        [
            (OsStr::new("DESKTOP_STARTUP_ID"), None),
            (OsStr::new("MARKER"), Some(OsStr::new("child-value"))),
            (OsStr::new("XDG_ACTIVATION_TOKEN"), None),
        ]
    );

    // A bare name keeps the child's PATH lookup.
    let bare = application_command(&["app".into()], None, &BTreeMap::new()).unwrap();
    assert_eq!(bare.get_program(), "app");
    assert_eq!(
        bare.get_current_dir(),
        Some(env::current_dir().unwrap().as_path())
    );
}

#[test]
fn reports_the_application_itself_outside_sway_stage_ancestry() {
    // The application outlives the checks below, then exits by itself.
    let fixture = ApplicationFixture::new("sleep 1\nexit 3");
    let (mut application, mut diagnostics) = fixture.start(LONG_SUPERVISION);

    let application_pid = fixture.recorded_pid();
    assert!(
        fixture
            .log()
            .contains(&format!("spawned pid={application_pid}")),
        "{}",
        fixture.log()
    );

    // Sway's launch-context lookup walks exactly these parent links from the window's PID.
    let application_ancestors = ancestors(application_pid);
    assert!(
        !application_ancestors.contains(&std::process::id()),
        "{application_ancestors:?}"
    );

    // The supervisor holds only standard I/O, the status pipe, and the pidfd.
    let supervisor_pid = application_ancestors[0];
    let supervisor_descriptors = fs::read_dir(format!("/proc/{supervisor_pid}/fd"))
        .unwrap()
        .count();
    assert_eq!(supervisor_descriptors, 5);

    let status = record_exit_when_reported(&mut application, &mut diagnostics);
    assert_eq!(status.and_then(|status| status.code()), Some(3));
    assert_eq!(application.exit_status(), status);
    // The supervisor reaped the application, then exited.
    wait_for(|| has_exited(supervisor_pid).then_some(()));
    assert!(has_exited(application_pid));
    assert!(
        fixture
            .log()
            .contains("launch process exited: exit status: 3; continuing window observation")
    );
}

#[test]
fn exit_statuses_come_from_the_application() {
    for (script_end, expected_code, expected_signal) in [
        ("exit 0", Some(0), None),
        ("exit 1", Some(1), None),
        ("kill -TERM $$", None, Some(libc::SIGTERM)),
    ] {
        let fixture = ApplicationFixture::new(script_end);
        let (mut application, mut diagnostics) = fixture.start(LONG_SUPERVISION);
        let status = record_exit_when_reported(&mut application, &mut diagnostics)
            .expect("the supervisor reports a status");
        assert_eq!(status.code(), expected_code, "{script_end}");
        assert_eq!(status.signal(), expected_signal, "{script_end}");
    }
}

#[test]
fn start_failures_are_spawn_errors_that_started_nothing() {
    let fixture = ApplicationFixture::new("exit 0");
    let mut diagnostics = Diagnostics::for_test(fixture.path.join("last-run.log"));
    // The application's exec fails after both forks. A missing cwd fails earlier, in the
    // intermediate, before any supervisor exists.
    let missing_cwd = fixture.path.join("missing-directory");
    for (argv, cwd) in [
        (vec!["./missing-application".into()], &fixture.path),
        (vec!["missing-sway-stage-fixture".into()], &fixture.path),
        (vec!["./app".into()], &missing_cwd),
    ] {
        let command = application_command(&argv, Some(cwd), &BTreeMap::new()).unwrap();
        let failure = LaunchedApplication::start(command, LONG_SUPERVISION, &mut diagnostics)
            .err()
            .expect("exec fails");
        assert_eq!(failure.cause, Some(Cause::ApplicationStart), "{failure}");
        assert!(
            failure
                .detail
                .starts_with("spawn application: No such file or directory"),
            "{failure}"
        );
    }
    assert!(!fixture.log().contains("spawned"), "{}", fixture.log());
}

#[test]
fn supervision_ends_when_sway_stage_stops_listening_or_at_its_limit() {
    // Dropping the handle ends supervision at once; the application keeps running.
    let fixture = ApplicationFixture::new("sleep 1");
    let (application, _diagnostics) = fixture.start(LONG_SUPERVISION);
    let application_pid = fixture.recorded_pid();
    let supervisor_pid = ancestors(application_pid)[0];
    drop(application);
    wait_for(|| has_exited(supervisor_pid).then_some(()));
    assert!(!has_exited(application_pid));
    assert_ne!(ancestors(application_pid)[0], supervisor_pid);

    // A retained handle still ends at the limit, without an exit status.
    let limited_fixture = ApplicationFixture::new("sleep 1");
    let supervision_limit = Duration::from_millis(300);
    let started_at = Instant::now();
    let (mut application, mut diagnostics) = limited_fixture.start(supervision_limit);
    let application_pid = limited_fixture.recorded_pid();
    let supervisor_pid = ancestors(application_pid)[0];
    assert_eq!(
        record_exit_when_reported(&mut application, &mut diagnostics),
        None
    );
    assert!(started_at.elapsed() >= supervision_limit);
    wait_for(|| has_exited(supervisor_pid).then_some(()));
    assert!(!has_exited(application_pid));
    assert!(
        limited_fixture
            .log()
            .contains("launch supervision ended without an exit status"),
        "{}",
        limited_fixture.log()
    );
}

#[test]
fn a_zero_supervision_limit_ends_supervision_at_once() {
    // A window deadline that has already passed leaves no time to supervise. The
    // application still starts, and the supervisor never waits for it.
    let fixture = ApplicationFixture::new("sleep 1");
    let (mut application, mut diagnostics) = fixture.start(Duration::ZERO);
    let application_pid = fixture.recorded_pid();

    assert_eq!(
        record_exit_when_reported(&mut application, &mut diagnostics),
        None
    );
    assert!(!has_exited(application_pid));
    assert!(
        fixture
            .log()
            .contains("launch supervision ended without an exit status"),
        "{}",
        fixture.log()
    );
}
