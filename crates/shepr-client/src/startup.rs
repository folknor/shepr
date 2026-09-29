use super::*;

/// Runs the thin client and enters the main event loop.
///
/// Returns the lines the binary prints once the host terminal is restored; a
/// failed run is a [`ClientRunError`], after which the binary exits nonzero.
pub fn run_client(
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
) -> Result<ClientExit, ClientRunError> {
    run_client_with_mode(
        config,
        paths,
        ClientLaunchMode::Shell,
        "connecting to server",
    )
}

/// Attaches the host terminal directly to one server terminal. Returns what
/// [`run_client`] returns.
pub fn run_terminal_attach(
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
    terminal_id: shepr_protocol::TerminalId,
    takeover: bool,
) -> Result<ClientExit, ClientRunError> {
    run_client_with_mode(
        config,
        paths,
        ClientLaunchMode::Attach {
            terminal_id,
            takeover,
            escape: AttachEscapeState::from_config(config),
        },
        "attaching to terminal",
    )
}
