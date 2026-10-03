//! The one listener on the server socket, for JSON API requests and TUI
//! connections alike. A connection's first byte, peeked so its service reads
//! from byte zero, tells the two apart: `S` (the preamble magic) is the TUI
//! protocol, anything else a JSON request line.
//!
//! The accept thread does only bounded, non-panicking work, because a dead
//! listener leaves a server nobody can reach. A peer whose first byte is
//! already there goes straight to its kind's admission. A silent one takes a
//! slot of a separate, small classification admission and waits for its byte
//! on its own thread. Over that, it goes to the refuser, which waits a short
//! bound for the byte and then tries the kind's admission once, so saturated
//! classification never refuses a peer whose own kind has room. Refusals are
//! spoken in the kind's language and name the limit that was reached.

use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use shepr_platform::ipc::{
    Accepted, FirstByte, LocalListener, LocalStream, PeerAdmission, accept_peer, peek_first_byte,
};
use tracing::{debug, error, info, warn};

use super::client_protocol::{
    ClientGate, ClientProtocolHandler, ConnectionAdmission, ConnectionSlot, refuse_client,
};
use super::{handle_connection, reject_busy_connection, send_busy_refusal};
use crate::limits::{
    ACCEPT_BACKOFF_MAX, ACCEPT_BACKOFF_MIN, BUSY_REFUSAL_QUEUE, BUSY_REQUEST_ID_TIMEOUT,
    INITIAL_REQUEST_TIMEOUT, MAX_ACTIVE_CLIENT_CONNECTIONS, MAX_API_INGRESS_CONNECTIONS,
    MAX_APP_REQUESTS_IN_FLIGHT, MAX_UNCLASSIFIED_CONNECTIONS,
};

#[derive(Clone, Copy)]
enum Kind {
    Api,
    Client,
}

fn kind(byte: u8) -> Kind {
    if byte == shepr_protocol::preamble::PREAMBLE_MAGIC[0] {
        Kind::Client
    } else {
        Kind::Api
    }
}

/// What every connection thread needs to admit and serve a classified
/// connection: the API's request channel and stop signal, the TUI gate, one
/// admission counter per kind, and the API's second counter for requests
/// that wait on the app loop.
#[derive(Clone)]
struct Dispatch {
    api_tx: crate::ApiRequestSender,
    stop: Arc<crate::ServerStopSignal>,
    gate: ClientGate,
    api: ConnectionAdmission,
    api_app: ConnectionAdmission,
    client: ConnectionAdmission,
}

fn connection_admission(cap: usize) -> ConnectionAdmission {
    ConnectionAdmission::new(
        Arc::new(AtomicUsize::new(0)),
        shepr_protocol::Limit::new(shepr_protocol::LimitKind::ConnectionCount, cap),
    )
}

/// The outcome of a kind's admission: served with a slot, or refused in the
/// kind's own language.
enum Service {
    Api(ConnectionSlot),
    Client(Arc<dyn ClientProtocolHandler>, ConnectionSlot),
    RefuseApi,
    RefuseClient(shepr_protocol::HandshakeRefusal),
}

impl Dispatch {
    fn admit(&self, kind: Kind) -> Service {
        match kind {
            Kind::Api => self
                .api
                .try_acquire()
                .map_or(Service::RefuseApi, Service::Api),
            Kind::Client => {
                let Some(handler) = self.gate.handler() else {
                    return Service::RefuseClient(shepr_protocol::HandshakeRefusal::ServerStarting);
                };
                self.client.try_acquire().map_or_else(
                    |error| {
                        Service::RefuseClient(shepr_protocol::HandshakeRefusal::ConnectionLimit(
                            error,
                        ))
                    },
                    |slot| Service::Client(handler, slot),
                )
            }
        }
    }

    fn serve(&self, stream: LocalStream, accepted: Instant, service: Service) {
        match service {
            Service::Api(slot) => {
                if let Err(error) = handle_connection(
                    stream,
                    accepted + INITIAL_REQUEST_TIMEOUT,
                    slot,
                    &self.api_app,
                    &self.api_tx,
                    &self.stop,
                    &self.gate,
                ) {
                    debug!(%error, "api connection failed");
                }
            }
            Service::Client(handler, slot) => handler.serve(stream, slot, accepted),
            Service::RefuseApi => reject_busy_connection(stream, &self.api),
            Service::RefuseClient(reason) => refuse_client(stream, reason),
        }
    }

