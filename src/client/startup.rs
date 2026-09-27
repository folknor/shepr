use super::*;

/// Runs the thin client and enters the main event loop.
pub fn run_client(
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
) -> io::Result<()> {
    run_client_with_mode(
        config,
        paths,
        ClientLaunchMode::Shell,
        "connecting to server",
    )
}

pub fn run_terminal_attach(
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
    terminal_id: String,
    takeover: bool,
) -> io::Result<()> {
    run_client_with_mode(
        config,
        paths,
        ClientLaunchMode::Attach {
            terminal_id: terminal_id.into(),
            takeover,
            escape: AttachEscapeState::from_config(config),
        },
        "attaching to terminal",
    )
}
