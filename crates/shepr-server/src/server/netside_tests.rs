use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use shepr_api as api;
use shepr_client::endpoint::{
    ClientEndpointId, ClientEndpointStatus, EndpointRegistry, EndpointTransport,
    PendingEndpointActivation, ProfileId, SavedSshEndpoint, SurfaceActivationProgress,
};
use shepr_protocol::ServerMessage;

use crate::server::ClientId;
use crate::server::client_transport::{RenderLaneReceiver, ServerEvent};
use crate::server::headless::tests as headless_tests;
use crate::test_support::ValidatedConfigFixture as _;

/// Maximum time an expected server control message may take in this test.
const SERVER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

fn recv_server_message(
    receiver: &std::sync::mpsc::Receiver<Vec<u8>>,
    expected: &str,
) -> ServerMessage {
    recv_server_message_until(receiver, Instant::now() + SERVER_RESPONSE_TIMEOUT, expected)
}

fn recv_server_message_until(
    receiver: &std::sync::mpsc::Receiver<Vec<u8>>,
    deadline: Instant,
    expected: &str,
) -> ServerMessage {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let bytes = receiver
        .recv_timeout(remaining)
        .unwrap_or_else(|error| match error {
            std::sync::mpsc::RecvTimeoutError::Timeout => {
                panic!("timed out waiting for {expected}")
            }
            std::sync::mpsc::RecvTimeoutError::Disconnected => {
                panic!("control channel closed while waiting for {expected}")
            }
        });
    headless_tests::read_server_message(bytes)
}

fn recv_render_server_message(receiver: &RenderLaneReceiver, expected: &str) -> ServerMessage {
    recv_render_server_message_until(receiver, Instant::now() + SERVER_RESPONSE_TIMEOUT, expected)
}

fn recv_render_server_message_until(
    receiver: &RenderLaneReceiver,
    deadline: Instant,
    expected: &str,
) -> ServerMessage {
    loop {
        match receiver.try_recv() {
            Ok(bytes) => return headless_tests::read_server_message(bytes),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {expected}"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("render channel closed while waiting for {expected}");
            }
        }
    }
}

/// The registry generation of the source (Local) connection.
const SOURCE_GENERATION: u64 = 1;
/// The registry generation of the target (remote) connection.
const TARGET_GENERATION: u64 = 7;

#[derive(Clone)]
struct CapturingEndpointTransport(Arc<Mutex<Vec<shepr_protocol::ClientMessage>>>);

impl EndpointTransport for CapturingEndpointTransport {
    fn send(&mut self, message: &shepr_protocol::ClientMessage) -> std::io::Result<()> {
        let mut sent = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("test mutex poisoned"))?;
        sent.push(message.clone());
        Ok(())
    }

    fn disconnect(&mut self) {}

    fn flush(&mut self, _deadline: Instant) -> std::io::Result<()> {
        Ok(())
    }

    fn take_error(&mut self) -> Option<std::io::Error> {
        None
    }
}

fn lifecycle_resize() -> shepr_protocol::ClientMessage {
    shepr_protocol::ClientMessage::ClientShellResize {
        geometry: shepr_protocol::TerminalGeometry::new(80, 24, 8, 16, false),
    }
}

fn begin_activation(
    shell: &shepr_client::ClientShellState,
    endpoints: &mut EndpointRegistry,
    target: &ClientEndpointId,
    serial: u64,
) -> PendingEndpointActivation {
    PendingEndpointActivation::prepare(
        shell,
        endpoints,
        target,
        None,
        lifecycle_resize(),
        serial,
        std::time::Instant::now(),
    )
    .and_then(|activation| activation.start_at(endpoints, std::time::Instant::now()))
    .expect("test precondition")
}

