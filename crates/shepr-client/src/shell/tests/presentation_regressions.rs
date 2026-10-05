use crate::endpoint::ClientEndpointId;
use crate::shell::config::ClientShellConfig;
use crate::shell::navigation::location::{Location, LocationTarget};
use crate::shell::notices::ClientEndpointNoticeKind;
use crate::shell::overlays::Overlay;
use crate::shell::state::{ClientShellAction, ClientShellInput, ClientShellState};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use shepr_config::ClientConfig;
use shepr_termio::input::raw_input::RawInputEvent;

use crate::shell::tests::{
    frame_rows, help_overlay, open_help, press_overlay_key, snapshot, surface,
};

use crate::tests::{test_pane_id, test_workspace_id};

#[test]
fn client_presentation_regression_server_notice_titles_follow_the_notice_kind() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    let notices = [
        (
            shepr_protocol::NoticeKind::PaneInputDropped {
                pane_id: test_pane_id("w1:p1"),
                events: 2,
            },
            "Pane input dropped",
        ),
        (
            shepr_protocol::NoticeKind::LimitExceeded(shepr_protocol::LimitExceeded::new(
                shepr_protocol::Limit::new(shepr_protocol::LimitKind::InputPayloadBytes, 10),
                20,
            )),
            "Paste rejected",
        ),
        (
            shepr_protocol::NoticeKind::LimitExceeded(shepr_protocol::LimitExceeded::new(
                shepr_protocol::Limit::new(shepr_protocol::LimitKind::SurfaceMessageBytes, 10),
                20,
            )),
            "Screen too large",
        ),
    ];

    for (kind, expected_title) in notices {
        assert!(state.receive_server_notice(&kind));
        let notice = state.notices.visible().expect("notice shown");
        assert_eq!(notice.title, expected_title);
        assert_eq!(notice.body, kind.to_string());
    }
}

#[test]
fn unavailable_view_respects_a_collapsed_single_endpoint_sidebar() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.chrome.set_collapsed(true);
    state.set_snapshot(Box::new(snapshot()));

    let frame = state.compose(100, 28).expect("unavailable view");
    let rows = frame_rows(&frame);
    let text = rows.join("\n");

    assert!(text.contains("Desk: online."));
    assert!(!text.contains("Select a connected machine."));
    assert!(
        state
            .drawn()
            .workspaces()
            .any(|hit| hit.location.endpoint.is_local()
                && hit.location.workspace_id() == Some(crate::tests::test_workspace_id("w1")))
    );
    assert!(state.drawn().machines().next().is_none());
}

#[test]
fn client_presentation_regression_removed_navigator_target_accepts_visible_fallback() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.open_navigator_overlay();
    let removed_target = crate::shell::navigation::location::Location::pane(
        state.active_endpoint_id().clone(),
        test_pane_id("w1:p1"),
    );
    let Some(Overlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("expected navigator");
    };
    navigator.selected = Some(removed_target);

    let mut changed = snapshot();
    changed.panes[0].pane_id = test_pane_id("w1:p2");
    changed.focused_pane_id = Some(test_pane_id("w1:p2"));
    state.set_snapshot(Box::new(changed));

    let Some(Overlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("expected navigator");
    };
    let rows = crate::shell::navigation::aggregate_navigation::navigator_rows(
        &state.endpoints,
        state.active_endpoint_id(),
        navigator,
    );
    assert_eq!(
        crate::shell::navigation::aggregate_navigation::navigator_selected_index(&rows, navigator),
        Some(0)
    );
    let expected = rows[0].target.clone();

    let mut outcome = ClientShellInput::default();
    crate::shell::tests::press_overlay_enter(&mut state, &mut outcome);

    assert!(state.overlay.is_none());
    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint(Location {
            endpoint: ClientEndpointId::Local,
            target: LocationTarget::Workspace(workspace_id),
        })] if workspace_id == &crate::tests::test_workspace_id("w1")
    ));
    assert_eq!(
        expected,
        crate::shell::navigation::location::Location::workspace(
            state.active_endpoint_id().clone(),
            test_workspace_id("w1"),
        )
    );
}

