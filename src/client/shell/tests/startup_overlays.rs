use super::*;

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
