pub use crate::limits::AGENT_RESUME_DETECTION_HOLD;
use shepr_core::env::{ChildEnv, EnvVar};
use shepr_protocol::PublicPaneId;
use shepr_pty::PtyCommand;

/// What a pane child sees of one registered variable. `PtyCommand` hands the
/// pane the server's whole environment (its `base_env` says why); this is the
/// one place that decides what is removed from it. Both of core's vocabularies,
/// the variables a shepr process interprets and the ones it writes or removes
/// in a child, are matched exhaustively, so a variable cannot be registered
/// without a pane decision. Variables shepr has never heard of pass through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneEnvPolicy {
    /// The inherited value or shepr's replacement remains visible to the child.
    Allowed,
    /// The inherited value describes something outside this pane (the outer
    /// terminal, an outer agent session, another scope's handoff) and is
    /// removed, but explicit launch values may opt back in.
    Scrubbed,
    /// The value is removed even if explicit launch values contain it.
    ServerOnly,
}

fn pane_env_policy(variable: EnvVar) -> PaneEnvPolicy {
    match variable {
        EnvVar::SheprStartupCwd | EnvVar::SheprDebugOscEvidence => PaneEnvPolicy::ServerOnly,
        EnvVar::Tmux | EnvVar::WeztermPane => PaneEnvPolicy::Scrubbed,
        EnvVar::SheprConfigPath
        | EnvVar::SheprSession
        | EnvVar::SheprSocketPath
        | EnvVar::SheprClientSocketPath
        | EnvVar::SheprPaneId
        | EnvVar::SheprEnv
        | EnvVar::SheprLog
        | EnvVar::Home
        | EnvVar::XdgConfigHome
        | EnvVar::XdgStateHome
        | EnvVar::XdgRuntimeDir
        | EnvVar::Shell
        | EnvVar::Path
        | EnvVar::SshAuthSock
        | EnvVar::SshConnection
        | EnvVar::SshTty
        | EnvVar::VscodeIpcHookCli
        | EnvVar::TermProgram
        | EnvVar::WaylandDisplay
        | EnvVar::Display
        | EnvVar::PiCodingAgentDir
        | EnvVar::PiConfigDir
        | EnvVar::ClaudeConfigDir
        | EnvVar::CodexHome
        | EnvVar::KimiCodeHome
        | EnvVar::CopilotHome
        | EnvVar::QoderConfigDir
        | EnvVar::QwenHome
        | EnvVar::CursorConfigDir
        | EnvVar::AntigravityCliConfigDir
        | EnvVar::GrokHome
        | EnvVar::GitCeilingDirectories
        | EnvVar::GitConfigGlobal
        | EnvVar::GitConfigSystem
        | EnvVar::GitConfigNoSystem
        | EnvVar::GitConfigCount
        | EnvVar::GitConfigParameters => PaneEnvPolicy::Allowed,
    }
}

/// Scrubbed: the outer terminal's or multiplexer's host handles, which never
/// name this pane; an outer agent session's markers, since a new pane is not a
/// child agent of the process that started the server; and the focus values
/// handed only to tab-bar status commands.
///
/// Allowed: the terminal identity and `SHEPR_BIN_PATH`, which the terminal and
/// launch layers below replace for every pane; the inherited shell inputs
/// (`SHELL` is rewritten to the resolved shell at spawn); and the user's own
/// askpass setup, which only shepr's SSH bridge overrides, on its own ssh child.
fn pane_child_env_policy(variable: ChildEnv) -> PaneEnvPolicy {
    match variable {
        ChildEnv::ItermSessionId
        | ChildEnv::LcTerminal
        | ChildEnv::LcTerminalVersion
        | ChildEnv::KittyWindowId
        | ChildEnv::WtSession
        | ChildEnv::TmuxPane
        | ChildEnv::Sty
        | ChildEnv::Zellij
        | ChildEnv::ZellijSessionName
        | ChildEnv::ZellijPaneId
        | ChildEnv::ClaudeCode
        | ChildEnv::ClaudeCodeChildSession
        | ChildEnv::ClaudeCodeSessionId
        | ChildEnv::ClaudeCodeMessagingToken
        | ChildEnv::CodexThreadId
        | ChildEnv::Ompcode
        | ChildEnv::SheprActiveWorkspaceId
        | ChildEnv::SheprActiveTabId
        | ChildEnv::SheprActivePaneId
        | ChildEnv::SheprActivePaneCwd => PaneEnvPolicy::Scrubbed,
        ChildEnv::Term
        | ChildEnv::Colorterm
        | ChildEnv::TermProgramVersion
        | ChildEnv::SheprBinPath
        | ChildEnv::Shell
        | ChildEnv::Path
        | ChildEnv::SshAskpass
        | ChildEnv::SshAskpassRequire => PaneEnvPolicy::Allowed,
    }
}

