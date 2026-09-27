use shepr_protocol::{PublicPaneId, PublicTabId, WorkspaceId};
use shepr_pty::PtyCommand;

/// Time allowed for a restored agent to appear after its resume launch.
pub(crate) const MANAGED_AGENT_RESUME_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

const PANE_COLORTERM: &str = "truecolor";
pub const SHEPR_PANE_ID_ENV_VAR: &str = "SHEPR_PANE_ID";
const SHEPR_TAB_ID_ENV_VAR: &str = "SHEPR_TAB_ID";
const SHEPR_WORKSPACE_ID_ENV_VAR: &str = "SHEPR_WORKSPACE_ID";

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
    cmd.env("TERM", shepr_vt::PANE_TERM);
    cmd.env("COLORTERM", PANE_COLORTERM);
    cmd.env("TERM_PROGRAM", "shepr");
    cmd.env("TERM_PROGRAM_VERSION", shepr_protocol::build_version());
    // Host handles refer to the outer terminal, never to this pane.
    for key in [
        "ITERM_SESSION_ID",
        "LC_TERMINAL",
        "LC_TERMINAL_VERSION",
        "WEZTERM_PANE",
        "KITTY_WINDOW_ID",
        "WT_SESSION",
        "TMUX",
        "TMUX_PANE",
        "STY",
        "ZELLIJ",
        "ZELLIJ_SESSION_NAME",
        "ZELLIJ_PANE_ID",
    ] {
        cmd.env_remove(key);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PaneLaunchEnv {
    extra: Vec<(String, String)>,
    identity: PaneLaunchIdentity,
    purpose: LaunchPurpose,
    api_socket_path: std::path::PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum PaneLaunchIdentity {
    #[default]
    Inherit,
    Managed {
        workspace_id: WorkspaceId,
        tab_id: PublicTabId,
        pane_id: PublicPaneId,
    },
}

impl PaneLaunchEnv {
    pub(crate) fn from_extra(extra: Vec<(String, String)>) -> Self {
        Self {
            extra,
            identity: PaneLaunchIdentity::Inherit,
            purpose: LaunchPurpose::Fresh,
            api_socket_path: std::path::PathBuf::new(),
        }
    }

    pub(crate) fn with_api_socket_path(mut self, path: std::path::PathBuf) -> Self {
        self.api_socket_path = path;
        self
    }

    pub(crate) fn for_agent_resume(mut self) -> Self {
        self.purpose = LaunchPurpose::AgentResume;
        self
    }

    pub(crate) fn with_identity(
        mut self,
        workspace_id: WorkspaceId,
        tab_id: PublicTabId,
        pane_id: PublicPaneId,
    ) -> Self {
        self.identity = PaneLaunchIdentity::Managed {
            workspace_id,
            tab_id,
            pane_id,
        };
        self
    }

    pub(super) fn purpose(&self) -> LaunchPurpose {
        self.purpose
    }
}

pub(super) fn apply_pane_launch_env(cmd: &mut PtyCommand, launch_env: &PaneLaunchEnv) {
    if let Some(path) = shepr_platform::ssh_agent::pane_agent_socket(&launch_env.api_socket_path) {
        cmd.env("SSH_AUTH_SOCK", path);
    }
    // A new pane is not a child agent of the process that started the server.
    // Explicit launch env below can opt back into an intentional child session.
    for key in shepr_agent::agent::launch_env_to_scrub() {
        cmd.env_remove(key);
    }
    for (key, value) in &launch_env.extra {
        cmd.env(key, value);
    }
    cmd.env(crate::SHEPR_ENV_VAR, crate::SHEPR_ENV_VALUE);
    cmd.env(
        shepr_config::SOCKET_PATH_ENV_VAR,
        &launch_env.api_socket_path,
    );
    if let Ok(executable) = shepr_platform::launch_executable() {
        cmd.env("SHEPR_BIN_PATH", executable);
    }
    match &launch_env.identity {
        PaneLaunchIdentity::Inherit => {}
        PaneLaunchIdentity::Managed {
            workspace_id,
            tab_id,
            pane_id,
        } => {
            cmd.env(SHEPR_WORKSPACE_ID_ENV_VAR, workspace_id.as_str());
            cmd.env(SHEPR_TAB_ID_ENV_VAR, tab_id.to_string());
            cmd.env(SHEPR_PANE_ID_ENV_VAR, pane_id.to_string());
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PaneShellConfig<'a> {
    pub(crate) default_shell: &'a str,
    pub(crate) login_shell: bool,
}

impl<'a> PaneShellConfig<'a> {
    pub(crate) fn new(default_shell: &'a str, login_shell: bool) -> Self {
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