    fn classify_and_serve(&self, stream: LocalStream, accepted: Instant, slot: ConnectionSlot) {
        let first = peek_first_byte(&stream, accepted + INITIAL_REQUEST_TIMEOUT);
        drop(slot);
        if let Ok(FirstByte::Byte(byte)) = first {
            self.serve(stream, accepted, self.admit(kind(byte)));
        }
    }

    fn spawn(&self, stream: LocalStream, accepted: Instant, service: Service) -> io::Result<()> {
        let dispatch = self.clone();
        std::thread::Builder::new()
            .name("shepr-conn".into())
            .spawn(move || dispatch.serve(stream, accepted, service))
            .map(|_| ())
    }
}

struct Pending {
    stream: LocalStream,
    accepted: Instant,
    kind: Option<Kind>,
}

fn spawn_refuser(dispatch: Dispatch) -> Option<SyncSender<Pending>> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Pending>(BUSY_REFUSAL_QUEUE);
    match std::thread::Builder::new()
        .name("shepr-refuser".into())
        .spawn(move || {
            for pending in rx {
                let Pending {
                    stream,
                    accepted,
                    kind: known,
                } = pending;
                let kind = match known {
                    Some(kind) => kind,
                    None => {
                        // clock-io-ok: bounds a real first-byte read of an overflow peer.
                        match peek_first_byte(&stream, Instant::now() + BUSY_REQUEST_ID_TIMEOUT) {
                            Ok(FirstByte::Byte(byte)) => kind(byte),
                            _ => continue,
                        }
                    }
                };
                let service = dispatch.admit(kind);
                match service {
                    Service::RefuseApi | Service::RefuseClient(_) => {
                        dispatch.serve(stream, accepted, service);
                    }
                    _ => {
                        if let Err(error) = dispatch.spawn(stream, accepted, service) {
                            warn!(%error, "refuser could not spawn connection worker");
                        }
                    }
                }
            }
        }) {
        Ok(_) => Some(tx),
        Err(error) => {
            warn!(%error, "connection refuser unavailable");
            None
        }
    }
}

fn hand_off(
    refuser: Option<&SyncSender<Pending>>,
    pending: Pending,
    api_admission: &ConnectionAdmission,
) {
    let pending = match refuser {
        Some(refuser) => match refuser.try_send(pending) {
            Ok(()) => return,
            Err(TrySendError::Full(pending) | TrySendError::Disconnected(pending)) => pending,
        },
        None => pending,
    };
    if matches!(pending.kind, Some(Kind::Api)) {
        send_busy_refusal(pending.stream, "", api_admission);
    }
}

pub(super) fn start_listener(
    listener: LocalListener,
    running: Arc<AtomicBool>,
    api_tx: crate::ApiRequestSender,
    stop: Arc<crate::ServerStopSignal>,
    gate: ClientGate,
) -> io::Result<std::thread::JoinHandle<()>> {
    let dispatch = Dispatch {
        api_tx,
        stop,
        gate,
        api: connection_admission(MAX_API_INGRESS_CONNECTIONS),
        api_app: connection_admission(MAX_APP_REQUESTS_IN_FLIGHT),
        client: connection_admission(MAX_ACTIVE_CLIENT_CONNECTIONS),
    };
    let unclassified = connection_admission(MAX_UNCLASSIFIED_CONNECTIONS);
    start_listener_with_dispatch(listener, running, dispatch, unclassified)
}

