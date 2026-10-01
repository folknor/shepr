use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};

use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::limits::{
    CLIENT_HANDSHAKE_REFUSAL_QUEUE_CAPACITY, CLIENT_LIMIT_HANDSHAKE_TIMEOUT,
    CLIENT_WRITE_STALL_TIMEOUT, MAX_ACTIVE_CLIENT_CONNECTIONS,
};
use crate::server::client_transport::{self, ServerEvent};
use crate::server::clients::ClientRegistry;
use shepr_platform::ipc::{LocalListener, LocalStream};

struct ConnectionAdmission {
    active: Arc<AtomicUsize>,
}

impl ConnectionAdmission {
    fn try_acquire(active: &Arc<AtomicUsize>) -> Option<Self> {
        active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_ACTIVE_CLIENT_CONNECTIONS).then_some(count + 1)
            })
            .ok()?;
        Some(Self {
            active: Arc::clone(active),
        })
    }
}

impl Drop for ConnectionAdmission {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Release);
    }
}

fn busy_client_refuser() -> Option<&'static SyncSender<LocalStream>> {
    static REFUSER: OnceLock<Option<SyncSender<LocalStream>>> = OnceLock::new();
    REFUSER.get_or_init(spawn_busy_client_refuser).as_ref()
}

fn spawn_busy_client_refuser() -> Option<SyncSender<LocalStream>> {
    let (sender, receiver) =
        std::sync::mpsc::sync_channel::<LocalStream>(CLIENT_HANDSHAKE_REFUSAL_QUEUE_CAPACITY);
    let spawned = std::thread::Builder::new()
        .name("shepr-client-refuser".into())
        .spawn(move || {
            for stream in receiver {
                reject_busy_client(stream);
            }
        });
    match spawned {
        Ok(_) => Some(sender),
        Err(err) => {
            warn!(error = %err, "client connection refuser thread unavailable; excess connections will close");
            None
        }
    }
}

fn hand_off_busy_connection(stream: LocalStream) {
    let Some(refuser) = busy_client_refuser() else {
        debug!("client connection limit refusal worker unavailable; closing excess connection");
        return;
    };
    match refuser.try_send(stream) {
        Ok(()) => {}
        Err(TrySendError::Full(_stream)) => {
            debug!("client connection limit refusal queue full; closing excess connection");
        }
        Err(TrySendError::Disconnected(_stream)) => {
            warn!("client connection limit refusal worker stopped; closing excess connection");
        }
    }
}

fn reject_busy_client(mut stream: LocalStream) {
    // Send the identity first: clients wait for it before sending their hello.
    if shepr_platform::write_client_stream(
        &stream,
        &shepr_protocol::preamble::local_preamble(),
        CLIENT_WRITE_STALL_TIMEOUT,
    )
    .is_err()
    {
        return;
    }
    let mut reader = shepr_platform::ipc::LocalStreamDeadlineReader::new(
        &mut stream,
        // clock-io-ok: bounds real reads from an excess client.
        std::time::Instant::now() + CLIENT_LIMIT_HANDSHAKE_TIMEOUT,
    );
    if shepr_protocol::preamble::read_preamble(&mut reader).is_err()
        || shepr_protocol::read_handshake_message::<_, shepr_protocol::ClientMessage>(&mut reader)
            .is_err()
    {
        return;
    }
    let limit = u32::try_from(MAX_ACTIVE_CLIENT_CONNECTIONS).unwrap_or(u32::MAX);
    let welcome = shepr_protocol::ServerMessage::EndpointWelcome(
        shepr_protocol::endpoint::EndpointServerWelcome::refused(
            shepr_protocol::HandshakeRefusal::ConnectionLimit(limit),
        ),
    );
    let framed = match shepr_protocol::encode_message(&welcome) {
        Ok(framed) => framed,
        Err(err) => {
            error!(error = %err, "failed to encode client connection limit refusal");
            return;
        }
    };
    if let Err(err) =
        shepr_platform::write_client_stream(&stream, &framed, CLIENT_WRITE_STALL_TIMEOUT)
    {
        debug!(error = %err, "failed to send client connection limit refusal");
    }
}

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

