//! A known production bug, reproduced: when Sway's own `exec` starts sway-stage, as any
//! keybinding does, including one for `sway-stage menu`, a profile's first window can appear
//! on the workspace that was focused at that `exec` instead of on the selected target.
//!
//! Sway 1.10.1 records the focused workspace in a launch context for every `exec`, and
//! exports an activation token naming that context to the started process. The first window
//! that presents the token before mapping, or whose process descends from the started
//! process, is placed by the context. When the recorded workspace was empty, Sway removed it
//! as focus left it; it then recreates a workspace with the same name for the window. The
//! context expires with its token 30 seconds after the `exec`.
//!
//! The cases leave Sway both paths, only the process ancestry, or only the token. Every
//! case states its expected placement explicitly, so a production fix changes only
//! `Case::expected_placement`.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crate::observation::WorkspaceIdentity;
use crate::process;
use crate::profile::{self, ExpectedWorkspaceEvent};
use crate::scenarios::{Scenario, standard_scenarios};

pub const CASES: [Case; 3] = [Case::ContextInherited, Case::AncestryOnly, Case::TokenOnly];

const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// What the context file must say: Sway exports both variables only when it created a
/// launch context for the `exec`. Presence alone is recorded, never the token itself.
const LAUNCH_CONTEXT_EXPORTED: &str = "activation_token=present\nstartup_id=present\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    /// What a keybinding gives sway-stage: the activation token in its environment, and
    /// Foot as a descendant of the process Sway started.
    ContextInherited,
    /// The token is removed before sway-stage starts; only process ancestry remains.
    AncestryOnly,
    /// sway-stage is orphaned from the process Sway started; only the token remains.
    TokenOnly,
}

impl Case {
    pub fn name(self) -> &'static str {
        match self {
            Self::ContextInherited => "profile-exec-context",
            Self::AncestryOnly => "profile-exec-ancestry-only",
            Self::TokenOnly => "profile-exec-token-only",
        }
    }

    /// Where the profile's only window is expected while the bug remains. A fix that keeps
    /// every launch on its selected target changes each of these to `Placement::Target`.
    pub fn expected_placement(self) -> Placement {
        match self {
            Self::ContextInherited | Self::AncestryOnly | Self::TokenOnly => {
                Placement::RecreatedOrigin
            }
        }
    }

    /// Which launch-context paths the case leaves open, for the scenario report.
    pub fn description(self) -> &'static str {
        match self {
            Self::ContextInherited => {
                "sway-stage inherited the token, and Foot descended from the exec'd process"
            }
            Self::AncestryOnly => {
                "the token was removed before sway-stage started; Foot descended from the exec'd process"
            }
            Self::TokenOnly => {
                "sway-stage inherited the token but was orphaned from the exec'd process"
            }
        }
    }

    /// Selects the entry script's branch.
    fn entry_mode(self) -> &'static str {
        match self {
            Self::ContextInherited => "inherited",
            Self::AncestryOnly => "without-token",
            Self::TokenOnly => "detached",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// The known production bug: the window is on a workspace that Sway recreated with the
    /// origin's name after removing the empty origin, and the selected target stays empty.
    /// sway-stage refuses the misplaced window and stops.
    RecreatedOrigin,
    /// The intended behavior: the window is on the selected target, and the load succeeds.
    #[allow(
        dead_code,
        reason = "no case expects the fixed behavior while the bug remains"
    )]
    Target,
}

/// `single-window` with recorded launches: one window is the smallest layout that shows
/// where Sway places a launch.
pub fn layout(
    case: Case,
    run_id: &str,
    foot: &str,
    sleep: &str,
    launch_script: &str,
    launch_record: &str,
) -> Scenario {
    let unique_id = format!("{run_id}-{}", case.name());
    let mut layout = standard_scenarios(&unique_id, foot, sleep)
        .into_iter()
        .find(|scenario| scenario.name == "single-window")
        .expect("standard single-window scenario");
    layout.name = case.name();
    profile::wrap_launches(
        &mut layout.layout,
        layout.name,
        launch_script,
        launch_record,
    );
    layout
}

/// One entry: the absent target, loaded with the case's layout.
pub fn profile_configuration(case: Case, target_name: &str) -> String {
    format!(
        "version: 1\nworkspaces:\n  - workspace: {target_name}\n    layout: {}\n",
        case.name()
    )
}

/// Runs sway-stage as `mode` selects and leaves evidence for the harness: whether Sway's
/// `exec` exported launch-context variables (presence only), sway-stage's output, and its
/// exit status, renamed into place only once complete.
const ENTRY_SCRIPT_BODY: &str = r#"if [ -n "${XDG_ACTIVATION_TOKEN-}" ]; then token=present; else token=absent; fi
if [ -n "${DESKTOP_STARTUP_ID-}" ]; then startup_id=present; else startup_id=absent; fi
printf 'activation_token=%s\nstartup_id=%s\n' "$token" "$startup_id" >"$evidence/context" || exit 1

run() {
    "$@" </dev/null >"$evidence/stdout" 2>"$evidence/stderr"
    printf '%s\n' "$?" >"$evidence/status.partial" && mv "$evidence/status.partial" "$evidence/status"
}

case $mode in
inherited)
    run "$@"
    ;;
without-token)
    unset XDG_ACTIVATION_TOKEN DESKTOP_STARTUP_ID
    run "$@"
    ;;
