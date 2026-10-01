use super::*;
use crate::endpoint::ClientEndpointId;

fn pending_request() -> (ClientShellState, Vec<ClientShellAction>) {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    submit_request(state)
}

fn submit_request(mut state: ClientShellState) -> (ClientShellState, Vec<ClientShellAction>) {
    state.open_rename_workspace_overlay();
    state.handle_input_bytes(b"renamed");
    let outcome = state.handle_input_bytes(b"\r");
    (state, outcome.actions)
}

fn request_id(actions: &[ClientShellAction]) -> &str {
    let [ClientShellAction::Endpoint { request, .. }] = actions else {
        panic!("expected one endpoint request");
    };
    &request.id
}

#[test]
fn cancelled_scroll_rolls_back_queued_target_even_without_a_presented_snapshot() {
    for missing_snapshot in [false, true] {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.set_snapshot(Box::new(snapshot()));
        state.set_pane_surface(surface());
        let pane_id = test_pane_id("w1:p1");
        let mut first = ClientShellInput::default();
        state.push_pane_scroll_offset(pane_id.clone(), 3, &mut first);
        let id = request_id(&first.actions).to_owned();
        let mut queued = ClientShellInput::default();
        state.push_pane_scroll_offset(pane_id.clone(), 7, &mut queued);
        assert!(queued.actions.is_empty());
        assert_eq!(state.pane_scroll_queued.get(&pane_id), Some(&7));
        if missing_snapshot {
            state.snapshot = None;
        }

        let cancelled = state.handle_endpoint_result_at(
            &crate::tests::test_boot_id("boot-1"),
            &id,
            Err(ClientShellEndpointError::Cancelled),
            std::time::Instant::now(),
        );

        assert!(cancelled.repaint);
        assert!(cancelled.actions.is_empty());
        assert!(cancelled.requests.is_empty());
        assert!(state.pending_requests.is_empty());
        assert!(state.pane_scroll_in_flight.is_empty());
        assert!(state.pane_scroll_queued.is_empty());
        assert!(state.pane_scroll_targets.is_empty());
        assert!(state.visible_endpoint_notice.is_none());
        state.set_snapshot(Box::new(snapshot()));
        let mut next = ClientShellInput::default();
        state.push_pane_scroll_offset(pane_id.clone(), 2, &mut next);
        assert!(matches!(
            next.actions.as_slice(),
            [ClientShellAction::Endpoint { .. }]
        ));
        assert!(state.pane_scroll_in_flight.contains_key(&pane_id));
    }
}

#[test]
fn mismatched_boot_scroll_result_rolls_back_queued_scroll_state() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let pane_id = test_pane_id("w1:p1");
    let mut first = ClientShellInput::default();
    state.push_pane_scroll_offset(pane_id.clone(), 3, &mut first);
    let id = request_id(&first.actions).to_owned();
    let mut queued = ClientShellInput::default();
    state.push_pane_scroll_offset(pane_id.clone(), 7, &mut queued);
    assert!(state.pane_scroll_queued.contains_key(&pane_id));

    let outcome = state.handle_endpoint_result_at(
        "replacement-boot",
        &id,
        Err(ClientShellEndpointError::Server(
            shepr_protocol::command::EndpointError::StaleBoot,
        )),
        std::time::Instant::now(),
    );

    assert!(outcome.repaint);
    assert!(state.pending_requests.is_empty());
    assert!(state.pane_scroll_in_flight.is_empty());
    assert!(state.pane_scroll_queued.is_empty());
    assert!(state.pane_scroll_targets.is_empty());
    assert!(state.visible_endpoint_notice.is_none());
}

#[test]
fn disconnecting_a_pending_scroll_does_not_show_an_interrupted_action_notice() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let pane_id = test_pane_id("w1:p1");
    let mut first = ClientShellInput::default();
    state.push_pane_scroll_offset(pane_id.clone(), 3, &mut first);
    let mut queued = ClientShellInput::default();
    state.push_pane_scroll_offset(pane_id.clone(), 7, &mut queued);

    state.mark_endpoint_disconnected(&ClientEndpointId::Local);

    assert!(state.pending_requests.is_empty());
    assert!(state.pane_scroll_in_flight.is_empty());
    assert!(state.pane_scroll_queued.is_empty());
    assert!(state.pane_scroll_targets.is_empty());
    assert!(state.visible_endpoint_notice.is_none());
}

