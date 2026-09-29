use super::*;

#[path = "workspace_navigation.rs"]
mod workspace_navigation;
use crate::endpoint::{ClientEndpointId, ClientEndpointStatus, ProfileId, SavedSshEndpoint};
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

fn remote_profile() -> SavedSshEndpoint {
    SavedSshEndpoint {
        id: ProfileId::parse("0123456789abcdef0123456789abcdef").expect("test precondition"),
        label: "Build".into(),
        target: shepr_remote::SshTarget::parse("dev@build.example").expect("test precondition"),
        session: "agents".into(),
    }
}

/// The first argument only documents which agent a test means; agents carry
/// no name of their own.
fn agent(
    _description: &str,
    status: shepr_api::schema::AgentStatus,
    state_change_seq: u64,
) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: "w1:p1".parse().expect("test precondition"),
        workspace_id: test_workspace_id("w1"),
        tab_id: test_tab_id("w1:t1"),
        agent: Some("pi".into()),
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: status,
        state_change_seq,
        focused: true,
    }
}

fn state_with_remote() -> (ClientShellState, ClientEndpointId) {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let profile = remote_profile();
    let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let mut remote = snapshot();
    remote.boot_id = crate::tests::test_boot_id("remote-boot");
    remote.workspaces[0].label = "remote-workspace".into();
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote));
    (state, endpoint_id)
}

#[test]
fn repeated_endpoint_snapshots_reuse_the_validated_config() {
    let (mut state, endpoint_id) = state_with_remote();

    assert!(state.activate_endpoint_projection(&endpoint_id));
    let first = std::sync::Arc::clone(
        &state
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| endpoint.resolved_config.as_ref())
            .expect("first endpoint snapshot resolves its config")
            .config,
    );

    assert!(state.activate_endpoint_projection(&endpoint_id));
    let second = std::sync::Arc::clone(
        &state
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| endpoint.resolved_config.as_ref())
            .expect("second endpoint snapshot keeps its cached config")
            .config,
    );

    assert!(std::sync::Arc::ptr_eq(&first, &second));
}

#[test]
fn switching_to_an_endpoint_with_an_undecodable_config_keeps_the_previous_one() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let profile = remote_profile();
    let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let mut remote = snapshot();
    remote.boot_id = crate::tests::test_boot_id("remote-boot");
    remote.workspaces[0].label = "remote-workspace".into();
    remote.resolved_config = vec![0xff; 3];
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote));

    assert!(!state.activate_endpoint_projection(&endpoint_id));
    assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
    assert!(state.pane_surface.is_some());
    assert_ne!(
        state
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.workspaces.first())
            .map(|workspace| workspace.label.as_str()),
        Some("remote-workspace")
    );
    assert!(
        state
            .endpoint_error
            .as_deref()
            .is_some_and(|error| error.contains("invalid endpoint configuration")),
        "{:?}",
        state.endpoint_error
    );
}

#[test]
fn inactive_endpoint_with_an_undecodable_config_is_flagged_at_once() {
    let (mut state, endpoint_id) = state_with_remote();
    let endpoint = |state: &ClientShellState| {
        state
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == endpoint_id)
            .expect("remote endpoint")
            .clone()
    };
    let good = endpoint(&state).snapshot.expect("remote snapshot");
    let mut bad = (*good).clone();
    bad.revision = bad.revision.checked_next().expect("test precondition");
    bad.resolved_config = vec![0xff; 3];
    state.cache_endpoint_snapshot(&endpoint_id, Box::new(bad.clone()));

    // Surfaced while the endpoint is still in the background, and the last
    // good config is kept rather than dropped.
    let flagged = endpoint(&state);
    assert_eq!(flagged.status, ClientEndpointStatus::Attention);
    assert!(flagged.resolved_config.is_some());
    assert!(flagged.resolved_config_error.is_some());
    // A connection reporting itself online does not hide the bad config.
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);
    assert_eq!(endpoint(&state).status, ClientEndpointStatus::Attention);

    assert!(!state.activate_endpoint_projection(&endpoint_id));
    assert!(
        state
            .endpoint_error
            .as_deref()
            .is_some_and(|error| error.starts_with("Build: invalid endpoint configuration")),
        "{:?}",
        state.endpoint_error
    );

    let mut fixed = (*good).clone();
    fixed.revision = bad.revision.checked_next().expect("test precondition");
    state.cache_endpoint_snapshot(&endpoint_id, Box::new(fixed));
    let recovered = endpoint(&state);
    assert_eq!(recovered.status, ClientEndpointStatus::Online);
    assert!(recovered.resolved_config_error.is_none());
    assert!(state.activate_endpoint_projection(&endpoint_id));
}

#[test]
fn inactive_endpoint_keeps_config_when_later_snapshots_omit_bytes() {
    let (mut state, endpoint_id) = state_with_remote();
    let mut later = state
        .endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        .and_then(|endpoint| endpoint.snapshot.as_deref())
        .expect("remote snapshot")
        .clone();
    later.revision = later.revision.checked_next().expect("test precondition");
    later.resolved_config.clear();
    later.workspaces[0].label = "later".into();
    state.cache_endpoint_snapshot(&endpoint_id, Box::new(later));

    assert!(state.activate_endpoint_projection(&endpoint_id));
    assert!(state.active_resolved_config.is_some());
    assert_eq!(
        state
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.workspaces.first())
            .map(|workspace| workspace.label.as_str()),
        Some("later")
    );
}

#[test]
fn multi_machine_sidebar_draws_the_workspace_drop_marker() {
    let (mut state, _) = state_with_remote();
    state.compose(120, 40).expect("test precondition");
    let local = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.endpoint_id == ClientEndpointId::Local)
        .expect("local workspace row")
        .rect;
    let row = local.bottom();
    state.chrome_drag = Some(ClientChromeDrag::Workspace {
        source_workspace_id: test_workspace_id("w1"),
        target: Some((None, row)),
    });
    let frame = state.compose(120, 40).expect("dragging frame");
    let cell = &frame.cells[usize::from(row) * 120 + usize::from(local.x)];
    assert_eq!(cell.symbol, "─");
    assert_eq!(
        cell.fg,
        shepr_protocol::WireColor::from_ratatui(state.config.palette.accent)
    );
}

#[test]
fn machine_diagnostic_badge_reopens_notice_without_collapsing_machine() {
    let (mut state, id) = state_with_remote();
    state.set_endpoint_status(&id, ClientEndpointStatus::Attention);
    // ssh exits 255 for its own failures; this is how an auth prompt failure arrives.
    state.set_machine_diagnostic(
        &id,
        &shepr_remote::SshFailureDiagnostic::from_ssh_output(
            Some(255),
            "Permission denied (keyboard-interactive)".into(),
        ),
    );
    for _ in 0..2 {
        state.compose(120, 40).expect("test precondition");
        let hit = state
            .hits
            .machines
            .iter()
            .find(|hit| hit.endpoint_id == id)
            .expect("test precondition");
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.status_badge.x,
            row: hit.status_badge.y,
            modifiers: KeyModifiers::NONE,
        };
        let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
        assert!(outcome.repaint);
        assert!(!state.collapsed_endpoints.contains(&id));
        let notice = state
            .visible_endpoint_notice
            .take()
            .expect("test precondition");
        assert!(notice.body.contains("Permission denied"));
        assert!(
            notice
                .title
                .contains("shepr machine reconnect 0123456789abcdef0123456789abcdef")
        );
    }
    state.set_endpoint_status(&id, ClientEndpointStatus::Online);
    state.compose(120, 40).expect("test precondition");
    assert!(
        !state.machine_diagnostics.required_for(
            state
                .endpoints
                .iter()
                .find(|endpoint| endpoint.endpoint_id == id)
                .expect("test precondition")
        )
    );
}

