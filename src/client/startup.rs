use super::*;

/// Runs the thin client and enters the main event loop.
pub fn run_client(config: &crate::config::Config) -> io::Result<()> {
    run_client_with_mode(config, None, None, "connecting to server")
}

pub fn run_terminal_attach(
    config: &crate::config::Config,
    terminal_id: String,
    takeover: bool,
) -> io::Result<()> {
    run_client_with_mode(
        config,
        Some((terminal_id, takeover)),
        Some(AttachEscapeState::default()),
        "attaching to terminal",
    )
}
