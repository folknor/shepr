use crate::endpoint::EndpointChoice;
use crate::endpoint::EndpointRegistry;
use crate::endpoint::commands::EndpointCommandCancellation;
use crate::endpoint::commands::EndpointCommands;
use crate::shell::endpoints::ClientEndpointFocusTarget;
use crate::shell::ledger::{DropReason, Work};
use crate::shell::overlays::notices::ClientEndpointNoticeKind;
use crate::shell::state::{
    ClientCopyOperation, ClientCopySearch, ClientRenameTarget, ClientShellConfig,
    ClientShellOverlay,
};
use crossterm::event::{KeyCode, KeyModifiers};
use shepr_config::ClientConfig;
use shepr_protocol::command::EndpointCommand;

use crate::shell::state::{
    ClientShellAction, ClientShellEndpointError, ClientShellInput, ClientShellState,
};

use shepr_protocol::ClientMessage;

use shepr_protocol::command::EndpointReply;

use crate::shell::tests::{snapshot, surface};
use crate::tests::{test_pane_id, test_workspace_id};

use crate::endpoint::ClientEndpointId;

fn pending_request() -> (ClientShellState, Vec<ClientShellAction>) {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
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
        state.receive_pane_surface(surface());
        let pane_id = test_pane_id("w1:p1");
        let mut first = ClientShellInput::default();
        state.push_pane_scroll_offset(pane_id.clone(), 3, &mut first);
        let id = request_id(&first.actions).to_owned();
        let mut queued = ClientShellInput::default();
        state.push_pane_scroll_offset(pane_id.clone(), 7, &mut queued);
        assert!(queued.actions.is_empty());
        assert_eq!(state.scroll_lanes.queued(&pane_id), Some(7));
        if missing_snapshot {
            state.snapshot = None;
        }

        assert_eq!(
            state.drop_request(&id, DropReason::Interrupted),
            crate::shell::state::Repaint::Needed
        );
        assert!(state.ledger.is_empty());
        assert!(state.scroll_lanes.is_idle());
        assert!(state.notices.visible().is_none());
        state.set_snapshot(Box::new(snapshot()));
        let mut next = ClientShellInput::default();
        state.push_pane_scroll_offset(pane_id.clone(), 2, &mut next);
        assert!(matches!(
            next.actions.as_slice(),
            [ClientShellAction::Endpoint { .. }]
        ));
        assert!(state.scroll_lanes.in_flight(&pane_id));
    }
}

#[test]
fn mismatched_boot_scroll_result_rolls_back_queued_scroll_state() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    let pane_id = test_pane_id("w1:p1");
    let mut first = ClientShellInput::default();
    state.push_pane_scroll_offset(pane_id.clone(), 3, &mut first);
    let id = request_id(&first.actions).to_owned();
    let mut queued = ClientShellInput::default();
    state.push_pane_scroll_offset(pane_id.clone(), 7, &mut queued);
    assert!(state.scroll_lanes.queued(&pane_id).is_some());

    let outcome = state.answer_request(
        &crate::tests::test_boot_id("replacement-boot"),
        &id,
        Err(ClientShellEndpointError::Server(
            shepr_protocol::command::EndpointError::StaleBoot,
        )),
        std::time::Instant::now(),
    );

    assert!(outcome.repaint);
    assert!(state.ledger.is_empty());
    assert!(state.scroll_lanes.is_idle());
    assert!(state.notices.visible().is_none());
}

#[test]
fn disconnecting_a_pending_scroll_does_not_show_an_interrupted_action_notice() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    let pane_id = test_pane_id("w1:p1");
    let mut first = ClientShellInput::default();
    state.push_pane_scroll_offset(pane_id.clone(), 3, &mut first);
    let mut queued = ClientShellInput::default();
    state.push_pane_scroll_offset(pane_id.clone(), 7, &mut queued);

    state.mark_endpoint_disconnected(&ClientEndpointId::Local);

    assert!(state.ledger.is_empty());
    assert!(state.scroll_lanes.is_idle());
    assert!(state.notices.visible().is_none());
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
    let mut endpoints = EndpointRegistry::new(TestTransport { fail: false }, 1);
    let mut commands = EndpointCommands::default();
    let choice = EndpointChoice::waiting_for(ClientEndpointId::Local);
    let mut shell = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    shell.endpoints.choice = choice;
    crate::shell_runtime::dispatch_client_shell_actions(
        vec![ClientShellAction::ActivateEndpoint {
            endpoint_id: ClientEndpointId::Local,
            target: Some(ClientEndpointFocusTarget::Workspace(
                shepr_test_fixtures::id("w1"),
            )),
        }],
        &mut commands,
        &mut endpoints,
        &mut std::io::sink(),
        false,
        &mut shell,
        std::time::Instant::now(),
    )
    .expect("dispatch writes nothing to the host");
    assert_eq!(
        shell
            .endpoints
            .choice
            .pending_start()
            .expect("waiting pick")
            .to,
        &ClientEndpointId::Local
    );
    assert!(shell.endpoints.choice.live().is_none());
}

