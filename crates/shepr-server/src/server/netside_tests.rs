//! The server's side of a client's endpoint move, against two headless servers. Each pass
//! moves a client from one server to the other and dispatches, at each step, the messages a
//! client sends then: the surface geometry and the view request to turn the target on, the
//! focus baseline and the replay request at the commit, focus loss and the view-off request
//! to release the previous server. shepr-client's endpoint move tests pin that its move sends
//! exactly these; this test pins how a real server answers them: the acknowledgement,
//! snapshot and surface the client's move commits from, the replayed host effects, and
//! which server ends up viewed and focused.

use std::time::{Duration, Instant};

use shepr_protocol::command::{ClientShellSurfaceSetParams, EndpointCommand, EndpointReply};
use shepr_protocol::{BootId, ClientMessage, ProjectionRevision, RequestId, ServerMessage};

use crate::server::ClientId;
use crate::server::client_transport::ServerEvent;
use crate::server::headless::tests as headless_tests;
use crate::server::outbox::RenderLaneReceiver;

/// Maximum time an expected server control message may take in this test.
const SERVER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

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

fn lifecycle_geometry() -> shepr_protocol::TerminalGeometry {
    shepr_protocol::TerminalGeometry::from_host(
        shepr_core::geometry::GridSize::clamped(80, 24),
        shepr_core::geometry::HostCell::from_host(8, 16, false),
    )
}

fn surface_interest(boot_id: &BootId, request_id: RequestId, active: bool) -> ClientMessage {
    ClientMessage::ClientShellEndpointRequest {
        boot_id: boot_id.clone(),
        request_id,
        command: EndpointCommand::ClientShellSurfaceSet(ClientShellSurfaceSetParams { active }),
    }
}

/// What a client sends to turn a server on as its move's target: the one surface geometry
/// (no theme updates are recorded here), then the view request. No focus: that follows the
/// commit.
fn turn_on(boot_id: &BootId, request_id: &RequestId) -> Vec<ClientMessage> {
    vec![
        ClientMessage::ClientShellResize {
            geometry: lifecycle_geometry(),
        },
        surface_interest(boot_id, request_id.clone(), true),
    ]
}

/// What a client sends the target when the move commits, with its host terminal focused.
fn commit() -> Vec<ClientMessage> {
    vec![
        ClientMessage::ClientShellFocus { focused: true },
        ClientMessage::ReplayHostEffects,
    ]
}

/// What a client sends the server it moved away from once the commit has shown the target.
fn release(boot_id: &BootId) -> Vec<ClientMessage> {
    vec![
        ClientMessage::ClientShellFocus { focused: false },
        surface_interest(boot_id, RequestId::allocate(), false),
    ]
}

/// Collects the target's answer to `turn_on` and checks it is what the client's move
/// commits from (shepr-client's `Preparing::ready`, for a move with no navigation): an
/// acknowledgement of the view request whose projection revision is the floor, a snapshot of
/// this boot at or above the revision the move started from, and a surface of this boot,
/// sized for the move's geometry, at exactly the snapshot's projection revision and at or
/// above the floor.
fn assert_view_evidence(
    control: &std::sync::mpsc::Receiver<Vec<u8>>,
    render: &RenderLaneReceiver,
    boot_id: &BootId,
    view_request: &RequestId,
    minimum_revision: ProjectionRevision,
) {
    let mut floor: Option<ProjectionRevision> = None;
    let mut snapshot_revision: Option<ProjectionRevision> = None;
    let deadline = Instant::now() + SERVER_RESPONSE_TIMEOUT;
    while floor.is_none() || snapshot_revision.is_none() {
        match recv_server_message_until(control, deadline, "view evidence") {
            ServerMessage::ClientShellEndpointResponse {
                boot_id: answered_boot,
                request_id,
                result,
            } if &request_id == view_request => {
                assert_eq!(&answered_boot, boot_id);
                let Ok(EndpointReply::ClientShellSurfaceSet {
                    active: true,
                    projection_revision,
                }) = result
                else {
                    panic!("the view request was not acknowledged as on: {result:?}");
                };
                floor = Some(projection_revision);
            }
            ServerMessage::EndpointSnapshot(snapshot) => {
                assert_eq!(&snapshot.boot_id, boot_id);
                assert!(snapshot.revision >= minimum_revision);
                snapshot_revision = snapshot_revision.max(Some(snapshot.revision));
            }
            _ => {}
        }
    }
    let ServerMessage::PaneSurface(surface) = recv_render_server_message(render, "view surface")
    else {
        panic!("expected full surface");
    };
    let floor = floor.expect("acknowledged");
    let snapshot_revision = snapshot_revision.expect("snapshot sent");
    assert_eq!(&surface.boot_id, boot_id);
    assert!(surface.is_sized_for(lifecycle_geometry().surface_size()));
    assert_eq!(surface.projection_revision, snapshot_revision);
    assert!(surface.projection_revision >= floor);
}