fn state_with_scrollable_agents() -> (ClientShellState, ClientEndpointId) {
    let (mut state, remote) = state_with_remote();
    for endpoint_id in [ClientEndpointId::Local, remote.clone()] {
        let mut projection = state
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == endpoint_id)
            .expect("test precondition")
            .snapshot
            .clone()
            .expect("test precondition");
        projection.agents = (0..8)
            .map(|index| ClientShellAgent {
                pane_id: shepr_protocol::PublicPaneId::new(
                    &crate::tests::test_workspace_id("w1"),
                    index + 1,
                ),
                focused: index == 0,
                ..agent(&format!("agent {index}"), AgentStatus::Idle, 1)
            })
            .collect();
        projection.panes = projection
            .agents
            .iter()
            .map(|agent| ClientShellPane {
                pane_id: agent.pane_id.clone(),
                focused: agent.focused,
                ..projection.panes[0].clone()
            })
            .collect();
        state.set_endpoint_snapshot(&endpoint_id, projection);
    }
    state.compose(100, 28).expect("test precondition");
    state.agent_scroll = 6;
    state.compose(100, 28).expect("test precondition");
    assert_eq!(state.agent_scroll, 6);
    (state, remote)
}

#[test]
fn agent_navigation_reveals_offscreen_targets() {
    use shepr_termio::input::KeybindAction;

    for action in [
        KeybindAction::NextAgent,
        KeybindAction::PreviousAgent,
        KeybindAction::FocusAgent(0),
    ] {
        let (mut state, remote) = state_with_scrollable_agents();
        let (endpoint_id, pane_id) = match action {
            KeybindAction::NextAgent => (ClientEndpointId::Local, "w1:p2"),
            KeybindAction::PreviousAgent => (remote, "w1:p8"),
            _ => (ClientEndpointId::Local, "w1:p1"),
        };
        state.agent_scroll = if action == KeybindAction::PreviousAgent {
            0
        } else {
            state.hits.agent_max_scroll
        };
        state.compose(100, 28).expect("test precondition");
        assert!(
            !state
                .hits
                .endpoint_agents
                .iter()
                .any(|(_, endpoint, pane)| {
                    endpoint == &endpoint_id && pane.as_str() == pane_id
                })
        );

        let mut outcome = ClientShellInput::default();
        assert!(state.handle_endpoint_navigation(action, &mut outcome));
        assert!(outcome.repaint, "agent navigation must request a frame");
        if endpoint_id != state.active_endpoint_id {
            assert!(state.activate_endpoint_projection(&endpoint_id));
        }
        state.compose(100, 28).expect("test precondition");
        assert!(
            state
                .hits
                .endpoint_agents
                .iter()
                .any(|(_, endpoint, pane)| {
                    endpoint == &endpoint_id && pane.as_str() == pane_id
                }),
            "{action:?} must reveal the selected agent"
        );
    }
}

#[test]
fn agent_navigation_reveal_is_cancelled_by_another_selection() {
    for select_pane in [false, true] {
        let (mut state, remote) = state_with_scrollable_agents();
        let scroll = state.agent_scroll;
        let mut outcome = ClientShellInput::default();
        assert!(state.handle_endpoint_navigation(
            shepr_termio::input::KeybindAction::PreviousAgent,
            &mut outcome,
        ));
        assert_eq!(state.agent_scroll, scroll);
        if select_pane {
            assert!(state.focus_or_activate(
                remote.clone(),
                ClientEndpointFocusTarget::Pane(test_pane_id("w1:p1")),
                &mut outcome,
            ));
        } else {
            assert!(state.activate_endpoint(remote.clone(), &mut outcome));
        }
        assert!(state.activate_endpoint_projection(&remote));
        state.compose(100, 28).expect("test precondition");
        assert_eq!(state.agent_scroll, scroll);
    }
}

#[test]
fn agent_navigation_keeps_scroll_when_target_is_visible() {
    let (mut state, _) = state_with_scrollable_agents();
    let (_, endpoint_id, pane_id) = state.hits.endpoint_agents[1].clone();
    let targets = super::super::aggregate_navigation::online_agent_targets(
        &state.endpoints,
        &state.active_endpoint_id,
        state.config.agent_panel_sort,
    );
    let index = targets
        .iter()
        .position(|target| {
            target.endpoint_id == endpoint_id && target.pane_id.as_str() == pane_id.as_str()
        })
        .expect("test precondition");
    let scroll = state.agent_scroll;
    assert!(state.handle_endpoint_navigation(
        shepr_termio::input::KeybindAction::FocusAgent(index),
        &mut ClientShellInput::default(),
    ));
    state.compose(100, 28).expect("test precondition");
    assert_eq!(state.agent_scroll, scroll);
}

#[test]
fn switching_machines_preserves_aggregate_agent_scroll_and_visible_rows() {
    let (mut state, remote) = state_with_scrollable_agents();
    for endpoint_id in [remote.clone(), ClientEndpointId::Local, remote] {
        let visible = state.hits.endpoint_agents.clone();
        let (rect, _, pane_id) = visible
            .iter()
            .find(|(_, endpoint, _)| endpoint == &endpoint_id)
            .expect("destination agent remains visible");
        let click = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 2,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        })]);
        assert!(matches!(
            click.actions.as_slice(),
            [ClientShellAction::ActivateEndpoint {
                endpoint_id: target,
                target: Some(ClientEndpointFocusTarget::Pane(target_pane)),
            }] if target == &endpoint_id && target_pane == &pane_id.to_string()
        ));

        state.workspace_scroll = 3;
        state.tab_scroll = 2;
        assert!(state.activate_endpoint_projection(&endpoint_id));
        assert_eq!(state.agent_scroll, 6);
        assert_eq!(state.workspace_scroll, 0);
        assert_eq!(state.tab_scroll, 0);
        assert!(state.pane_surface.is_none());

        let mut next_surface = surface();
        next_surface.boot_id = state
            .endpoint_boot_id(&endpoint_id)
            .expect("test precondition")
            .clone();
        state.set_pane_surface(next_surface);
        state.compose(100, 28).expect("test precondition");
        assert_eq!(state.agent_scroll, 6);
        assert_eq!(state.hits.endpoint_agents, visible);
    }
}

#[test]
fn local_agent_click_can_cancel_a_pending_remote_switch() {
    for reconnecting in [false, true] {
        let (mut state, remote) = state_with_scrollable_agents();
        assert!(state.activate_endpoint(remote, &mut ClientShellInput::default()));
        if reconnecting {
            state.mark_endpoint_disconnected(&ClientEndpointId::Local);
        }
        state.compose(100, 28).expect("test precondition");
        let (rect, _, pane_id) = state
            .hits
            .endpoint_agents
            .iter()
            .find(|(_, endpoint, _)| endpoint.is_local())
            .expect("test precondition")
            .clone();
        let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 2,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        })]);
        assert!(
            matches!(outcome.actions.as_slice(), [ClientShellAction::ActivateEndpoint {
            endpoint_id: ClientEndpointId::Local,
            target: Some(ClientEndpointFocusTarget::Pane(target)),
        }] if target == &pane_id.to_string())
        );
    }
}

#[test]
fn aggregate_agent_scroll_still_clamps_when_rows_shrink_on_activation() {
    let (mut state, remote) = state_with_scrollable_agents();
    for endpoint_id in [ClientEndpointId::Local, remote.clone()] {
        let mut projection = state
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == endpoint_id)
            .expect("test precondition")
            .snapshot
            .clone()
            .expect("test precondition");
        projection.revision = projection
            .revision
            .checked_next()
            .expect("test precondition");
        projection.agents.truncate(1);
        state.set_endpoint_snapshot(&endpoint_id, projection);
    }
    assert!(state.activate_endpoint_projection(&remote));
    state.compose(100, 28).expect("test precondition");
    assert_eq!(state.agent_scroll, 0);
    assert_eq!(state.hits.agent_max_scroll, 0);
    assert_eq!(state.hits.endpoint_agents.len(), 2);
}

