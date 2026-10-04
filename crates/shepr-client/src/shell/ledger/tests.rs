//! The request ledger: ids and tickets, and through the shell, how each kind of
//! request is answered, dropped and rolled back.

use crate::endpoint::ClientEndpointId;
use crate::shell::config::ClientShellConfig;
use crate::shell::ledger::{DropReason, Ledger, Submitted, Work};
use crate::shell::navigation::location::LocationTarget;
use crate::shell::notices::ClientEndpointNoticeKind;
use crate::shell::overlays::Overlay;
use crate::shell::overlays::rename::RenameTarget;
use crate::shell::state::{
    ClientShellAction, ClientShellEndpointError, ClientShellInput, ClientShellState,
};
use crate::shell::tests::{
    answer, copy_search, copy_shell, pending_request, ready_shell, request_id, snapshot, surface,
};
use crate::tests::{test_pane_id, test_workspace_id};
use shepr_config::ClientConfig;
use shepr_protocol::command::{CommandKind, EndpointCommand, EndpointReply};

#[test]
fn ids_are_unique_and_never_reused() {
    let mut l = Ledger::default();
    let boot = crate::tests::test_boot_id("boot");
    let a = l.open(boot.clone(), CommandKind::WorkspaceRename, Work::Plain);
    l.take(&a);
    let b = l.open(boot, CommandKind::WorkspaceRename, Work::Plain);
    assert_ne!(a, b);
}
#[test]
fn tickets_are_never_reissued() {
    let mut l = Ledger::default();
    let tickets: Vec<_> = (0..1000).map(|_| l.ticket()).collect();
    assert!(tickets.windows(2).all(|pair| pair[0].0 < pair[1].0));
}
#[test]
fn an_entry_is_taken_once() {
    let mut l = Ledger::default();
    let id = l.open(
        crate::tests::test_boot_id("boot"),
        CommandKind::WorkspaceRename,
        Work::Plain,
    );
    assert!(l.take(&id).is_some());
    assert!(l.take(&id).is_none());
}