#[test]
fn selecting_the_shown_endpoint_is_a_noop_but_with_nothing_shown_it_reproves() {
    for shown in [false, true] {
        let mut endpoints = EndpointRegistry::new(TestTransport { fail: false }, 1);
        let mut commands = EndpointCommands::default();
        let choice = if shown {
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
        shell.endpoints.choice = choice;
        crate::shell_runtime::dispatch_client_shell_actions(
            vec![ClientShellAction::ActivateEndpoint {
                endpoint_id: ClientEndpointId::Local,
                target: None,
            }],
            &mut commands,
            &mut endpoints,
            &mut std::io::sink(),
            false,
            &mut shell,
            std::time::Instant::now(),
        )
        .expect("dispatch writes nothing to the host");
        if shown {
            assert!(shell.endpoints.choice.pending_start().is_none());
            assert_eq!(
                shell.endpoints.choice.live(),
                Some(&ClientEndpointId::Local)
            );
        } else {
            assert_eq!(
                shell
                    .endpoints
                    .choice
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
        let repaint = crate::shell_runtime::dispatch_client_shell_actions(
            actions,
            &mut commands,
            &mut endpoints,
            &mut std::io::sink(),
            false,
            &mut state,
            std::time::Instant::now(),
        )
        .expect("dispatch writes nothing to the host");
        assert!(repaint.is_needed());
        assert!(state.ledger.is_empty());
        // A request refused before it entered the send queue has a known
        // outcome and is not reported as interrupted; one whose send failed
        // may have reached the server.
        assert_eq!(
            state
                .notices
                .visible()
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
    state.drop_request(&stale_id, DropReason::Unsent);
    assert!(state.notices.visible().is_none());
    assert!(
        commands
            .receive_response(
                &ClientEndpointId::Local,
                1,
                &crate::tests::test_boot_id("boot-1"),
                &stale_id.clone().into(),
                Ok(EndpointReply::Done)
            )
            .is_none()
    );
    assert!(
        commands
            .receive_response(
                &ClientEndpointId::Local,
                2,
                &crate::tests::test_boot_id("boot-1"),
                &current_id.clone().into(),
                Ok(EndpointReply::Done)
            )
            .is_some()
    );
    assert!(state.ledger.contains(current_id.as_str()));
}

#[test]
fn an_expired_command_is_settled_even_when_its_connection_was_lost_first() {
    use crate::endpoint::EndpointRegistry;

    for connection_lost in [false, true] {
        let (mut state, actions) = pending_request();
        let mut endpoints = EndpointRegistry::new(TestTransport { fail: false }, 1);
        let mut commands = EndpointCommands::default();
        for action in actions {
            let ClientShellAction::Endpoint {
                endpoint_id,
                boot_id,
                request,
            } = action
            else {
                panic!("expected endpoint request");
            };
            commands.enqueue(endpoint_id, 1, boot_id, request);
        }
        let sent_at = std::time::Instant::now();
        let cancelled = commands.send_next(&ClientEndpointId::Local, &mut endpoints, sent_at);
        assert_eq!(cancelled, EndpointCommandCancellation::default());
        assert!(!state.ledger.is_empty());
        if connection_lost {
            // A failed health check on the same timer tick removes the connection before the
            // command expires; the lane disconnect comes only with the next reconcile.
            endpoints.fail(
                &ClientEndpointId::Local,
                &std::io::Error::new(std::io::ErrorKind::TimedOut, "health check timed out"),
            );
        }

        let outcome = crate::shell_runtime::settle_expired_endpoint_commands(
            &mut commands,
            &endpoints,
            &mut state,
            sent_at + crate::limits::ENDPOINT_COMMAND_TIMEOUT,
        );

        assert!(outcome.repaint);
        assert!(state.ledger.is_empty());
        let title = state.notices.visible().map(|notice| notice.title.as_str());
        let expected = if connection_lost {
            "Action interrupted"
        } else {
            "Server timed out"
        };
        assert_eq!(title, Some(expected));
        assert_eq!(
            commands.disconnect(&ClientEndpointId::Local),
            EndpointCommandCancellation::default()
        );
    }
}

#[test]
fn failed_selection_copy_does_not_send_terminal_input() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    state.mouse_selection.selection = Some(shepr_vt::selection::Selection::range(
        test_pane_id("w1:p1"),
        shepr_vt::Point::new(shepr_vt::AbsRow(0), 0),
        shepr_vt::Point::new(shepr_vt::AbsRow(0), 2),
    ));
    for result in [
        Some(Ok(EndpointReply::PaneSelection {
            pane_id: test_pane_id("w1:p1"),
            text: String::new(),
        })),
        None,
        Some(Err(ClientShellEndpointError::Server(
            shepr_protocol::command::EndpointError::Rejected(
                "selection text is unavailable".into(),
            ),
        ))),
    ] {
        let mut outcome = ClientShellInput::default();
        state.request_selection_copy(&mut outcome);
        if let Some(result) = result {
            let answer = state.handle_endpoint_result(
                &crate::tests::test_boot_id("boot-1"),
                request_id(&outcome.actions),
                result,
            );
            assert!(answer.actions.is_empty());
            assert!(answer.requests.is_empty());
        } else {
            state.drop_request(request_id(&outcome.actions), DropReason::Interrupted);
        }
        assert!(state.ledger.is_empty());
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
        let notice = state.notices.visible().expect("notice");
        (
            notice.key.kind,
            notice.key.code,
            notice.title.clone(),
            notice.body.clone(),
        )
    };

    let (kind, code, title, body) = answer(EndpointError::ShuttingDown);
    assert_eq!(kind, ClientEndpointNoticeKind::Unavailable);
    assert_eq!(code, crate::shell::overlays::notices::NoticeCode::Server);
    assert_eq!(title, "Server unavailable");
    assert_eq!(body, EndpointError::ShuttingDown.to_string());

    for error in [
        EndpointError::Rejected("no such pane".into()),
        EndpointError::StaleBoot,
        EndpointError::SurfaceInactive,
        EndpointError::LimitExceeded(shepr_protocol::LimitExceeded::new(
            shepr_protocol::Limit::new(shepr_protocol::LimitKind::EndpointResponseBytes, 1),
            2,
        )),
    ] {
        let (kind, code, title, body) = answer(error.clone());
        assert_eq!(kind, ClientEndpointNoticeKind::Rejected);
        assert_eq!(
            code,
            crate::shell::overlays::notices::NoticeCode::Command(
                shepr_protocol::command::CommandKind::WorkspaceRename
            )
        );
        assert_eq!(title, "Action rejected");
        assert_eq!(body, error.to_string());
    }
}

fn ready_shell() -> ClientShellState {
    let mut s = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    s.set_snapshot(Box::new(snapshot()));
    s.receive_pane_surface(surface());
    s.compose(106, 20).expect("compose");
    s
}
fn copy_shell() -> ClientShellState {
    let mut s = ready_shell();
    assert!(s.enter_copy_mode(&mut ClientShellInput::default()));
    s
}
fn copy_search(s: &mut ClientShellState) -> String {
    let outcome = s.handle_input_bytes(b"/needle\r");
    request_id(&outcome.actions).to_owned()
}
fn answer(
    s: &mut ClientShellState,
    id: &str,
    result: Result<EndpointReply, ClientShellEndpointError>,
) -> ClientShellInput {
    s.answer_request(
        &crate::tests::test_boot_id("boot-1"),
        id,
        result,
        std::time::Instant::now(),
    )
}
fn scroll_reply(offset: u64) -> EndpointReply {
    EndpointReply::PaneInfo {
        pane: Box::new(shepr_protocol::command::PaneInfo {
            pane_id: test_pane_id("w1:p1"),
            scroll: Some(shepr_protocol::command::PaneScrollInfo::new(
                usize::try_from(offset).expect("test offset fits usize"),
                20,
                2,
                shepr_vt::AbsRow(0),
            )),
        }),
    }
}
fn start_scroll(s: &mut ClientShellState, offset: usize) -> String {
    let mut out = ClientShellInput::default();
    s.push_pane_scroll_offset(test_pane_id("w1:p1"), offset, &mut out);
    request_id(&out.actions).to_owned()
}
fn start_word(s: &mut ClientShellState) -> String {
    let hit = s.hits.panes[0].clone();
    let mut out = ClientShellInput::default();
    s.request_word_selection(&hit, hit.scroll.expect("scroll"), 0, 1, &mut out);
    request_id(&out.actions).to_owned()
}
fn start_label(s: &mut ClientShellState) -> String {
    let mut out = ClientShellInput::default();
    s.open_new_workspace_overlay(&mut out);
    request_id(&out.actions).to_owned()
}
#[test]
fn a_dropped_request_runs_its_rollback_and_sends_nothing() {
    for kind in 0..7 {
        let mut s = copy_shell();
        let id = match kind {
            0 | 1 => {
                let mut out = ClientShellInput::default();
                s.submit(
                    EndpointCommand::PaneFocus(shepr_protocol::command::PaneTarget {
                        pane_id: test_pane_id("w1:p1"),
                    }),
                    if kind == 0 {
                        Work::Plain
                    } else {
                        Work::SelectionCopy
                    },
                    &mut out,
                )
                .expect("submit")
                .to_string()
            }
            2 => start_label(&mut s),
            3 => {
                let id = start_scroll(&mut s, 3);
                s.push_pane_scroll_offset(
                    test_pane_id("w1:p1"),
                    7,
                    &mut ClientShellInput::default(),
                );
                id
            }
            4 => start_word(&mut s),
            5 => {
                let out = s.handle_input_bytes(b"w");
                request_id(&out.actions).to_owned()
            }
            _ => copy_search(&mut s),
        };
        if matches!(kind, 5 | 6) {
            s.copy_pipeline
                .push_key(shepr_termio::input::TerminalKey::new(
                    KeyCode::Char('j'),
                    KeyModifiers::empty(),
                ));
            s.copy_pipeline.push_op(ClientCopyOperation::Motion(
                shepr_protocol::command::PaneCopyMotion::Word(
                    shepr_protocol::command::PaneWordMotion::NextStart,
                ),
            ));
            s.copy_mode
                .as_mut()
                .expect("copy")
                .search
                .get_or_insert_with(ClientCopySearch::default)
                .copy_after_result = true;
        }
        let count = s.ledger.len();
        assert!(count > 0);
        let mark = s.ledger.mark();
        s.drop_request(&id, DropReason::Unsent);
        assert_eq!(s.ledger.len(), count - 1);
        // The rollback opened no request, not even one the drop path then removed as an
        // orphan: the ledger issued no serial across the drop.
        assert_eq!(s.ledger.mark(), mark, "rollback must not open requests");
        match kind {
            2 => {
                let Some(ClientShellOverlay::Rename(rename)) = &s.overlay else {
                    panic!("overlay")
                };
                assert!(matches!(
                    &rename.target,
                    ClientRenameTarget::NewWorkspace {
                        label_lookup: None,
                        ..
                    }
                ));
            }
            3 => assert!(s.scroll_lanes.is_idle()),
            4 => assert!(s.mouse_selection.word_gesture.is_none()),
            5 | 6 => {
                assert!(!s.copy_pipeline.in_flight());
                assert!(s.copy_pipeline.ops_is_empty());
                assert!(s.copy_pipeline.keys_is_empty());
                assert!(
                    !s.copy_mode
                        .as_ref()
                        .expect("copy")
                        .search
                        .as_ref()
                        .is_some_and(|search| search.copy_after_result)
                );
            }
            _ => {}
        }
    }
}
#[test]
fn an_ignored_answer_still_reports_its_server_error() {
    let mut s = copy_shell();
    let old = copy_search(&mut s);
    s.reset_copy_pipeline();
    let current = s.handle_input_bytes(b"w");
    let current = request_id(&current.actions).to_owned();
    let out = answer(&mut s, &old, Err(ClientShellEndpointError::Timeout));
    assert!(out.repaint);
    assert!(out.actions.is_empty());
    assert!(s.copy_pipeline.is_awaiting(&current.clone().into()));
    assert!(
        s.notices
            .timeout_suppressed(crate::shell::overlays::notices::NoticeCode::Command(
                shepr_protocol::command::CommandKind::PaneCopySearch
            ))
    );
    s.reset_copy_pipeline();
    let another = copy_search(&mut s);
    s.reset_copy_pipeline();
    let current = s.handle_input_bytes(b"w");
    let current = request_id(&current.actions).to_owned();
    answer(&mut s, &another, Ok(EndpointReply::Done));
    assert!(
        !s.notices
            .timeout_suppressed(crate::shell::overlays::notices::NoticeCode::Command(
                shepr_protocol::command::CommandKind::PaneCopySearch
            ))
    );
    assert!(s.copy_pipeline.is_awaiting(&current.into()));
}
#[test]
fn answering_a_request_twice_applies_it_once() {
    let mut s = ready_shell();
    let mut out = ClientShellInput::default();
    let id = s
        .submit(
            EndpointCommand::PaneSelectionRead(shepr_protocol::command::PaneSelectionReadParams {
                pane_id: test_pane_id("w1:p1"),
                anchor: shepr_protocol::command::PaneTextPoint {
                    row: shepr_vt::AbsRow(0),
                    col: 0,
                },
                cursor: shepr_protocol::command::PaneTextPoint {
                    row: shepr_vt::AbsRow(0),
                    col: 1,
                },
            }),
            Work::SelectionCopy,
            &mut out,
        )
        .expect("submit");
    let reply = EndpointReply::PaneSelection {
        pane_id: test_pane_id("w1:p1"),
        text: "text".into(),
    };
    assert_eq!(answer(&mut s, &id, Ok(reply.clone())).actions.len(), 1);
    let duplicate = answer(&mut s, &id, Ok(reply));
    assert!(duplicate.actions.is_empty());
    assert!(!duplicate.repaint);
}
#[test]
fn a_cancelled_scroll_takes_its_queued_offset_with_it() {
    let mut s = ready_shell();
    let id = start_scroll(&mut s, 3);
    s.push_pane_scroll_offset(test_pane_id("w1:p1"), 7, &mut ClientShellInput::default());
    s.drop_request(&id, DropReason::Interrupted);
    assert!(s.scroll_lanes.is_idle());
    assert!(answer(&mut s, &id, Ok(scroll_reply(3))).actions.is_empty());
}
#[test]
fn a_scroll_answer_does_not_bring_back_a_target_a_surface_already_showed() {
    let mut s = ready_shell();
    let id = start_scroll(&mut s, 3);
    let mut shown = surface();
    {
        let metrics = shown.panes[0].scroll.as_mut().expect("scroll");
        *metrics =
            shepr_vt::ScrollMetrics::new(3, 20, metrics.viewport_rows, metrics.history_origin);
    }
    s.receive_pane_surface(shown);
    assert!(s.scroll_lanes.target(&test_pane_id("w1:p1")).is_none());
    answer(&mut s, &id, Ok(scroll_reply(3)));
    assert!(s.scroll_lanes.is_idle());
}
#[test]
fn a_copy_answer_after_the_pipeline_was_reset_is_ignored() {
    let mut s = copy_shell();
    let out = s.handle_input_bytes(b"w");
    let id = request_id(&out.actions).to_owned();
    let before = s.copy_mode.as_ref().expect("copy").cursor;
    s.reset_copy_pipeline();
    let out = answer(
        &mut s,
        &id,
        Ok(EndpointReply::PaneCopyMotion {
            pane_id: test_pane_id("w1:p1"),
            cursor: shepr_protocol::command::PaneTextPoint {
                row: shepr_vt::AbsRow(0),
                col: 3,
            },
        }),
    );
    assert!(out.actions.is_empty());
    assert_eq!(s.copy_mode.as_ref().expect("copy").cursor, before);
}
#[test]
fn an_abandoned_copy_search_does_not_defer_a_later_copy() {
    let mut s = copy_shell();
    let id = copy_search(&mut s);
    s.abandon_copy_operation();
    let out = s.handle_input_bytes(b"y");
    assert!(s.copy_mode.is_none());
    assert!(s.ledger.contains(&id));
    assert!(!out.actions.is_empty());
}
#[test]
fn a_word_selection_answer_for_a_replaced_gesture_is_ignored() {
    let mut s = ready_shell();
    let old = start_word(&mut s);
    let current = start_word(&mut s);
    let out = answer(
        &mut s,
        &old,
        Ok(EndpointReply::PaneSelection {
            pane_id: test_pane_id("w1:p1"),
            text: "word".into(),
        }),
    );
    assert!(!out.repaint);
    assert!(s.mouse_selection.selection.is_none());
    assert!(!s.drop_word_selection(&old.into()).is_needed());
    assert!(s.drop_word_selection(&current.into()).is_needed());
}
#[test]
fn a_workspace_label_answer_for_a_reopened_overlay_is_ignored() {
    let mut s = ready_shell();
    let old = start_label(&mut s);
    let current = start_label(&mut s);
    let out = answer(
        &mut s,
        &old,
        Ok(EndpointReply::WorkspaceCheckoutRoot {
            root: Some("/different".into()),
            home: None,
        }),
    );
    assert!(!out.repaint);
    let Some(ClientShellOverlay::Rename(rename)) = s.overlay.as_ref() else {
        panic!("overlay")
    };
    assert!(matches!(
        &rename.target,
        ClientRenameTarget::NewWorkspace {
            label_lookup: Some(id),
            ..
        } if id.as_str() == current
    ));
}
#[test]
fn a_projection_reset_drops_every_request_with_its_feature_state() {
    let mut s = copy_shell();
    copy_search(&mut s);
    start_word(&mut s);
    start_scroll(&mut s, 3);
    start_label(&mut s);
    s.reset_endpoint_projection();
    assert!(s.ledger.is_empty());
    assert!(s.scroll_lanes.is_idle());
    assert!(!s.copy_pipeline.in_flight());
    assert!(s.copy_pipeline.ops_is_empty());
    assert!(s.copy_pipeline.keys_is_empty());
    assert!(s.mouse_selection.word_gesture.is_none());
    assert!(s.overlay.is_none());
    assert!(s.pending_workspace_highlight.is_none());
    assert!(s.notices.visible().is_none());
}
#[test]
fn a_failed_focus_releases_only_its_own_highlight() {
    let mut s = ready_shell();
    let mut snap = snapshot();
    for number in [2, 3] {
        let mut w = snap.workspaces[0].clone();
        w.workspace_id = test_workspace_id(&format!("w{number}"));
        w.number = number;
        snap.workspaces.push(w);
    }
    s.set_snapshot(Box::new(snap));
    let first = s.focus_endpoint_target(ClientEndpointFocusTarget::Workspace(test_workspace_id(
        "w2",
    )));
    let old = request_id(&first).to_owned();
    let second = s.focus_endpoint_target(ClientEndpointFocusTarget::Workspace(test_workspace_id(
        "w3",
    )));
    let current = request_id(&second).to_owned();
    answer(&mut s, &old, Err(ClientShellEndpointError::Timeout));
    assert_eq!(
        s.pending_workspace_highlight
            .as_ref()
            .expect("highlight")
            .request_id,
        current
    );
    s.drop_request(&current, DropReason::Interrupted);
    assert!(s.pending_workspace_highlight.is_none());
}
#[test]
fn only_a_plain_request_shows_the_interruption_notice_and_only_when_it_may_have_been_sent() {
    for plain in [true, false] {
        for reason in [
            DropReason::Interrupted,
            DropReason::Unsent,
            DropReason::WrongBoot,
            DropReason::Reset,
        ] {
            let mut s = ready_shell();
            let mut out = ClientShellInput::default();
            let id = s
                .submit(
                    EndpointCommand::PaneFocus(shepr_protocol::command::PaneTarget {
                        pane_id: test_pane_id("w1:p1"),
                    }),
                    if plain {
                        Work::Plain
                    } else {
                        Work::SelectionCopy
                    },
                    &mut out,
                )
                .expect("submit");
            s.drop_request(&id, reason);
            assert_eq!(
                s.notices.visible().is_some(),
                plain && matches!(reason, DropReason::Interrupted)
            );
        }
    }
}
