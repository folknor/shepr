//! Operator-facing next-step guidance shared by API and CLI error paths.

use std::path::Path;

/// The context needed to render one operator-facing next step.
#[derive(Clone, Copy)]
pub enum OperatorGuidance<'a> {
    /// A server socket has no listening server.
    ServerNotRunning {
        /// The server socket that was checked.
        socket_path: &'a Path,
        /// Command that starts or attaches to this server.
        attach_command: &'a str,
    },
}

/// Renders the next step for a server condition.
pub fn operator_guidance(target: OperatorGuidance<'_>) -> String {
    match target {
        OperatorGuidance::ServerNotRunning {
            socket_path,
            attach_command,
        } => format!(
            "no shepr server is running at {}; run `{attach_command}` to start or attach it",
            socket_path.display()
        ),
    }
}