#[test]
fn same_machine_reboot_still_resets_agent_scroll() {
    let (mut state, _) = state_with_scrollable_agents();
    let mut projection = state.snapshot.clone().expect("test precondition");
    projection.boot_id = crate::tests::test_boot_id("restarted-local");
    state.cache_endpoint_snapshot(&ClientEndpointId::Local, projection);
    assert!(state.activate_endpoint_projection(&ClientEndpointId::Local));
    assert_eq!(state.agent_scroll, 0);
}

#[test]
fn switching_machines_from_copy_mode_restores_terminal_input() {
    let (mut state, remote) = state_with_remote();
    let mut local_surface = surface();
    local_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 20,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.set_pane_surface(local_surface);
    state.compose(100, 28).expect("test precondition");
    assert!(state.enter_copy_mode(&mut ClientShellInput::default()));
    assert_eq!(state.mode, ClientShellMode::Copy);

    assert!(state.activate_endpoint_projection(&remote));
    let mut remote_surface = surface();
    remote_surface.boot_id = crate::tests::test_boot_id("remote-boot");
    state.set_pane_surface(remote_surface);
    state.compose(100, 28).expect("test precondition");

    assert!(state.copy_mode.is_none());
    assert_eq!(state.mode, ClientShellMode::Terminal);
    let input = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::NONE),
    )]);
    assert!(matches!(
        input.requests.as_slice(),
        [ClientMessage::ClientShellPaneInput { pane_id, events }]
            if pane_id == "w1:p1" && events.len() == 1
    ));
}

#[test]
fn live_catalog_rename_preserves_snapshot_and_remove_readd_clears_it() {
    let (mut state, remote) = state_with_remote();
    let mut profile = remote_profile();
    profile.label = "Renamed".into();
    state.set_endpoint_catalog(&[profile.clone()]);
    assert_eq!(state.endpoint_label(&remote), "Renamed");
    assert!(state.endpoint_is_online(&remote));
    assert_eq!(
        state.endpoint_boot_id(&remote),
        Some(&crate::tests::test_boot_id("remote-boot"))
    );
    state.set_endpoint_catalog(&[]);
    assert_eq!(state.endpoint_status(&remote), None);
    assert!(!state.endpoint_has_snapshot(&remote));
    state.set_endpoint_catalog(&[profile]);
    assert_eq!(
        state.endpoint_status(&remote),
        Some(ClientEndpointStatus::Connecting)
    );
    assert!(!state.endpoint_has_snapshot(&remote));
}

#[test]
fn live_catalog_active_removal_does_not_retain_remote_projection_or_input() {
    let (mut state, remote) = state_with_remote();
    assert!(state.activate_endpoint_projection(&remote));
    state.set_pane_surface(surface());
    state.mode = ClientShellMode::Prefix;
    state.overlay = Some(ClientShellOverlay::GlobalMenu(ClientGlobalMenuOverlay {
        highlighted: 0,
    }));
    state.select_unavailable_local();
    state.set_endpoint_catalog(&[]);
    assert!(state.endpoint_is_active(&ClientEndpointId::Local));
    assert!(state.snapshot.is_none());
    assert!(state.pane_surface.is_none());
    assert!(state.pending_pane_surface.is_none());
    assert!(state.overlay.is_none());
    assert_eq!(state.mode, ClientShellMode::Terminal);
    assert!(state.endpoint_has_snapshot(&ClientEndpointId::Local));
    let frame = state.compose(100, 30).expect("test precondition");
    let buffer = frame.to_ratatui_buffer().expect("test precondition");
    let text = buffer
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(!text.contains("remote-workspace"));
}

#[test]
fn machine_navigation_does_not_require_a_local_snapshot_or_surface() {
    for (cols, rows) in [(100, 28), (36, 18)] {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        let profile = remote_profile();
        let remote = ClientEndpointId::Ssh(profile.id.clone());
        state.set_endpoint_catalog(&[profile]);
        state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Reconnecting);
        state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
        state.set_endpoint_snapshot(&remote, Box::new(snapshot()));
        assert!(state.snapshot.is_none());
        assert!(state.pane_surface.is_none());
        let frame = state
            .compose(cols, rows)
            .expect("connection chrome without Local");
        let local = state
            .hits
            .machines
            .iter()
            .find(|hit| hit.endpoint_id.is_local())
            .expect("test precondition")
            .rect;
        let buffer = frame.to_ratatui_buffer().expect("test precondition");
        let local_row = (local.x..local.right())
            .map(|x| buffer[(x, local.y)].symbol())
            .collect::<String>();
        assert!(!local_row.contains("reconnecting"));
        assert!(
            !local_row.contains('◐'),
            "Local never gets a connection badge"
        );
        let hit = state
            .hits
            .machines
            .iter()
            .find(|hit| hit.endpoint_id == remote)
            .expect("test precondition")
            .rect;
        let mut outcome = ClientShellInput::default();
        state.handle_mouse(
            crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: hit.x + 5,
                row: hit.y,
                modifiers: KeyModifiers::NONE,
            },
            std::time::Instant::now(),
            &mut outcome,
        );
        assert!(
            matches!(outcome.actions.as_slice(), [ClientShellAction::ActivateEndpoint { endpoint_id, .. }] if endpoint_id == &remote)
        );
        assert!(
            state.snapshot.is_none(),
            "selection is committed only by coherent activation"
        );
    }
}

#[test]
fn sidebar_renders_local_and_saved_ssh_endpoints_with_status() {
    let (mut state, _) = state_with_remote();
    let frame = state.compose(100, 28).expect("combined endpoint frame");
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
    assert!(text.contains("Local"));
    assert!(text.contains("Build"));
    // The server's workspace number takes the leading column, so the name is
    // clipped at this sidebar width.
    assert!(text.contains("1  ○ remote-workspa"), "{text}");
    let local = state
        .hits
        .machines
        .iter()
        .find(|hit| hit.endpoint_id.is_local())
        .expect("local machine row")
        .rect;
    let remote = state
        .hits
        .machines
        .iter()
        .find(|hit| !hit.endpoint_id.is_local())
        .expect("remote machine row")
        .rect;
    let buffer = frame.to_ratatui_buffer().expect("frame should reconstruct");
    assert_ne!(buffer[(local.right() - 1, local.y)].symbol(), "●");
    assert_eq!(buffer[(remote.right() - 1, remote.y)].symbol(), "●");
    assert_eq!(
        buffer[(remote.right() - 1, remote.y)].fg,
        state.config.palette.green
    );

    state.sidebar_collapsed = true;
    let frame = state.compose(100, 28).expect("collapsed endpoint frame");
    let local = state
        .hits
        .machines
        .iter()
        .find(|hit| hit.endpoint_id.is_local())
        .expect("collapsed local machine row")
        .rect;
    let remote = state
        .hits
        .machines
        .iter()
        .find(|hit| !hit.endpoint_id.is_local())
        .expect("collapsed remote machine row")
        .rect;
    let buffer = frame.to_ratatui_buffer().expect("frame should reconstruct");
    assert_ne!(buffer[(local.right() - 1, local.y)].symbol(), "●");
    assert_eq!(buffer[(remote.right() - 1, remote.y)].symbol(), "●");
}

#[test]
fn local_and_ssh_sidebars_show_server_workspace_numbers() {
    let (mut state, endpoint_id) = state_with_remote();
    let mut local = state.snapshot.as_deref().expect("local snapshot").clone();
    local.workspaces[0].number = 17;
    state.set_snapshot(Box::new(local));

    let mut remote = state
        .endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        .and_then(|endpoint| endpoint.snapshot.as_deref())
        .expect("remote snapshot")
        .clone();
    remote.workspaces[0].number = 42;
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote));

    for collapsed in [false, true] {
        state.sidebar_collapsed = collapsed;
        let frame = state.compose(100, 28).expect("workspace sidebar");
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
        assert!(text.contains("17"), "local server workspace number: {text}");
        assert!(text.contains("42"), "SSH server workspace number: {text}");
    }
}