/// Every registered name whose pane policy satisfies `wanted`. `SHELL` and
/// `PATH` are in both vocabularies with the same policy, so they may appear
/// twice; removal is idempotent.
fn registered_names_where(
    wanted: impl Fn(PaneEnvPolicy) -> bool,
) -> impl Iterator<Item = &'static str> {
    let interpreted = EnvVar::ALL
        .iter()
        .map(|&variable| (variable.name(), pane_env_policy(variable)));
    let written = ChildEnv::ALL
        .iter()
        .map(|&variable| (variable.name(), pane_child_env_policy(variable)));
    interpreted
        .chain(written)
        .filter(move |&(_, policy)| wanted(policy))
        .map(|(name, _)| name)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum LaunchPurpose {
    #[default]
    Fresh,
    AgentResume,
}

pub(super) fn apply_pane_terminal_env(cmd: &mut PtyCommand) {
    // Each pane is rendered by shepr's own terminal layer, not the outer terminal
    // that launched the app. Advertising the inherited TERM leaks the host terminal
    // identity into shells and across SSH, which breaks redraw and cursor movement
    // when the remote side lacks matching terminfo entries.
    cmd.env(ChildEnv::Term, shepr_vt::PANE_TERM);
    cmd.env(ChildEnv::Colorterm, shepr_vt::PANE_COLORTERM);
    cmd.env(EnvVar::TermProgram, "shepr");
    cmd.env(
        ChildEnv::TermProgramVersion,
        shepr_protocol::build_version(),
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneLaunchEnv {
    extra: Vec<(String, String)>,
    /// The public id of a managed pane; `None` inherits whatever the server
    /// environment carries.
    pane_id: Option<PublicPaneId>,
    purpose: LaunchPurpose,
    /// Resolved API socket path supplied by the server. An explicit
    /// `--session` can select a socket that is not present in the environment.
    api_socket_path: std::path::PathBuf,
}

impl PaneLaunchEnv {
    pub fn from_extra(extra: Vec<(String, String)>, api_socket_path: std::path::PathBuf) -> Self {
        Self {
            extra,
            pane_id: None,
            purpose: LaunchPurpose::Fresh,
            api_socket_path,
        }
    }

    pub fn for_agent_resume(mut self) -> Self {
        self.purpose = LaunchPurpose::AgentResume;
        self
    }

    pub fn with_pane_id(mut self, pane_id: PublicPaneId) -> Self {
        self.pane_id = Some(pane_id);
        self
    }

    pub(super) fn purpose(&self) -> LaunchPurpose {
        self.purpose
    }
}

pub(super) fn apply_pane_launch_env(cmd: &mut PtyCommand, launch_env: &PaneLaunchEnv) {
    if let Some(path) = shepr_platform::ssh_agent::pane_agent_socket(&launch_env.api_socket_path) {
        cmd.env(EnvVar::SshAuthSock, path);
    }
    // Explicit launch env below can opt back into a scrubbed variable, such as
    // an intentional child agent session or host handle.
    for name in registered_names_where(|policy| policy != PaneEnvPolicy::Allowed) {
        cmd.env_remove(name);
    }
    for (key, value) in &launch_env.extra {
        cmd.env(key, value);
    }
    // The startup directory is a one-time handoff, and OSC evidence capture
    // belongs to this server because it can log pane payloads. Neither setting
    // is part of the environment passed to a pane child.
    for name in registered_names_where(|policy| policy == PaneEnvPolicy::ServerOnly) {
        cmd.env_remove(name);
    }
    cmd.env(EnvVar::SheprEnv, shepr_core::env::SHEPR_ENV_IN_PANE);
    cmd.env(EnvVar::SheprSocketPath, &launch_env.api_socket_path);
    cmd.env_remove(ChildEnv::SheprBinPath);
    if let Ok(executable) = shepr_platform::launch_executable() {
        cmd.env(ChildEnv::SheprBinPath, executable);
    }
    if let Some(pane_id) = &launch_env.pane_id {
        cmd.env(EnvVar::SheprPaneId, pane_id.to_string());
    }
}

#[derive(Clone, Copy)]
pub struct PaneShellConfig<'a> {
    pub default_shell: &'a str,
    pub login_shell: bool,
}

impl<'a> PaneShellConfig<'a> {
    pub fn new(default_shell: &'a str, login_shell: bool) -> Self {
        Self {
            default_shell,
            login_shell,
        }
    }
}

/// Config has selected the shell at launch; the PTY verifies the resolved path
/// again when it builds the child command and uses it for exec and `SHELL`.
pub(super) fn pane_shell_command_builder(shell_config: PaneShellConfig<'_>) -> PtyCommand {
    PtyCommand::interactive_shell(shell_config.default_shell, shell_config.login_shell)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every registered name with its pane policy, across both vocabularies.
    fn every_policy() -> Vec<(&'static str, PaneEnvPolicy)> {
        EnvVar::ALL
            .iter()
            .map(|&variable| (variable.name(), pane_env_policy(variable)))
            .chain(
                ChildEnv::ALL
                    .iter()
                    .map(|&variable| (variable.name(), pane_child_env_policy(variable))),
            )
            .collect()
    }

    #[test]
    fn every_registered_environment_variable_has_a_pane_policy() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let mut command = PtyCommand::new("shell");
        for (name, _) in every_policy() {
            command.env(name, "inherited");
        }
        command.env("SHEPR_TEST_UNREGISTERED", "inherited");

        apply_pane_terminal_env(&mut command);
        apply_pane_launch_env(
            &mut command,
            &PaneLaunchEnv::from_extra(
                vec![
                    (EnvVar::SheprStartupCwd.name().into(), "override".into()),
                    (EnvVar::SheprDebugOscEvidence.name().into(), "true".into()),
                ],
                "/run/user/1000/shepr-test.sock".into(),
            ),
        );

        for (name, policy) in every_policy() {
            match policy {
                PaneEnvPolicy::Allowed => assert!(
                    command.get_env(name).is_some(),
                    "{name} must remain available to pane children"
                ),
                PaneEnvPolicy::Scrubbed | PaneEnvPolicy::ServerOnly => assert!(
                    command.get_env(name).is_none(),
                    "{name} must not reach pane children"
                ),
            }
        }
        assert_eq!(
            command.get_env("SHEPR_TEST_UNREGISTERED"),
            Some(std::ffi::OsStr::new("inherited")),
            "a variable shepr does not know passes through to the pane"
        );
        assert_eq!(
            command.get_env(EnvVar::TermProgram),
            Some(std::ffi::OsStr::new("shepr"))
        );
        assert_eq!(
            command.get_env(ChildEnv::TermProgramVersion),
            Some(std::ffi::OsStr::new(&shepr_protocol::build_version()))
        );
    }

    #[test]
    fn a_name_in_both_vocabularies_has_one_pane_policy() {
        for &child in ChildEnv::ALL {
            if let Some(&interpreted) = EnvVar::ALL
                .iter()
                .find(|variable| variable.name() == child.name())
            {
                assert_eq!(
                    pane_env_policy(interpreted),
                    pane_child_env_policy(child),
                    "{child}"
                );
            }
        }
    }

    #[test]
    fn explicit_launch_env_opts_back_into_scrubbed_but_not_server_only_variables() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let extra: Vec<(String, String)> = every_policy()
            .into_iter()
            .filter(|&(_, policy)| policy != PaneEnvPolicy::Allowed)
            .map(|(name, _)| (name.to_owned(), "explicit".to_owned()))
            .collect();
        let mut command = PtyCommand::new("shell");
        for (name, _) in &extra {
            command.env(name, "inherited");
        }

        apply_pane_terminal_env(&mut command);
        apply_pane_launch_env(
            &mut command,
            &PaneLaunchEnv::from_extra(extra, "/run/user/1000/shepr-test.sock".into()),
        );

        for (name, policy) in every_policy() {
            match policy {
                PaneEnvPolicy::Allowed => {}
                PaneEnvPolicy::Scrubbed => assert_eq!(
                    command.get_env(name),
                    Some(std::ffi::OsStr::new("explicit")),
                    "{name}"
                ),
                PaneEnvPolicy::ServerOnly => {
                    assert!(command.get_env(name).is_none(), "{name}");
                }
            }
        }
    }
}
