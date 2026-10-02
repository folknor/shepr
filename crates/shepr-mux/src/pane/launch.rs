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
    /// removed from the child environment.
    Scrubbed,
    /// Inherited values are removed. A dedicated typed launch field may
    /// install its value after this policy runs.
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
        EnvVar::SheprSocketPath
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
        | ChildEnv::ClaudeJobDir
        | ChildEnv::ClaudeCodeSessionKind
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
    /// The public id of a managed pane. When absent, `SHEPR_PANE_ID` stays
    /// unset rather than inheriting an enclosing pane's id.
    pane_id: Option<PublicPaneId>,
    purpose: LaunchPurpose,
    /// Resolved server socket exported to every pane.
    socket_path: std::path::PathBuf,
}

impl PaneLaunchEnv {
    pub fn new(socket_path: std::path::PathBuf) -> Self {
        Self {
            socket_path,
            pane_id: None,
            purpose: LaunchPurpose::Fresh,
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
    // Strip every inherited value that describes an enclosing terminal,
    // multiplexer or agent scope. Dedicated typed fields are installed below.
    for name in registered_names_where(|policy| policy != PaneEnvPolicy::Allowed) {
        cmd.env_remove(name);
    }
    // The startup directory and OSC evidence capture belong to this server;
    // an inherited pane id belongs to an enclosing launch. The assigned id is
    // installed from its typed field below.
    cmd.env(EnvVar::SheprEnv, shepr_core::env::SHEPR_ENV_IN_PANE);
    // The socket is exported as the server resolved it, replacing any
    // inherited value. Every agent integration reports through the
    // socket variable, so it is always set.
    cmd.env(EnvVar::SheprSocketPath, &launch_env.socket_path);
    // Names the profile whose server owns this pane, so a process of another
    // profile started inside it does not follow the socket variable above.
    cmd.env(
        EnvVar::SheprBuildProfile,
        shepr_config::BuildProfile::current().marker(),
    );
    cmd.env_remove(ChildEnv::SheprBinPath);
    if let Some(executable) = launch_executable() {
        cmd.env(ChildEnv::SheprBinPath, executable);
    }
    if let Some(pane_id) = &launch_env.pane_id {
        cmd.env(EnvVar::SheprPaneId, pane_id.to_string());
    }
}

/// The path panes are told to run shepr by, resolved once: resolving it stats
/// the server's own binary, which no pane spawn may do on the event loop. An
/// install that replaces the binary keeps the same path, which is what the
/// resolution would find again.
fn launch_executable() -> Option<&'static std::path::Path> {
    static EXECUTABLE: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    EXECUTABLE
        .get_or_init(|| shepr_platform::launch_executable().ok())
        .as_deref()
}

/// Does once, at server startup, everything a pane launch would otherwise do
/// on its first spawn that may touch the filesystem or NSS: bind the launch
/// status listener, read the passwd home, resolve the server binary path.
pub fn init_pane_launches() -> std::io::Result<()> {
    let _ = launch_executable();
    shepr_pty::launch::init()
}

#[derive(Clone, Copy)]
pub struct PaneShellConfig<'a> {
    pub default_shell: &'a str,
    pub login_shell: bool,
    require_cwd: bool,
}

impl<'a> PaneShellConfig<'a> {
    pub fn new(default_shell: &'a str, login_shell: bool) -> Self {
        Self {
            default_shell,
            login_shell,
            require_cwd: false,
        }
    }

    /// Fail the shell launch if its requested working directory has gone
    /// away. Resumed agent commands use this because redirecting one to `HOME`
    /// could act on a different project or session.
    pub fn require_cwd(mut self) -> Self {
        self.require_cwd = true;
        self
    }
}

/// Config has selected the shell at launch; the PTY verifies the resolved path
/// again when it builds the child command and uses it for exec and `SHELL`.
pub(super) fn pane_shell_command_builder(shell_config: PaneShellConfig<'_>) -> PtyCommand {
    let mut command =
        PtyCommand::interactive_shell(shell_config.default_shell, shell_config.login_shell);
    if shell_config.require_cwd {
        command.require_cwd();
    }
    command
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
        let mut command = PtyCommand::interactive_shell("shell", false);
        for (name, _) in every_policy() {
            command.env(name, "inherited");
        }
        command.env("SHEPR_TEST_UNREGISTERED", "inherited");
        command.env("SSH_AUTH_SOCK", "/run/user/1000/agent.sock");

        apply_pane_terminal_env(&mut command);
        apply_pane_launch_env(
            &mut command,
            &PaneLaunchEnv::new("/run/user/1000/shepr-test.sock".into()),
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
    fn pane_id_is_not_inherited_but_an_assigned_id_is_exported() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let workspace_id = "w1".parse().expect("test workspace id");
        let inherited = PublicPaneId::new(&workspace_id, 17);
        let mut command = PtyCommand::interactive_shell("shell", false);
        command.env(EnvVar::SheprPaneId, inherited.to_string());

        apply_pane_launch_env(&mut command, &PaneLaunchEnv::new("/run/shepr.sock".into()));
        assert!(command.get_env(EnvVar::SheprPaneId).is_none());

        let assigned = PublicPaneId::new(&"w2".parse().expect("test workspace id"), 3);
        apply_pane_launch_env(
            &mut command,
            &PaneLaunchEnv::new("/run/shepr.sock".into()).with_pane_id(assigned.clone()),
        );
        assert_eq!(
            command.get_env(EnvVar::SheprPaneId),
            Some(std::ffi::OsStr::new(assigned.as_str()))
        );
    }

    #[test]
    fn an_inherited_socket_variable_gives_way_to_the_resolved_socket() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let mut command = PtyCommand::interactive_shell("shell", false);
        command.env(EnvVar::SheprSocketPath, "/inherited/server.sock");
        let socket = std::path::PathBuf::from("/custom/shepr.sock");
        apply_pane_launch_env(&mut command, &PaneLaunchEnv::new(socket.clone()));
        assert_eq!(
            command.get_env(EnvVar::SheprSocketPath),
            Some(socket.as_os_str())
        );
    }
}