/// Two real servers emit every acknowledgement, snapshot and surface used as evidence.
#[tokio::test]
async fn two_headless_servers_answer_each_step_of_an_endpoint_move() {
    let mut source_server = headless_tests::test_headless_server();
    headless_tests::enable_window_title(&mut source_server, "{workspace}");
    let _source_input =
        headless_tests::install_focused_test_runtime(&mut source_server, b"local source");
    let (source_writer, source_control, source_render) = headless_tests::test_client_writer();
    let source_client_id = ClientId::test_new(78);
    assert!(headless_tests::handle_server_event(
        &mut source_server,
        ServerEvent::ShellConnected {
            client_id: source_client_id,
            geometry: shepr_core::geometry::HostGeometry::new(
                shepr_core::geometry::GridSize::clamped(80, 24),
                shepr_core::geometry::HostCell::from_host(8, 16, false)
            ),
            mouse_capture: true,
            surface_active: true,
            outbox: source_writer,
        }
    ));
    let source_snapshot = headless_tests::client_shell_snapshot(&source_control);

    let mut target_server = headless_tests::test_headless_server();
    headless_tests::enable_window_title(&mut target_server, "{workspace}");
    let _target_input =
        headless_tests::install_focused_test_runtime(&mut target_server, b"remote target");
    let (target_writer, target_control, target_render) = headless_tests::test_client_writer();
    let target_client_id = ClientId::test_new(79);
    assert!(headless_tests::handle_server_event(
        &mut target_server,
        ServerEvent::ShellConnected {
            client_id: target_client_id,
            geometry: shepr_core::geometry::HostGeometry::new(
                shepr_core::geometry::GridSize::clamped(80, 24),
                shepr_core::geometry::HostCell::from_host(8, 16, false)
            ),
            mouse_capture: true,
            surface_active: false,
            outbox: target_writer,
        }
    ));
    let target_snapshot = headless_tests::client_shell_snapshot(&target_control);

    headless_tests::dispatch_lifecycle_messages(
        &mut source_server,
        source_client_id,
        vec![ClientMessage::ClientShellFocus { focused: true }],
    );
    assert_eq!(
        headless_tests::outer_terminal_focus(&source_server, source_client_id),
        Some(true)
    );
    for returning in [false, true] {
        // The server on screen before this pass's move is viewed and focused.
        let (shown_server, shown_client) = if returning {
            (&target_server, target_client_id)
        } else {
            (&source_server, source_client_id)
        };
        assert!(headless_tests::client_is_viewed(shown_server, shown_client));
        assert_eq!(
            headless_tests::outer_terminal_focus(shown_server, shown_client),
            Some(true)
        );
        // The move's lease: the boot and revision of the snapshot the client last cached
        // for the server it moves to, the one it sent when the client connected.
        let (server, client, control, render, to_snapshot, from_snapshot) = if returning {
            (
                &mut source_server,
                source_client_id,
                &source_control,
                &source_render,
                &source_snapshot,
                &target_snapshot,
            )
        } else {
            (
                &mut target_server,
                target_client_id,
                &target_control,
                &target_render,
                &target_snapshot,
                &source_snapshot,
            )
        };
        // A non-viewed connection still supplies metadata; drain earlier control effects.
        while control.try_recv().is_ok() {}
        let view_request = RequestId::allocate();
        headless_tests::dispatch_lifecycle_messages(
            server,
            client,
            turn_on(&to_snapshot.boot_id, &view_request),
        );
        headless_tests::render_now(server);
        assert_view_evidence(
            control,
            render,
            &to_snapshot.boot_id,
            &view_request,
            to_snapshot.revision,
        );

        headless_tests::dispatch_lifecycle_messages(server, client, commit());
        let mut mouse = false;
        let mut keyboard = false;
        let mut title = false;
        let deadline = Instant::now() + SERVER_RESPONSE_TIMEOUT;
        while !mouse || !keyboard || !title {
            match recv_server_message_until(control, deadline, "replayed host effects") {
                ServerMessage::MouseCapture { .. } => mouse = true,
                ServerMessage::ClientShellKeyboardReportAll { .. } => keyboard = true,
                ServerMessage::WindowTitle { .. } => title = true,
                _ => {}
            }
        }
        // Until the release, both servers have the client focused.
        assert_eq!(
            headless_tests::outer_terminal_focus(&source_server, source_client_id),
            Some(true)
        );
        assert_eq!(
            headless_tests::outer_terminal_focus(&target_server, target_client_id),
            Some(true)
        );

        let released = release(&from_snapshot.boot_id);
        let (previous_server, previous_client, current_server, current_client) = if returning {
            (
                &mut target_server,
                target_client_id,
                &source_server,
                source_client_id,
            )
        } else {
            (
                &mut source_server,
                source_client_id,
                &target_server,
                target_client_id,
            )
        };
        headless_tests::dispatch_lifecycle_messages(previous_server, previous_client, released);
        assert_eq!(
            headless_tests::outer_terminal_focus(previous_server, previous_client),
            Some(false)
        );
        assert!(!headless_tests::client_is_viewed(
            previous_server,
            previous_client
        ));
        assert!(headless_tests::client_is_viewed(
            current_server,
            current_client
        ));
    }
    headless_tests::shutdown_test_runtimes(&mut source_server);
    headless_tests::shutdown_test_runtimes(&mut target_server);
}

#[test]
#[should_panic(expected = "timed out waiting for test control response")]
fn server_control_response_wait_has_a_deadline() {
    let (_sender, receiver) = std::sync::mpsc::channel();
    recv_server_message_until(&receiver, Instant::now(), "test control response");
}
