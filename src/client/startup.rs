use super::*;

/// Runs the thin client and enters the main event loop.
pub fn run_client(
    config: &crate::config::ValidatedConfig,
    paths: &crate::config::AppPaths,
) -> io::Result<()> {
    run_client_with_mode(config, paths, None, None, "connecting to server")
}

pub fn run_terminal_attach(
    config: &crate::config::ValidatedConfig,
    paths: &crate::config::AppPaths,
    terminal_id: String,
    takeover: bool,
) -> io::Result<()> {
    run_client_with_mode(
        config,
        paths,
        Some((terminal_id, takeover)),
        Some(AttachEscapeState::from_config(config)),
        "attaching to terminal",
    )
}
