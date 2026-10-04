use crate::shell::config::ClientShellConfig;
use crate::shell::notices::{ClientEndpointNoticeKind, NoticeCode};
use crate::shell::overlays::Overlay;
use crate::shell::overlays::help::HelpOverlay;
use crate::shell::state::{ClientShellInput, ClientShellState};
use shepr_config::ClientConfig;

use crate::shell::tests::{frame_rows, snapshot};

#[test]
fn endpoint_notice_expires_without_a_click() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    assert!(state.push_endpoint_notice(
        ClientEndpointNoticeKind::Rejected,
        NoticeCode::SelectionEmpty,
        "title",
        "body",
    ));
    // Not drawn yet: its lifetime has not started.
    let far = std::time::Instant::now() + std::time::Duration::from_secs(60);
    assert!(!state.tick_transient_banners(far));
    state.compose(106, 20).expect("notice frame");
    let drawn = std::time::Instant::now();
    assert!(!state.tick_transient_banners(drawn));
    assert!(!state.tick_transient_banners(drawn + std::time::Duration::from_secs(5)));

    // A replacement does not inherit its predecessor's lifetime.
    assert!(state.push_endpoint_notice(
        ClientEndpointNoticeKind::Rejected,
        NoticeCode::PasteRejected,
        "title",
        "body",
    ));
    assert!(!state.tick_transient_banners(far));
    state.compose(106, 20).expect("replacement frame");
    assert!(state.notices.visible().is_some());
    assert!(state.tick_transient_banners(far));
    assert!(state.notices.visible().is_none());
}

#[test]
fn transient_shell_deadlines_schedule_their_expiry() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_endpoint_error("failure", state.now);
    state.compose(106, 20).expect("frame");
    let error_deadline = state.endpoint_error.deadline().expect("error deadline");
    assert_eq!(state.next_timer_deadline(), Some(error_deadline));
    state.endpoint_error.dismiss();

    assert!(state.push_endpoint_notice(
        ClientEndpointNoticeKind::Rejected,
        NoticeCode::SelectionEmpty,
        "title",
        "body",
    ));
    state.compose(106, 20).expect("notice frame");
    let notice_deadline = state.notices.deadline().expect("notice deadline");
    assert_eq!(state.next_timer_deadline(), Some(notice_deadline));
}

#[test]
fn overlays_render_without_a_pane_surface_or_snapshot() {
    for with_snapshot in [false, true] {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        if with_snapshot {
            state.set_snapshot(Box::new(snapshot()));
        }
        state.open_navigator_overlay();
        if let Some(Overlay::Navigator(navigator)) = state.overlay.as_mut() {
            navigator.search_focused = true;
        }
        let frame = state.compose(106, 30).expect("navigator frame");
        assert!(frame.cursor().is_some());
        assert!(!state.drawn().navigator_popup().is_empty());
        assert!(!state.drawn().navigator_search().is_empty());
        assert!(state.pane_hits().is_empty());

        state.overlay = Some(Overlay::Help(HelpOverlay::default()));
        state.compose(106, 30).expect("help frame");
        assert!(!state.drawn().help_popup().is_empty());
        assert!(state.drawn().navigator_popup().is_empty());
    }
}

#[test]
fn unavailable_global_menu_renders_and_activates_without_snapshot() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.compose(106, 30).expect("placeholder frame");
    state.toggle_global_menu();
    let frame = state.compose(106, 30).expect("menu frame");
    assert_eq!(state.drawn().global_menu_rows().len(), 2);
    assert!(
        frame_rows(&frame)
            .iter()
            .any(|row| row.contains("keybinds"))
    );
    let mut outcome = ClientShellInput::default();
    crate::shell::tests::press_overlay_enter(&mut state, &mut outcome);
    assert!(matches!(state.overlay, Some(Overlay::Help(_))));
}

#[test]
fn unavailable_small_popup_uses_the_common_hint_and_clears_hits() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.open_navigator_overlay();
    state.compose(106, 30).expect("navigator frame");
    assert!(!state.drawn().navigator_popup().is_empty());
    let frame = state.compose(40, 3).expect("small frame");
    assert!(state.drawn().navigator_popup().is_empty());
    assert!(state.drawn().navigator_rows().next().is_none());
    assert!(
        frame_rows(&frame)
            .iter()
            .any(|row| row.contains("window too small"))
    );
    assert!(state.overlay.is_some());
    assert!(frame.cursor().is_none());
}
