//! A machine move driven through the loop: what each connection is sent, what is shown and
//! viewed at each turn, and how a move ends when a step fails.

use super::Fixture;
use crate::client_loop::ClientLoopAction;
use crate::endpoint::{self, ClientEndpointId};
use crate::events::{ClientLoopEvent, ParsedHostInput};
use crate::shell;
use crate::shell_runtime::{ShellInputDisposition, finish_client_shell_input, resize_views};
use crate::tests::endpoints::{boot, remote, snapshot, surface};
use crate::tests::{test_generation, test_pane_id, test_workspace_id};
use shepr_protocol::{
    ClientMessage, ServerMessage,
    command::{EndpointCommand, EndpointReply},
};
use std::io;

fn off(messages: &[ClientMessage]) -> bool {
    messages.iter().any(|m| matches!(m, ClientMessage::ClientShellEndpointRequest { command: EndpointCommand::ClientShellSurfaceSet(p), .. } if !p.active))
}

#[test]
fn selecting_a_machine_turns_it_on_and_leaves_the_source_live_until_commit() {
    let mut f = Fixture::new();
    f.start();
    assert!(f.local.take().is_empty());
    f.inbound(
        &ClientEndpointId::Local,
        ServerMessage::PaneSurface(surface(&ClientEndpointId::Local, 1, f.size(), "LIVE")),
    );
    assert!(f.output().contains("LIVE"));
    f.input(ClientMessage::ClientShellPaneInput {
        pane_id: test_pane_id("w1:p1"),
        events: vec![shepr_protocol::ClientPaneInputEvent::Paste("typing".into())],
    });
    assert!(matches!(
        f.local.take().as_slice(),
        [ClientMessage::ClientShellPaneInput { .. }]
    ));
    assert!(f.client.hub().registry().viewed(&ClientEndpointId::Local));
}
#[test]
fn the_source_is_released_after_the_commit_and_not_before() {
    let mut f = Fixture::new();
    f.start();
    assert!(f.local.take().is_empty());
    f.commit();
    let messages = f.local.take();
    assert!(matches!(
        messages.first(),
        Some(ClientMessage::ClientShellFocus { focused: false })
    ));
    assert!(off(&messages));
    f.assert_views();
}
#[test]
fn commit_sends_the_focus_baseline_then_replay_to_the_target() {
    let mut f = Fixture::new();
    f.start();
    f.evidence();
    f.target.take();
    f.reconcile();
    assert!(matches!(
        f.target.take().as_slice(),
        [
            ClientMessage::ClientShellFocus { focused: true },
            ClientMessage::ReplayHostEffects
        ]
    ));
}
#[test]
fn target_host_effects_are_dropped_until_commit_and_the_replay_applies_after() {
    let mut f = Fixture::new();
    f.start();
    f.clear_output();
    for message in [
        ServerMessage::MouseCapture {
            mode: shepr_term::mouse::HostMouseCapture::Cells,
        },
        ServerMessage::ClientShellKeyboardReportAll { enabled: true },
        ServerMessage::Clipboard {
            data: b"text".to_vec(),
        },
    ] {
        f.inbound(&remote(), message);
    }
    assert!(f.output().is_empty());
    assert!(!f.client.state().host_modes.keyboard_report_all_active());
    f.commit();
    f.clear_output();
    f.inbound(
        &remote(),
        ServerMessage::ClientShellKeyboardReportAll { enabled: true },
    );
    assert!(!f.output().is_empty());
    assert!(f.client.state().host_modes.keyboard_report_all_active());
}
#[test]
fn a_failed_move_releases_the_target() {
    for cause in ["ack", "timeout", "focus"] {
        let mut f = Fixture::new();
        if cause == "focus" {
            f.select(shell::Location::workspace(
                remote(),
                test_workspace_id("w1"),
            ));
            f.reconcile();
        } else {
            f.start();
        }
        if cause == "timeout" {
            f.now += crate::limits::ENDPOINT_MOVE_TIMEOUT;
        } else {
            let request_id = if cause == "ack" {
                f.on_request()
            } else {
                f.target
                    .sent
                    .lock()
                    .expect("messages")
                    .iter()
                    .find_map(|m| match m {
                        ClientMessage::ClientShellEndpointRequest {
                            request_id,
                            command: EndpointCommand::WorkspaceFocus(_),
                            ..
                        } => Some(request_id.clone()),
                        _ => None,
                    })
                    .expect("focus request")
            };
            f.inbound(
                &remote(),
                ServerMessage::ClientShellEndpointResponse {
                    boot_id: boot(&remote()),
                    request_id,
                    result: Ok(EndpointReply::Done),
                },
            );
        }
        f.reconcile();
        assert!(off(&f.target.take()));
        assert!(f.client.hub().registry().connection(&remote()).is_some());
        assert_eq!(
            f.client.state().shell.endpoints.choice().live(),
            Some(&ClientEndpointId::Local)
        );
        f.assert_views();
    }
}
#[test]
fn a_move_timeout_returns_to_the_source_and_reports_it() {
    let mut f = Fixture::new();
    f.start();
    f.now += crate::limits::ENDPOINT_MOVE_TIMEOUT;
    f.reconcile();
    assert_eq!(
        f.client.state().shell.endpoints.choice().live(),
        Some(&ClientEndpointId::Local)
    );
    assert!(f.output().contains("coherent surface in time"));
}
#[test]
fn a_failure_queued_at_the_deadline_reports_the_interruption() {
    let mut f = Fixture::new();
    f.start();
    f.now += crate::limits::ENDPOINT_MOVE_TIMEOUT;
    f.client
        .handle_event(
            ClientLoopEvent::ServerDisconnected {
                endpoint_id: remote(),
                generation: test_generation(7),
                error: io::Error::new(io::ErrorKind::BrokenPipe, "lost"),
            },
            f.now,
        )
        .expect("disconnect");
    f.reconcile();
    assert!(f.output().contains("machine switch interrupted"));
    assert!(!f.output().contains("coherent surface in time"));
}
#[test]
fn a_send_failure_while_preparing_with_nothing_shown_waits_for_a_new_connection() {
    let mut f = Fixture::new();
    f.lose_local();
    f.pick(remote());
    f.target.fail_next();
    f.reconcile();
    assert!(
        f.client
            .state()
            .shell
            .endpoints
            .choice()
            .preparing()
            .is_some()
    );
    f.reconcile();
    assert!(
        f.client
            .state()
            .shell
            .endpoints
            .choice()
            .pending_start()
            .is_some()
    );
    assert!(f.client.state().shell.endpoints.choice().live().is_none());
    assert!(f.client.hub().registry().connection(&remote()).is_none());
}
#[test]
fn a_failed_commit_send_completes_the_switch_and_then_reports_the_loss() {
    let mut f = Fixture::new();
    f.start();
    f.evidence();
    f.target.fail_next();
    f.reconcile();
    assert_eq!(
        f.client.state().shell.endpoints.choice().live(),
        Some(&remote())
    );
    assert!(f.client.state().shell.endpoint_is_active(&remote()));
    f.reconcile();
    assert!(f.client.state().shell.endpoints.choice().live().is_none());
    assert!(f.output().contains("connection was lost"));
}
#[test]
fn shown_implies_viewed_across_every_transition() {
    let mut f = Fixture::new();
    f.assert_views();
    f.start();
    f.assert_views();
    f.pick(ClientEndpointId::Local);
    f.reconcile();
    f.assert_views();
    f.start();
    f.commit();
    f.assert_views();
    f.client.hub_mut().registry_mut().fail(
        &remote(),
        &io::Error::new(io::ErrorKind::BrokenPipe, "lost"),
    );
    f.reconcile();
    f.assert_views();
    let mut f = Fixture::new();
    f.target.fail_next();
    f.start();
    f.assert_views();
    f.reconcile();
    f.assert_views();
    let mut f = Fixture::new();
    f.start();
    f.evidence();
    f.target.fail_next();
    f.reconcile();
    f.assert_views();
    f.reconcile();
    f.assert_views();
}
#[test]
fn selecting_local_while_a_remote_prepares_releases_the_remote() {
    let mut f = Fixture::new();
    f.start();
    f.pick(ClientEndpointId::Local);
    f.reconcile();
    assert!(off(&f.target.take()));
    assert!(f.local.take().is_empty());
}
#[test]
fn local_selection_waits_for_metadata_while_the_shown_endpoint_stays_live() {
    let mut f = Fixture::new();
    f.start();
    f.commit();
    f.client.hub_mut().registry_mut().insert(
        ClientEndpointId::Local,
        f.local.clone(),
        test_generation(2),
        false,
        f.now,
    );
    f.pick(ClientEndpointId::Local);
    f.reconcile();
    assert_eq!(
        f.client.state().shell.endpoints.choice().live(),
        Some(&remote())
    );
    assert!(
        f.client
            .state()
            .shell
            .endpoints
            .choice()
            .pending_start()
            .is_some()
    );
    // Wide enough that the notice, which wraps within the pane side, keeps its
    // sentence on one line.
    let frame = f
        .client
        .state()
        .shell
        .compose_frame(200, 30)
        .expect("chrome");
    f.client.state_mut().present_frame(frame);
    assert!(f.output().contains(&format!(
        "{} is waiting for its workspace snapshot; selection will resume when it is ready",
        shepr_test_fixtures::FIXTURE_LOCAL_LABEL
    )));
}
#[test]
fn a_remote_pick_without_metadata_waits_with_a_notice() {
    let mut f = Fixture::new();
    f.client.hub_mut().registry_mut().insert(
        remote(),
        f.target.clone(),
        test_generation(8),
        false,
        f.now,
    );
    let (state, hub) = f.client.parts_mut();
    let dispatched = hub.dispatch(
        &mut state.shell,
        vec![shell::ClientShellAction::ActivateEndpoint(
            shell::Location::machine(remote()),
        )],
        f.now,
    );
    assert!(dispatched.repaint.is_needed());
    f.reconcile();
    assert!(
        f.client
            .state()
            .shell
            .endpoints
            .choice()
            .pending_start()
            .is_some()
    );
    let frame = f
        .client
        .state()
        .shell
        .compose_frame(100, 30)
        .expect("chrome");
    f.client.state_mut().present_frame(frame);
    assert!(
        f.output()
            .contains("build is waiting for its workspace snapshot")
    );
}
#[test]
fn a_remote_pick_without_a_connection_is_abandoned_with_one_notice() {
    let mut f = Fixture::new();
    f.client.hub_mut().registry_mut().disconnect(&remote());
    f.pick(remote());
    let frame = f
        .client
        .state()
        .shell
        .compose_frame(100, 30)
        .expect("chrome");
    f.client.state_mut().present_frame(frame);
    assert!(
        !f.output().contains("selection will resume"),
        "a pick the reconcile abandons must not promise to resume"
    );
    f.reconcile();
    assert_eq!(
        f.client.state().shell.endpoints.choice().live(),
        Some(&ClientEndpointId::Local)
    );
    assert!(
        f.client
            .state()
            .shell
            .endpoints
            .choice()
            .pending_start()
            .is_none()
    );
    assert!(f.output().contains("build is not ready"));
}
#[test]
fn a_newer_selection_replaces_a_waiting_one() {
    let mut f = Fixture::new();
    // Nothing is shown and the choice waits for Local's next connection.
    f.lose_local();
    assert_eq!(
        f.client
            .state()
            .shell
            .endpoints
            .choice()
            .pending_start()
            .expect("waiting")
            .to,
        &ClientEndpointId::Local
    );
    f.pick(remote());
    f.reconcile();
    assert_eq!(
        f.client
            .state()
            .shell
            .endpoints
            .choice()
            .preparing()
            .expect("preparing")
            .lease()
            .endpoint_id,
        remote()
    );
}
#[test]
fn pane_input_and_commands_go_only_to_the_shown_endpoint() {
    let mut f = Fixture::new();
    f.start();
    let input = ClientMessage::ClientShellPaneInput {
        pane_id: test_pane_id("w1:p1"),
        events: vec![shepr_protocol::ClientPaneInputEvent::Paste("input".into())],
    };
    f.input(input.clone());
    let actions = f
        .client
        .state_mut()
        .shell
        .focus_endpoint_target(shell::LocationTarget::Workspace(test_workspace_id("w1")));
    let (state, hub) = f.client.parts_mut();
    hub.dispatch(&mut state.shell, actions, f.now);
    let source = f.local.take();
    assert!(source.contains(&input));
    assert!(source.iter().any(|m| matches!(
        m,
        ClientMessage::ClientShellEndpointRequest {
            command: EndpointCommand::WorkspaceFocus(_),
            ..
        }
    )));
    assert!(
        !f.target
            .take()
            .iter()
            .any(|m| matches!(m, ClientMessage::ClientShellPaneInput { .. }))
    );
}
#[test]
fn commit_retires_the_previous_command_lane() {
    let mut f = Fixture::new();
    let actions = f
        .client
        .state_mut()
        .shell
        .focus_endpoint_target(shell::LocationTarget::Workspace(test_workspace_id("w1")));
    let request_id = match &actions[0] {
        shell::ClientShellAction::Endpoint { request, .. } => request.id.clone(),
        _ => panic!("command"),
    };
    let (state, hub) = f.client.parts_mut();
    hub.dispatch(&mut state.shell, actions, f.now);
    f.start();
    f.commit();
    assert!(!f.client.state().shell.has_request(&request_id));
    assert_eq!(
        f.client
            .hub_mut()
            .commands_mut()
            .disconnect(&ClientEndpointId::Local),
        endpoint::commands::EndpointCommandCancellation::default()
    );
}
#[test]
fn a_resize_reaches_every_viewed_connection_and_drops_the_recorded_surface() {
    let mut f = Fixture::new();
    f.start();
    f.evidence();
    f.local.take();
    f.target.take();
    f.client
        .handle_event(
            ClientLoopEvent::Resize(shepr_core::geometry::HostGeometry::new(
                shepr_core::geometry::GridSize::clamped(101, 31),
                shepr_core::geometry::HostCell::from_host(8, 16, false),
            )),
            f.now,
        )
        .expect("resize");
    assert!(
        f.client
            .state()
            .shell
            .endpoints
            .choice()
            .preparing()
            .expect("preparing")
            .ready()
            .is_none()
    );
    for sent in [f.local.take(), f.target.take()] {
        assert!(matches!(
            sent.as_slice(),
            [ClientMessage::ClientShellResize { .. }]
        ));
    }
}
#[test]
fn a_resize_with_an_unchanged_geometry_keeps_the_move_evidence() {
    let mut f = Fixture::new();
    f.start();
    f.evidence();
    let geometry = f.client.state().reported_geometry;
    f.client
        .handle_event(ClientLoopEvent::Resize(geometry), f.now)
        .expect("resize");
    assert!(
        f.client
            .state()
            .shell
            .endpoints
            .choice()
            .preparing()
            .expect("preparing")
            .ready()
            .is_some()
    );
    f.reconcile();
    assert_eq!(
        f.client.state().shell.endpoints.choice().live(),
        Some(&remote())
    );
}
#[test]
fn host_theme_updates_reach_a_new_target_before_its_on_request() {
    let mut f = Fixture::new();
    let update = shepr_protocol::ClientHostThemeUpdate::Appearance(
        shepr_protocol::ClientHostAppearance::Dark,
    );
    f.input(ClientMessage::ClientShellHostTheme {
        update: update.clone(),
    });
    f.start();
    let sent = f.target.take();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ClientShellResize { .. }, ClientMessage::ClientShellHostTheme { update: got }, ClientMessage::ClientShellEndpointRequest { .. }] if got == &update)
    );
}
#[test]
fn every_viewed_connection_gets_the_one_surface_geometry() {
    let mut f = Fixture::new();
    f.start();
    f.local.take();
    f.target.take();
    let (state, hub) = f.client.parts_mut();
    resize_views(state, hub);
    assert_eq!(f.local.take(), f.target.take());
}
#[test]
fn a_reconnected_local_is_prepared_once_per_connection() {
    let mut f = Fixture::new();
    f.lose_local();
    f.client.hub_mut().registry_mut().insert(
        ClientEndpointId::Local,
        f.local.clone(),
        test_generation(2),
        false,
        f.now,
    );
    f.reconcile();
    assert!(
        f.client
            .state()
            .shell
            .endpoints
            .choice()
            .preparing()
            .is_none()
    );
    f.inbound(
        &ClientEndpointId::Local,
        ServerMessage::EndpointSnapshot(snapshot(&ClientEndpointId::Local, 1)),
    );
    f.reconcile();
    assert!(
        f.client
            .state()
            .shell
            .endpoints
            .choice()
            .preparing()
            .is_some()
    );
    f.local.take();
    f.reconcile();
    assert!(f.local.take().is_empty());
}
#[test]
fn an_interactive_detach_goes_to_the_shown_endpoint() {
    let mut f = Fixture::new();
    f.start();
    f.target.take();
    let (state, hub) = f.client.parts_mut();
    let detached = finish_client_shell_input(
        state,
        shell::ClientShellInput {
            detach: true,
            ..Default::default()
        },
        hub,
        f.now,
    )
    .expect("detach");
    assert_eq!(detached, ShellInputDisposition::Detach);
    assert!(matches!(f.local.take().as_slice(), [ClientMessage::Detach]));
    assert!(f.target.take().is_empty());
}
/// The Detach key (prefix, then q) ends the loop as a detach, which the run
/// reports so the binary can say how to get back; a quit event ends it as a
/// plain exit.
#[test]
fn the_detach_key_ends_the_loop_as_a_detach() {
    let mut f = Fixture::new();
    let inputs = shepr_test_fixtures::parse_raw_input_bytes_sync(b"\x02q")
        .into_iter()
        .map(|event| ParsedHostInput {
            event,
            pixel_mouse: None,
            keyboard_mode: shepr_termio::input::HostKeyboardInputMode::default(),
        })
        .collect();
    let action = f
        .client
        .handle_event(ClientLoopEvent::StdinInput(inputs), f.now)
        .expect("detach key");
    assert!(matches!(action, ClientLoopAction::Detach));
    assert!(matches!(
        f.local.take().as_slice(),
        [.., ClientMessage::Detach]
    ));
    let quit = f
        .client
        .handle_event(ClientLoopEvent::Quit, f.now)
        .expect("quit");
    assert!(matches!(quit, ClientLoopAction::Exit));
}
#[test]
fn the_shell_projects_the_shown_endpoint() {
    let mut f = Fixture::new();
    f.start();
    assert!(
        f.client
            .state()
            .shell
            .endpoint_is_active(&ClientEndpointId::Local)
    );
    f.pick(ClientEndpointId::Local);
    f.reconcile();
    f.assert_views();
    f.start();
    f.commit();
    f.assert_views();
    f.client.hub_mut().registry_mut().fail(
        &remote(),
        &io::Error::new(io::ErrorKind::BrokenPipe, "lost"),
    );
    f.reconcile();
    assert!(f.client.state().shell.endpoints.choice().live().is_none());
    assert!(f.client.state().shell.endpoint_is_active(&remote()));
}
#[test]
fn local_selection_never_waits_for_a_remote() {
    for failed in [false, true] {
        let mut f = Fixture::new();
        f.start();
        if failed {
            f.target.fail_next();
        }
        f.pick(ClientEndpointId::Local);
        f.reconcile();
        assert_eq!(
            f.client.state().shell.endpoints.choice().live(),
            Some(&ClientEndpointId::Local)
        );
        assert!(
            f.client
                .state()
                .shell
                .endpoints
                .choice()
                .preparing()
                .is_none()
        );
        f.assert_views();
    }
}

