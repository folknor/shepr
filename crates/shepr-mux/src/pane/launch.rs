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
    /// Inherited and raw extra values are removed. A dedicated typed launch
    /// field may install its value after this policy runs.
    ServerOnly,
}

fn pane_env_policy(variable: EnvVar) -> PaneEnvPolicy {
    match variable {
        // A pane id belongs to the launch that assigned it. Never let a
        // server's enclosing pane id stand in for an id this launch omitted.
        EnvVar::SheprStartupCwd | EnvVar::SheprDebugOscEvidence | EnvVar::SheprPaneId => {
            PaneEnvPolicy::ServerOnly
        }
        EnvVar::Tmux | EnvVar::WeztermPane => PaneEnvPolicy::Scrubbed,
        EnvVar::SheprConfigPath
        | EnvVar::SheprSocketPath
        | EnvVar::SheprClientSocketPath
        | EnvVar::SheprEnv
        | EnvVar::SheprBuildProfile
        | EnvVar::SheprLog
        | EnvVar::Home
        | EnvVar::XdgConfigHome
        | EnvVar::XdgStateHome
        | EnvVar::XdgRuntimeDir
        | EnvVar::Shell
        | EnvVar::Path
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
/// child agent of the process that started the server.
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
        | ChildEnv::Ompcode => PaneEnvPolicy::Scrubbed,
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
    /// The public id of a managed pane. When absent, `SHEPR_PANE_ID` stays
    /// unset rather than inheriting an enclosing pane's id.
    pane_id: Option<PublicPaneId>,
    purpose: LaunchPurpose,
    /// Resolved socket pair supplied by the server, which need not be present
    /// in the environment; the pane exports both.
    api_socket_path: std::path::PathBuf,
    client_socket_path: std::path::PathBuf,
}

impl PaneLaunchEnv {
    pub fn from_extra(extra: Vec<(String, String)>, api_socket_path: std::path::PathBuf) -> Self {
        let client_socket_path =
            shepr_config::derive_client_socket_from_api_socket(&api_socket_path);
        Self::from_extra_with_socket_paths(extra, api_socket_path, client_socket_path)
    }

    pub fn from_extra_with_socket_paths(
        extra: Vec<(String, String)>,
        api_socket_path: std::path::PathBuf,
        client_socket_path: std::path::PathBuf,
    ) -> Self {
        Self {
            extra,
            pane_id: None,
            purpose: LaunchPurpose::Fresh,
            api_socket_path,
            client_socket_path,
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
    // Explicit launch env below can opt back into a scrubbed variable, such as
    // an intentional child agent session or host handle.
    for name in registered_names_where(|policy| policy != PaneEnvPolicy::Allowed) {
        cmd.env_remove(name);
    }
    for (key, value) in &launch_env.extra {
        cmd.env(key, value);
    }
    // The startup directory is a one-time handoff, OSC evidence capture
    // belongs to this server because it can log pane payloads, and an
    // inherited pane id belongs to an enclosing launch. None is passed raw to
    // a child; an assigned pane id is installed from its typed field below.
    for name in registered_names_where(|policy| policy == PaneEnvPolicy::ServerOnly) {
        cmd.env_remove(name);
    }
    cmd.env(EnvVar::SheprEnv, shepr_core::env::SHEPR_ENV_IN_PANE);
    // Both sockets are exported as the server resolved them, replacing any
    // inherited value. Every agent integration reports through the API
    // socket variable, so it is always set. Under a client-socket-only
    // override that costs a nested shepr client its client socket: the API
    // variable takes precedence and derives the runtime client socket, which
    // this server does not listen on. Dropping the API variable instead would
    // switch off every integration in such a pane.
    cmd.env(EnvVar::SheprSocketPath, &launch_env.api_socket_path);
    cmd.env(
        EnvVar::SheprClientSocketPath,
        &launch_env.client_socket_path,
    );
    // Names the profile whose server owns this pane, so a process of another
    // profile started inside it does not follow the socket variables above.
    cmd.env(
        EnvVar::SheprBuildProfile,
        shepr_config::BuildProfile::current().marker(),
    );
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
        command.env("SSH_AUTH_SOCK", "/run/user/1000/agent.sock");

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
            command.get_env("SSH_AUTH_SOCK"),
            Some(std::ffi::OsStr::new("/run/user/1000/agent.sock")),
            "a pane reaches the server's own SSH agent"
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

    #[test]
    fn pane_id_is_not_inherited_but_an_assigned_id_is_exported() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let workspace_id = "w1".parse().expect("test workspace id");
        let inherited = PublicPaneId::new(&workspace_id, 17);
        let mut command = PtyCommand::new("shell");
        command.env(EnvVar::SheprPaneId, inherited.to_string());

        apply_pane_launch_env(
            &mut command,
            &PaneLaunchEnv::from_extra(Vec::new(), "/run/shepr.sock".into()),
        );
        assert!(command.get_env(EnvVar::SheprPaneId).is_none());

        let assigned = PublicPaneId::new(&"w2".parse().expect("test workspace id"), 3);
        apply_pane_launch_env(
            &mut command,
            &PaneLaunchEnv::from_extra(Vec::new(), "/run/shepr.sock".into())
                .with_pane_id(assigned.clone()),
        );
        assert_eq!(
            command.get_env(EnvVar::SheprPaneId),
            Some(std::ffi::OsStr::new(assigned.as_str()))
        );
    }

    #[test]
    fn pane_launch_exports_a_resolved_socket_pair() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let api_socket = std::path::PathBuf::from("/run/shepr.sock");
        let client_socket = std::path::PathBuf::from("/run/shepr-client.sock");
        let mut command = PtyCommand::new("shell");

        apply_pane_launch_env(
            &mut command,
            &PaneLaunchEnv::from_extra_with_socket_paths(
                Vec::new(),
                api_socket.clone(),
                client_socket.clone(),
            ),
        );

        assert_eq!(
            command.get_env(EnvVar::SheprSocketPath),
            Some(api_socket.as_os_str())
        );
        assert_eq!(
            command.get_env(EnvVar::SheprClientSocketPath),
            Some(client_socket.as_os_str())
        );
    }

    #[test]
    fn inherited_socket_variables_give_way_to_the_resolved_pair() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let api_socket = std::path::PathBuf::from("/run/shepr/shepr.sock");
        let client_socket = std::path::PathBuf::from("/custom/shepr-client.sock");
        let mut command = PtyCommand::new("shell");
        command.env(EnvVar::SheprSocketPath, "/inherited/shepr.sock");
        command.env(EnvVar::SheprClientSocketPath, "/inherited/client.sock");

        apply_pane_launch_env(
            &mut command,
            &PaneLaunchEnv::from_extra_with_socket_paths(
                Vec::new(),
                api_socket.clone(),
                client_socket.clone(),
            ),
        );

        // Integrations report through the API socket, so it stays exported
        // even when the client socket came from a client-only override.
        assert_eq!(
            command.get_env(EnvVar::SheprSocketPath),
            Some(api_socket.as_os_str())
        );
        assert_eq!(
            command.get_env(EnvVar::SheprClientSocketPath),
            Some(client_socket.as_os_str())
        );
    }
}
