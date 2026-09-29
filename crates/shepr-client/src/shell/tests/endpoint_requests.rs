use super::*;
use crate::endpoint::ClientEndpointId;

fn pending_request() -> (ClientShellState, Vec<ClientShellAction>) {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
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
    crate::shell_runtime::dispatch_client_shell_actions(
        vec![ClientShellAction::ActivateEndpoint {
            endpoint_id: ClientEndpointId::Local,
            target: Some(ClientEndpointFocusTarget::Workspace("w1".into())),
        }],
        &mut commands,
        &mut endpoints,
        &mut std::io::sink(),
        false,
        None,
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
            &mut std::io::sink(),
            false,
            Some(&mut state),
            &mut scheduled,
            std::time::Instant::now(),
        );
        assert!(repaint);
        assert!(state.pending_requests.is_empty());
        assert!(
            state
                .visible_endpoint_notice
                .as_ref()
                .is_some_and(|notice| { notice.title == "Action interrupted" })
        );
        assert!(commands.disconnect(&ClientEndpointId::Local).is_empty());
    }
}

#[test]
fn stale_queued_request_is_cancelled_without_blocking_the_current_generation() {
    use crate::endpoint::EndpointRegistry;
    use crate::endpoint::commands::EndpointCommands;

    let (mut state, actions) = pending_request();
    let stale_id = request_id(&actions).to_owned();
    let current = state.focus_endpoint_target(ClientEndpointFocusTarget::Workspace("w1".into()));
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
    assert_eq!(cancelled, vec![stale_id.clone()]);
    state.cancel_endpoint_request(&stale_id);
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
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.selection = Some(shepr_vt::selection::Selection::range(
        test_pane_id("w1:p1"),
        shepr_vt::Point::new(shepr_vt::AbsRow(0), 0),
        shepr_vt::Point::new(shepr_vt::AbsRow(0), 2),
    ));
    for result in [
        Ok(shepr_api::schema::ResponseResult::PaneSelection {
            pane_id: "w1:p1".into(),
            text: String::new(),
        }),
        Err(ClientShellEndpointError {
            code: Some("endpoint_cancelled".into()),
            message: "cancelled".into(),
        }),
        Err(ClientShellEndpointError {
            code: Some("selection_unavailable".into()),
            message: "selection text is unavailable".into(),
        }),
    ] {
        let mut outcome = ClientShellInput::default();
        state.request_selection_copy(&mut outcome, false);
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
