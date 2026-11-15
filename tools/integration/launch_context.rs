//! Scenarios that start sway-stage through Sway's own `exec`, as any keybinding does:
//! standalone `load`, `profile load`, and `menu`, which selects a layout or a profile
//! through a Fuzzel substitute.
//!
//! Sway 1.10.1 records the focused workspace in a launch context for every `exec`, and
//! exports an activation token naming that context to the started process. The first window
//! that presents the token before mapping, or whose process descends from the started
//! process, is placed by the context. When the recorded workspace was empty, Sway removed it
//! as focus left it; it would then recreate a workspace with the same name for the window.
//! The context expires with its token 30 seconds after the `exec`. sway-stage therefore
//! starts each application without the inherited variables and outside its own process
//! ancestry.
//!
//! The profile cases select another workspace, so the context names the wrong one. They
//! leave Sway both paths, only the process ancestry, or only the token: either one alone
//! misplaced the window before sway-stage was fixed. A layout loads on the focused
//! workspace that the context names anyway, so the layout cases establish the entry points
//! rather than the misplacement regression.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crate::headless::OUTPUT_NAME;
use crate::observation::WorkspaceIdentity;
use crate::process;
use crate::profile::{self, ExpectedWorkspaceEvent};
use crate::scenarios::{Scenario, standard_scenarios};

pub const CASES: [Case; 6] = [
    Case::LayoutLoad,
    Case::MenuLayout,
    Case::ContextInherited,
    Case::AncestryOnly,
    Case::TokenOnly,
    Case::MenuProfile,
];

const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// What the context file must say: Sway exports both variables only when it created a
/// launch context for the `exec`. Presence alone is recorded, never the token itself.
const LAUNCH_CONTEXT_EXPORTED: &str = "activation_token=present\nstartup_id=present\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    /// `sway-stage load` from a keybinding, on the focused workspace.
    LayoutLoad,
    /// `sway-stage menu` from a keybinding, selecting the layout.
    MenuLayout,
    /// What a keybinding gives `sway-stage profile load`: the activation token in its
    /// environment, and descent from the process Sway started.
    ContextInherited,
    /// The token is removed before sway-stage starts; only process ancestry remains.
    AncestryOnly,
    /// sway-stage is orphaned from the process Sway started; only the token remains.
    TokenOnly,
    /// `sway-stage menu` from a keybinding, selecting the profile.
    MenuProfile,
}

impl Case {
    pub fn name(self) -> &'static str {
        match self {
            Self::LayoutLoad => "layout-exec-direct",
            Self::MenuLayout => "layout-exec-menu",
            Self::ContextInherited => "profile-exec-context",
            Self::AncestryOnly => "profile-exec-ancestry-only",
            Self::TokenOnly => "profile-exec-token-only",
            Self::MenuProfile => "profile-exec-menu",
        }
    }

    /// The entry point and the launch-context paths the case leaves open, for the report.
    pub fn description(self) -> &'static str {
        match self {
            Self::LayoutLoad => {
                "standalone load inherited the token and descended from the exec'd process"
            }
            Self::MenuLayout => {
                "menu selected the layout; it inherited the token and descended from the exec'd process"
            }
            Self::ContextInherited => {
                "sway-stage inherited the token and descended from the exec'd process"
            }
            Self::AncestryOnly => {
                "the token was removed before sway-stage, which descended from the exec'd process"
            }
            Self::TokenOnly => {
                "sway-stage inherited the token but was orphaned from the exec'd process"
            }
            Self::MenuProfile => {
                "menu selected the profile; it inherited the token and descended from the exec'd process"
            }
        }
    }

    /// Whether sway-stage loads the case's profile, which selects another workspace, rather
    /// than its layout on the focused workspace.
    pub fn loads_profile(self) -> bool {
        !matches!(self, Self::LayoutLoad | Self::MenuLayout)
    }

    /// The menu row that the Fuzzel substitute selects, for the cases that run `menu`.
    pub fn menu_selection(self) -> Option<String> {
        match self {
            Self::MenuLayout => Some(format!("Layout: {}", self.name())),
            Self::MenuProfile => Some(format!("Profile: {}", self.name())),
            Self::LayoutLoad | Self::ContextInherited | Self::AncestryOnly | Self::TokenOnly => {
                None
            }
        }
    }

    /// Selects the entry script's branch.
    fn entry_mode(self) -> &'static str {
        match self {
            Self::AncestryOnly => "without-token",
            Self::TokenOnly => "detached",
            Self::LayoutLoad | Self::MenuLayout | Self::ContextInherited | Self::MenuProfile => {
                "inherited"
            }
        }
    }

    /// One window is the smallest layout that shows where Sway places a launch. A layout
    /// loaded on the focused workspace has two, so that its hierarchy and focus are checked.
    fn base_layout(self) -> &'static str {
        if self.loads_profile() {
            "single-window"
        } else {
            "horizontal-pair"
        }
    }
}