#[test]
fn client_presentation_regression_help_scrolls_to_its_last_entry_in_a_narrow_terminal() {
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
    let groups = shepr_termio::input::keybind_help_groups(
        &state.config.keybinds.keybinds,
        state.config.keybinds.prefix,
    );
    let (_, last_entries) = groups.last().expect("help groups");
    let last_label = last_entries.last().expect("help entries").label;
    let last_word = last_label
        .split_whitespace()
        .last()
        .expect("label text")
        .to_owned();
    // End asks for the last row; with no Help drawn yet the request is stored unclamped.
    open_help(&mut state);
    press_overlay_key(&mut state, KeyCode::End);

    // Narrow enough that word wrapping produces more rows than character division predicts;
    // composition clamps the scroll to the computed maximum on the first frame.
    state.compose(26, 24).expect("help frame");
    let frame = state.compose(26, 24).expect("help frame");
    let rows = frame_rows(&frame);
    let popup = state.drawn().help_popup();
    let text_row = |y: u16| {
        rows[usize::from(y)]
            .chars()
            .skip(usize::from(popup.x + 1))
            .take(usize::from(popup.width.saturating_sub(3)))
            .collect::<String>()
    };
    // The help text ends with its last entry and one blank line. At the maximum scroll those
    // are the body's last two rows exactly when the range counts the rows the wrapper draws:
    // an undercount cuts the end off, an overcount leaves blank rows below it.
    let body_bottom = popup.bottom() - 4;

    assert!(
        text_row(body_bottom).trim().is_empty(),
        "the help text must scroll to its end: {rows:#?}"
    );
    assert!(
        text_row(body_bottom - 1).contains(&last_word),
        "the last help entry must sit just above the trailing blank line: \
         {last_word:?} {rows:#?}"
    );
}

#[test]
fn help_scroll_survives_a_window_too_small_for_help() {
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
    open_help(&mut state);
    press_overlay_key(&mut state, KeyCode::End);
    let help_scroll = |state: &ClientShellState| {
        let Some(help) = help_overlay(state) else {
            panic!("help overlay");
        };
        help.scroll()
    };
    // The first frame resolves the requested scroll to the end of the text.
    state.compose(106, 24).expect("help frame");
    let scrolled = help_scroll(&state);
    assert!(scrolled > 0 && scrolled < usize::MAX);

    state.compose(20, 4).expect("frame too small for help");
    assert_eq!(help_scroll(&state), scrolled);

    state.compose(106, 24).expect("help frame again");
    assert_eq!(help_scroll(&state), scrolled);
}

#[test]
fn client_presentation_regression_notice_card_keeps_diagnostic_lines_visible() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    assert!(state.push_endpoint_notice(
        ClientEndpointNoticeKind::Unavailable,
        crate::shell::notices::NoticeCode::MachineDiagnostic,
        "Buildbox: restart shepr to authenticate",
        "first diagnostic line\nsecond diagnostic line\nthird diagnostic line",
    ));

    let frame = state.compose(80, 12).expect("notice frame");
    let rows = frame_rows(&frame);
    assert!(
        rows.iter()
            .any(|row| row.contains("second diagnostic line"))
    );
    assert!(rows.iter().any(|row| row.contains("third diagnostic line")));
}

#[test]
fn restore_cards_keep_the_source_boot_and_survive_projection_resets() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let kind = shepr_protocol::SessionRestoreNotice {
        loss: shepr_protocol::SessionRestoreLoss::Damaged(shepr_protocol::SessionRestoreDamage {
            renamed_workspaces: 1,
            ..Default::default()
        }),
        backup_dir: "/state/session-backups".into(),
    };
    let first = crate::tests::test_boot_id("restored-first");
    let second = first.clone();
    let remote =
        ClientEndpointId::Ssh(shepr_config::MachineLabel::parse("Build").expect("test label"));
    assert!(state.receive_restore_notice(&ClientEndpointId::Local, &first, &kind));
    assert!(state.receive_restore_notice(&remote, &second, &kind));
    assert!(!state.receive_restore_notice(&ClientEndpointId::Local, &first, &kind));
    // The presented server reboots, which resets the projection.
    let mut rebooted = snapshot();
    rebooted.boot_id = crate::tests::test_boot_id("rebooted");
    state.set_snapshot(Box::new(rebooted));
    assert_eq!(
        state.notices.visible().expect("first card").key.boot_id,
        Some(first)
    );
    let now = std::time::Instant::now();
    state.notices.drawn(now);
    assert!(
        state
            .tick_timers(now + crate::limits::ENDPOINT_NOTICE_TIMEOUT)
            .repaint
    );
    assert_eq!(
        state.notices.visible().expect("second card").key.boot_id,
        Some(second)
    );
    assert_eq!(
        state.notices.visible().expect("remote card").title,
        "Build: saved session IDs repaired"
    );
    let body = &state.notices.visible().expect("remote card").body;
    assert!(body.contains("1 duplicate workspace ID was reassigned"));
    assert!(!body.contains("saved panes"));
    // A queued card receives a full lifetime only after it is actually drawn.
    assert!(
        !state
            .tick_timers(now + crate::limits::ENDPOINT_NOTICE_TIMEOUT)
            .repaint
    );
}

