//! Operator-facing next-step guidance shared by API and CLI error paths.

use std::path::Path;

/// The context needed to render one operator-facing next step.
#[derive(Clone, Copy)]
pub enum OperatorGuidance<'a> {
    /// A local command reached a server from another build.
    LocalBuildMismatch {
        /// Command that stops the selected server.
        stop_command: &'a str,
        /// Command that attaches to the selected server, if known.
        attach_command: Option<&'a str>,
    },
    /// A command reached a saved machine running another build.
    MachineBuildMismatch {
        /// The saved machine's label.
        label: &'a str,
        /// The session selected for the saved machine.
        session: &'a str,
        /// The saved machine's profile ID.
        id: &'a str,
    },
    /// A server API socket has no listening server.
    ServerNotRunning {
        /// The API socket that was checked.
        socket_path: &'a Path,
        /// Command that starts or attaches to this server.
        attach_command: &'a str,
    },
}

/// Renders the next step for a server or machine condition.
pub fn operator_guidance(target: OperatorGuidance<'_>) -> String {
    match target {
        OperatorGuidance::LocalBuildMismatch {
            stop_command,
            attach_command,
        } => {
            let restart = match attach_command {
                Some(command) => {
                    format!(
                        "Run `{stop_command} {}`, then run `{command}` again.",
                        crate::session::FORCE_STOP_FLAG
                    )
                }
                None => format!(
                    "Run `{stop_command} {}`, then restart Shepr with the same socket override.",
                    crate::session::FORCE_STOP_FLAG
                ),
            };
            format!(
                "To keep the running server and its panes, run this build in a session of its own: pass `--session <name>` with a name no running server uses.\nTo use this build here instead, stop the running server; stopping exits its pane processes. {restart}"
            )
        }
        OperatorGuidance::MachineBuildMismatch { label, session, id } => format!(
            "Install the same Shepr build on machine '{label}'. To keep its running server (session {session}) and its panes, save the machine again with a session of its own: `shepr machine add <ssh-target> --label <label> --remote-session <name>`. To replace that server instead, stop it with `shepr --machine {id} server stop {}`; the next connection starts it again. Stopping the server exits its pane processes.",
            crate::session::FORCE_STOP_FLAG
        ),
        OperatorGuidance::ServerNotRunning {
            socket_path,
            attach_command,
        } => format!(
            "no shepr server is running at {}; run `{attach_command}` to start or attach it",
            socket_path.display()
        ),
    }
}
