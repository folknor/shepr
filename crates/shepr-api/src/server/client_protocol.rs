//! The seam between the server socket's listener and the TUI protocol, which
//! lives above this crate: the handler trait the server implements, the gate
//! it is installed through once panes are restored, the admission slot a
//! connection holds, and the refusal a TUI peer gets in its own protocol.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use shepr_platform::ipc::{LocalStream, LocalStreamDeadlineReader};
use shepr_protocol::preamble::{PreambleError, local_preamble, read_preamble};
use tracing::debug;

use crate::limits::{BUSY_CLIENT_HANDSHAKE_TIMEOUT, STREAM_WRITE_TIMEOUT};

/// The server's TUI protocol, as the listener sees it.
pub trait ClientProtocolHandler: Send + Sync + 'static {
    /// One TUI connection, on its own thread, from byte zero. The slot is the
    /// connection's admission; hold it until the connection ends. `accepted`
    /// is when the listener accepted the stream: the handshake deadline
    /// counts from it, so time spent classifying is part of the handshake
    /// budget.
    fn serve(&self, stream: LocalStream, slot: ConnectionSlot, accepted: Instant);
}

/// Whether the server accepts TUI connections yet, and who serves them. It is
/// closed from bind until the server has restored its panes and installed its
/// handler; meanwhile `ping` answers `starting` and a TUI connection is
/// refused as `ServerStarting`. Clones share one slot.
#[derive(Clone, Default)]
pub struct ClientGate {
    handler: Arc<OnceLock<Arc<dyn ClientProtocolHandler>>>,
}

impl ClientGate {
    /// Installs the handler. A second open logs an error and keeps the first.
    pub fn open(&self, handler: Arc<dyn ClientProtocolHandler>) {
        if self.handler.set(handler).is_err() {
            shepr_platform::structured_log!(
                ERROR,
                event = api.protocol_open,
                outcome = "already_open",
                "client protocol gate was opened twice; keeping its first handler"
            );
        }
    }

    pub fn is_open(&self) -> bool {
        self.handler.get().is_some()
    }

    pub(super) fn handler(&self) -> Option<Arc<dyn ClientProtocolHandler>> {
        self.handler.get().cloned()
    }
}

/// A connection's admission, released when the serving thread finishes.
pub struct ConnectionSlot {
    active: Arc<AtomicUsize>,
}

/// One connection class's shared counter and named admission limit.
#[derive(Clone)]
pub(crate) struct ConnectionAdmission {
    active: Arc<AtomicUsize>,
    limit: shepr_protocol::Limit,
}

impl ConnectionAdmission {
    pub(crate) fn new(active: Arc<AtomicUsize>, limit: shepr_protocol::Limit) -> Self {
        Self { active, limit }
    }

    pub(crate) fn try_acquire(&self) -> Result<ConnectionSlot, shepr_protocol::LimitExceeded> {
        match self
            .active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < self.limit.max()).then_some(count + 1)
            }) {
            Ok(_) => Ok(ConnectionSlot {
                active: Arc::clone(&self.active),
            }),
            Err(count) => Err(shepr_protocol::LimitExceeded::new(
                self.limit,
                count.saturating_add(1),
            )),
        }
    }

    pub(crate) fn limit(&self) -> shepr_protocol::Limit {
        self.limit
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Release);
    }
}

/// What a TUI listener learned from one bounded client opening.
#[derive(Debug)]
pub enum ClientHandshakeOutcome {
    /// The same-build preamble was answered and the client's first message decoded.
    Hello(shepr_protocol::ClientMessage),
    /// Another build id was answered with this process's preamble;
    /// the foreign hello was not decoded.
    Foreign(shepr_protocol::preamble::PeerBuild),
    /// No usable hello arrived. A recognized preamble, if one arrived, was
    /// already answered with this process's preamble.
    Silent(ClientHandshakeSilence),
    /// The peer did not send the shepr preamble magic and received no answer.
    NotShepr,
}

/// The read failure that left a recognized client opening without a hello.
#[derive(Debug)]
pub enum ClientHandshakeSilence {
    /// The preamble ended early or failed to read.
    Preamble(PreambleError),
    /// The first framed message was absent or could not be decoded.
    Hello(shepr_protocol::FramingError),
}