#[test]
fn a_failed_local_proof_waits_for_another_generation() {
    let mut f = Fixture::new();
    f.lose_local();
    f.client.hub_mut().registry_mut().insert(
        ClientEndpointId::Local,
        f.local.clone(),
        test_generation(2),
        false,
        f.now,
    );
    f.inbound(
        &ClientEndpointId::Local,
        ServerMessage::EndpointSnapshot(snapshot(&ClientEndpointId::Local, 1)),
    );
    f.reconcile();
    f.now += crate::limits::ENDPOINT_MOVE_TIMEOUT;
    f.reconcile();
    f.local.take();
    f.reconcile();
    assert!(f.local.take().is_empty());
    assert_eq!(
        f.client
            .state()
            .shell
            .endpoints
            .choice()
            .pending_start()
            .expect("failed")
            .failed_generation,
        Some(test_generation(2))
    );
    f.client.hub_mut().registry_mut().insert(
        ClientEndpointId::Local,
        f.local.clone(),
        test_generation(3),
        false,
        f.now,
    );
    f.inbound(
        &ClientEndpointId::Local,
        ServerMessage::EndpointSnapshot(snapshot(&ClientEndpointId::Local, 1)),
    );
    f.reconcile();
    assert_eq!(
        f.client
            .state()
            .shell
            .endpoints
            .choice()
            .preparing()
            .expect("preparing")
            .lease()
            .generation,
        test_generation(3)
    );
}