#[test]
fn expanded_machine_sidebar_reveals_newly_focused_workspace() {
    let (mut state, remote_id) = state_with_remote();
    let mut initial = snapshot();
    let template = initial.workspaces[0].clone();
    initial.workspaces = (1..=12)
        .map(|number| ClientShellWorkspace {
            workspace_id: test_workspace_id(&format!("w{number}")),
            number,
            label: format!("space-{number}"),
            focused: number == 1,
            ..template.clone()
        })
        .collect();
    // Reuse workspace IDs across machines so revealing must be endpoint-scoped.
    let mut remote = initial.clone();
    remote.boot_id = crate::tests::test_boot_id("remote-boot");
    remote.workspaces.push(ClientShellWorkspace {
        workspace_id: test_workspace_id("w13"),
        number: 13,
        focused: false,
        ..template.clone()
    });
    state.set_endpoint_snapshot(&remote_id, Box::new(remote));
    state.set_snapshot(Box::new(initial));
    state.compose(106, 20).expect("full machines sidebar");
    assert!(state.hits.workspace_max_scroll > 0);

    let mut update = state.snapshot.as_deref().expect("snapshot").clone();
    update.revision = shepr_protocol::ProjectionRevision::new(2);
    update.workspaces.push(ClientShellWorkspace {
        workspace_id: test_workspace_id("w13"),
        number: 13,
        label: "new-space".into(),
        ..template
    });
    update.focused_workspace_id = Some(test_workspace_id("w13"));
    for workspace in &mut update.workspaces {
        workspace.focused = workspace.workspace_id == "w13";
    }
    state.set_snapshot(Box::new(update));
    let mut updated_surface = surface();
    updated_surface.projection_revision = shepr_protocol::ProjectionRevision::new(2);
    state.set_pane_surface(updated_surface);
    state.compose(106, 2).expect("zero-height workspace body");
    assert!(state.reveal_focused_workspace);
    state.compose(106, 20).expect("new workspace revealed");
    assert!(
        state
            .hits
            .workspaces
            .iter()
            .any(|hit| { hit.endpoint_id == ClientEndpointId::Local && hit.workspace_id == "w13" })
    );

    state.workspace_scroll = 0;
    state.compose(106, 20).expect("manual scroll");
    assert_eq!(state.workspace_scroll, 0);
    assert!(
        !state
            .hits
            .workspaces
            .iter()
            .any(|hit| { hit.endpoint_id == ClientEndpointId::Local && hit.workspace_id == "w13" })
    );
    let unchanged = state.snapshot.as_deref().expect("snapshot").clone();
    state.set_snapshot(Box::new(unchanged));
    state
        .compose(106, 20)
        .expect("unchanged focus preserves scroll");
    assert_eq!(state.workspace_scroll, 0);
}

#[test]
fn expanded_machine_sidebar_applies_space_row_gap_within_each_machine() {
    let (mut state, remote_id) = state_with_remote();
    state.config.spaces.row_gap = 1;

    let add_second_workspace = |snapshot: &mut ClientShellSnapshot| {
        let mut workspace = snapshot.workspaces[0].clone();
        workspace.workspace_id = test_workspace_id("w2");
        workspace.active_tab_id = test_tab_id("w2:t1");
        workspace.number = 2;
        workspace.label = "second-workspace".into();
        workspace.focused = false;
        snapshot.workspaces.push(workspace);
    };
    let mut local = snapshot();
    add_second_workspace(&mut local);
    state.set_snapshot(Box::new(local));
    let mut remote = snapshot();
    remote.boot_id = crate::tests::test_boot_id("remote-boot");
    add_second_workspace(&mut remote);
    let mut third = remote.workspaces[1].clone();
    third.workspace_id = test_workspace_id("w3");
    third.number = 3;
    third.label = "third-workspace".into();
    remote.workspaces.push(third);
    state.set_endpoint_snapshot(&remote_id, Box::new(remote));

    state.compose(100, 40).expect("combined endpoint frame");
    let local_workspaces = state
        .hits
        .workspaces
        .iter()
        .filter(|hit| hit.endpoint_id.is_local())
        .collect::<Vec<_>>();
    assert_eq!(local_workspaces.len(), 2);
    assert_eq!(
        local_workspaces[1].rect.y,
        local_workspaces[0].rect.bottom() + 1
    );

    let local_machine = state
        .hits
        .machines
        .iter()
        .find(|hit| hit.endpoint_id.is_local())
        .expect("local machine");
    let remote_machine = state
        .hits
        .machines
        .iter()
        .find(|hit| hit.endpoint_id == remote_id)
        .expect("remote machine");
    assert_eq!(local_workspaces[0].rect.y, local_machine.rect.bottom());
    assert_eq!(remote_machine.rect.y, local_workspaces[1].rect.bottom());

    let remote_workspaces = state
        .hits
        .workspaces
        .iter()
        .filter(|hit| hit.endpoint_id == remote_id)
        .collect::<Vec<_>>();
    assert_eq!(remote_workspaces.len(), 3);
    assert_eq!(remote_workspaces[0].rect.y, remote_machine.rect.bottom());
    assert_eq!(
        remote_workspaces[1].rect.y,
        remote_workspaces[0].rect.bottom() + 1
    );
    assert_eq!(
        remote_workspaces[2].rect.y,
        remote_workspaces[1].rect.bottom() + 1
    );

    // 22 rows gives an 8-row workspace body: exactly three two-row workspaces
    // with a gap after each of the first two.
    state.workspace_scroll = usize::MAX;
    state.compose(100, 22).expect("scrolled endpoint frame");
    let metrics = state
        .hits
        .workspace_scroll_metrics
        .expect("workspace scroll metrics");
    assert!(metrics.max_offset_from_bottom > 0);
    assert_eq!(metrics.offset_from_bottom, 0);
    assert_eq!(state.workspace_scroll, metrics.max_offset_from_bottom);
    let visible_remote = state
        .hits
        .workspaces
        .iter()
        .filter(|hit| hit.endpoint_id == remote_id)
        .collect::<Vec<_>>();
    assert_eq!(visible_remote.len(), 3);
    let gap_y = visible_remote[1].rect.bottom();
    assert_eq!(visible_remote[2].rect.y, gap_y + 1);
    assert!(visible_remote[2].rect.bottom() <= state.hits.workspace_body.bottom());
    assert!(
        state
            .hits
            .workspaces
            .iter()
            .all(|hit| gap_y < hit.rect.top() || gap_y >= hit.rect.bottom())
    );
}

#[test]
fn active_workspace_is_the_only_highlight_when_machine_is_expanded() {
    let (mut state, endpoint_id) = state_with_remote();
    assert!(state.activate_endpoint_projection(&endpoint_id));
    let mut remote_surface = surface();
    remote_surface.boot_id = crate::tests::test_boot_id("remote-boot");
    state.set_pane_surface(remote_surface);

    let frame = state.compose(100, 28).expect("combined endpoint frame");
    let machine = state
        .hits
        .machines
        .iter()
        .find(|hit| hit.endpoint_id == endpoint_id)
        .expect("remote machine hit")
        .rect;
    let workspace = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.endpoint_id == endpoint_id)
        .expect("remote workspace hit")
        .rect;
    let buffer = frame.to_ratatui_buffer().expect("frame should reconstruct");
    assert_ne!(
        buffer[(machine.x, machine.y)].bg,
        state.config.palette.active_row_bg
    );
    assert_eq!(
        buffer[(workspace.x + 2, workspace.y)].bg,
        state.config.palette.active_row_bg
    );

    state.collapsed_endpoints.insert(endpoint_id.clone());
    let frame = state.compose(100, 28).expect("collapsed endpoint frame");
    let machine = state
        .hits
        .machines
        .iter()
        .find(|hit| hit.endpoint_id == endpoint_id)
        .expect("remote machine hit")
        .rect;
    let buffer = frame.to_ratatui_buffer().expect("frame should reconstruct");
    assert_eq!(
        buffer[(machine.x, machine.y)].bg,
        state.config.palette.active_row_bg
    );
}

