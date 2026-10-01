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
fn a_pick_is_applied_at_once_without_an_event_round_trip() {
    use crate::endpoint::{EndpointChoice, EndpointRegistry, commands::EndpointCommands};
    let mut endpoints = EndpointRegistry::new(TestTransport { fail: false }, 1);
    let mut commands = EndpointCommands::default();
    let mut choice = EndpointChoice::waiting_for(ClientEndpointId::Local);
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
        &mut choice,
        &mut std::io::sink(),
        false,
        &mut shell,
        std::time::Instant::now(),
    );
    assert_eq!(
        choice.pending_start().expect("waiting pick").to,
        &ClientEndpointId::Local
    );
    assert!(choice.shown().is_none());
}

#[test]
fn selecting_the_shown_endpoint_is_a_noop_but_with_nothing_shown_it_reproves() {
    use crate::endpoint::{EndpointChoice, EndpointRegistry, commands::EndpointCommands};
    for shown in [false, true] {
        let mut endpoints = EndpointRegistry::new(TestTransport { fail: false }, 1);
        let mut commands = EndpointCommands::default();
        let mut choice = if shown {
            EndpointChoice::showing(ClientEndpointId::Local)
        } else {
            // Nothing shown, and a proof of Local already failed on this generation: only an
            // explicit pick may retry it there.
            let mut choice = EndpointChoice::waiting_for(ClientEndpointId::Local);
            choice.begin_preparing(
                crate::endpoint::ViewLease {
                    endpoint_id: ClientEndpointId::Local,
                    generation: 1,
                    boot_id: crate::tests::test_boot_id("boot-1"),
                    minimum_revision: 1,
                },
                "client-shell-view:1:on".into(),
                shepr_protocol::TerminalGeometry::new(80, 24, 8, 16, false),
                std::time::Instant::now(),
            );
            choice.fail_move();
            assert_eq!(
                choice
                    .pending_start()
                    .expect("failed proof")
                    .failed_generation,
                Some(1)
            );
            choice
        };
        let mut shell =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        crate::shell_runtime::dispatch_client_shell_actions(
            vec![ClientShellAction::ActivateEndpoint {
                endpoint_id: ClientEndpointId::Local,
                target: None,
            }],
            &mut commands,
            &mut endpoints,
            &mut choice,
            &mut std::io::sink(),
            false,
            &mut shell,
            std::time::Instant::now(),
        );
        if shown {
            assert!(choice.pending_start().is_none());
            assert_eq!(choice.shown(), Some(&ClientEndpointId::Local));
        } else {
            assert_eq!(
                choice
                    .pending_start()
                    .expect("rearmed proof")
                    .failed_generation,
                None
            );
        }
    }
}

#[test]
fn dispatcher_cancels_pending_requests_on_an_unviewed_endpoint_or_failed_send() {
    use crate::endpoint::EndpointRegistry;
    use crate::endpoint::commands::EndpointCommands;

    for fail_send in [false, true] {
        let (mut state, actions) = pending_request();
        let mut endpoints = EndpointRegistry::new(TestTransport { fail: fail_send }, 1);
        endpoints.set_viewed(&ClientEndpointId::Local, fail_send);
        let mut commands = EndpointCommands::default();
        let mut choice = crate::endpoint::EndpointChoice::showing(ClientEndpointId::Local);
        let repaint = crate::shell_runtime::dispatch_client_shell_actions(
            actions,
            &mut commands,
            &mut endpoints,
            &mut choice,
            &mut std::io::sink(),
            false,
            &mut state,
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
    assert_eq!(
        commands.response_kind(
            &ClientEndpointId::Local,
            1,
            &crate::tests::test_boot_id("boot-1"),
            &stale_id
        ),
        crate::endpoint::commands::CommandResponseKind::Untracked
    );
    assert_eq!(
        commands.response_kind(
            &ClientEndpointId::Local,
            2,
            &crate::tests::test_boot_id("boot-1"),
            &current_id
        ),
        crate::endpoint::commands::CommandResponseKind::Active
    );
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
