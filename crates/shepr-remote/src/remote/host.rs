//! Remote-host side of the SSH stdio bridge.

use std::io;
use std::time::Duration;

pub fn run_remote_client_bridge(paths: &shepr_config::AppPaths) -> io::Result<()> {
    ensure_remote_server_running(paths)?;
    let _ssh_agent = super::ssh_agent::Registration::start(paths);

    let socket_path = paths.server_address().client_socket().to_path_buf();
    let stream = shepr_platform::ipc::connect_local_stream(&socket_path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to connect to remote Shepr client socket {}: {err}",
                socket_path.display()
            ),
        )
    })?;

    shepr_platform::forward_remote_bridge_stdio(stream, true)
}

/// Starts the server when none is listening. A running server of another build
/// is not screened here: the client's handshake through this bridge reads its
/// build-identity preamble and reports a typed mismatch, which the client
/// classifies as needing attention. Failing here instead would reach the client
/// only as this command's stderr and exit status, which it classifies as an
/// ordinary retryable failure.
fn ensure_remote_server_running(paths: &shepr_config::AppPaths) -> io::Result<()> {
    let socket_path = paths.server_address().client_socket().to_path_buf();
    if super::local_server::is_server_listening(paths)? {
        return Ok(());
    }

    super::local_server::spawn_server_daemon(paths)?;
    super::local_server::wait_for_server_socket(&socket_path, Duration::from_secs(5), paths)
}