#[test]
fn aggregate_agents_use_configured_rows_machine_token_and_status_colors() {
    use shepr_api::schema::AgentStatus;
    use shepr_config::{AgentSidebarToken, StatusIndicatorStyle};

    let mut config = Config::default();
    config.ui.status_indicators = StatusIndicatorStyle::Symbols;
    config.ui.sidebar.agents.rows = vec![vec![
        AgentSidebarToken::StateIcon,
        AgentSidebarToken::Machine,
        AgentSidebarToken::Agent,
    ]];
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    let profile = remote_profile();
    let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);

    let mut local = snapshot();
    local.agents = vec![agent("local agent", AgentStatus::Idle, 1)];
    state.set_snapshot(Box::new(local));
    state.set_pane_surface(surface());
    let mut remote = snapshot();
    remote.boot_id = crate::tests::test_boot_id("remote-boot");
    remote.agents = vec![agent("remote agent", AgentStatus::Blocked, 1)];
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote));

    let frame = state.compose(100, 28).expect("combined endpoint frame");
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
    assert!(text.contains("○ Local · pi"), "frame: {text}");
    assert!(text.contains("× Build · pi"), "frame: {text}");
    assert!(text.contains("grouped"), "frame: {text}");
    let toggle = state.hits.agent_sort_toggle;
    assert!(!toggle.is_empty());
    let click = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: toggle.x,
        row: toggle.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert_eq!(
        state.config.agent_panel_sort,
        shepr_config::AgentPanelSortConfig::Priority
    );
    assert!(click.actions.is_empty());

    let buffer = frame
        .to_ratatui_buffer()
        .expect("aggregate frame should reconstruct");
    assert!(
        buffer
            .content()
            .iter()
            .any(|cell| cell.symbol() == "×" && cell.fg == state.config.palette.red)
    );
}

#[test]
fn aggregate_priority_uses_client_observed_recency_across_machines() {
    use shepr_api::schema::AgentStatus;
    use shepr_config::AgentSidebarToken;

    let mut config = Config::default();
    config.ui.agent_panel_sort = shepr_config::AgentPanelSortConfig::Priority;
    config.ui.sidebar.agents.rows =
        vec![vec![AgentSidebarToken::Machine, AgentSidebarToken::Agent]];
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    let profile = remote_profile();
    let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);

    let mut local = snapshot();
    local.agents = vec![agent("local agent", AgentStatus::Idle, 1)];
    state.set_snapshot(Box::new(local));
    state.set_pane_surface(surface());
    let mut remote = snapshot();
    remote.boot_id = crate::tests::test_boot_id("remote-boot");
    remote.agents = vec![agent("remote agent", AgentStatus::Idle, 1)];
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote.clone()));

    let mut local = snapshot();
    local.agents = vec![agent("local agent", AgentStatus::Idle, 2)];
    state.set_snapshot(Box::new(local));
    let frame_text = |state: &mut ClientShellState| {
        let frame = state.compose(100, 28).expect("combined endpoint frame");
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
    };
    let text = frame_text(&mut state);
    assert!(
        text.find("Local · pi").expect("local agent")
            < text.find("Build · pi").expect("remote agent")
    );

    remote.agents = vec![agent("remote agent", AgentStatus::Working, 2)];
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote.clone()));
    let text = frame_text(&mut state);
    assert!(
        text.find("Build · pi").expect("remote agent")
            < text.find("Local · pi").expect("local agent")
    );

    remote.agents = vec![agent("remote agent", AgentStatus::Idle, 3)];
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote));
    let text = frame_text(&mut state);
    assert!(
        text.find("Build · pi").expect("remote agent")
            < text.find("Local · pi").expect("local agent")
    );
    let mut outcome = ClientShellInput::default();
    assert!(state.handle_endpoint_navigation(
        shepr_termio::input::KeybindAction::FocusAgent(0),
        &mut outcome,
    ));
    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint {
            endpoint_id: activated,
            target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
        }] if activated == &endpoint_id && pane_id == "w1:p1"
    ));
}

#[test]
fn unselected_endpoint_snapshot_keeps_server_idle_status() {
    use shepr_api::schema::AgentStatus;

    let (mut state, endpoint_id) = state_with_remote();
    let mut remote = snapshot();
    remote.boot_id = crate::tests::test_boot_id("remote-boot");
    remote.agents = vec![agent("background agent", AgentStatus::Working, 2)];
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote.clone()));
    remote.revision = shepr_protocol::ProjectionRevision::new(2);
    remote.agents = vec![agent("background agent", AgentStatus::Idle, 3)];

    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote));

    let status = state
        .endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        .and_then(|endpoint| endpoint.snapshot.as_deref())
        .and_then(|snapshot| snapshot.agents.first())
        .map(|agent| agent.agent_status);
    assert_eq!(status, Some(AgentStatus::Idle));
    assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
}

#[test]
fn clicking_remote_machine_name_requests_activation_without_mutating_projection() {
    let (mut state, endpoint_id) = state_with_remote();
    state.compose(100, 28).expect("combined endpoint frame");
    let hit = state
        .hits
        .machines
        .iter()
        .find(|hit| hit.endpoint_id == endpoint_id)
        .expect("remote endpoint hit")
        .rect;
    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.x + 3,
        row: hit.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint {
            endpoint_id: activated,
            target: None,
        }] if activated == &endpoint_id
    ));
    assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
    assert_eq!(
        state.snapshot.as_deref().map(|snapshot| &snapshot.boot_id),
        Some(&crate::tests::test_boot_id("boot-1"))
    );
}

#[test]
fn clicking_local_can_cancel_a_remote_switch_while_local_is_still_displayed() {
    for workspace in [false, true] {
        let (mut state, remote) = state_with_remote();
        state.compose(100, 28).expect("test precondition");
        let mut pending = ClientShellInput::default();
        assert!(state.activate_endpoint(remote, &mut pending));
        assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
        let rect = if workspace {
            state
                .hits
                .workspaces
                .iter()
                .find(|hit| hit.endpoint_id.is_local())
                .expect("test precondition")
                .rect
        } else {
            state
                .hits
                .machines
                .iter()
                .find(|hit| hit.endpoint_id.is_local())
                .expect("test precondition")
                .rect
        };
        let outcome = state.handle_raw_events(vec![
            RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 5,
                row: rect.y,
                modifiers: KeyModifiers::empty(),
            }),
            RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: rect.x + 5,
                row: rect.y,
                modifiers: KeyModifiers::empty(),
            }),
        ]);
        assert!(
            matches!(outcome.actions.as_slice(), [ClientShellAction::ActivateEndpoint {
            endpoint_id: ClientEndpointId::Local, target,
        }] if target.is_some() == workspace)
        );
    }
}

#[test]
fn reconnecting_local_selection_still_reaches_the_runtime() {
    let (mut state, _) = state_with_remote();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    let mut outcome = ClientShellInput::default();
    state.focus_or_activate(
        ClientEndpointId::Local,
        ClientEndpointFocusTarget::Workspace(shepr_test_fixtures::id("w1")),
        &mut outcome,
    );
    assert!(
        matches!(outcome.actions.as_slice(), [ClientShellAction::ActivateEndpoint {
        endpoint_id: ClientEndpointId::Local,
        target: Some(ClientEndpointFocusTarget::Workspace(id)),
    }] if id == "w1")
    );
}