struct TestTransport {
    fail: bool,
}

impl crate::endpoint::EndpointTransport for TestTransport {
    fn send(&mut self, _: &ClientMessage) -> std::io::Result<()> {
        if self.fail {
            Err(std::io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
        }
    }

    fn disconnect(&mut self) {}

    fn flush(&mut self, _deadline: std::time::Instant) -> std::io::Result<()> {
        Ok(())
    }

    fn take_error(&mut self) -> Option<std::io::Error> {
        None
    }
}

#[test]
fn local_selection_is_scheduled_ahead_of_a_full_event_queue() {
    use crate::{
        ClientLoopEvent, endpoint::EndpointRegistry, endpoint::commands::EndpointCommands,
    };
    let mut endpoints = EndpointRegistry::new(TestTransport { fail: false }, 1);
    let mut commands = EndpointCommands::default();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(ClientLoopEvent::Timer)
        .expect("test precondition");
    let mut scheduled = None;
    let mut shell = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    crate::shell_runtime::dispatch_client_shell_actions(
        vec![ClientShellAction::ActivateEndpoint {
            endpoint_id: ClientEndpointId::Local,
            target: Some(ClientEndpointFocusTarget::Workspace(
                shepr_test_fixtures::id("w1"),
            )),
        }],
        &mut commands,
        &mut endpoints,
        &crate::Presentation::Owned,
        &mut std::io::sink(),
        false,
        &mut shell,
        &mut scheduled,
        std::time::Instant::now(),
    );
    let next = scheduled.take().or_else(|| rx.try_recv().ok());
    assert!(matches!(next, Some(ClientLoopEvent::ActivateEndpoint {
        endpoint_id: ClientEndpointId::Local,
        target: Some(ClientEndpointFocusTarget::Workspace(id)), ..
    }) if id == "w1"));
    assert!(matches!(rx.try_recv(), Ok(ClientLoopEvent::Timer)));
}

#[test]
fn current_owned_targetless_pick_is_a_noop_but_unowned_pick_reproves() {
    use crate::endpoint::commands::EndpointCommands;
    use crate::{ClientLoopEvent, Presentation, endpoint::EndpointRegistry};

    for (presentation, should_schedule) in [
        (Presentation::Owned, false),
        (Presentation::Unavailable, true),
    ] {
        let mut endpoints = EndpointRegistry::new(TestTransport { fail: false }, 1);
        let mut commands = EndpointCommands::default();
        let mut scheduled = None;
        let mut shell =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        crate::shell_runtime::dispatch_client_shell_actions(
            vec![ClientShellAction::ActivateEndpoint {
                endpoint_id: ClientEndpointId::Local,
                target: None,
            }],
            &mut commands,
            &mut endpoints,
            &presentation,
            &mut std::io::sink(),
            false,
            &mut shell,
            &mut scheduled,
            std::time::Instant::now(),
        );
        assert_eq!(
            matches!(scheduled, Some(ClientLoopEvent::ActivateEndpoint { .. })),
            should_schedule
        );
    }
}

#[test]
fn dispatcher_cancels_pending_requests_on_frozen_surface_or_failed_send() {
    use crate::endpoint::EndpointRegistry;
    use crate::endpoint::commands::EndpointCommands;

    for fail_send in [false, true] {
        let (mut state, actions) = pending_request();
        let mut endpoints = EndpointRegistry::new(TestTransport { fail: fail_send }, 1);
        endpoints.set_surface_active(&ClientEndpointId::Local, fail_send);
        let mut commands = EndpointCommands::default();
        let mut scheduled = None;
        let repaint = crate::shell_runtime::dispatch_client_shell_actions(
            actions,
            &mut commands,
            &mut endpoints,
            &crate::Presentation::Owned,
            &mut std::io::sink(),
            false,
            &mut state,
            &mut scheduled,
            std::time::Instant::now(),
        );
        assert!(repaint);
        assert!(state.pending_requests.is_empty());
        // A request refused before it entered the send queue has a known
        // outcome and is not reported as interrupted; one whose send failed
        // may have reached the server.
        assert_eq!(
            state
                .visible_endpoint_notice
                .as_ref()
                .is_some_and(|notice| { notice.title == "Action interrupted" }),
            fail_send
        );
        assert_eq!(
            commands.disconnect(&ClientEndpointId::Local),
            crate::endpoint::commands::EndpointCommandCancellation::default()
        );
    }
}

#[test]
fn stale_queued_request_is_cancelled_without_blocking_the_current_generation() {
    use crate::endpoint::EndpointRegistry;
    use crate::endpoint::commands::EndpointCommands;

    let (mut state, actions) = pending_request();
    let stale_id = request_id(&actions).to_owned();
    let current = state.focus_endpoint_target(ClientEndpointFocusTarget::Workspace(
        shepr_test_fixtures::id("w1"),
    ));
    let current_id = request_id(&current).to_owned();
    let mut commands = EndpointCommands::default();
    for (generation, actions) in [(1, actions), (2, current)] {
        for action in actions {
            let ClientShellAction::Endpoint {
                endpoint_id,
                boot_id,
                request,
            } = action
            else {
                panic!("expected endpoint request");
            };
            commands.enqueue(endpoint_id, generation, boot_id, request);
        }
    }
    let mut endpoints = EndpointRegistry::new(TestTransport { fail: false }, 2);
    let cancelled = commands.send_next(
        &ClientEndpointId::Local,
        &mut endpoints,
        std::time::Instant::now(),
    );
    assert_eq!(cancelled.unsent, vec![stale_id.clone()]);
    assert!(cancelled.possibly_sent.is_empty());
    state.cancel_unsent_endpoint_request(&stale_id);
    assert!(state.visible_endpoint_notice.is_none());
    assert!(!commands.accepts_response(
        &ClientEndpointId::Local,
        1,
        &crate::tests::test_boot_id("boot-1"),
        &stale_id
    ));
    assert!(commands.accepts_response(
        &ClientEndpointId::Local,
        2,
        &crate::tests::test_boot_id("boot-1"),
        &current_id
    ));
    assert!(state.pending_requests.contains_key(current_id.as_str()));
}

#[test]
fn failed_selection_copy_does_not_send_terminal_input() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.selection = Some(shepr_vt::selection::Selection::range(
        test_pane_id("w1:p1"),
        shepr_vt::Point::new(shepr_vt::AbsRow(0), 0),
        shepr_vt::Point::new(shepr_vt::AbsRow(0), 2),
    ));
    for result in [
        Ok(EndpointReply::PaneSelection {
            pane_id: shepr_test_fixtures::id("w1:p1"),
            text: String::new(),
        }),
        Err(ClientShellEndpointError::Cancelled),
        Err(ClientShellEndpointError::Server(
            shepr_protocol::command::EndpointError::Rejected(
                "selection text is unavailable".into(),
            ),
        )),
    ] {
        let mut outcome = ClientShellInput::default();
        state.request_selection_copy(&mut outcome);
        let actions = state
            .handle_endpoint_result(
                &crate::tests::test_boot_id("boot-1"),
                request_id(&outcome.actions),
                result,
            )
            .actions;
        assert!(actions.is_empty());
        assert!(state.pending_requests.is_empty());
    }
}

