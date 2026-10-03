//! Remote-host side of the SSH stdio bridge.

use std::io;

use shepr_launch::RemoteFailureClass;
use shepr_launch::local_server::{self, BuildCheck, SERVER_READY_TIMEOUT};
use shepr_launch::status::RuntimeStatus;

use crate::relay::{RemoteBridgeOutcome, answer_remote_bridge, forward_remote_bridge_stdio};

/// Marker that leads the classification record of a bridge that failed on the
/// remote host before relaying anything: the marker, then a
/// [`RemoteFailureClass`] token, alone on one stderr line, with the
/// diagnostic on the lines after it. The local bridge consumes the record into
/// the endpoint failure vocabulary.
pub(super) const BRIDGE_FAILURE_MARKER: &str = "shepr-remote-bridge-failure:";

/// A failure of the bridge on this host, led by its classification record so
/// the client can tell a host that needs repair from a transient failure. The
/// binary prints it on stderr as the command's error. The kind is kept for
/// this host's own diagnostics; the client reads only the record.
pub fn classified_bridge_failure(
    class: RemoteFailureClass,
    kind: io::ErrorKind,
    diagnostic: &dyn std::fmt::Display,
) -> io::Error {
    io::Error::new(
        kind,
        format!("{BRIDGE_FAILURE_MARKER}{}\n{diagnostic}", class.token()),
    )
}

/// Relays this process's stdio to the server socket until either side
/// closes or the idle watchdog fires. The outcome goes back to the binary: on
/// [`RemoteBridgeOutcome::IdleExpired`] it must end the
/// process promptly with status 1, without writing to stdout.
pub fn run_remote_client_bridge(paths: &shepr_paths::AppPaths) -> io::Result<RemoteBridgeOutcome> {
    let status = ensure_remote_server_running(paths)?;
    // A server of another build is answered here, never connected to: its
    // socket may not speak this build's client protocol at all, and the
    // client must still read a typed mismatch rather than an EOF it would
    // retry forever.
    if !status.build_id.is_this_build() {
        answer_remote_bridge(&shepr_protocol::preamble::preamble_for(
            &status.build_id.to_string(),
        ))?;
        return Ok(RemoteBridgeOutcome::Closed);
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

    forward_remote_bridge_stdio(stream.into_local_stream())
}

/// Starts the server when none is listening, through the launcher the local
/// TUI uses: a `shepr-server` beside this executable, started under the launch
/// lock and verified to be this build before the bridge relays anything. A
/// running server of another build is returned, not refused: the bridge then
/// answers the client with that build's preamble, so the client reports a
/// typed mismatch, which it classifies as needing attention, whatever socket
/// layout that server has. Every launch failure leads its error with a
/// [`BRIDGE_FAILURE_MARKER`] record of its
/// [`LaunchError::remote_failure_class`](shepr_launch::local_server::LaunchError::remote_failure_class),
/// which the client's SSH bridge turns into a typed endpoint failure (one the
/// host must repair needs attention, a timeout is retried), keeping the
/// launch's diagnostic as its message.
fn ensure_remote_server_running(paths: &shepr_paths::AppPaths) -> io::Result<RuntimeStatus> {
    local_server::ensure_running(paths, SERVER_READY_TIMEOUT, BuildCheck::AtClientHandshake)
        .map_err(|error| {
            classified_bridge_failure(error.remote_failure_class(), error.kind(), &error)
        })
}