#[test]
fn machine_arrow_toggles_inactive_machine_without_switching() {
    for sidebar_collapsed in [false, true] {
        for status in [
            ClientEndpointStatus::Online,
            ClientEndpointStatus::Reconnecting,
        ] {
            let (mut state, remote_id) = state_with_remote();
            let mut other_profile = remote_profile();
            other_profile.id =
                ProfileId::parse("1123456789abcdef0123456789abcdef").expect("test precondition");
            let other_id = ClientEndpointId::Ssh(other_profile.id.clone());
            state.set_endpoint_catalog(&[remote_profile(), other_profile]);
            state.set_endpoint_status(&other_id, ClientEndpointStatus::Online);
            state.set_endpoint_snapshot(&other_id, Box::new(snapshot()));
            state.set_endpoint_status(&remote_id, status);
            state.sidebar_collapsed = sidebar_collapsed;

            for collapsed in [true, false] {
                let frame = state.compose(100, 28).expect("three machine frame");
                let machine = state
                    .hits
                    .machines
                    .iter()
                    .find(|hit| hit.endpoint_id == remote_id)
                    .expect("remote machine")
                    .rect;
                let column = machine.x + u16::from(!sidebar_collapsed);
                let buffer = frame.to_ratatui_buffer().expect("frame buffer");
                assert_eq!(
                    buffer[(column, machine.y)].symbol(),
                    if collapsed { "▾" } else { "▸" }
                );
                let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row: machine.y,
                    modifiers: KeyModifiers::empty(),
                })]);
                assert!(
                    outcome.actions.is_empty(),
                    "collapse must not switch machines"
                );
                assert!(outcome.requests.is_empty());
                assert!(outcome.repaint);
                assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
                assert_eq!(
                    state.snapshot.as_ref().expect("test precondition").boot_id,
                    crate::tests::test_boot_id("boot-1")
                );
                assert_eq!(
                    state
                        .snapshot
                        .as_ref()
                        .expect("test precondition")
                        .focused_workspace_id
                        .as_deref(),
                    Some("w1")
                );
                assert_eq!(state.collapsed_endpoints.contains(&remote_id), collapsed);
                assert!(!state.collapsed_endpoints.contains(&ClientEndpointId::Local));
                assert!(!state.collapsed_endpoints.contains(&other_id));
                assert!(state.endpoint_error.is_none());

                state.compose(100, 28).expect("toggled machine frame");
                assert_eq!(
                    state
                        .hits
                        .workspaces
                        .iter()
                        .any(|hit| hit.endpoint_id == remote_id),
                    !collapsed
                );
                for endpoint_id in [&ClientEndpointId::Local, &other_id] {
                    assert!(
                        state
                            .hits
                            .workspaces
                            .iter()
                            .any(|hit| &hit.endpoint_id == endpoint_id)
                    );
                }
            }
        }
    }
}

#[test]
fn context_menu_lookup_ignores_inactive_endpoint_workspaces() {
    let (mut state, endpoint_id) = state_with_remote();
    state.compose(100, 28).expect("combined endpoint frame");
    let remote = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.endpoint_id == endpoint_id)
        .expect("remote workspace")
        .rect;
    let local = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.endpoint_id.is_local())
        .expect("local workspace")
        .rect;

    assert_eq!(
        state.active_endpoint_workspace_at((remote.x, remote.y)),
        None
    );
    assert_eq!(
        state.active_endpoint_workspace_at((local.x, local.y)),
        Some(shepr_test_fixtures::id("w1"))
    );
}

#[test]
fn future_surface_waits_for_its_exact_snapshot_revision() {
    let (mut state, _) = state_with_remote();
    let mut future = surface();
    future.projection_revision = shepr_protocol::ProjectionRevision::new(2);
    future.surface_revision = shepr_protocol::SurfaceRevision::new(2);
    state.set_pane_surface(future);
    assert_eq!(
        state
            .pane_surface
            .as_ref()
            .map(|surface| surface.projection_revision),
        Some(shepr_protocol::ProjectionRevision::new(1))
    );
    assert_eq!(
        state
            .pending_pane_surface
            .as_ref()
            .map(|surface| surface.projection_revision),
        Some(shepr_protocol::ProjectionRevision::new(2))
    );

    let mut next = snapshot();
    next.revision = shepr_protocol::ProjectionRevision::new(2);
    state.set_snapshot(Box::new(next));
    assert_eq!(
        state
            .pane_surface
            .as_ref()
            .map(|surface| surface.projection_revision),
        Some(shepr_protocol::ProjectionRevision::new(2))
    );
    assert!(state.pending_pane_surface.is_none());
}

#[test]
fn inactive_endpoint_snapshot_cache_never_regresses_revision() {
    let (mut state, endpoint_id) = state_with_remote();
    let mut newest = snapshot();
    newest.boot_id = crate::tests::test_boot_id("remote-boot");
    newest.revision = shepr_protocol::ProjectionRevision::new(3);
    newest.workspaces[0].label = "newest".into();
    state.set_endpoint_snapshot(&endpoint_id, Box::new(newest));
    let mut delayed = snapshot();
    delayed.boot_id = crate::tests::test_boot_id("remote-boot");
    delayed.revision = shepr_protocol::ProjectionRevision::new(2);
    delayed.workspaces[0].label = "delayed".into();

    state.set_endpoint_snapshot(&endpoint_id, Box::new(delayed));

    let label = state
        .endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        .and_then(|endpoint| endpoint.snapshot.as_deref())
        .and_then(|snapshot| snapshot.workspaces.first())
        .map(|workspace| workspace.label.as_str());
    assert_eq!(label, Some("newest"));
}

#[test]
fn new_connection_generation_accepts_a_lower_same_boot_projection_revision() {
    let (mut state, endpoint_id) = state_with_remote();
    let mut previous = snapshot();
    previous.boot_id = crate::tests::test_boot_id("shared-server-boot");
    previous.revision = shepr_protocol::ProjectionRevision::new(9);
    previous.workspaces[0].label = "old connection".into();
    state.cache_endpoint_snapshot_for_generation(&endpoint_id, 4, Box::new(previous));
    let mut reconnected = snapshot();
    reconnected.boot_id = crate::tests::test_boot_id("shared-server-boot");
    reconnected.revision = shepr_protocol::ProjectionRevision::new(1);
    reconnected.workspaces[0].label = "new connection".into();

    state.cache_endpoint_snapshot_for_generation(&endpoint_id, 5, Box::new(reconnected));

    let endpoint = state
        .endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        .expect("remote endpoint");
    assert_eq!(endpoint.snapshot_generation, Some(5));
    assert_eq!(
        endpoint
            .snapshot
            .as_ref()
            .expect("test precondition")
            .revision,
        1
    );
    assert_eq!(
        endpoint
            .snapshot
            .as_ref()
            .expect("test precondition")
            .workspaces[0]
            .label,
        "new connection"
    );
}

