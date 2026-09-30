use std::io;
use std::sync::Arc;

use interprocess::local_socket::traits::Listener as _;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::server::client_transport::{self, ServerEvent};
use crate::server::clients::ClientRegistry;
use shepr_platform::ipc::LocalListener;

/// Whether an accept failure belongs to the one pending connection it tried to
/// take (accept(2): the peer went away, or an interrupted call), leaving the
/// listener and the rest of the backlog usable.
fn accept_failed_for_one_connection(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Interrupted
        || matches!(
            err.raw_os_error(),
            Some(libc::ECONNABORTED | libc::EPROTO | libc::EPERM)
        )
}

/// Whether an accept failure is the process or host running out of file
/// descriptors or memory. The server hosts every pane on this host, so such a
/// failure must not stop it: the backlog stays queued and is retried later.
pub(crate) fn accept_resources_exhausted(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
    )
}

/// Accepts one pending thin-client connection and starts its handshake reader.
///
/// `Ok` means the caller may accept again: a connection was taken (and maybe
/// refused), or accept failed for that one connection only. `WouldBlock` means
/// the listener is drained; its `AsyncFd` caller then clears readiness. Any
/// other error leaves the backlog queued: [`accept_resources_exhausted`] says
/// whether it is worth retrying later, otherwise the listener is unusable.
pub(crate) fn accept_client_connection(
    listener: &LocalListener,
    clients: &mut ClientRegistry,
    should_quit: &Arc<shepr_api::ServerStopSignal>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
) -> io::Result<()> {
    let stream = match listener.accept() {
        Ok(stream) => stream,
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Err(err),
        Err(err) if accept_failed_for_one_connection(&err) => {
            warn!(error = %err, "client connection failed before it was accepted");
            return Ok(());
        }
        Err(err) if accept_resources_exhausted(&err) => {
            warn!(error = %err, "client listener accept is out of resources; will retry");
            return Err(err);
        }
        Err(err) => {
            error!(error = %err, "client listener accept failed");
            return Err(err);
        }
    };

    // The socket file is owner-only; this is the second check, for a socket
    // whose mode was loosened or a path bound in a shared directory. Dropping
    // the stream closes it.
    match shepr_platform::ipc::peer_is_same_user(&stream) {
        Ok(true) => {}
        Ok(false) => {
            warn!("client connection from another user refused");
            return Ok(());
        }
        Err(err) => {
            warn!(error = %err, "client peer credentials unavailable; refused");
            return Ok(());
        }
    }

    let client_id = clients.allocate_client_id();

    let should_quit = Arc::clone(should_quit);
    let server_event_tx = server_event_tx.clone();
    // The listener is nonblocking for accept only, leaving this stream
    // blocking for the handshake thread's deadline reader.
    std::thread::spawn(move || {
        if let Err(err) = client_transport::handle_client_handshake(
            stream,
            client_id,
            &server_event_tx,
            &should_quit,
        ) {
            debug!(
                ?client_id,
                error = %err,
                "client handshake failed"
            );
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_failures_are_classified_so_descriptor_pressure_never_stops_the_server() {
        let os = io::Error::from_raw_os_error;
        for errno in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            assert!(accept_resources_exhausted(&os(errno)), "{errno}");
            assert!(!accept_failed_for_one_connection(&os(errno)), "{errno}");
        }
        for errno in [libc::ECONNABORTED, libc::EPROTO, libc::EPERM, libc::EINTR] {
            assert!(accept_failed_for_one_connection(&os(errno)), "{errno}");
            assert!(!accept_resources_exhausted(&os(errno)), "{errno}");
        }
        for errno in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK] {
            assert!(!accept_failed_for_one_connection(&os(errno)), "{errno}");
            assert!(!accept_resources_exhausted(&os(errno)), "{errno}");
        }
    }
}