/// Accepts one pending thin-client connection and starts its bounded transport worker.
///
/// `Ok` means the caller may accept again: a connection was taken (and maybe
/// refused), or accept failed for that one connection only. `WouldBlock` means
/// the listener is drained; its `AsyncFd` caller then clears readiness. Any
/// other error leaves the backlog queued: [`accept_resources_exhausted`] says
/// whether it is worth retrying later, otherwise the listener is unusable.
pub(crate) fn accept_client_connection(
    listener: &LocalListener,
    clients: &mut ClientRegistry,
    active_connections: &Arc<AtomicUsize>,
    should_quit: &Arc<shepr_api::ServerStopSignal>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
) -> io::Result<()> {
    let stream = match listener.accept() {
        Ok((stream, _)) => stream,
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

    let Some(admission) = ConnectionAdmission::try_acquire(active_connections) else {
        hand_off_busy_connection(stream);
        return Ok(());
    };

    let client_id = clients.allocate_client_id();

    let should_quit = Arc::clone(should_quit);
    let server_event_tx = server_event_tx.clone();
    // The listener is nonblocking for accept only, leaving this stream
    // blocking for the handshake thread's deadline reader.
    let spawned = std::thread::Builder::new()
        .name("shepr-client-transport".into())
        .spawn(move || {
            // The handshake handler becomes the connection reader, so retain
            // the admission slot until the client disconnects.
            let _admission = admission;
            if let Err(err) = client_transport::handle_client_handshake(
                stream,
                client_id,
                &server_event_tx,
                &should_quit,
            ) {
                debug!(?client_id, error = %err, "client transport failed");
            }
        });
    if let Err(err) = spawned {
        warn!(?client_id, error = %err, "failed to start client transport worker");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_connection_admission_caps_handshake_workers_and_releases_slots() {
        let active = Arc::new(AtomicUsize::new(0));
        let mut admissions = (0..MAX_ACTIVE_CLIENT_CONNECTIONS)
            .map(|_| ConnectionAdmission::try_acquire(&active).expect("available slot"))
            .collect::<Vec<_>>();

        assert_eq!(
            active.load(Ordering::Acquire),
            MAX_ACTIVE_CLIENT_CONNECTIONS
        );
        assert!(ConnectionAdmission::try_acquire(&active).is_none());

        drop(admissions.pop());
        let replacement = ConnectionAdmission::try_acquire(&active).expect("released slot");
        assert_eq!(
            active.load(Ordering::Acquire),
            MAX_ACTIVE_CLIENT_CONNECTIONS
        );
        drop(replacement);
        drop(admissions);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn independent_servers_have_independent_admission_slots() {
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let slots = (0..MAX_ACTIVE_CLIENT_CONNECTIONS)
            .map(|_| ConnectionAdmission::try_acquire(&first).expect("first server slot"))
            .collect::<Vec<_>>();
        assert!(ConnectionAdmission::try_acquire(&first).is_none());
        let other = ConnectionAdmission::try_acquire(&second).expect("second server slot");
        assert_eq!(second.load(Ordering::Acquire), 1);
        drop(slots);
        assert_eq!(first.load(Ordering::Acquire), 0);
        assert_eq!(second.load(Ordering::Acquire), 1);
        drop(other);
        assert_eq!(second.load(Ordering::Acquire), 0);
    }

    #[test]
    fn limit_refusal_waits_for_hello_after_publishing_its_preamble() {
        let (mut client, server) = LocalStream::pair().expect("socket pair");
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("client deadline");
        let worker = std::thread::spawn(move || reject_busy_client(server));
        shepr_protocol::preamble::read_preamble(&mut client).expect("server identity");
        // A client may wait for the server's identity before sending its hello.
        std::thread::sleep(std::time::Duration::from_millis(20));
        shepr_protocol::preamble::write_preamble(&mut client).expect("client identity");
        let hello = shepr_protocol::ClientMessage::EndpointHello(
            shepr_protocol::endpoint::EndpointClientHello {
                geometry: shepr_protocol::TerminalGeometry::new(80, 24, 8, 16, true),
                mouse_capture: true,
                surface_active: true,
            },
        );
        shepr_protocol::write_message(&mut client, &hello).expect("hello survives refusal");
        let welcome: shepr_protocol::ServerMessage =
            shepr_protocol::read_message(&mut client).expect("reliable refusal");
        assert_eq!(
            welcome,
            shepr_protocol::ServerMessage::EndpointWelcome(
                shepr_protocol::endpoint::EndpointServerWelcome::refused(
                    shepr_protocol::HandshakeRefusal::ConnectionLimit(
                        u32::try_from(MAX_ACTIVE_CLIENT_CONNECTIONS).expect("connection limit"),
                    ),
                ),
            )
        );
        worker.join().expect("refuser worker");
    }

    #[test]
    fn silent_excess_client_cannot_hold_the_refuser() {
        let (mut client, server) = LocalStream::pair().expect("socket pair");
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("client deadline");
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            reject_busy_client(server);
            done_tx.send(()).expect("report completion");
        });
        shepr_protocol::preamble::read_preamble(&mut client).expect("server identity");
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("bounded hello read finishes for a silent peer");
        worker.join().expect("refuser worker");
    }

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