#[test]
fn reconnect_same_endpoint_accepts_new_generation_surface_revision() {
    for previous_revision in [9, 1] {
        let (mut state, endpoint_id) = state_with_remote();
        let mut previous = snapshot();
        previous.boot_id = crate::tests::test_boot_id("shared-server-boot");
        previous.revision = shepr_protocol::ProjectionRevision::new(previous_revision);
        state.cache_endpoint_snapshot_for_generation(&endpoint_id, 4, Box::new(previous));
        assert!(state.activate_endpoint_projection(&endpoint_id));
        let mut previous_surface = surface();
        previous_surface.boot_id = crate::tests::test_boot_id("shared-server-boot");
        previous_surface.projection_revision =
            shepr_protocol::ProjectionRevision::new(previous_revision);
        previous_surface.surface_revision = shepr_protocol::SurfaceRevision::new(9);
        state.set_pane_surface(previous_surface.clone());
        previous_surface.projection_revision = previous_surface
            .projection_revision
            .checked_next()
            .expect("test precondition");
        state.set_pane_surface(previous_surface);
        assert!(state.pending_pane_surface.is_some());
        state.agent_scroll = 7;

        state.mark_endpoint_disconnected(&endpoint_id);
        let mut reconnected = snapshot();
        reconnected.boot_id = crate::tests::test_boot_id("shared-server-boot");
        reconnected.revision = shepr_protocol::ProjectionRevision::new(1);
        state.cache_endpoint_snapshot_for_generation(&endpoint_id, 5, Box::new(reconnected));
        assert_eq!(
            state.snapshot.as_ref().expect("test precondition").revision,
            previous_revision
        );
        assert_eq!(
            state
                .pane_surface
                .as_ref()
                .expect("test precondition")
                .surface_revision,
            9
        );

        state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);
        assert!(state.activate_endpoint_projection(&endpoint_id));
        assert!(state.compose(106, 20).is_none());
        let mut reconnected_surface = surface();
        reconnected_surface.boot_id = crate::tests::test_boot_id("shared-server-boot");
        reconnected_surface.projection_revision = shepr_protocol::ProjectionRevision::new(1);
        reconnected_surface.surface_revision = shepr_protocol::SurfaceRevision::new(1);
        state.set_pane_surface(reconnected_surface);

        assert_eq!(
            state.snapshot.as_ref().expect("test precondition").revision,
            1
        );
        assert_eq!(
            state
                .pane_surface
                .as_ref()
                .expect("test precondition")
                .projection_revision,
            1
        );
        assert_eq!(
            state
                .pane_surface
                .as_ref()
                .expect("test precondition")
                .surface_revision,
            1
        );
        assert!(state.pending_pane_surface.is_none());
        assert_eq!(state.agent_scroll, 7);
        assert!(state.compose(106, 20).is_some());
    }
}

#[test]
fn reconnect_snapshot_waits_for_coherent_activation_before_replacing_projection() {
    let (mut state, endpoint_id) = state_with_remote();
    assert!(state.activate_endpoint_projection(&endpoint_id));
    assert_eq!(
        state
            .snapshot
            .as_deref()
            .expect("test precondition")
            .boot_id,
        crate::tests::test_boot_id("remote-boot")
    );

    state.mark_endpoint_disconnected(&endpoint_id);
    let mut replacement = snapshot();
    replacement.boot_id = crate::tests::test_boot_id("replacement-boot");
    state.cache_endpoint_snapshot(&endpoint_id, Box::new(replacement));
    assert_eq!(
        state
            .snapshot
            .as_deref()
            .expect("test precondition")
            .boot_id,
        crate::tests::test_boot_id("remote-boot")
    );

    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);
    assert!(state.activate_endpoint_projection(&endpoint_id));
    assert_eq!(
        state
            .snapshot
            .as_deref()
            .expect("test precondition")
            .boot_id,
        crate::tests::test_boot_id("replacement-boot")
    );
}

#[test]
fn disconnected_active_endpoint_freezes_surface_and_marks_cached_ui_stale() {
    use shepr_api::schema::AgentStatus;
    use shepr_config::{AgentSidebarToken, StatusIndicatorStyle};

    let (mut state, endpoint_id) = state_with_remote();
    state.config.status_indicators = StatusIndicatorStyle::Symbols;
    state.config.agents.rows = vec![vec![
        AgentSidebarToken::StateIcon,
        AgentSidebarToken::Machine,
        AgentSidebarToken::Agent,
    ]];
    let endpoint = state
        .endpoints
        .iter_mut()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        .expect("remote endpoint");
    endpoint.snapshot.as_mut().expect("remote snapshot").agents =
        vec![agent("remote agent", AgentStatus::Blocked, 1)];
    assert!(state.activate_endpoint_projection(&endpoint_id));
    let mut remote_surface = surface();
    remote_surface.boot_id = crate::tests::test_boot_id("remote-boot");
    state.set_pane_surface(remote_surface);

    state.mark_endpoint_disconnected(&endpoint_id);
    let frame = state.compose(100, 28).expect("frozen endpoint frame");
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

    assert_eq!(
        state.endpoint_status(&endpoint_id),
        Some(ClientEndpointStatus::Reconnecting)
    );
    assert!(text.contains("◐ reconnecting"), "frame: {text}");
    assert!(text.contains("Build · pi"), "frame: {text}");
    assert!(
        text.contains("LIVE"),
        "frozen surface should remain: {text}"
    );
    assert!(state.hits.panes.is_empty());
    assert!(frame.cursor.is_none());
    let buffer = frame.to_ratatui_buffer().expect("frame should reconstruct");
    let stale_icon = buffer
        .content()
        .iter()
        .find(|cell| cell.symbol() == "×")
        .expect("stale blocked icon");
    assert_eq!(stale_icon.fg, state.config.palette.overlay0);
}

#[test]
fn navigator_uses_machine_parents_only_for_federated_clients() {
    let (mut state, _) = state_with_remote();
    state.open_navigator_overlay();
    let ClientShellOverlay::Navigator(navigator) = state.overlay.as_ref().expect("navigator")
    else {
        panic!("expected navigator");
    };
    let rows =
        render::client_navigator_rows(&state.endpoints, &state.active_endpoint_id, navigator);
    let machines = rows
        .iter()
        .filter(|row| matches!(row.target, ClientNavigatorTarget::Machine { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        machines
            .iter()
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>(),
        vec!["Local", "Build"]
    );
    assert!(rows.iter().all(|row| {
        matches!(row.target, ClientNavigatorTarget::Machine { .. })
            || (!row.label.contains("Local ·") && !row.label.contains("Build ·"))
    }));
    assert!(rows.iter().all(|row| match row.target {
        ClientNavigatorTarget::Machine { .. } => row.depth == 0 && row.status.is_none(),
        ClientNavigatorTarget::Workspace { .. } => row.depth == 1 && row.status.is_none(),
        ClientNavigatorTarget::Pane { .. } => row.depth == 2 && row.status.is_some(),
    }));
    assert_eq!(rows.iter().filter(|row| row.current).count(), 1);

    let frame = state.compose(106, 30).expect("federated navigator");
    for (rect, target) in &state.hits.navigator_rows {
        let expected = match target {
            ClientNavigatorTarget::Machine { .. } => " ",
            ClientNavigatorTarget::Workspace { .. } => "   ",
            ClientNavigatorTarget::Pane { .. } => "   └─ ",
        };
        let prefix = frame.cells[rect.y as usize * frame.width as usize + rect.x as usize..]
            .iter()
            .take(expected.chars().count())
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        assert_eq!(prefix, expected, "{target:?}");
    }

    let mut local = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    local.set_snapshot(Box::new(snapshot()));
    local.set_pane_surface(surface());
    let frame = local.compose(100, 28).expect("local-only sidebar");
    assert!(local.hits.machines.is_empty());
    assert!(
        !frame
            .cells
            .chunks(frame.width as usize)
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
            .contains(" machines")
    );
    local.open_navigator_overlay();
    let ClientShellOverlay::Navigator(navigator) = local.overlay.as_ref().expect("navigator")
    else {
        panic!("expected navigator");
    };
    let rows =
        render::client_navigator_rows(&local.endpoints, &local.active_endpoint_id, navigator);
    assert!(
        rows.iter()
            .all(|row| !matches!(row.target, ClientNavigatorTarget::Machine { .. }))
    );
    assert!(rows.iter().all(|row| match row.target {
        ClientNavigatorTarget::Workspace { .. } => row.depth == 0,
        ClientNavigatorTarget::Pane { .. } => row.depth == 1,
        ClientNavigatorTarget::Machine { .. } => false,
    }));
}

#[test]
fn navigator_keeps_saved_machine_visible_before_metadata_arrives() {
    let (mut state, endpoint_id) = state_with_remote();
    let endpoint = state
        .endpoints
        .iter_mut()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        .expect("saved remote endpoint");
    endpoint.snapshot = None;
    endpoint.status = ClientEndpointStatus::Connecting;
    state.open_navigator_overlay();
    let ClientShellOverlay::Navigator(navigator) = state.overlay.as_ref().expect("navigator")
    else {
        panic!("expected navigator");
    };

    let rows =
        render::client_navigator_rows(&state.endpoints, &state.active_endpoint_id, navigator);

    assert!(rows.iter().any(|row| {
        matches!(
            &row.target,
            ClientNavigatorTarget::Machine { endpoint_id: target } if target == &endpoint_id
        ) && row.label == "Build"
            && row.stale
    }));
    assert!(!rows.iter().any(|row| match &row.target {
        ClientNavigatorTarget::Machine { .. } => false,
        ClientNavigatorTarget::Workspace {
            endpoint_id: target,
            ..
        }
        | ClientNavigatorTarget::Pane {
            endpoint_id: target,
            ..
        } => target == &endpoint_id,
    }));
}

#[test]
fn navigator_machine_selection_opens_its_remembered_view() {
    let (mut state, endpoint_id) = state_with_remote();
    state.open_navigator_overlay();
    let selected = {
        let ClientShellOverlay::Navigator(navigator) = state.overlay.as_ref().expect("navigator")
        else {
            panic!("expected navigator");
        };
        render::client_navigator_rows(&state.endpoints, &state.active_endpoint_id, navigator)
            .into_iter()
            .find(|row| {
                matches!(
                    &row.target,
                    ClientNavigatorTarget::Machine { endpoint_id: target } if target == &endpoint_id
                )
            })
            .map(|row| row.target)
            .expect("remote machine row")
    };
    if let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() {
        navigator.selected = Some(selected);
    }

    let mut outcome = ClientShellInput::default();
    state.accept_navigator_selection(&mut outcome);

    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint {
            endpoint_id: activated,
            target: None,
        }] if activated == &endpoint_id
    ));
    assert!(state.overlay.is_none());
}