/// Reads and answers the client-protocol preamble by the listener's one rule.
/// Every recognized build gets this process's preamble; only a same-build
/// peer's hello is decoded. The same deadline covers both reads. A failed
/// preamble write is returned as an I/O error.
pub fn read_client_handshake(
    stream: &mut LocalStream,
    deadline: Instant,
) -> std::io::Result<ClientHandshakeOutcome> {
    let build = {
        let mut reader = LocalStreamDeadlineReader::new(stream, deadline);
        match read_preamble(&mut reader) {
            Ok(()) => None,
            Err(PreambleError::DifferentBuild(peer)) => Some(peer),
            Err(PreambleError::NotShepr) => return Ok(ClientHandshakeOutcome::NotShepr),
            Err(error) => {
                return Ok(ClientHandshakeOutcome::Silent(
                    ClientHandshakeSilence::Preamble(error),
                ));
            }
        }
    };

    shepr_platform::write_client_stream(stream, &local_preamble(), STREAM_WRITE_TIMEOUT)?;

    if let Some(peer) = build {
        return Ok(ClientHandshakeOutcome::Foreign(peer));
    }

    let hello = {
        let mut reader = LocalStreamDeadlineReader::new(stream, deadline);
        shepr_protocol::read_handshake_message::<_, shepr_protocol::ClientMessage>(&mut reader)
    };
    match hello {
        Ok(message) => Ok(ClientHandshakeOutcome::Hello(message)),
        Err(error) => Ok(ClientHandshakeOutcome::Silent(
            ClientHandshakeSilence::Hello(error),
        )),
    }
}

/// Refuses a TUI connection for `reason`. The shared handshake reader answers
/// recognized preambles before this refusal is encoded, and foreign builds
/// never have their hello decoded.
pub(super) fn refuse_client(mut stream: LocalStream, reason: shepr_protocol::HandshakeRefusal) {
    // clock-io-ok: bounds real reads of the refused peer's preamble and hello.
    let deadline = Instant::now() + BUSY_CLIENT_HANDSHAKE_TIMEOUT;
    match read_client_handshake(&mut stream, deadline) {
        Ok(ClientHandshakeOutcome::Hello(_)) => {
            let welcome = shepr_protocol::ServerMessage::EndpointWelcome(
                shepr_protocol::endpoint::EndpointServerWelcome::refused(reason),
            );
            match shepr_protocol::encode_message(&welcome) {
                Ok(framed) => {
                    if let Err(error) =
                        shepr_platform::write_client_stream(&stream, &framed, STREAM_WRITE_TIMEOUT)
                    {
                        debug!(%error, "failed to send client refusal");
                    }
                }
                Err(error) => {
                    shepr_platform::structured_log!(ERROR, event = api.refusal_encode, outcome = "error", %error, "failed to encode client refusal");
                }
            }
        }
        Ok(ClientHandshakeOutcome::Foreign(peer)) => {
            debug!(build_id = %peer.build_id, "refusing a client from another build");
        }
        Ok(ClientHandshakeOutcome::Silent(error)) => {
            debug!(?error, "refused client sent no readable hello");
        }
        Ok(ClientHandshakeOutcome::NotShepr) => {}
        Err(error) => debug!(%error, "failed to send client build-identity preamble"),
    }
}

#[cfg(test)]
impl ConnectionAdmission {
    pub(crate) fn active_count(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_slots_cap_at_their_limit_and_release_on_drop() {
        let active = Arc::new(AtomicUsize::new(0));
        let cap = crate::limits::MAX_API_INGRESS_CONNECTIONS;
        let admission = ConnectionAdmission::new(
            Arc::clone(&active),
            shepr_protocol::Limit::new(shepr_protocol::LimitKind::ConnectionCount, cap),
        );
        let mut slots = (0..cap)
            .map(|_| admission.try_acquire().expect("slot"))
            .collect::<Vec<_>>();
        assert!(admission.try_acquire().is_err());
        drop(slots.pop());
        let replacement = admission.try_acquire().expect("released slot");
        assert_eq!(active.load(Ordering::Acquire), cap);
        drop(replacement);
        drop(slots);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn independent_counters_have_independent_slots() {
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let first = ConnectionAdmission::new(
            first,
            shepr_protocol::Limit::new(shepr_protocol::LimitKind::ConnectionCount, 1),
        );
        let second = ConnectionAdmission::new(
            second,
            shepr_protocol::Limit::new(shepr_protocol::LimitKind::ConnectionCount, 1),
        );
        let slot = first.try_acquire().expect("first");
        assert!(first.try_acquire().is_err());
        let other = second.try_acquire().expect("second");
        drop(slot);
        assert_eq!(first.active_count(), 0);
        assert_eq!(second.active_count(), 1);
        drop(other);
    }
}
