use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use shepr_client::endpoint::{
    ClientEndpointId, EndpointRegistry, EndpointTransport,
    view::{self, HostBaseline, StartOutcome},
};
use shepr_protocol::ServerMessage;

use crate::server::ClientId;
use crate::server::client_transport::ServerEvent;
use crate::server::headless::tests as headless_tests;
use crate::server::outbox::RenderLaneReceiver;
use shepr_test_fixtures::ValidatedClientConfigFixture as _;

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

fn lifecycle_geometry() -> shepr_protocol::TerminalGeometry {
    shepr_protocol::TerminalGeometry::new(80, 24, 8, 16, false)
}

/// Two real servers emit every acknowledgement, snapshot and surface used as evidence.
#[tokio::test]
async fn two_headless_servers_switch_endpoints_without_a_lease() {
    let mut source_server = headless_tests::test_headless_server();
    let _source_input =
        headless_tests::install_focused_test_runtime(&mut source_server, b"local source");
    let (source_writer, source_control, source_render) = headless_tests::test_client_writer();
    let source_client_id = ClientId::test_new(78);
    assert!(headless_tests::handle_server_event(
        &mut source_server,
        ServerEvent::ShellConnected {
            client_id: source_client_id,
            geometry: shepr_core::geometry::HostGeometry::new(80, 24, 8, 16, false),
            mouse_capture: true,
            surface_active: true,
            outbox: source_writer,
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
        ServerEvent::ShellConnected {
            client_id: target_client_id,
            geometry: shepr_core::geometry::HostGeometry::new(80, 24, 8, 16, false),
            mouse_capture: true,
            surface_active: false,
            outbox: target_writer,
        }
    ));
    let remote_snapshot = headless_tests::client_shell_snapshot(&target_control);

    let machine = shepr_config::MachineConfig {
        label: shepr_config::MachineLabel::parse("Remote").expect("test precondition"),
        ssh: shepr_config::SshTarget::parse("dev@example.com").expect("test precondition"),
    };
    let target_id = ClientEndpointId::Ssh(machine.label.clone());
    let now = std::time::Instant::now();
    let mut shell = shepr_client::ClientShellState::new_at(
        shepr_client::ClientShellConfig::from_validated_config(
            &shepr_config::ValidatedClientConfig::test_default(),
        ),
        now,
    );
    shell.set_machines(&[machine]);
    shell.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        SOURCE_GENERATION,
        source_snapshot,
    );
    shell.endpoint_connected(&target_id, TARGET_GENERATION);
    shell.set_endpoint_snapshot_for_generation(&target_id, TARGET_GENERATION, remote_snapshot);

    let source_sent = Arc::new(Mutex::new(Vec::new()));
    let target_sent = Arc::new(Mutex::new(Vec::new()));
    let mut endpoints = EndpointRegistry::new_at(
        CapturingEndpointTransport(Arc::clone(&source_sent)),
        SOURCE_GENERATION,
        now,
    );
    endpoints.insert(
        target_id.clone(),
        CapturingEndpointTransport(Arc::clone(&target_sent)),
        TARGET_GENERATION,
        false,
        now,
    );
    let mut serial = view::ViewSerialAllocator::new();
    headless_tests::dispatch_lifecycle_messages(
        &mut source_server,
        source_client_id,
        vec![shepr_protocol::ClientMessage::ClientShellFocus { focused: true }],
    );
    assert_eq!(
        headless_tests::outer_terminal_focus(&source_server, source_client_id),
        Some(true)
    );
    let baseline = || HostBaseline {
        geometry: lifecycle_geometry(),
        theme: &[],
    };
    for returning in [false, true] {
        // The endpoint on screen before this pass's move is viewed and focused.
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
        let (to, generation, server, client, control, render, sent, previous_sent) = if returning {
            (
                ClientEndpointId::Local,
                SOURCE_GENERATION,
                &mut source_server,
                source_client_id,
                &source_control,
                &source_render,
                &source_sent,
                &target_sent,
            )
        } else {
            (
                target_id.clone(),
                TARGET_GENERATION,
                &mut target_server,
                target_client_id,
                &target_control,
                &target_render,
                &target_sent,
                &source_sent,
            )
        };
        // A non-viewed connection still supplies metadata; drain earlier control effects.
        while control.try_recv().is_ok() {}
        shell.endpoint_choice_mut().select(to.clone(), None);
        assert_eq!(
            view::start_move(
                &mut endpoints,
                &mut shell,
                |_| baseline(),
                &mut serial,
                Instant::now()
            ),
            StartOutcome::Started
        );
        // Nothing reaches the endpoint on screen when the move starts, so its server keeps
        // the client viewed and focused (asserted at the top of this pass).
        assert!(previous_sent.lock().expect("messages").is_empty());
        let messages = std::mem::take(&mut *sent.lock().expect("messages"));
        assert!(matches!(
            messages.first(),
            Some(shepr_protocol::ClientMessage::ClientShellResize { .. })
        ));
        assert!(matches!(
            messages.last(),
            Some(shepr_protocol::ClientMessage::ClientShellEndpointRequest {
                command: shepr_protocol::command::EndpointCommand::ClientShellSurfaceSet(
                    shepr_protocol::command::ClientShellSurfaceSetParams { active: true }
                ),
                ..
            })
        ));
        assert!(
            !messages
                .iter()
                .any(|m| matches!(m, shepr_protocol::ClientMessage::ClientShellFocus { .. }))
        );
        // The view request's ID is allocated opaquely (no ":on" suffix to
        // recognise), so the acknowledgement is matched to the sent request.
        let Some(shepr_protocol::ClientMessage::ClientShellEndpointRequest {
            request_id: view_request_id,
            ..
        }) = messages.last()
        else {
            panic!("expected the view request last");
        };
        let view_request_id = view_request_id.clone();
        headless_tests::dispatch_lifecycle_messages(server, client, messages);
        headless_tests::render_now(server);
        let mut ack = false;
        let mut snapshot = false;
        let deadline = Instant::now() + SERVER_RESPONSE_TIMEOUT;
        while !ack || !snapshot {
            match recv_server_message_until(control, deadline, "view evidence") {
                ServerMessage::ClientShellEndpointResponse {
                    boot_id,
                    request_id,
                    result,
                } if request_id == view_request_id => {
                    shell
                        .endpoint_choice_mut()
                        .preparing_mut()
                        .expect("preparing")
                        .receive_response(&to, generation, &boot_id, &request_id, result);
                    ack = true;
                }
                ServerMessage::EndpointSnapshot(s) => {
                    shell
                        .endpoint_choice_mut()
                        .preparing_mut()
                        .expect("preparing")
                        .receive_snapshot(&to, generation, &s);
                    shell.set_endpoint_snapshot_for_generation(&to, generation, s);
                    snapshot = true;
                }
                _ => {}
            }
        }
        let ServerMessage::PaneSurface(surface) =
            recv_render_server_message(render, "view surface")
        else {
            panic!("expected full surface");
        };
        shell
            .endpoint_choice_mut()
            .preparing_mut()
            .expect("preparing")
            .receive_surface(&to, generation, surface);
        assert!(
            shell
                .endpoint_choice()
                .preparing()
                .expect("preparing")
                .ready()
                .is_some()
        );
        view::send_focus(shell.endpoint_choice_mut(), &mut endpoints);
        assert!(
            view::commit_move(&mut endpoints, &mut shell, true)
                .expect("commit")
                .is_some()
        );
        assert_eq!(shell.endpoint_choice().presented(), &to);
        assert!(shell.endpoint_is_active(&to));
        assert!(endpoints.viewed(&to));
        let messages = std::mem::take(&mut *sent.lock().expect("messages"));
        assert!(matches!(
            messages.as_slice(),
            [
                shepr_protocol::ClientMessage::ClientShellFocus { focused: true },
                shepr_protocol::ClientMessage::ReplayHostEffects
            ]
        ));
        headless_tests::dispatch_lifecycle_messages(server, client, messages);
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
        assert_eq!(
            headless_tests::outer_terminal_focus(&source_server, source_client_id),
            Some(true)
        );
        assert_eq!(
            headless_tests::outer_terminal_focus(&target_server, target_client_id),
            Some(true)
        );
        assert_eq!(
            view::release_unwanted(shell.endpoint_choice(), &mut endpoints, &shell, &mut serial),
            1
        );
        let messages = std::mem::take(&mut *previous_sent.lock().expect("messages"));
        if returning {
            headless_tests::dispatch_lifecycle_messages(
                &mut target_server,
                target_client_id,
                messages,
            );
            assert_eq!(
                headless_tests::outer_terminal_focus(&target_server, target_client_id),
                Some(false)
            );
            assert!(!headless_tests::client_is_viewed(
                &target_server,
                target_client_id
            ));
            assert!(headless_tests::client_is_viewed(
                &source_server,
                source_client_id
            ));
        } else {
            headless_tests::dispatch_lifecycle_messages(
                &mut source_server,
                source_client_id,
                messages,
            );
            assert_eq!(
                headless_tests::outer_terminal_focus(&source_server, source_client_id),
                Some(false)
            );
            assert!(!headless_tests::client_is_viewed(
                &source_server,
                source_client_id
            ));
            assert!(headless_tests::client_is_viewed(
                &target_server,
                target_client_id
            ));
        }
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
