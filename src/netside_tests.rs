use std::sync::{Arc, Mutex};

use crate::client::endpoint::{
    ClientEndpointId, ClientEndpointStatus, EndpointRegistry, EndpointTransport, ProfileId,
    SavedSshEndpoint,
};
use crate::server::ClientId;
use crate::server::client_transport::ServerEvent;
use crate::server::headless::tests as headless_tests;
use shepr_api as api;
use shepr_protocol::ServerMessage;

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
}

fn lifecycle_resize() -> shepr_protocol::ClientMessage {
    shepr_protocol::ClientMessage::ClientShellResize {
        geometry: shepr_protocol::TerminalGeometry::new(80, 24, 8, 16, false),
    }
}

/// A real two-server/client lifecycle harness. Both source-off and target-on traverse the
/// production HeadlessServer endpoint request path; the client test only routes its emitted wire
/// messages and never authors an acknowledgement, snapshot, or surface response.
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
        target: crate::remote::SshTarget::parse("dev@example.com").expect("test precondition"),
        session: "main".into(),
    };
    let target_id = ClientEndpointId::Ssh(profile.id.clone());
    let mut shell = crate::client::ClientShellState::new(
        crate::client::ClientShellConfig::from_config(&shepr_config::Config::default()),
    );
    shell.set_endpoint_catalog(&[profile]);
    shell.set_snapshot(source_snapshot);
    shell.set_endpoint_status(&target_id, ClientEndpointStatus::Online);
    shell.set_endpoint_snapshot(&target_id, remote_snapshot);

    let source_sent = Arc::new(Mutex::new(Vec::new()));
    let target_sent = Arc::new(Mutex::new(Vec::new()));
    let mut endpoints =
        EndpointRegistry::new(CapturingEndpointTransport(Arc::clone(&source_sent)), 1);
    endpoints.insert(
        target_id.clone(),
        CapturingEndpointTransport(Arc::clone(&target_sent)),
        7,
        false,
    );
    let mut activation = crate::client::endpoint::PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        &target_id,
        None,
        lifecycle_resize(),
        41,
        std::time::Instant::now(),
    )
    .expect("test precondition");

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
    let source_release_data = loop {
        let message = headless_tests::read_server_message(
            source_control.recv().expect("source typed release ack"),
        );
        match message {
            ServerMessage::ClientShellEndpointResponseChunk {
                request_id, data, ..
            } if request_id == source_release_request_id => break data,
            ServerMessage::EndpointSnapshot(_)
            | ServerMessage::MouseCapture { .. }
            | ServerMessage::ClientShellKeyboardReportAll { .. }
            | ServerMessage::WindowTitle { .. }
            | ServerMessage::ClientShellEndpointResponseChunk { .. } => continue,
            other => panic!("unexpected source release message: {other:?}"),
        }
    };
    assert_eq!(
        activation.receive_response(
            &ClientEndpointId::Local,
            1,
            &source_release_request_id,
            &source_release_data,
            &mut endpoints,
        ),
        crate::client::endpoint::SurfaceActivationProgress::Pending
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
        request_id, data, ..
    } = headless_tests::read_server_message(
        target_control.recv().expect("target typed activation ack"),
    )
    else {
        panic!("expected target activation acknowledgement");
    };
    assert_eq!(
        activation.receive_response(&target_id, 7, &request_id, &data, &mut endpoints),
        crate::client::endpoint::SurfaceActivationProgress::Pending
    );

    headless_tests::render_and_stream(&mut target_server);
    let coherent_snapshot = headless_tests::client_shell_snapshot(&target_control);
    let snapshot_progress = activation.receive_snapshot(&target_id, 7, &coherent_snapshot);
    shell.set_endpoint_snapshot(&target_id, coherent_snapshot);
    assert_eq!(
        snapshot_progress,
        crate::client::endpoint::SurfaceActivationProgress::Pending
    );
    let ServerMessage::PaneSurface(coherent_surface) = headless_tests::read_server_message(
        target_render.recv().expect("target replacement surface"),
    ) else {
        panic!("expected target pane surface");
    };
    assert_eq!(
        activation.receive_surface(&target_id, 7, coherent_surface),
        crate::client::endpoint::SurfaceActivationProgress::Ready
    );

    assert!(matches!(
        activation.complete(&mut shell, &mut endpoints),
        Ok(crate::client::endpoint::ActivationCompletion::AwaitingPresentationSync {
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
    let (sync_request_id, sync_data) = loop {
        let message = headless_tests::read_server_message(
            target_control.recv().expect("presentation sync ack"),
        );
        if let ServerMessage::ClientShellEndpointResponseChunk {
            request_id, data, ..
        } = message
            && request_id.ends_with(":presentation-sync")
        {
            break (request_id, data);
        }
    };
    assert_eq!(
        activation.receive_response(&target_id, 7, &sync_request_id, &sync_data, &mut endpoints,),
        crate::client::endpoint::SurfaceActivationProgress::Pending
    );
    headless_tests::render_and_stream(&mut target_server);
    let sync_snapshot = loop {
        let message = headless_tests::read_server_message(
            target_control.recv().expect("presentation sync snapshot"),
        );
        if let ServerMessage::EndpointSnapshot(snapshot) = message {
            break *snapshot;
        }
    };
    let sync_progress = activation.receive_snapshot(&target_id, 7, &sync_snapshot);
    shell.set_endpoint_snapshot_for_generation(&target_id, 7, Box::new(sync_snapshot));
    assert_eq!(
        sync_progress,
        crate::client::endpoint::SurfaceActivationProgress::Pending
    );
    let ServerMessage::PaneSurface(sync_surface) = headless_tests::read_server_message(
        target_render.recv().expect("presentation sync surface"),
    ) else {
        panic!("expected synchronized target surface");
    };
    assert_eq!(
        activation.receive_surface(&target_id, 7, sync_surface),
        crate::client::endpoint::SurfaceActivationProgress::Ready
    );
    assert_eq!(
        activation.complete(&mut shell, &mut endpoints),
        Ok(crate::client::endpoint::ActivationCompletion::AwaitingPresentationEffects)
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
    loop {
        match headless_tests::read_server_message(
            target_control.recv().expect("presentation effect or fence"),
        ) {
            ServerMessage::MouseCapture { .. } => replayed_mouse = true,
            ServerMessage::ClientShellKeyboardReportAll { .. } => replayed_keyboard = true,
            ServerMessage::PresentationReady(data) => {
                assert_eq!(data, effects_token);
                assert_eq!(
                    activation.receive_presentation_effects_ready(&target_id, 7, &data),
                    crate::client::endpoint::SurfaceActivationProgress::Ready
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
        activation.complete(&mut shell, &mut endpoints),
        Ok(crate::client::endpoint::ActivationCompletion::Activated)
    );
    endpoints.unfreeze_input();
    assert_eq!(endpoints.active_id(), &target_id);
    assert!(endpoints.active_surface_available());
    assert!(shell.endpoint_is_active(&target_id));

    target_sent.lock().expect("test precondition").clear();
    let mut returning = crate::client::endpoint::PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        &ClientEndpointId::Local,
        None,
        lifecycle_resize(),
        42,
        std::time::Instant::now(),
    )
    .expect("test precondition");
    headless_tests::dispatch_lifecycle_messages(
        &mut target_server,
        target_client_id,
        std::mem::take(&mut *target_sent.lock().expect("test precondition")),
    );
    loop {
        if let ServerMessage::ClientShellEndpointResponseChunk {
            request_id, data, ..
        } =
            headless_tests::read_server_message(target_control.recv().expect("test precondition"))
        {
            returning.receive_response(&target_id, 7, &request_id, &data, &mut endpoints);
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