/// The case's standard layout with recorded launches, named after the case.
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
        .find(|scenario| scenario.name == case.base_layout())
        .expect("standard base layout");
    layout.name = case.name();
    profile::wrap_launches(
        &mut layout.layout,
        layout.name,
        launch_script,
        launch_record,
    );
    layout
}

/// One entry: the absent target, loaded with the case's layout. Every case has this
/// profile, so the menu offers a layout and a profile with the same name.
pub fn profile_configuration(case: Case, target_name: &str) -> String {
    format!(
        "version: 1\nworkspaces:\n  - workspace: {target_name}\n    layout: {}\n",
        case.name()
    )
}

/// sway-stage's command line for the case, with both private directories.
pub fn sway_stage_arguments(
    case: Case,
    sway_stage: &str,
    layouts_directory: &str,
    profiles_directory: &str,
) -> Vec<String> {
    let mut arguments = vec![
        sway_stage.to_owned(),
        "--layouts-dir".to_owned(),
        layouts_directory.to_owned(),
        "--profiles-dir".to_owned(),
        profiles_directory.to_owned(),
    ];
    let entry_point: &[&str] = if case.menu_selection().is_some() {
        &["menu"]
    } else if case.loads_profile() {
        &["profile", "load", case.name()]
    } else {
        &["load", case.name()]
    };
    arguments.extend(entry_point.iter().map(|argument| (*argument).to_owned()));
    arguments
}

/// Runs sway-stage as `mode` selects and leaves evidence for the harness: whether Sway's
/// `exec` exported launch-context variables (presence only), sway-stage's output, and its
/// exit status, renamed into place only once complete.
const ENTRY_SCRIPT_BODY: &str = r#"if [ -n "${XDG_ACTIVATION_TOKEN-}" ]; then token=present; else token=absent; fi
if [ -n "${DESKTOP_STARTUP_ID-}" ]; then startup_id=present; else startup_id=absent; fi
printf 'activation_token=%s\nstartup_id=%s\n' "$token" "$startup_id" >"$evidence/context" || exit 1

# sway-stage's menu runs `fuzzel` from PATH; the substitute comes before any real one.
if [ -n "$fuzzel_directory" ]; then
    PATH=$fuzzel_directory:$PATH
    export PATH
fi

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
/// expands or splits the harness's paths or sway-stage's arguments. Only the menu cases
/// have a `fuzzel_directory`.
pub fn entry_script(
    case: Case,
    evidence_directory: &str,
    fuzzel_directory: Option<&str>,
    sway_stage_command: &[String],
) -> String {
    let quoted_command: Vec<String> = sway_stage_command
        .iter()
        .map(|argument| shell_quote(argument))
        .collect();
    format!(
        "#!/bin/sh\nmode={}\nevidence={}\nfuzzel_directory={}\nset -- {}\n\n{ENTRY_SCRIPT_BODY}",
        shell_quote(case.entry_mode()),
        shell_quote(evidence_directory),
        shell_quote(fuzzel_directory.unwrap_or_default()),
        quoted_command.join(" ")
    )
}

/// Fuzzel's `--dmenu --index` protocol, as sway-stage's menu uses it: read one row per
/// line, then print the selected row's 0-based index. The substitute selects the one row
/// equal to `selection`, using only shell builtins and no display. It records its
/// arguments and the rows it read; noclobber makes a second invocation fail. Any other
/// outcome exits 1, which sway-stage reports as a Fuzzel failure.
const FUZZEL_SUBSTITUTE_BODY: &str = r#"set -C
printf '%s\n' "$@" >"$evidence/menu-arguments" || exit 1
: >"$evidence/menu-rows" || exit 1

row_index=0
selected_index=
while IFS= read -r row; do
    printf '%s\n' "$row" >>"$evidence/menu-rows" || exit 1
    if [ "$row" = "$selection" ]; then
        [ -z "$selected_index" ] || exit 1
        selected_index=$row_index
    fi
    row_index=$((row_index + 1))