detached)
    # The subshell exits at once and the kernel reparents its background child, so Sway's
    # parent-PID walk from Foot never reaches the process that Sway's exec started.
    ( run "$@" & )
    ;;
*)
    exit 2
    ;;
esac
"#;

/// The script Sway's `exec` runs. Every value is a single-quoted literal, so no shell
/// expands or splits the harness's paths or sway-stage's arguments.
pub fn entry_script(case: Case, evidence_directory: &str, sway_stage_command: &[String]) -> String {
    let quoted_command: Vec<String> = sway_stage_command
        .iter()
        .map(|argument| shell_quote(argument))
        .collect();
    format!(
        "#!/bin/sh\nmode={}\nevidence={}\nset -- {}\n\n{ENTRY_SCRIPT_BODY}",
        shell_quote(case.entry_mode()),
        shell_quote(evidence_directory),
        quoted_command.join(" ")
    )
}

/// One literal sh word. Single quotes preserve every byte except `'` itself, which closes
/// the quotes, adds an escaped quote, and reopens them.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// What the entry script recorded once sway-stage exited.
pub struct ExecOutcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// Which launch-context variables Sway's `exec` exported to the script.
    context: String,
}

impl ExecOutcome {
    /// Waits for the status file, which appears only after sway-stage has exited. The
    /// process is not ours and is never signalled; after a timeout it stops with the
    /// compositor.
    pub fn wait(evidence_directory: &Path, timeout: Duration) -> Result<Self, String> {
        let status_path = evidence_directory.join("status");
        let deadline = Instant::now() + timeout;
        let status_text = loop {
            process::check_interruption()?;
            match fs::read_to_string(&status_path) {
                Ok(text) => break text,
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(format!("read exec'd sway-stage exit status: {error}")),
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "sway-stage started by Sway's exec did not exit within {timeout:?}; see {}",
                    evidence_directory.display()
                ));
            }
            thread::sleep(STATUS_POLL_INTERVAL);
        };

        let exit_code = status_text
            .strip_suffix('\n')
            .and_then(|code| code.parse::<i32>().ok())
            .ok_or_else(|| format!("invalid exec'd sway-stage exit status {status_text:?}"))?;
        let read_evidence = |name: &str| {
            fs::read_to_string(evidence_directory.join(name))
                .map_err(|error| format!("read exec'd sway-stage {name}: {error}"))
        };
        Ok(Self {
            exit_code,
            stdout: read_evidence("stdout")?,
            stderr: read_evidence("stderr")?,
            context: read_evidence("context")?,
        })
    }

    /// Without a launch context, nothing here could reproduce the bug.
    pub fn require_launch_context(&self) -> Result<(), String> {
        if self.context != LAUNCH_CONTEXT_EXPORTED {
            return Err(format!(
                "Sway's exec exported no launch context: {:?}",
                self.context
            ));
        }
        Ok(())
    }

    /// The intended outcome: a successful load with nothing on stdout.
    pub fn require_success(&self) -> Result<(), String> {
        if self.exit_code != 0 || !self.stdout.trim().is_empty() {
            return Err(format!(
                "profile load through Sway's exec failed: exit {}: {}",
                self.exit_code,
                self.stderr.trim()
            ));
        }
        Ok(())
    }

    /// sway-stage's refusal of the misplaced window: exit 1, naming the window, where it was
    /// found, and the selected target, with no completed workspace and no cleanup.
    pub fn require_misplacement_refusal(
        &self,
        window_id: i64,
        recreated_workspace_id: i64,
        target_workspace_id: i64,
    ) -> Result<(), String> {
        let placement_failure = format!(
            "placement failure: wrong workspace: matching window id={window_id} \
             (workspace=Some({recreated_workspace_id}) "
        );
        let expected_target = format!("expected workspace={target_workspace_id};");
        let refused = self.exit_code == 1
            && self.stdout.trim().is_empty()
            && self.stderr.contains(&placement_failure)
            && self.stderr.contains(&expected_target)
            && self.stderr.contains("completed 0 workspaces []")
            && self.stderr.contains("stopped without cleanup or relaunch");
        if !refused {
            return Err(format!(
                "expected sway-stage to refuse window id={window_id} on workspace \
                 id={recreated_workspace_id} (exit 1), got exit {}: {}",
                self.exit_code,
                self.stderr.trim()
            ));
        }
        Ok(())
    }
}

/// The load's exact workspace events. Creating and selecting the target removes the empty
/// origin; with the known bug, Sway then recreates a workspace with the origin's name for
/// the window.
pub fn expected_workspace_events<'a>(
    origin: &'a WorkspaceIdentity,
    target: &'a WorkspaceIdentity,
    recreated_origin: Option<&'a WorkspaceIdentity>,
) -> Vec<ExpectedWorkspaceEvent<'a>> {
    let mut expected = vec![
        ExpectedWorkspaceEvent::Init(target),
        ExpectedWorkspaceEvent::Focus {
            from_id: origin.id,
            to: target,
        },
        ExpectedWorkspaceEvent::Empty(origin),
    ];
    if let Some(recreated_origin) = recreated_origin {
        expected.push(ExpectedWorkspaceEvent::Init(recreated_origin));
    }
    expected
}

#[cfg(test)]
#[path = "launch_context_tests.rs"]
mod tests;