#[test]
fn host_focus_changes_only_reach_the_shown_endpoint_until_commit() {
    let mut f = Fixture::new();
    f.start();
    let request_id = f.on_request();
    f.target.take();
    f.client
        .handle_event(
            ClientLoopEvent::StdinInput(vec![ParsedHostInput {
                event: shepr_termio::input::raw_input::RawInputEvent::OuterFocusLost,
                pixel_mouse: None,
                keyboard_mode: shepr_termio::input::HostKeyboardInputMode::default(),
            }]),
            f.now,
        )
        .expect("host focus");
    assert!(matches!(
        f.local.take().as_slice(),
        [ClientMessage::ClientShellFocus { focused: false }]
    ));
    assert!(f.target.take().is_empty());
    f.evidence_for(request_id);
    f.reconcile();
    assert!(matches!(
        f.target.take().as_slice(),
        [
            ClientMessage::ClientShellFocus { focused: false },
            ClientMessage::ReplayHostEffects
        ]
    ));
}

#[test]
fn host_theme_changes_reach_both_viewed_endpoints_during_a_move() {
    let mut f = Fixture::new();
    f.start();
    f.target.take();
    let update = ClientMessage::ClientShellHostTheme {
        update: shepr_protocol::ClientHostThemeUpdate::Appearance(
            shepr_protocol::ClientHostAppearance::Light,
        ),
    };
    f.input(update.clone());
    assert_eq!(f.local.take(), vec![update.clone()]);
    assert_eq!(f.target.take(), vec![update]);
}

