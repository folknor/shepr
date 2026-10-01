//! The seam between the server socket's listener and the TUI protocol, which
//! lives above this crate: the handler trait the server implements, the gate
//! it is installed through once panes are restored, the admission slot a
//! connection holds, and the refusal a TUI peer gets in its own protocol.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use shepr_platform::ipc::{LocalStream, LocalStreamDeadlineReader};
use shepr_protocol::preamble::{PreambleError, local_preamble, read_preamble};
use tracing::{debug, error};

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
            error!("client protocol gate was opened twice; keeping its first handler");
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

impl ConnectionSlot {
    pub(crate) fn try_acquire(active: &Arc<AtomicUsize>, cap: usize) -> Option<Self> {
        active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < cap).then_some(count + 1)
            })
            .ok()?;
        Some(Self {
            active: Arc::clone(active),
        })
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Release);
    }
}

/// Refuses a TUI connection for `reason`, in the client protocol. The client
/// writes its preamble and hello together before reading, so both are read
/// (within one short bound) before anything is written, and the refusal then
/// goes out in one write with this build's preamble ahead of it. A preamble of
/// another build gets this build's preamble alone, without its hello being
/// decoded, and so does a same-build client whose hello does not arrive: a
/// recognisable preamble is always answered with this build's identity. A
/// peer that sent no recognisable preamble is closed without an answer.
pub(super) fn refuse_client(mut stream: LocalStream, reason: shepr_protocol::HandshakeRefusal) {
    // clock-io-ok: bounds real reads of the refused peer's preamble and hello.
    let deadline = Instant::now() + BUSY_CLIENT_HANDSHAKE_TIMEOUT;
    let mut reader = LocalStreamDeadlineReader::new(&mut stream, deadline);
    let mut answer = local_preamble().to_vec();
    match read_preamble(&mut reader) {
        Ok(()) => {
            let hello = shepr_protocol::read_handshake_message::<_, shepr_protocol::ClientMessage>(
                &mut reader,
            );
            if let Err(error) = hello {
                debug!(%error, "refused client sent no readable hello");
            } else {
                let welcome = shepr_protocol::ServerMessage::EndpointWelcome(
                    shepr_protocol::endpoint::EndpointServerWelcome::refused(reason),
                );
                match shepr_protocol::encode_message(&welcome) {
                    Ok(framed) => answer.extend_from_slice(&framed),
                    Err(error) => error!(%error, "failed to encode client refusal"),
                }
            }
        }
        Err(PreambleError::DifferentBuild(_)) => {}
        Err(_) => return,
    }
    if let Err(error) = shepr_platform::write_client_stream(&stream, &answer, STREAM_WRITE_TIMEOUT)
    {
        debug!(%error, "failed to send client refusal");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_slots_cap_at_their_limit_and_release_on_drop() {
        let active = Arc::new(AtomicUsize::new(0));
        let cap = crate::limits::MAX_ACTIVE_CONNECTIONS;
        let mut slots = (0..cap)
            .map(|_| ConnectionSlot::try_acquire(&active, cap).expect("slot"))
            .collect::<Vec<_>>();
        assert!(ConnectionSlot::try_acquire(&active, cap).is_none());
        drop(slots.pop());
        let replacement = ConnectionSlot::try_acquire(&active, cap).expect("released slot");
        assert_eq!(active.load(Ordering::Acquire), cap);
        drop(replacement);
        drop(slots);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn independent_counters_have_independent_slots() {
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let slot = ConnectionSlot::try_acquire(&first, 1).expect("first");
        assert!(ConnectionSlot::try_acquire(&first, 1).is_none());
        let other = ConnectionSlot::try_acquire(&second, 1).expect("second");
        drop(slot);
        assert_eq!(first.load(Ordering::Acquire), 0);
        assert_eq!(second.load(Ordering::Acquire), 1);
        drop(other);
    }
}