#[test]
fn navigator_foreign_pane_selection_activates_its_endpoint() {
    let (mut state, endpoint_id) = state_with_remote();
    state.open_navigator_overlay();
    let selected = {
        let ClientShellOverlay::Navigator(navigator) = state.overlay.as_ref().expect("navigator")
        else {
            panic!("expected navigator");
        };
        render::client_navigator_rows(&state.endpoints, &state.active_endpoint_id, navigator)
            .iter()
            .find(|row| {
                matches!(
                    &row.target,
                    ClientNavigatorTarget::Pane {
                        endpoint_id: target_endpoint,
                        pane_id,
                    } if target_endpoint == &endpoint_id && pane_id == "w1:p1"
                )
            })
            .map(|row| row.target.clone())
            .expect("remote pane row")
    };
    if let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() {
        navigator.selected = Some(selected);
    }
    let mut local = snapshot();
    let mut inserted = local.workspaces[0].clone();
    inserted.workspace_id = test_workspace_id("w2");
    inserted.focused = false;
    local.workspaces.push(inserted);
    state.set_snapshot(Box::new(local));

    let mut outcome = ClientShellInput::default();
    state.accept_navigator_selection(&mut outcome);

    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint {
            endpoint_id: activated,
            target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
        }] if activated == &endpoint_id && pane_id == "w1:p1"
    ));
    assert!(state.overlay.is_none());
}

#[test]
fn focus_agent_index_uses_online_aggregate_rows() {
    use shepr_api::schema::AgentStatus;

    let (mut state, endpoint_id) = state_with_remote();
    state
        .endpoints
        .iter_mut()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        .expect("remote endpoint")
        .snapshot
        .as_mut()
        .expect("remote snapshot")
        .agents = vec![agent("remote agent", AgentStatus::Working, 2)];
    let focus_agent = |index| {
        shepr_termio::input::KeybindMatch::Action(shepr_termio::input::KeybindAction::FocusAgent(
            index,
        ))
    };

    assert!(state.indexed_navigation_target_exists(&focus_agent(0)));
    assert!(!state.indexed_navigation_target_exists(&focus_agent(1)));

    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Reconnecting);
    assert!(!state.indexed_navigation_target_exists(&focus_agent(0)));
}

#[test]
fn workspace_drag_rejects_foreign_endpoint_slots() {
    let (mut state, endpoint_id) = state_with_remote();
    state.compose(100, 28).expect("aggregate sidebar");
    let local = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.endpoint_id.is_local())
        .expect("local workspace")
        .rect;
    let remote = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.endpoint_id == endpoint_id)
        .expect("remote workspace")
        .rect;
    let mouse = |kind, rect: Rect| {
        RawInputEvent::Mouse(MouseEvent {
            kind,
            column: rect.x.saturating_add(1),
            row: rect.y,
            modifiers: KeyModifiers::empty(),
        })
    };

    state.handle_raw_events(vec![mouse(MouseEventKind::Down(MouseButton::Left), local)]);
    state.handle_raw_events(vec![mouse(MouseEventKind::Drag(MouseButton::Left), remote)]);

    assert!(state.chrome_drag.is_none());
}

#[test]
fn collapsed_aggregate_workspace_status_uses_its_status_color() {
    use shepr_api::schema::AgentStatus;

    let (mut state, endpoint_id) = state_with_remote();
    state
        .endpoints
        .iter_mut()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        .expect("remote endpoint")
        .snapshot
        .as_mut()
        .expect("remote snapshot")
        .workspaces[0]
        .agent_status = AgentStatus::Blocked;
    state.sidebar_collapsed = true;

    let frame = state.compose(100, 28).expect("collapsed aggregate sidebar");
    let workspace = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.endpoint_id == endpoint_id)
        .expect("remote workspace")
        .rect;
    let buffer = frame.to_ratatui_buffer().expect("frame buffer");
    assert_eq!(
        buffer[(workspace.x.saturating_add(2), workspace.y)].fg,
        state.config.palette.red
    );
}

#[test]
fn navigator_workspace_arrows_cross_machine_headings_without_activating_them() {
    let (mut state, endpoint_id) = state_with_remote();
    state.open_navigator_overlay();
    for (key, expected_endpoint) in [
        (KeyCode::Right, endpoint_id),
        (KeyCode::Left, ClientEndpointId::Local),
    ] {
        let outcome = state.handle_raw_events(vec![RawInputEvent::Key(
            shepr_termio::input::TerminalKey::new(key, KeyModifiers::empty()),
        )]);
        assert!(outcome.actions.is_empty());
        let Some(ClientShellOverlay::Navigator(navigator)) = &state.overlay else {
            panic!("navigator");
        };
        assert_eq!(
            navigator.selected,
            Some(ClientNavigatorTarget::Pane {
                endpoint_id: expected_endpoint,
                pane_id: test_pane_id("w1:p1"),
            })
        );
    }
}

#[test]
fn navigator_foreign_workspace_heading_keeps_the_workspace_target() {
    let (mut state, endpoint_id) = state_with_remote();
    state.open_navigator_overlay();
    let selected = {
        let ClientShellOverlay::Navigator(navigator) = state.overlay.as_ref().expect("navigator")
        else {
            panic!("expected navigator");
        };
        render::client_navigator_rows(&state.endpoints, &state.active_endpoint_id, navigator)
            .iter()
            .find(|row| {
                matches!(
                    &row.target,
                    ClientNavigatorTarget::Workspace {
                        endpoint_id: target_endpoint,
                        workspace_id,
                    } if target_endpoint == &endpoint_id && workspace_id == "w1"
                )
            })
            .map(|row| row.target.clone())
            .expect("remote workspace heading")
    };
    if let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() {
        navigator.selected = Some(selected);
    }

    let mut outcome = ClientShellInput::default();
    state.accept_navigator_selection(&mut outcome);

    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint {
            endpoint_id: activated,
            target: Some(ClientEndpointFocusTarget::Workspace(workspace_id)),
        }] if activated == &endpoint_id && workspace_id == "w1"
    ));
}
