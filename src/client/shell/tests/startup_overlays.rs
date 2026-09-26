use super::*;

#[test]
fn startup_config_diagnostics_are_client_rendered_and_persist_until_replaced() {
    let config = ClientShellConfig::from_config(&Config::default())
        .with_startup_config_diagnostic(Some("local config warning".into()));
    let mut state = ClientShellState::new(config);
    let mut shared_snapshot = snapshot();
    shared_snapshot.config_diagnostic = Some("local config warning".into());
    state.set_snapshot(Box::new(shared_snapshot));
    assert_eq!(
        state.config_diagnostic.as_deref(),
        Some("client + endpoint: local config warning")
    );

    let mut endpoint_snapshot = snapshot();
    endpoint_snapshot.config_diagnostic = Some("endpoint config warning".into());
    state.set_snapshot(Box::new(endpoint_snapshot));
    state.set_pane_surface(surface());

    let frame = state.compose(106, 20).expect("diagnostic frame");
    let text = frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("client: local config warning"));
    assert!(text.contains("endpoint: endpoint config warning"));

    state.handle_input_bytes(b"x");
    assert!(state.config_diagnostic.is_some());

    state.set_snapshot(Box::new(snapshot()));
    assert_eq!(
        state.config_diagnostic.as_deref(),
        Some("local config warning")
    );
}

fn frame_text(frame: &FrameData) -> String {
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn config_diagnostic_banner_expires_and_returns_only_when_the_text_changes() {
    let config = ClientShellConfig::from_config(&Config::default())
        .with_startup_config_diagnostic(Some("local config warning".into()));
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let frame = state.compose(106, 20).expect("diagnostic frame");
    assert!(frame_text(&frame).contains("local config warning"));

    let start = std::time::Instant::now();
    assert!(!state.tick_transient_banners(start));
    assert!(!state.tick_transient_banners(start + std::time::Duration::from_secs(5)));
    assert!(state.tick_transient_banners(start + std::time::Duration::from_secs(21)));
    assert!(state.visible_config_diagnostic().is_none());
    // The diagnostic itself is still known; only its banner is gone.
    assert!(state.config_diagnostic.is_some());
    let frame = state.compose(106, 20).expect("frame without banner");
    assert!(!frame_text(&frame).contains("local config warning"));
    assert!(state.hits.config_diagnostic.is_empty());

    // A snapshot with the same diagnostic keeps it hidden; a different one shows again.
    state.set_snapshot(Box::new(snapshot()));
    assert!(state.visible_config_diagnostic().is_none());
    let mut changed = snapshot();
    changed.config_diagnostic = Some("endpoint config warning".into());
    state.set_snapshot(Box::new(changed));
    assert!(state.visible_config_diagnostic().is_some());
}

#[test]
fn clicking_the_config_diagnostic_banner_dismisses_it() {
    let config = ClientShellConfig::from_config(&Config::default())
        .with_startup_config_diagnostic(Some("local config warning".into()));
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("diagnostic frame");
    let banner = state.hits.config_diagnostic;
    assert!(!banner.is_empty());
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(
        crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: banner.x,
            row: banner.y,
            modifiers: KeyModifiers::NONE,
        },
        &mut outcome,
    );
    assert!(outcome.repaint);
    assert!(state.visible_config_diagnostic().is_none());
}

#[test]
fn endpoint_notice_expires_without_a_click() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    assert!(state.push_endpoint_notice(
        ClientEndpointNoticeKind::Rejected,
        "code",
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
        "other",
        "title",
        "body",
    ));
    assert!(!state.tick_transient_banners(far));
    state.compose(106, 20).expect("replacement frame");
    assert!(state.visible_endpoint_notice.is_some());
    assert!(state.tick_transient_banners(far));
    assert!(state.visible_endpoint_notice.is_none());
}