#[test]
fn cancelled_scroll_rolls_back_queued_target_even_without_a_presented_snapshot() {
    for missing_snapshot in [false, true] {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.set_snapshot(Box::new(snapshot()));
        state.receive_pane_surface_from(
            surface(),
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
        let pane_id = test_pane_id("w1:p1");
        let mut first = ClientShellInput::default();
        state.push_pane_scroll_offset(pane_id, 3, &mut first);
        let id = request_id(&first.actions).to_owned();
        let mut queued = ClientShellInput::default();
        state.push_pane_scroll_offset(pane_id, 7, &mut queued);
        assert!(queued.actions.is_empty());
        assert_eq!(state.scroll_lanes.queued(&pane_id), Some(7));
        if missing_snapshot {
            state.endpoints.active.clear_snapshot();
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
        state.push_pane_scroll_offset(pane_id, 2, &mut next);
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
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    let pane_id = test_pane_id("w1:p1");
    let mut first = ClientShellInput::default();
    state.push_pane_scroll_offset(pane_id, 3, &mut first);
    let id = request_id(&first.actions).to_owned();
    let mut queued = ClientShellInput::default();
    state.push_pane_scroll_offset(pane_id, 7, &mut queued);
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
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    let pane_id = test_pane_id("w1:p1");
    let mut first = ClientShellInput::default();
    state.push_pane_scroll_offset(pane_id, 3, &mut first);
    let mut queued = ClientShellInput::default();
    state.push_pane_scroll_offset(pane_id, 7, &mut queued);

    state.mark_endpoint_disconnected(&ClientEndpointId::Local);

    assert!(state.ledger.is_empty());
    assert!(state.scroll_lanes.is_idle());
    assert!(state.notices.visible().is_none());
}

#[test]
fn failed_selection_copy_does_not_send_terminal_input() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.mouse_selection.selection = Some(shepr_term::selection::Selection::range(
        test_pane_id("w1:p1"),
        shepr_term::Point::new(shepr_term::AbsRow(0), 0),
        shepr_term::Point::new(shepr_term::AbsRow(0), 2),
    ));
    for result in [
        Some(Ok(EndpointReply::PaneSelection {
            pane_id: test_pane_id("w1:p1"),
            text: String::new(),
        })),
        None,
        Some(Err(ClientShellEndpointError::Server(
            shepr_protocol::command::EndpointError::Unavailable(
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
    assert_eq!(code, crate::shell::notices::NoticeCode::Server);
    assert_eq!(title, "Server unavailable");
    assert_eq!(body, EndpointError::ShuttingDown.to_string());

    for error in [
        EndpointError::PaneGone(test_pane_id("w1:p9")),
        EndpointError::InvalidArgument("not a directory".into()),
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
            crate::shell::notices::NoticeCode::Command(
                shepr_protocol::command::CommandKind::WorkspaceRename
            )
        );
        assert_eq!(title, "Action rejected");
        assert_eq!(body, error.to_string());
    }
}

fn scroll_reply(offset: u64) -> EndpointReply {
    EndpointReply::PaneInfo {
        pane: Box::new(shepr_protocol::command::PaneInfo {
            pane_id: test_pane_id("w1:p1"),
            scroll: Some(shepr_protocol::command::PaneScrollInfo::new(
                usize::try_from(offset).expect("test offset fits usize"),
                20,
                2,
                shepr_term::AbsRow(0),
            )),
        }),
    }
}
fn start_scroll(s: &mut ClientShellState, offset: usize) -> shepr_protocol::RequestId {
    let mut out = ClientShellInput::default();
    s.push_pane_scroll_offset(test_pane_id("w1:p1"), offset, &mut out);
    request_id(&out.actions).to_owned()
}
fn start_word(s: &mut ClientShellState) -> shepr_protocol::RequestId {
    let hit = s.pane_hits()[0].clone();
    let mut out = ClientShellInput::default();
    s.request_word_selection(&hit, hit.scroll.expect("scroll"), 0, 1, &mut out);
    request_id(&out.actions).to_owned()
}
fn start_label(s: &mut ClientShellState) -> shepr_protocol::RequestId {
    let mut out = ClientShellInput::default();
    s.open_new_workspace_overlay(&mut out);
    request_id(&out.actions).to_owned()
}

/// The copy-mode motion and search requests have their own test in `copy`.
#[test]
fn a_dropped_request_runs_its_rollback_and_sends_nothing() {
    for kind in 0..5 {
        let mut s = copy_shell();
        let id = match kind {
            0 | 1 => {
                let mut out = ClientShellInput::default();
                assert_eq!(
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
                    ),
                    Submitted::Opened
                );
                request_id(&out.actions).to_owned()
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
            _ => start_word(&mut s),
        };
        let count = s.ledger.len();
        assert!(count > 0);
        s.drop_request(&id, DropReason::Unsent);
        // Nothing removes an orphan: a rollback that opened a request would leave it here.
        assert_eq!(s.ledger.len(), count - 1);
        match kind {
            2 => {
                let Some(Overlay::Rename(rename)) = &s.overlay else {
                    panic!("overlay")
                };
                assert!(matches!(
                    &rename.target,
                    RenameTarget::NewWorkspace {
                        label_lookup: None,
                        ..
                    }
                ));
            }
            3 => assert!(s.scroll_lanes.is_idle()),
            4 => assert!(s.mouse_selection.word_gesture.is_none()),
            _ => {}
        }
    }
}

#[test]
fn answering_a_request_twice_applies_it_once() {
    let mut s = ready_shell();
    let mut out = ClientShellInput::default();
    assert_eq!(
        s.submit(
            EndpointCommand::PaneSelectionRead(shepr_protocol::command::PaneSelectionReadParams {
                pane_id: test_pane_id("w1:p1"),
                anchor: shepr_protocol::command::PaneTextPoint {
                    row: shepr_term::AbsRow(0),
                    col: 0,
                },
                cursor: shepr_protocol::command::PaneTextPoint {
                    row: shepr_term::AbsRow(0),
                    col: 1,
                },
            }),
            Work::SelectionCopy,
            &mut out,
        ),
        Submitted::Opened
    );
    let id = request_id(&out.actions).to_owned();
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
            shepr_term::ScrollMetrics::new(3, 20, metrics.viewport_rows, metrics.history_origin);
    }
    s.receive_pane_surface_from(
        shown,
        s.endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    assert!(s.scroll_lanes.target(&test_pane_id("w1:p1")).is_none());
    answer(&mut s, &id, Ok(scroll_reply(3)));
    assert!(s.scroll_lanes.is_idle());
}

#[test]
fn a_scroll_answer_for_a_lane_rebuilt_after_its_pane_left_is_ignored() {
    let mut s = ready_shell();
    let pane = test_pane_id("w1:p1");
    let a = start_scroll(&mut s, 3);
    let mut without_pane = snapshot();
    without_pane.panes.clear();
    s.set_snapshot(Box::new(without_pane));
    assert!(s.scroll_lanes.is_idle());
    s.set_snapshot(Box::new(snapshot()));
    let b = start_scroll(&mut s, 5);
    assert_ne!(a, b);

    let stale = answer(&mut s, &a, Ok(scroll_reply(3)));
    assert!(stale.actions.is_empty());
    assert!(s.scroll_lanes.in_flight(&pane));

    answer(&mut s, &b, Ok(scroll_reply(5)));
    assert!(!s.scroll_lanes.in_flight(&pane));
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
    assert!(s.mouse_selection.word_gesture.is_some());
    assert!(!s.drop_request(&old, DropReason::Unsent).is_needed());
    assert!(s.mouse_selection.word_gesture.is_some());
    assert!(s.drop_request(&current, DropReason::Unsent).is_needed());
    assert!(s.mouse_selection.word_gesture.is_none());
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
    let Some(Overlay::Rename(rename)) = s.overlay.as_ref() else {
        panic!("overlay")
    };
    assert!(matches!(
        &rename.target,
        RenameTarget::NewWorkspace {
            label_lookup: Some(_),
            ..
        }
    ));
    answer(
        &mut s,
        &current,
        Ok(EndpointReply::WorkspaceCheckoutRoot {
            root: Some("/different".into()),
            home: None,
        }),
    );
    let Some(Overlay::Rename(rename)) = s.overlay.as_ref() else {
        panic!("overlay")
    };
    assert!(matches!(
        &rename.target,
        RenameTarget::NewWorkspace {
            label_lookup: None,
            ..
        }
    ));
}

#[test]
fn a_projection_reset_drops_every_request_with_its_feature_state() {
    let mut s = copy_shell();
    copy_search(&mut s);
    start_word(&mut s);
    start_scroll(&mut s, 3);
    start_label(&mut s);
    s.reset_endpoint_projection(crate::shell::endpoints::ProjectionReset::Rebooted);
    assert!(s.ledger.is_empty());
    assert!(s.scroll_lanes.is_idle());
    assert!(!s.copy_in_flight());
    assert!(s.copy_ops_empty());
    assert!(s.copy_keys_empty());
    assert!(s.mouse_selection.word_gesture.is_none());
    assert!(s.overlay.is_none());
    assert!(s.pending_workspace_highlight.is_none());
    assert!(s.notices.visible().is_none());
}

#[test]
fn a_reset_drops_requests_before_resetting_features() {
    let mut s = copy_shell();
    let motion = s.handle_input_bytes(b"w");
    request_id(&motion.actions);
    assert!(s.copy_in_flight());
    start_word(&mut s);
    start_label(&mut s);
    assert!(!s.ledger.is_empty());

    // A reboot of the presented endpoint resets the projection, which drops every
    // request while each feature still holds its state, then resets the features.
    let mut rebooted = snapshot();
    rebooted.boot_id = crate::tests::test_boot_id("rebooted");
    s.set_snapshot(Box::new(rebooted));

    assert!(s.ledger.is_empty());
    assert!(s.copy.is_none());
    assert!(!s.copy_in_flight());
    assert!(s.copy_keys_empty());
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
        snap.workspaces.push(w);
    }
    s.set_snapshot(Box::new(snap));
    let first = s.focus_endpoint_target(LocationTarget::Workspace(test_workspace_id("w2")));
    let old = request_id(&first).to_owned();
    let second = s.focus_endpoint_target(LocationTarget::Workspace(test_workspace_id("w3")));
    let current = request_id(&second).to_owned();
    answer(&mut s, &old, Err(ClientShellEndpointError::Timeout));
    assert!(s.pending_workspace_highlight.is_some());
    s.drop_request(&current, DropReason::Interrupted);
    assert!(s.pending_workspace_highlight.is_none());
}

#[test]
fn only_state_changing_requests_show_the_interruption_notice_and_only_when_they_may_have_been_sent()
{
    for kind in 0..3 {
        for reason in [
            DropReason::Interrupted,
            DropReason::Unsent,
            DropReason::WrongBoot,
            DropReason::Reset,
        ] {
            let mut s = ready_shell();
            let mut out = ClientShellInput::default();
            let work = match kind {
                0 => Work::Plain,
                1 => Work::Focus {
                    highlight: s.ledger.ticket(),
                },
                _ => Work::SelectionCopy,
            };
            assert_eq!(
                s.submit(
                    EndpointCommand::PaneFocus(shepr_protocol::command::PaneTarget {
                        pane_id: test_pane_id("w1:p1"),
                    }),
                    work,
                    &mut out,
                ),
                Submitted::Opened
            );
            let id = request_id(&out.actions).to_owned();
            s.drop_request(&id, reason);
            assert_eq!(
                s.notices.visible().is_some(),
                kind < 2 && matches!(reason, DropReason::Interrupted)
            );
        }
    }
}
