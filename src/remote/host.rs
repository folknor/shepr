//! Remote-host side of the SSH stdio bridge.

use std::io;
use std::time::Duration;

pub(crate) fn run_remote_client_bridge(
    idle_timeout: bool,
    paths: &crate::config::AppPaths,
) -> io::Result<()> {
    ensure_remote_server_running(paths)?;
    let _ssh_agent = super::ssh_agent::Registration::start(paths);

    let socket_path = paths.server_address().client_socket().to_path_buf();
    let stream = crate::ipc::connect_local_stream(&socket_path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to connect to remote Shepr client socket {}: {err}",
                socket_path.display()
            ),
        )
    })?;

    shepr_platform::forward_remote_bridge_stdio(stream, idle_timeout)
}

/// Starts the server when none is listening. A running server of another build
/// is not screened here: the client's handshake through this bridge reads its
/// build-identity preamble and reports the mismatch.
fn ensure_remote_server_running(paths: &crate::config::AppPaths) -> io::Result<()> {
    let socket_path = paths.server_address().client_socket().to_path_buf();
    if super::autodetect::is_server_listening(paths) {
        return Ok(());
    }

    super::autodetect::spawn_server_daemon(paths)?;
    super::autodetect::wait_for_server_socket(&socket_path, Duration::from_secs(5), paths)
}
