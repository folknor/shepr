use shepr_core::env::{ChildEnv, EnvVar};
use shepr_protocol::PublicPaneId;
use shepr_pty::PtyCommand;

/// Time allowed for a restored agent to appear after its resume launch.
pub const MANAGED_AGENT_RESUME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub(super) const PANE_COLORTERM: &str = "truecolor";

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
    cmd.env(ChildEnv::Colorterm, PANE_COLORTERM);
    cmd.env(EnvVar::TermProgram, "shepr");
    cmd.env(
        ChildEnv::TermProgramVersion,
        shepr_protocol::build_version(),
    );
    // Host handles refer to the outer terminal, never to this pane.
    for key in [
        ChildEnv::ItermSessionId.name(),
        ChildEnv::LcTerminal.name(),
        ChildEnv::LcTerminalVersion.name(),
        EnvVar::WeztermPane.name(),
        ChildEnv::KittyWindowId.name(),
        ChildEnv::WtSession.name(),
        EnvVar::Tmux.name(),
        ChildEnv::TmuxPane.name(),
        ChildEnv::Sty.name(),
        ChildEnv::Zellij.name(),
        ChildEnv::ZellijSessionName.name(),
        ChildEnv::ZellijPaneId.name(),
    ] {
        cmd.env_remove(key);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneLaunchEnv {
    extra: Vec<(String, String)>,
    /// The public id of a managed pane; `None` inherits whatever the server
    /// environment carries.
    pane_id: Option<PublicPaneId>,
    purpose: LaunchPurpose,
    api_socket_path: std::path::PathBuf,
}

impl PaneLaunchEnv {
    pub fn from_extra(extra: Vec<(String, String)>) -> Self {
        Self {
            extra,
            pane_id: None,
            purpose: LaunchPurpose::Fresh,
            api_socket_path: std::path::PathBuf::new(),
        }
    }

    pub fn with_api_socket_path(mut self, path: std::path::PathBuf) -> Self {
        self.api_socket_path = path;
        self
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
    // A new pane is not a child agent of the process that started the server.
    // Explicit launch env below can opt back into an intentional child session.
    for key in shepr_agent::agent::launch_env_to_scrub() {
        cmd.env_remove(key);
    }
    for (key, value) in &launch_env.extra {
        cmd.env(key, value);
    }
    cmd.env(EnvVar::SheprEnv, shepr_core::env::SHEPR_ENV_IN_PANE);
    cmd.env(EnvVar::SheprSocketPath, &launch_env.api_socket_path);
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

/// `PtyCommand` selects and resolves the shell at spawn, and uses that resolved
/// path for both exec and the child-visible `SHELL`.
pub(super) fn pane_shell_command_builder(shell_config: PaneShellConfig<'_>) -> PtyCommand {
    PtyCommand::interactive_shell(shell_config.default_shell, shell_config.login_shell)
}