#[test]
fn a_saves_stopped_card_shows_once_per_boot_beside_the_restore_card() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let boot = crate::tests::test_boot_id("saves-stopped");
    let kind = shepr_protocol::SessionRestoreNotice {
        loss: shepr_protocol::SessionRestoreLoss::Damaged(shepr_protocol::SessionRestoreDamage {
            renamed_workspaces: 1,
            ..Default::default()
        }),
        backup_dir: "/state/session-backups".into(),
    };
    assert!(state.receive_restore_notice(&ClientEndpointId::Local, &boot, &kind));
    assert!(state.receive_session_saves_stopped(&ClientEndpointId::Local, &boot));
    // Every later projection of the same boot repeats the flag.
    assert!(!state.receive_session_saves_stopped(&ClientEndpointId::Local, &boot));
    let now = std::time::Instant::now();
    state.notices.drawn(now);
    assert!(
        state
            .tick_timers(now + crate::limits::ENDPOINT_NOTICE_TIMEOUT)
            .repaint
    );
    let card = state.notices.visible().expect("saves stopped card");
    assert!(
        card.title.ends_with(": session saves stopped"),
        "{}",
        card.title
    );
    assert!(card.body.contains("not restored"), "{}", card.body);
    assert!(state.receive_session_saves_stopped(
        &ClientEndpointId::Local,
        &crate::tests::test_boot_id("saves-stopped-next")
    ));
}

#[test]
fn transient_cards_do_not_discard_queued_restore_cards() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    let boot = crate::tests::test_boot_id("restored");
    let kind = shepr_protocol::SessionRestoreNotice {
        loss: shepr_protocol::SessionRestoreLoss::Damaged(shepr_protocol::SessionRestoreDamage {
            renamed_workspaces: 1,
            ..Default::default()
        }),
        backup_dir: "/state/session-backups".into(),
    };
    state.receive_restore_notice(&ClientEndpointId::Local, &boot, &kind);
    state.receive_paste_rejection("too large".into());
    let now = std::time::Instant::now();
    state.notices.drawn(now);
    assert!(
        state
            .tick_timers(now + crate::limits::ENDPOINT_NOTICE_TIMEOUT)
            .repaint
    );
    assert_eq!(
        state.notices.visible().expect("restore card").key.boot_id,
        Some(boot)
    );
}

#[test]
fn dismissing_a_restore_card_immediately_shows_the_next_queued_card() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let first = crate::tests::test_boot_id("restored-first");
    let second = crate::tests::test_boot_id("restored-second");
    let remote = ClientEndpointId::Ssh(shepr_config::MachineLabel::parse("Build").expect("label"));
    let kind = shepr_protocol::SessionRestoreNotice {
        loss: shepr_protocol::SessionRestoreLoss::Damaged(shepr_protocol::SessionRestoreDamage {
            renamed_workspaces: 1,
            ..Default::default()
        }),
        backup_dir: "/state/session-backups".into(),
    };
    state.receive_restore_notice(&ClientEndpointId::Local, &first, &kind);
    state.receive_restore_notice(&remote, &second, &kind);
    state.compose(106, 20).expect("first notice frame");
    let toast = state.drawn().notification_toast();
    assert!(!toast.is_empty());

    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: toast.x,
        row: toast.y,
        modifiers: KeyModifiers::empty(),
    })]);

    assert_eq!(
        state
            .notices
            .visible()
            .expect("second restore card")
            .key
            .boot_id,
        Some(second)
    );
    assert_eq!(state.notices.queued(), 0);
    assert!(state.notices.deadline().is_none());
    assert_eq!(state.next_timer_deadline(), None);

    state.compose(106, 20).expect("second notice frame");
    assert!(state.notices.deadline().is_some());
}

#[test]
fn an_unpaired_compose_records_no_view() {
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
    state.compose(106, 20).expect("first frame");
    assert_eq!(state.drawn().size, (106, 20));

    // A surface for a projection the snapshot has not reached waits for its snapshot, so the
    // presented surface is held unpaired and the last frame stays on screen.
    let mut future = surface();
    future.projection_revision = shepr_test_fixtures::counter_at(3);
    state.receive_pane_surface_from(
        future,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    assert!(state.compose(80, 24).is_none());

    assert_eq!(state.drawn().size, (106, 20));
    assert!(!state.pane_hits().is_empty());
}

#[test]
fn navigate_reveal_follows_a_size_change() {
    let mut initial = snapshot();
    let template = initial.workspaces[0].clone();
    initial.workspaces = (1..=30)
        .map(|number| shepr_protocol::ClientShellWorkspace {
            workspace_id: test_workspace_id(&format!("w{number}")),
            label: format!("space-{number}"),
            branch: None,
            ..template.clone()
        })
        .collect();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(initial));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 20).expect("full sidebar");

    let preview = state.navigation_target(state.endpoints.presented(), &test_workspace_id("w30"));
    state.mode.enter_navigate(preview);
    assert!(
        state
            .drawn()
            .workspaces()
            .all(|hit| hit.location.workspace_id() != Some(test_workspace_id("w30")))
    );

    state.compose(106, 22).expect("resized frame");

    assert!(
        state
            .drawn()
            .workspaces()
            .any(|hit| hit.location.workspace_id() == Some(test_workspace_id("w30")))
    );
}