fn start_listener_with_dispatch(
    listener: LocalListener,
    running: Arc<AtomicBool>,
    dispatch: Dispatch,
    unclassified: ConnectionAdmission,
) -> io::Result<std::thread::JoinHandle<()>> {
    let refuser = spawn_refuser(dispatch.clone());
    std::thread::Builder::new()
        .name("shepr-listener".into())
        .spawn(move || {
            let mut backoff = AcceptBackoff::default();
            loop {
                if !running.load(Ordering::Acquire) {
                    break;
                }
                let peer = match accept_peer(listener.as_raw_fd(), PeerAdmission::OwnerOrRoot) {
                    Accepted::Peer(peer) => peer,
                    Accepted::RetryNow => {
                        continue;
                    }
                    Accepted::Backoff(error) => {
                        backoff.failed("server accept failed", &error);
                        continue;
                    }
                    Accepted::Fatal(error) => {
                        error!(%error, "server listener cannot accept connections");
                        break;
                    }
                };
                let stream = LocalStream::from(peer.fd);
                // clock-io-ok: starts both kinds' real socket-read deadlines.
                let accepted = Instant::now();
                let result = match peek_first_byte(&stream, accepted) {
                    Ok(FirstByte::Closed) => continue,
                    Ok(FirstByte::Byte(byte)) => {
                        let kind = kind(byte);
                        let service = dispatch.admit(kind);
                        match service {
                            Service::RefuseApi | Service::RefuseClient(_) => {
                                hand_off(
                                    refuser.as_ref(),
                                    Pending {
                                        stream,
                                        accepted,
                                        kind: Some(kind),
                                    },
                                    &dispatch.api,
                                );
                                Ok(())
                            }
                            _ => dispatch.spawn(stream, accepted, service),
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                        if let Ok(slot) = unclassified.try_acquire() {
                            let dispatch = dispatch.clone();
                            std::thread::Builder::new()
                                .name("shepr-conn".into())
                                .spawn(move || {
                                    dispatch.classify_and_serve(stream, accepted, slot);
                                })
                                .map(|_| ())
                        } else {
                            hand_off(
                                refuser.as_ref(),
                                Pending {
                                    stream,
                                    accepted,
                                    kind: None,
                                },
                                &dispatch.api,
                            );
                            Ok(())
                        }
                    }
                    Err(error) => {
                        debug!(%error, "could not classify connection");
                        continue;
                    }
                };
                match result {
                    Ok(()) => backoff.recovered(),
                    Err(error) => backoff.failed("connection worker spawn failed", &error),
                }
            }
            debug!("server listener exiting");
        })
}

#[derive(Default)]
struct AcceptBackoff {
    delay: Option<Duration>,
    failures: u64,
}

impl AcceptBackoff {
    fn failed(&mut self, what: &'static str, error: &io::Error) {
        self.failures = self.failures.saturating_add(1);
        if self.failures == 1 {
            error!(%error, "{what}; retrying");
        } else {
            debug!(%error, failures = self.failures, "{what}; retrying");
        }
        let delay = self
            .delay
            .map_or(ACCEPT_BACKOFF_MIN, |delay| delay.saturating_mul(2))
            .min(ACCEPT_BACKOFF_MAX);
        self.delay = Some(delay);
        std::thread::sleep(delay);
    }
    fn recovered(&mut self) {
        if self.failures > 0 {
            info!(failures = self.failures, "server listener recovered");
        }
        self.delay = None;
        self.failures = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_protocol::endpoint::{EndpointClientHello, EndpointServerWelcome};
    use shepr_protocol::preamble::{local_preamble, read_preamble};
    use shepr_protocol::{ClientMessage, ServerMessage};
    use std::io::{BufRead, BufReader, Read, Write};

    fn dispatch() -> Dispatch {
        let (api_tx, _rx) = tokio::sync::mpsc::channel(1);
        Dispatch {
            api_tx,
            stop: Arc::default(),
            gate: ClientGate::default(),
            api: connection_admission(MAX_API_INGRESS_CONNECTIONS),
            api_app: connection_admission(MAX_APP_REQUESTS_IN_FLIGHT),
            client: connection_admission(MAX_ACTIVE_CLIENT_CONNECTIONS),
        }
    }

    struct RecordingHandler(std::sync::mpsc::Sender<()>);
    impl ClientProtocolHandler for RecordingHandler {
        fn serve(&self, mut stream: LocalStream, slot: ConnectionSlot, _accepted: Instant) {
            let _slot = slot;
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("read deadline");
            read_preamble(&mut stream).expect("unconsumed preamble");
            let hello: ClientMessage = shepr_protocol::read_message(&mut stream).expect("hello");
            assert!(matches!(hello, ClientMessage::EndpointHello(_)));
            stream.write_all(&local_preamble()).expect("identity");
            shepr_protocol::write_message(
                &mut stream,
                &ServerMessage::EndpointWelcome(EndpointServerWelcome::Accepted),
            )
            .expect("welcome");
            self.0.send(()).expect("record connection");
            let mut rest = Vec::new();
            drop(stream.read_to_end(&mut rest));
        }
    }

    fn open_gate(dispatch: &Dispatch) -> std::sync::mpsc::Receiver<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        dispatch.gate.open(Arc::new(RecordingHandler(tx)));
        rx
    }

    fn hello(stream: &mut LocalStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read deadline");
        let mut bytes = local_preamble().to_vec();
        bytes.extend(
            shepr_protocol::encode_message(&ClientMessage::EndpointHello(EndpointClientHello {
                geometry: shepr_protocol::TerminalGeometry::new(80, 24, 8, 16, true),
                mouse_capture: true,
                surface_active: true,
            }))
            .expect("encode hello"),
        );
        stream.write_all(&bytes).expect("hello");
    }

    fn welcome(stream: &mut LocalStream) -> EndpointServerWelcome {
        read_preamble(stream).expect("server identity");
        let message: ServerMessage = shepr_protocol::read_message(stream).expect("welcome");
        let ServerMessage::EndpointWelcome(welcome) = message else {
            panic!("expected welcome");
        };
        welcome
    }

    fn ping(stream: &mut LocalStream) -> serde_json::Value {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read deadline");
        stream
            .write_all(b"{\"id\":\"ping\",\"method\":\"ping\",\"params\":{}}\n")
            .expect("ping");
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .expect("response");
        serde_json::from_str(&line).expect("json")
    }

    fn hold(admission: &ConnectionAdmission, count: usize) -> Vec<ConnectionSlot> {
        (0..count)
            .map(|_| admission.try_acquire().expect("slot"))
            .collect()
    }

    fn server(
        dispatch: Dispatch,
        unclassified: Arc<AtomicUsize>,
    ) -> (shepr_test_support::ScratchDir, super::super::ServerHandle) {
        let scratch = shepr_test_support::ScratchDir::new("merged-listener");
        let path = scratch.join("server.sock");
        let (listener, lock, identity) =
            shepr_platform::ipc::bind_private_socket(&path).expect("bind");
        let running = Arc::new(AtomicBool::new(true));
        let gate = dispatch.gate.clone();
        let thread = start_listener_with_dispatch(
            listener,
            Arc::clone(&running),
            dispatch,
            ConnectionAdmission::new(
                unclassified,
                shepr_protocol::Limit::new(
                    shepr_protocol::LimitKind::ConnectionCount,
                    MAX_UNCLASSIFIED_CONNECTIONS,
                ),
            ),
        )
        .expect("listener");
        let handle = super::super::ServerHandle {
            thread: Some(thread),
            path,
            identity,
            running,
            gate,
            _startup_lock: lock,
        };
        (scratch, handle)
    }

    fn connect(handle: &super::super::ServerHandle) -> LocalStream {
        shepr_platform::ipc::connect_local_stream(&handle.path).expect("connect")
    }

    #[test]
    fn a_preamble_connection_reaches_the_client_handler_and_a_json_one_the_api() {
        let dispatch = dispatch();
        let received = open_gate(&dispatch);
        let (_scratch, handle) = server(dispatch, Arc::default());
        let mut client = connect(&handle);
        hello(&mut client);
        assert_eq!(welcome(&mut client), EndpointServerWelcome::Accepted);
        received
            .recv_timeout(Duration::from_secs(1))
            .expect("handler served client");
        assert_eq!(ping(&mut connect(&handle))["result"]["type"], "pong");
    }

    #[test]
    fn a_connection_that_sends_nothing_releases_its_classification_slot() {
        let dispatch = dispatch();
        let active: Arc<AtomicUsize> = Arc::default();
        let admission = ConnectionAdmission::new(
            Arc::clone(&active),
            shepr_protocol::Limit::new(shepr_protocol::LimitKind::ConnectionCount, 1),
        );
        let slot = admission.try_acquire().expect("slot");
        let (_peer, stream) = LocalStream::pair().expect("pair");
        dispatch.classify_and_serve(stream, Instant::now() - INITIAL_REQUEST_TIMEOUT, slot);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn client_and_api_connections_are_admitted_separately() {
        let dispatch = dispatch();
        let received = open_gate(&dispatch);
        let api_slots = hold(&dispatch.api, MAX_API_INGRESS_CONNECTIONS);
        let (_scratch, handle) = server(dispatch.clone(), Arc::default());
        let mut client = connect(&handle);
        hello(&mut client);
        assert_eq!(welcome(&mut client), EndpointServerWelcome::Accepted);
        received
            .recv_timeout(Duration::from_secs(1))
            .expect("client admission independent");
        drop(client);
        drop(api_slots);
        wait_admission_count(&dispatch.client, 0);
        let client_slots = hold(&dispatch.client, MAX_ACTIVE_CLIENT_CONNECTIONS);
        assert_eq!(ping(&mut connect(&handle))["result"]["type"], "pong");
        drop(client_slots);
    }

    fn wait_count(active: &AtomicUsize, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while active.load(Ordering::Acquire) != count {
            assert!(
                Instant::now() < deadline,
                "counter did not reach {count}: {}",
                active.load(Ordering::Acquire)
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_admission_count(admission: &ConnectionAdmission, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while admission.active_count() != count {
            assert!(
                Instant::now() < deadline,
                "counter did not reach {count}: {}",
                admission.active_count()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn classification_saturation_still_serves_a_peer_whose_kind_has_room() {
        let dispatch = dispatch();
        let received = open_gate(&dispatch);
        let active: Arc<AtomicUsize> = Arc::default();
        let (_scratch, handle) = server(dispatch, Arc::clone(&active));
        let silent = (0..MAX_UNCLASSIFIED_CONNECTIONS)
            .map(|_| connect(&handle))
            .collect::<Vec<_>>();
        wait_count(&active, MAX_UNCLASSIFIED_CONNECTIONS);
        // Stay silent past the accept thread's one immediate peek, so each
        // overflow peer is handed to the refuser unclassified, then speak.
        let mut api = connect(&handle);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(ping(&mut api)["result"]["type"], "pong");
        let mut client = connect(&handle);
        std::thread::sleep(Duration::from_millis(50));
        hello(&mut client);
        assert_eq!(welcome(&mut client), EndpointServerWelcome::Accepted);
        received
            .recv_timeout(Duration::from_secs(1))
            .expect("client served through overflow");
        drop(silent);
        wait_count(&active, 0);
    }

    #[test]
    fn a_full_refusal_queue_refuses_at_once_without_reading() {
        let (mut peer, stream) = LocalStream::pair().expect("pair");
        let (tx, _rx) = std::sync::mpsc::sync_channel(0);
        hand_off(
            Some(&tx),
            Pending {
                stream,
                accepted: Instant::now(),
                kind: Some(Kind::Api),
            },
            &connection_admission(MAX_API_INGRESS_CONNECTIONS),
        );
        let mut line = String::new();
        BufReader::new(&mut peer)
            .read_line(&mut line)
            .expect("refusal");
        let answer: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(answer["id"], "");
        assert_eq!(answer["error"]["code"], "endpoint_busy");
    }

    #[test]
    fn the_busy_refuser_thread_echoes_the_request_id() {
        let dispatch = dispatch();
        let _slots = hold(&dispatch.api, MAX_API_INGRESS_CONNECTIONS);
        let api_admission = dispatch.api.clone();
        let tx = spawn_refuser(dispatch).expect("refuser");
        let (mut peer, stream) = LocalStream::pair().expect("pair");
        hand_off(
            Some(&tx),
            Pending {
                stream,
                accepted: Instant::now(),
                kind: Some(Kind::Api),
            },
            &api_admission,
        );
        let answer = ping(&mut peer);
        assert_eq!(answer["id"], "ping");
        assert_eq!(answer["error"]["code"], "endpoint_busy");
    }

    #[test]
    fn the_refuser_classifies_an_unclassified_peer_and_serves_its_kind() {
        let dispatch = dispatch();
        let tx = spawn_refuser(dispatch).expect("refuser");
        let (mut peer, stream) = LocalStream::pair().expect("pair");
        hand_off(
            Some(&tx),
            Pending {
                stream,
                accepted: Instant::now(),
                kind: None,
            },
            &connection_admission(MAX_API_INGRESS_CONNECTIONS),
        );
        assert_eq!(ping(&mut peer)["result"]["type"], "pong");
    }

    #[test]
    fn an_unclassified_connection_with_a_full_refusal_queue_is_closed() {
        let (mut peer, stream) = LocalStream::pair().expect("pair");
        let (tx, _rx) = std::sync::mpsc::sync_channel(0);
        hand_off(
            Some(&tx),
            Pending {
                stream,
                accepted: Instant::now(),
                kind: None,
            },
            &connection_admission(MAX_API_INGRESS_CONNECTIONS),
        );
        let mut bytes = Vec::new();
        peer.read_to_end(&mut bytes).expect("closed");
        assert!(bytes.is_empty());
    }

    #[test]
    fn an_excess_client_connection_is_refused_after_its_hello() {
        let (mut peer, stream) = LocalStream::pair().expect("pair");
        let worker = std::thread::spawn(move || {
            refuse_client(
                stream,
                shepr_protocol::HandshakeRefusal::ConnectionLimit(
                    shepr_protocol::LimitExceeded::new(
                        shepr_protocol::Limit::new(shepr_protocol::LimitKind::ConnectionCount, 64),
                        65,
                    ),
                ),
            );
        });
        hello(&mut peer);
        assert_eq!(
            welcome(&mut peer),
            EndpointServerWelcome::Refused(shepr_protocol::HandshakeRefusal::ConnectionLimit(
                shepr_protocol::LimitExceeded::new(
                    shepr_protocol::Limit::new(shepr_protocol::LimitKind::ConnectionCount, 64,),
                    65,
                ),
            ))
        );
        worker.join().expect("refuser");
    }

    #[test]
    fn a_client_connection_before_the_gate_opens_is_refused_as_starting() {
        let (_scratch, handle) = server(dispatch(), Arc::default());
        let mut peer = connect(&handle);
        hello(&mut peer);
        assert_eq!(
            welcome(&mut peer),
            EndpointServerWelcome::Refused(shepr_protocol::HandshakeRefusal::ServerStarting)
        );
    }

    #[test]
    fn a_foreign_client_is_answered_with_this_build_when_refused() {
        for full in [false, true] {
            let dispatch = dispatch();
            let _received = full.then(|| open_gate(&dispatch));
            let _slots = if full {
                hold(&dispatch.client, MAX_ACTIVE_CLIENT_CONNECTIONS)
            } else {
                Vec::new()
            };
            let (_scratch, handle) = server(dispatch, Arc::default());
            let mut peer = connect(&handle);
            peer.set_read_timeout(Some(Duration::from_secs(2)))
                .expect("deadline");
            let mut foreign = local_preamble();
            let last = foreign.last_mut().expect("identity byte");
            *last = if *last == b'0' { b'1' } else { b'0' };
            peer.write_all(&foreign).expect("foreign preamble");
            read_preamble(&mut peer).expect("this build's identity");
            let mut rest = Vec::new();
            peer.read_to_end(&mut rest).expect("closed after identity");
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn silent_excess_client_cannot_hold_the_refuser() {
        let (mut peer, stream) = LocalStream::pair().expect("pair");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("read bound");
        let worker = std::thread::spawn(move || {
            refuse_client(
                stream,
                shepr_protocol::HandshakeRefusal::ConnectionLimit(
                    shepr_protocol::LimitExceeded::new(
                        shepr_protocol::Limit::new(shepr_protocol::LimitKind::ConnectionCount, 64),
                        65,
                    ),
                ),
            );
        });
        let mut bytes = Vec::new();
        peer.read_to_end(&mut bytes)
            .expect("bounded silent connection");
        assert!(bytes.is_empty());
        worker.join().expect("refuser");
    }

    #[test]
    fn ping_reports_starting_until_the_gate_opens() {
        let dispatch = dispatch();
        let (_scratch, handle) = server(dispatch.clone(), Arc::default());
        assert_eq!(ping(&mut connect(&handle))["result"]["starting"], true);
        let _received = open_gate(&dispatch);
        assert_eq!(ping(&mut connect(&handle))["result"]["starting"], false);
    }

    #[test]
    fn a_json_peer_gets_one_request_deadline_from_accept() {
        let dispatch = dispatch();
        let (mut peer, stream) = LocalStream::pair().expect("pair");
        peer.write_all(b"{").expect("late first byte");
        let ingress = dispatch.api.try_acquire().expect("slot");
        let error = handle_connection(
            stream,
            Instant::now(),
            ingress,
            &dispatch.api_app,
            &dispatch.api_tx,
            &dispatch.stop,
            &dispatch.gate,
        )
        .expect_err("no renewed deadline after classification");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