done

[ -n "$selected_index" ] || exit 1
printf '%s\n' "$selected_index" >"$evidence/menu-selection" || exit 1
printf '%s\n' "$selected_index"
"#;

/// The executable that sway-stage's menu finds as `fuzzel`, with the case's row and the
/// evidence directory as single-quoted literals.
pub fn fuzzel_substitute_script(selection: &str, evidence_directory: &str) -> String {
    format!(
        "#!/bin/sh\nselection={}\nevidence={}\n\n{FUZZEL_SUBSTITUTE_BODY}",
        shell_quote(selection),
        shell_quote(evidence_directory)
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

    /// Without an exported launch context, the case would test nothing.
    pub fn require_launch_context(&self) -> Result<(), String> {
        if self.context != LAUNCH_CONTEXT_EXPORTED {
            return Err(format!(
                "Sway's exec exported no launch context: {:?}",
                self.context
            ));
        }
        Ok(())
    }

    /// A successful load, with nothing on stdout.
    pub fn require_success(&self) -> Result<(), String> {
        if self.exit_code != 0 || !self.stdout.trim().is_empty() {
            return Err(format!(
                "sway-stage started by Sway's exec failed: exit {}: {}",
                self.exit_code,
                self.stderr.trim()
            ));
        }
        Ok(())
    }
}

/// What sway-stage's menu passed to the Fuzzel substitute, and which row it selected.
pub struct MenuEvidence {
    arguments: String,
    rows: String,
    selected_index: String,
}

impl MenuEvidence {
    /// Read after sway-stage has exited, so the substitute has too.
    pub fn read(evidence_directory: &Path) -> Result<Self, String> {
        let read_evidence = |name: &str| {
            fs::read_to_string(evidence_directory.join(name))
                .map_err(|error| format!("read Fuzzel substitute {name}: {error}"))
        };
        Ok(Self {
            arguments: read_evidence("menu-arguments")?,
            rows: read_evidence("menu-rows")?,
            selected_index: read_evidence("menu-selection")?,
        })
    }

    /// Requires the menu's one selection prompt, on the virtual output that its snapshot
    /// recorded, offering exactly the rows the specification orders, with the case's row
    /// selected by its index. The empty focused workspace offers layouts rather than
    /// workspace capture, and the fixture's capturable workspace makes profile capture
    /// eligible.
    pub fn verify(&self, case: Case) -> Result<(), String> {
        let selection = case
            .menu_selection()
            .expect("only menu cases have menu evidence");
        let expected_arguments = format!(
            "--dmenu\n--no-exit-on-keyboard-focus-loss\n--no-sort\n--index\n--output={OUTPUT_NAME}\n--prompt=> \n"
        );
        if self.arguments != expected_arguments {
            return Err(format!(
                "menu prompt arguments differ: expected {expected_arguments:?}, observed {:?}",
                self.arguments
            ));
        }

        let expected_rows = [
            "Capture session profile…".to_owned(),
            format!("Layout: {}", case.name()),
            format!("Profile: {}", case.name()),
        ];
        let observed_rows: Vec<&str> = self.rows.lines().collect();
        if observed_rows != expected_rows {
            return Err(format!(
                "menu rows differ: expected {expected_rows:?}, observed {observed_rows:?}"
            ));
        }

        let selected_index = expected_rows
            .iter()
            .position(|row| *row == selection)
            .expect("the case's row is offered");
        if self.selected_index != format!("{selected_index}\n") {
            return Err(format!(
                "menu selection differs: expected index {selected_index} ({selection:?}), observed {:?}",
                self.selected_index
            ));
        }
        Ok(())
    }
}

/// A profile load's exact workspace events: creating and selecting the target removes the
/// empty origin, and no workspace is recreated for the window.
pub fn expected_workspace_events<'a>(
    origin: &'a WorkspaceIdentity,
    target: &'a WorkspaceIdentity,
) -> [ExpectedWorkspaceEvent<'a>; 3] {
    [
        ExpectedWorkspaceEvent::Init(target),
        ExpectedWorkspaceEvent::Focus {
            from_id: origin.id,
            to: target,
        },
        ExpectedWorkspaceEvent::Empty(origin),
    ]
}

#[cfg(test)]
#[path = "launch_context_tests.rs"]
mod tests;
