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
    /// A server API socket has no listening server.
    ServerNotRunning {
        /// The API socket that was checked.
        socket_path: &'a Path,
        /// Command that starts or attaches to this server.
        attach_command: &'a str,
    },
}

/// Renders the next step for a server condition.
pub fn operator_guidance(target: OperatorGuidance<'_>) -> String {
    match target {
        OperatorGuidance::LocalBuildMismatch {
            stop_command,
            attach_command,
        } => {
            let restart = match attach_command {
                Some(command) => format!("Run `{stop_command}`, then run `{command}` again."),
                None => format!(
                    "Run `{stop_command}`, then restart Shepr with the same socket override."
                ),
            };
            format!(
                "To keep the running server and its panes, keep using the shepr build that started it.\nTo use this build here instead, stop the running server; stopping exits its pane processes. {restart}"
            )
        }
        OperatorGuidance::ServerNotRunning {
            socket_path,
            attach_command,
        } => format!(
            "no shepr server is running at {}; run `{attach_command}` to start or attach it",
            socket_path.display()
        ),
    }
}
