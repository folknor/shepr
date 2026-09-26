//! Remote-host side of the SSH stdio bridge.

use std::io;
use std::time::Duration;

pub(crate) fn run_remote_client_bridge(
    idle_timeout: bool,
    paths: &crate::config::AppPaths,
) -> io::Result<()> {
    ensure_remote_server_running(paths)?;
    let _ssh_agent = super::ssh_agent::Registration::start(paths);

    let socket_path = crate::server::socket_paths::client_socket_path(paths);
    let stream = crate::ipc::connect_local_stream(&socket_path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to connect to remote Shepr client socket {}: {err}",
                socket_path.display()
            ),
        )
    })?;

    crate::platform::forward_remote_bridge_stdio(stream, idle_timeout)
}

/// Starts the server when none is listening. A running server of another build
/// is not screened here: the client's handshake through this bridge reads its
/// build-identity preamble and reports the mismatch.
fn ensure_remote_server_running(paths: &crate::config::AppPaths) -> io::Result<()> {
    let socket_path = crate::server::socket_paths::client_socket_path(paths);
    if crate::server::autodetect::is_server_listening(paths) {
        return Ok(());
    }

    crate::server::autodetect::spawn_server_daemon(paths)?;
    crate::server::autodetect::wait_for_server_socket(&socket_path, Duration::from_secs(5), paths)
}
