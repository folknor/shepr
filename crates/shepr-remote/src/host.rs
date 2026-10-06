//! Remote-host side of the SSH stdio bridge.

use std::io;

use shepr_launch::RemoteFailureClass;
use shepr_launch::local_server::{self, BuildCheck, LaunchError, SERVER_READY_TIMEOUT};
use shepr_launch::status::{RuntimeStatus, ServerPresence};

use crate::relay::{RemoteBridgeOutcome, answer_remote_bridge, forward_remote_bridge_stdio};

/// Marker that leads the classification record of a bridge that failed on the
/// remote host before relaying anything: the marker, then a
/// [`RemoteFailureClass`] token, alone on one stderr line, with the
/// diagnostic on the lines after it. The local bridge consumes the record into
/// the endpoint failure vocabulary.
pub(crate) const BRIDGE_FAILURE_MARKER: &str = "shepr-remote-bridge-failure:";

/// Whether a bridge may start the host's server. Every connection the client
/// makes by itself attaches only; starting is reserved for the operator's
/// explicit Connect or Restart on that machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeMode {
    /// Relay to a server that is already running. With none running, or one
    /// stopping, the bridge fails with a [`RemoteFailureClass::NoServer`] or
    /// [`RemoteFailureClass::Stopping`] record and starts nothing.
    Attach,
    /// Start the server when none is listening, then relay to it.
    Start,
}

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
pub fn run_remote_client_bridge(
    paths: &shepr_paths::AppPaths,
    mode: BridgeMode,
) -> io::Result<RemoteBridgeOutcome> {
    let status = match mode {
        BridgeMode::Attach => attached_server_status(paths)?,
        BridgeMode::Start => ensure_remote_server_running(paths)?,
    };
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
                    "failed to connect to remote shepr server socket {}: {err}",
                    socket_path.display()
                ),
            )
        })?;

    forward_remote_bridge_stdio(stream.into_local_stream())
}

/// The status of the server already running on this host, never starting
/// one. A starting server is relayed to like a running one: its handshake
/// refuses the client as starting, which the client retries. No server, a
/// stopping one and one that does not answer each fail with their own
/// classification record.
fn attached_server_status(paths: &shepr_paths::AppPaths) -> io::Result<RuntimeStatus> {
    let presence = local_server::server_presence(paths).map_err(|error| {
        let error = LaunchError::Io(error);
        classified_bridge_failure(error.remote_failure_class(), error.kind(), &error)
    })?;
    let socket = paths.server_address().socket().display();
    match presence {
        ServerPresence::Running(status) | ServerPresence::Starting(status) => Ok(status),
        ServerPresence::Gone => Err(classified_bridge_failure(
            RemoteFailureClass::NoServer,
            io::ErrorKind::NotFound,
            &format!("no shepr server is running at {socket}"),
        )),
        ServerPresence::Stopping(_) => Err(classified_bridge_failure(
            RemoteFailureClass::Stopping,
            io::ErrorKind::ConnectionAborted,
            &format!("the shepr server at {socket} is stopping"),
        )),
        ServerPresence::Unresponsive => Err(classified_bridge_failure(
            RemoteFailureClass::Repair,
            io::ErrorKind::TimedOut,
            &format!(
                "a shepr server is listening at {socket}, but it is not answering status requests"
            ),
        )),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An attach-only bridge on a host with no server reports that distinctly
    /// and leaves the host as it found it: no socket, and no launch lock or
    /// boot log, which any launch attempt creates first.
    #[test]
    fn an_attach_only_bridge_starts_no_server_and_reports_none() {
        let scratch = shepr_test_support::ScratchDir::new("bridge-attach-only");
        let paths = shepr_paths::AppPaths::rooted_at(&scratch, Some(&scratch), None)
            .expect("scratch roots fit a socket");
        let runtime_before = std::fs::read_dir(paths.runtime_dir()).map_or(0, Iterator::count);

        let error = run_remote_client_bridge(&paths, BridgeMode::Attach)
            .expect_err("no server runs, and an attach-only bridge starts none");

        let message = error.to_string();
        assert!(
            message.starts_with(&format!(
                "{BRIDGE_FAILURE_MARKER}{}\n",
                RemoteFailureClass::NoServer.token()
            )),
            "{message}"
        );
        assert!(
            !paths
                .server_address()
                .socket()
                .try_exists()
                .expect("stat the socket path")
        );
        let runtime_after = std::fs::read_dir(paths.runtime_dir()).map_or(0, Iterator::count);
        assert_eq!(runtime_after, runtime_before, "nothing was launched");
    }
}