#[test]
fn server_errors_become_unavailable_or_rejected_notices() {
    use shepr_protocol::command::EndpointError;

    let answer = |error: EndpointError| {
        let (mut state, actions) = pending_request();
        state.handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            request_id(&actions),
            Err(ClientShellEndpointError::Server(error)),
        );
        let notice = state.visible_endpoint_notice.take().expect("notice");
        (notice.key.kind, notice.key.code, notice.title, notice.body)
    };

    let (kind, code, title, body) = answer(EndpointError::ShuttingDown);
    assert_eq!(kind, ClientEndpointNoticeKind::Unavailable);
    assert_eq!(code, "server");
    assert_eq!(title, "Server unavailable");
    assert_eq!(body, EndpointError::ShuttingDown.to_string());

    for error in [
        EndpointError::Rejected("no such pane".into()),
        EndpointError::StaleBoot,
        EndpointError::SurfaceInactive,
        EndpointError::ResponseTooLarge { size: 2, limit: 1 },
    ] {
        let (kind, code, title, body) = answer(error.clone());
        assert_eq!(kind, ClientEndpointNoticeKind::Rejected);
        assert_eq!(code, format!("workspace.rename:{error}"));
        assert_eq!(title, "Action rejected");
        assert_eq!(body, error.to_string());
    }
}