#[test]
fn navigation_is_acknowledged_and_in_the_first_committed_projection() {
    let mut f = Fixture::new();
    f.select(shell::Location::workspace(
        remote(),
        test_workspace_id("w1"),
    ));
    f.reconcile();
    let request_id = f
        .target
        .sent
        .lock()
        .expect("messages")
        .iter()
        .find_map(|message| match message {
            ClientMessage::ClientShellEndpointRequest {
                request_id,
                command: EndpointCommand::WorkspaceFocus(_),
                ..
            } => Some(request_id.clone()),
            _ => None,
        })
        .expect("navigation request");
    f.evidence();
    f.reconcile();
    assert_eq!(
        f.client.state().shell.endpoints.choice().live(),
        Some(&ClientEndpointId::Local)
    );
    f.inbound(
        &remote(),
        ServerMessage::ClientShellEndpointResponse {
            boot_id: boot(&remote()),
            request_id,
            result: Ok(EndpointReply::WorkspaceInfo {
                workspace: shepr_protocol::command::WorkspaceInfo {
                    workspace_id: test_workspace_id("w1"),
                    label: "target".into(),
                    pane_count: 1,
                    agent_status: shepr_protocol::AgentStatus::Idle,
                },
            }),
        },
    );
    f.reconcile();
    assert_eq!(
        f.client.state().shell.endpoints.choice().live(),
        Some(&remote())
    );
    assert!(f.client.state().shell.endpoint_is_active(&remote()));
    assert!(f.output().contains("TARGET"));
}
