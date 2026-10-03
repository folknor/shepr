//! Remote-host side of the SSH stdio bridge.

use std::io;

/// Marker on the first stderr line for a daemon that exited during boot. The
/// local bridge consumes the record into the endpoint failure vocabulary.
pub(super) const DAEMON_BOOT_EXIT_MARKER: &str = "shepr-remote-daemon-boot-exit:";

/// Relays this process's stdio to the server socket until either side
/// closes or the idle watchdog fires. The outcome goes back to the binary: on
/// [`shepr_platform::RemoteBridgeOutcome::IdleExpired`] it must end the
/// process promptly with status 1, without writing to stdout.
pub fn run_remote_client_bridge(
    paths: &shepr_config::AppPaths,
) -> io::Result<shepr_platform::RemoteBridgeOutcome> {
    let status = ensure_remote_server_running(paths)?;
    // A server of another build is answered here, never connected to: its
    // socket may not speak this build's client protocol at all, and the
    // client must still read a typed mismatch rather than an EOF it would
    // retry forever.
    if !status.build_id.is_this_build() {
        shepr_platform::answer_remote_bridge(&shepr_protocol::preamble::preamble_for(
            &status.build_id.to_string(),
        ))?;
        return Ok(shepr_platform::RemoteBridgeOutcome::Closed);
    }

    let socket_path = paths.server_address().socket().to_path_buf();
    // The server's owner is checked before the first relayed byte reaches it.
    let stream =
        shepr_platform::ipc::connect_trusted_local_stream(&socket_path).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "failed to connect to remote Shepr server socket {}: {err}",
                    socket_path.display()
                ),
            )
        })?;

    shepr_platform::forward_remote_bridge_stdio(stream)
}

/// Starts the server when none is listening, through the launcher the local
/// TUI uses: a `shepr-server` beside this executable, started under the launch
/// lock and verified to be this build before the bridge relays anything. A
/// running server of another build is returned, not refused: the bridge then
/// answers the client with that build's preamble, so the client reports a
/// typed mismatch, which it classifies as needing attention, whatever socket
/// layout that server has. Any other failure here reaches the client only as
/// this command's stderr and exit status, which it treats as an ordinary
/// retryable failure. A daemon that exited during boot is the exception: its
/// exit class leads the error as a [`DAEMON_BOOT_EXIT_MARKER`] record, which
/// the client's SSH bridge turns into a typed endpoint failure (a refused
/// configuration or failed start needs attention), keeping the daemon output
/// as its diagnostic.
fn ensure_remote_server_running(
    paths: &shepr_config::AppPaths,
) -> io::Result<shepr_api::RuntimeStatus> {
    match super::local_server::ensure_running(
        paths,
        super::local_server::SERVER_READY_TIMEOUT,
        super::local_server::BuildCheck::AtClientHandshake,
    ) {
        Err(error) => {
            let Some(class) = super::local_server::daemon_boot_exit_class(&error) else {
                return Err(error);
            };
            Err(io::Error::new(
                error.kind(),
                format!("{DAEMON_BOOT_EXIT_MARKER}{}\n{error}", class.code()),
            ))
        }
        result => result,
    }
}