/// A real two-server/client lifecycle harness. Both source-off and target-on traverse the
/// production HeadlessServer endpoint request path; the client test only routes its emitted wire
/// messages and never authors an acknowledgement, snapshot, or surface response. Snapshots are
/// installed for the registry generation of the connection they arrived on, as the client loop
/// installs them.
#[tokio::test]
async fn two_headless_servers_drive_atomic_endpoint_handoff() {
    let mut source_server = headless_tests::test_headless_server();
    let _source_input =
        headless_tests::install_focused_test_runtime(&mut source_server, b"local source");
    let (source_writer, source_control, _source_render) = headless_tests::test_client_writer();
    let source_client_id = ClientId::test_new(78);
    assert!(headless_tests::handle_server_event(
        &mut source_server,
        ServerEvent::ClientShellConnected {
            client_id: source_client_id,
            surface_cols: 80,
            surface_rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
            mouse_capture: true,
            surface_active: true,
            writer: source_writer,
        }
    ));
    let source_snapshot = headless_tests::client_shell_snapshot(&source_control);

    let mut target_server = headless_tests::test_headless_server();
    let _target_input =
        headless_tests::install_focused_test_runtime(&mut target_server, b"remote target");
    let (target_writer, target_control, target_render) = headless_tests::test_client_writer();
    let target_client_id = ClientId::test_new(79);
    assert!(headless_tests::handle_server_event(
        &mut target_server,
        ServerEvent::ClientShellConnected {
            client_id: target_client_id,
            surface_cols: 80,
            surface_rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
            mouse_capture: true,
            surface_active: false,
            writer: target_writer,
        }
    ));
    let remote_snapshot = headless_tests::client_shell_snapshot(&target_control);

    let profile = SavedSshEndpoint {
        id: ProfileId::parse("0123456789abcdef0123456789abcdef").expect("test precondition"),
        label: "Remote".into(),
        target: shepr_remote::SshTarget::parse("dev@example.com").expect("test precondition"),
        session: "main".into(),
    };
    let target_id = ClientEndpointId::Ssh(profile.id.clone());
    let mut shell = shepr_client::ClientShellState::new(
        shepr_client::ClientShellConfig::from_validated_config(
            &shepr_config::ValidatedConfig::test_default(),
        ),
    );
    shell.set_endpoint_catalog(&[profile]);
    shell.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        SOURCE_GENERATION,
        source_snapshot,
    );
    shell.set_endpoint_status(&target_id, ClientEndpointStatus::Online);
    shell.set_endpoint_snapshot_for_generation(&target_id, TARGET_GENERATION, remote_snapshot);

    let source_sent = Arc::new(Mutex::new(Vec::new()));
    let target_sent = Arc::new(Mutex::new(Vec::new()));
    let mut endpoints = EndpointRegistry::new(
        CapturingEndpointTransport(Arc::clone(&source_sent)),
        SOURCE_GENERATION,
    );
    endpoints.insert(
        target_id.clone(),
        CapturingEndpointTransport(Arc::clone(&target_sent)),
        TARGET_GENERATION,
        false,
        std::time::Instant::now(),
    );
    let mut activation = begin_activation(&shell, &mut endpoints, &target_id, 41);

    // Route the source-off-first client messages through a second real HeadlessServer. Its
    // typed response is the only source acknowledgement supplied to the activation state.
    let mut source_release_request_id = None;
    for message in std::mem::take(&mut *source_sent.lock().expect("test precondition")) {
        match message {
            shepr_protocol::ClientMessage::ClientShellFocus { focused } => {
                assert!(headless_tests::handle_server_event(
                    &mut source_server,
                    ServerEvent::ClientShellFocus {
                        client_id: source_client_id,
                        focused,
                    }
                ));
            }
            shepr_protocol::ClientMessage::ClientShellEndpointRequest { boot_id, request } => {
                let request = serde_json::from_str::<api::schema::Request>(&request)
                    .expect("test precondition");
                if matches!(
                    request.method,
                    api::schema::Method::ClientShellSurfaceSet(
                        api::schema::ClientShellSurfaceSetParams { active: false }
                    )
                ) {
                    source_release_request_id = Some(request.id.clone());
                }
                assert!(headless_tests::handle_server_event(
                    &mut source_server,
                    ServerEvent::ClientShellEndpointRequest {
                        client_id: source_client_id,
                        boot_id,
                        request: Box::new(request),
                    }
                ));
            }
            other => panic!("unexpected source lifecycle message: {other:?}"),
        }
    }
    let source_release_request_id = source_release_request_id.expect("client source-off request");
    let source_release_deadline = Instant::now() + SERVER_RESPONSE_TIMEOUT;
    let (source_release_boot_id, source_release_data) = loop {
        let message = recv_server_message_until(
            &source_control,
            source_release_deadline,
            "source typed release acknowledgement",
        );
        match message {
            ServerMessage::ClientShellEndpointResponseChunk {
                boot_id,
                request_id,
                data,
                ..
            } if request_id == source_release_request_id => break (boot_id, data),
            ServerMessage::EndpointSnapshot(_)
            | ServerMessage::MouseCapture { .. }
            | ServerMessage::ClientShellKeyboardReportAll { .. }
            | ServerMessage::WindowTitle { .. }
            | ServerMessage::ClientShellEndpointResponseChunk { .. } => continue,
            other => panic!("unexpected source release message: {other:?}"),
        }
    };
    assert_eq!(
        activation.receive_response_for_boot_at(
            &ClientEndpointId::Local,
            SOURCE_GENERATION,
            &source_release_boot_id,
            &source_release_request_id,
            &source_release_data,
            &mut endpoints,
            Instant::now(),
        ),
        SurfaceActivationProgress::Pending
    );

    headless_tests::dispatch_lifecycle_messages(
        &mut target_server,
        target_client_id,
        std::mem::take(&mut *target_sent.lock().expect("test precondition")),
    );
    assert_eq!(
        headless_tests::outer_terminal_focus(&target_server, target_client_id),
        Some(true)
    );
    assert_eq!(
        headless_tests::outer_terminal_focus(&source_server, source_client_id),
        Some(false)
    );
    let ServerMessage::ClientShellEndpointResponseChunk {
        boot_id,
        request_id,
        data,
        ..
    } = recv_server_message(&target_control, "target typed activation acknowledgement")
    else {
        panic!("expected target activation acknowledgement");
    };
    assert_eq!(
        activation.receive_response_for_boot_at(
            &target_id,
            TARGET_GENERATION,
            &boot_id,
            &request_id,
            &data,
            &mut endpoints,
            Instant::now(),
        ),
        SurfaceActivationProgress::Pending
    );

    headless_tests::render_and_stream(&mut target_server);
    let coherent_snapshot = headless_tests::client_shell_snapshot(&target_control);
    let snapshot_progress =
        activation.receive_snapshot(&target_id, TARGET_GENERATION, &coherent_snapshot);
    shell.set_endpoint_snapshot_for_generation(&target_id, TARGET_GENERATION, coherent_snapshot);
    assert_eq!(snapshot_progress, SurfaceActivationProgress::Pending);
    let ServerMessage::PaneSurface(coherent_surface) =
        recv_render_server_message(&target_render, "target replacement surface")
    else {
        panic!("expected target pane surface");
    };
    assert_eq!(
        activation.receive_surface(&target_id, TARGET_GENERATION, coherent_surface),
        SurfaceActivationProgress::Ready
    );

    assert!(matches!(
        activation.complete_at(&mut shell, &mut endpoints, Instant::now()),
        Ok(shepr_client::endpoint::ActivationCompletion::AwaitingPresentationSync {
            endpoint,
            ..
        }) if endpoint == target_id
    ));
    assert!(
        !endpoints.active_surface_available(),
        "target input remains fenced while presentation effects resynchronize"
    );

    let sync_request = target_sent
        .lock()
        .expect("test precondition")
        .iter()
        .find_map(|message| match message {
            shepr_protocol::ClientMessage::ClientShellEndpointRequest { boot_id, request } => {
                let request = serde_json::from_str::<api::schema::Request>(request).ok()?;
                request
                    .id
                    .ends_with(":presentation-sync")
                    .then(|| (boot_id.clone(), Box::new(request)))
            }
            _ => None,
        })
        .expect("client presentation synchronization request");
    assert!(headless_tests::handle_server_event(
        &mut target_server,
        ServerEvent::ClientShellEndpointRequest {
            client_id: target_client_id,
            boot_id: sync_request.0,
            request: sync_request.1,
        }
    ));
    let sync_response_deadline = Instant::now() + SERVER_RESPONSE_TIMEOUT;
    let (sync_boot_id, sync_request_id, sync_data) = loop {
        let message = recv_server_message_until(
            &target_control,
            sync_response_deadline,
            "presentation synchronization acknowledgement",
        );
        if let ServerMessage::ClientShellEndpointResponseChunk {
            boot_id,
            request_id,
            data,
            ..
        } = message
            && request_id.ends_with(":presentation-sync")
        {
            break (boot_id, request_id, data);
        }
    };
    assert_eq!(
        activation.receive_response_for_boot_at(
            &target_id,
            TARGET_GENERATION,
            &sync_boot_id,
            &sync_request_id,
            &sync_data,
            &mut endpoints,
            Instant::now(),
        ),
        SurfaceActivationProgress::Pending
    );
    headless_tests::render_and_stream(&mut target_server);
    let sync_snapshot_deadline = Instant::now() + SERVER_RESPONSE_TIMEOUT;
    let sync_snapshot = loop {
        let message = recv_server_message_until(
            &target_control,
            sync_snapshot_deadline,
            "presentation synchronization snapshot",
        );
        if let ServerMessage::EndpointSnapshot(snapshot) = message {
            break *snapshot;
        }
    };
    let sync_progress = activation.receive_snapshot(&target_id, TARGET_GENERATION, &sync_snapshot);
    shell.set_endpoint_snapshot_for_generation(
        &target_id,
        TARGET_GENERATION,
        Box::new(sync_snapshot),
    );
    assert_eq!(sync_progress, SurfaceActivationProgress::Pending);
    let ServerMessage::PaneSurface(sync_surface) =
        recv_render_server_message(&target_render, "presentation synchronization surface")
    else {
        panic!("expected synchronized target surface");
    };
    assert_eq!(
        activation.receive_surface(&target_id, TARGET_GENERATION, sync_surface),
        SurfaceActivationProgress::Ready
    );
    assert_eq!(
        activation.complete_at(&mut shell, &mut endpoints, Instant::now()),
        Ok(shepr_client::endpoint::ActivationCompletion::AwaitingPresentationEffects)
    );
    let effects_token = target_sent
        .lock()
        .expect("test precondition")
        .iter()
        .find_map(|message| match message {
            shepr_protocol::ClientMessage::PresentationSync(data) => Some(data.clone()),
            _ => None,
        })
        .expect("client presentation effects fence");
    assert!(headless_tests::handle_server_event(
        &mut target_server,
        ServerEvent::ClientShellPresentationSync {
            client_id: target_client_id,
            token: effects_token.clone(),
        }
    ));
    let mut replayed_mouse = false;
    let mut replayed_keyboard = false;
    let presentation_effects_deadline = Instant::now() + SERVER_RESPONSE_TIMEOUT;
    loop {
        match recv_server_message_until(
            &target_control,
            presentation_effects_deadline,
            "presentation effects or readiness fence",
        ) {
            ServerMessage::MouseCapture { .. } => replayed_mouse = true,
            ServerMessage::ClientShellKeyboardReportAll { .. } => replayed_keyboard = true,
            ServerMessage::PresentationReady(data) => {
                assert_eq!(data, effects_token);
                assert_eq!(
                    activation.receive_presentation_effects_ready(
                        &target_id,
                        TARGET_GENERATION,
                        &data
                    ),
                    SurfaceActivationProgress::Ready
                );
                break;
            }
            ServerMessage::WindowTitle { .. } => {}
            other => panic!("unexpected presentation fence message: {other:?}"),
        }
    }
    assert!(replayed_mouse);
    assert!(replayed_keyboard);
    assert_eq!(
        activation.complete_at(&mut shell, &mut endpoints, Instant::now()),
        Ok(shepr_client::endpoint::ActivationCompletion::Activated)
    );
    endpoints.unfreeze_input();
    assert_eq!(endpoints.active_id(), &target_id);
    assert!(endpoints.active_surface_available());
    assert!(shell.endpoint_is_active(&target_id));

    target_sent.lock().expect("test precondition").clear();
    let mut returning = begin_activation(&shell, &mut endpoints, &ClientEndpointId::Local, 42);
    let returning_messages = std::mem::take(&mut *target_sent.lock().expect("test precondition"));
    let returning_release_request_id = returning_messages
        .iter()
        .find_map(|message| match message {
            shepr_protocol::ClientMessage::ClientShellEndpointRequest { request, .. } => {
                let request = serde_json::from_str::<api::schema::Request>(request)
                    .expect("test precondition");
                matches!(
                    request.method,
                    api::schema::Method::ClientShellSurfaceSet(
                        api::schema::ClientShellSurfaceSetParams { active: false }
                    )
                )
                .then_some(request.id)
            }
            _ => None,
        })
        .expect("client remote-off request");
    headless_tests::dispatch_lifecycle_messages(
        &mut target_server,
        target_client_id,
        returning_messages,
    );
    let returning_activation_deadline = Instant::now() + SERVER_RESPONSE_TIMEOUT;
    loop {
        // Earlier responses from the first handoff may still be queued; only
        // the answer to this activation's release request counts.
        if let ServerMessage::ClientShellEndpointResponseChunk {
            boot_id,
            request_id,
            data,
            ..
        } = recv_server_message_until(
            &target_control,
            returning_activation_deadline,
            "returning activation acknowledgement",
        ) && request_id == returning_release_request_id
        {
            // Returning to Local releases the remote best-effort and goes
            // straight to activating Local, so Local never waits on this
            // remote acknowledgement: the activation reports it as stale.
            assert_eq!(
                returning.receive_response_for_boot_at(
                    &target_id,
                    TARGET_GENERATION,
                    &boot_id,
                    &request_id,
                    &data,
                    &mut endpoints,
                    Instant::now(),
                ),
                SurfaceActivationProgress::Stale
            );
            break;
        }
    }
    headless_tests::dispatch_lifecycle_messages(
        &mut source_server,
        source_client_id,
        std::mem::take(&mut *source_sent.lock().expect("test precondition")),
    );
    assert_eq!(
        headless_tests::outer_terminal_focus(&source_server, source_client_id),
        Some(true),
        "returning to Local must restore focus without a host focus event"
    );
    assert_eq!(
        headless_tests::outer_terminal_focus(&target_server, target_client_id),
        Some(false)
    );
    headless_tests::shutdown_test_runtimes(&mut source_server);
    headless_tests::shutdown_test_runtimes(&mut target_server);
}

#[test]
#[should_panic(expected = "timed out waiting for test control response")]
fn server_control_response_wait_has_a_deadline() {
    let (_sender, receiver) = std::sync::mpsc::channel();
    recv_server_message_until(&receiver, Instant::now(), "test control response");
}
