use shepr_core::env::{ChildEnv, EnvVar, PaneEnvPolicy, RegisteredEnv};
use shepr_protocol::PublicPaneId;
use shepr_pty::PtyCommand;

/// Every registered name a pane child must not inherit: the scrubbed and
/// server-only names, and every agent descriptor's session markers. A marker
/// no descriptor lists would pass through, which the policy test refuses.
/// A pane id belongs to the launch that assigned it, so an enclosing pane's
/// id is removed even when this launch assigns none.
fn scrubbed_pane_names() -> impl Iterator<Item = RegisteredEnv> {
    RegisteredEnv::all()
        .filter(|variable| {
            !matches!(
                variable.pane_policy(),
                PaneEnvPolicy::Allowed | PaneEnvPolicy::AgentSession
            )
        })
        .chain(
            shepr_agent::AGENTS
                .iter()
                .flat_map(|agent| agent.session_markers.iter().copied())
                .map(RegisteredEnv::from),
        )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchKind {
    Fresh,
    Restored,
    AgentResume,
}

impl LaunchKind {
    pub fn requires_cwd(self) -> bool {
        !matches!(self, Self::Fresh)
    }
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
pub(super) struct PaneLaunchEnv {
    /// The public id of a managed pane. When absent, `SHEPR_PANE_ID` stays
    /// unset rather than inheriting an enclosing pane's id.
    pane_id: Option<PublicPaneId>,
    /// Resolved server socket exported to every pane.
    socket_path: std::path::PathBuf,
}

impl PaneLaunchEnv {
    pub(super) fn new(socket_path: std::path::PathBuf) -> Self {
        Self {
            socket_path,
            pane_id: None,
        }
    }

    pub(super) fn with_pane_id(mut self, pane_id: PublicPaneId) -> Self {
        self.pane_id = Some(pane_id);
        self
    }
}

pub(super) fn apply_pane_launch_env(cmd: &mut PtyCommand, launch_env: &PaneLaunchEnv) {
    // Strip every inherited value that describes an enclosing terminal,
    // multiplexer or agent scope. Dedicated typed fields are installed below.
    for name in scrubbed_pane_names() {
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
    pub default_shell: &'a shepr_core::shell::ResolvedShell,
    pub login_shell: bool,
}

impl<'a> PaneShellConfig<'a> {
    pub fn new(default_shell: &'a shepr_core::shell::ResolvedShell, login_shell: bool) -> Self {
        Self {
            default_shell,
            login_shell,
        }
    }
}

/// Carry the validated shell unchanged into the child command.
pub(super) fn pane_shell_command_builder(
    shell_config: PaneShellConfig<'_>,
    kind: LaunchKind,
) -> PtyCommand {
    let mut command =
        PtyCommand::interactive_shell(shell_config.default_shell, shell_config.login_shell);
    if kind.requires_cwd() {
        command.require_cwd();
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::fixture::resolved_shell as test_shell;

    /// Every registered name with its pane policy, across both vocabularies.
    fn every_policy() -> Vec<(RegisteredEnv, PaneEnvPolicy)> {
        RegisteredEnv::all()
            .map(|variable| (variable, variable.pane_policy()))
            .collect()
    }

    #[test]
    fn every_registered_environment_variable_has_a_pane_policy() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let inherited = [
            ("SHEPR_TEST_UNREGISTERED".into(), "inherited".into()),
            ("SSH_AUTH_SOCK".into(), "/run/user/1000/agent.sock".into()),
        ]
        .into_iter()
        .collect();
        let mut command = PtyCommand::interactive_shell(&test_shell("/shell"), false)
            .with_inherited_env(inherited);
        for (name, _) in every_policy() {
            command.env(name, "inherited");
        }

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
                PaneEnvPolicy::Scrubbed
                | PaneEnvPolicy::ServerOnly
                | PaneEnvPolicy::AgentSession => assert!(
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
    fn pane_id_is_not_inherited_but_an_assigned_id_is_exported() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let workspace_id = "w1".parse().expect("test workspace id");
        let inherited = PublicPaneId::new(
            &workspace_id,
            shepr_protocol::PanePublicNumber::new(17).expect("nonzero literal"),
        );
        let mut command = PtyCommand::interactive_shell(&test_shell("/shell"), false);
        command.env(EnvVar::SheprPaneId, inherited.to_string());

        apply_pane_launch_env(&mut command, &PaneLaunchEnv::new("/run/shepr.sock".into()));
        assert!(command.get_env(EnvVar::SheprPaneId).is_none());

        let assigned = PublicPaneId::new(
            &"w2".parse().expect("test workspace id"),
            shepr_protocol::PanePublicNumber::new(3).expect("nonzero literal"),
        );
        apply_pane_launch_env(
            &mut command,
            &PaneLaunchEnv::new("/run/shepr.sock".into()).with_pane_id(assigned),
        );
        assert_eq!(
            command.get_env(EnvVar::SheprPaneId),
            Some(std::ffi::OsStr::new(assigned.to_string().as_str()))
        );
    }

    #[test]
    fn an_inherited_socket_variable_gives_way_to_the_resolved_socket() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let mut command = PtyCommand::interactive_shell(&test_shell("/shell"), false);
        command.env(EnvVar::SheprSocketPath, "/inherited/server.sock");
        let socket = std::path::PathBuf::from("/custom/shepr.sock");
        apply_pane_launch_env(&mut command, &PaneLaunchEnv::new(socket.clone()));
        assert_eq!(
            command.get_env(EnvVar::SheprSocketPath),
            Some(socket.as_os_str())
        );
    }
}
