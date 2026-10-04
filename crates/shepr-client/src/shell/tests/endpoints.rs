//! Several endpoints in one shell: projections, connection generations and reconnects,
//! switching machines, and agent navigation across them.

use crate::endpoint::{ClientEndpointId, ClientEndpointStatus, EndpointFailureStatus};
use crate::shell::config::ClientShellConfig;
use crate::shell::navigation::location::{Location, LocationTarget};
use crate::shell::state::{
    ClientShellAction, ClientShellInput, ClientShellMode, ClientShellRequest, ClientShellState,
};
use crate::shell::tests::{
    agent, frame_cell, machine_named, remote_machine, snapshot, snapshot_with_agent,
    state_with_machines, state_with_remote, surface,
};
use crate::tests::{test_pane_id, test_workspace_id};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use shepr_config::{AgentSidebarToken, ClientConfig, StatusIndicatorStyle};
use shepr_protocol::command::EndpointCommand;
use shepr_protocol::{AgentStatus, ClientMessage, ClientShellAgent, ClientShellPane};
use shepr_surface::ratatui_conversion::WireColorExt as _;
use shepr_termio::input::raw_input::RawInputEvent;

#[test]
fn every_endpoint_gets_the_same_pane_surface() {
    let (mut state, remote_id) = state_with_remote();

    // The pane surface is the whole main area, whatever the projection shows.
    let layout = state.layout(100, 30);
    assert_eq!(layout.pane_surface.y, 0);
    assert_eq!(layout.pane_surface.height, 30);
    assert_eq!(layout.pane_surface.x, layout.sidebar.width);
    assert_eq!(layout.pane_surface.width, 100 - layout.sidebar.width);
    let local = state.surface_size(100, 30);
    assert_eq!(local.rows, layout.pane_surface.height);
    assert_eq!(local.cols, layout.pane_surface.width);

    assert!(state.activate_endpoint_projection(&remote_id));
    assert_eq!(
        state.surface_size(100, 30),
        local,
        "activating another endpoint's projection leaves the layout alone"
    );
}

fn prefix_key(state: &ClientShellState) -> (crossterm::event::KeyCode, KeyModifiers) {
    let prefix = state.config.keybinds.prefix;
    (prefix.code, prefix.modifiers)
}

#[test]
fn client_keymap_and_modes_survive_snapshots_and_endpoint_switches() {
    for mode in [
        ClientShellMode::Prefix,
        ClientShellMode::Navigate,
        ClientShellMode::Resize,
    ] {
        let (mut state, remote) = state_with_remote();
        let prefix = prefix_key(&state);
        let bindings = format!("{:?}", state.config.keybinds);
        assert!(state.activate_endpoint_projection(&remote));
        assert_eq!(prefix_key(&state), prefix);
        assert_eq!(format!("{:?}", state.config.keybinds), bindings);
        if mode == ClientShellMode::Navigate {
            state.mode.enter_navigate(None);
        } else {
            state.mode.set(mode);
        }
        let mut next = state.endpoints.active.snapshot().expect("snapshot").clone();
        next.revision = next.revision.checked_next().expect("revision");
        state.set_endpoint_snapshot_for_generation(
            &remote,
            crate::tests::test_generation(1),
            Box::new(next.clone()),
        );
        assert_eq!(state.mode.kind(), mode);
        next.revision = next.revision.checked_next().expect("revision");
        state.set_endpoint_snapshot_for_generation(
            &remote,
            crate::tests::test_generation(2),
            Box::new(next),
        );
        assert_eq!(state.mode.kind(), mode);
        assert_eq!(prefix_key(&state), prefix);
        assert!(state.activate_endpoint_projection(&ClientEndpointId::Local));
        assert_eq!(prefix_key(&state), prefix);
        assert_eq!(format!("{:?}", state.config.keybinds), bindings);
    }
}

#[test]
fn a_failed_handshake_marks_only_its_endpoint() {
    let (mut state, failed) = state_with_machines(&[
        remote_machine(),
        machine_named("Other", "dev@other.example"),
    ]);
    let other = ClientEndpointId::Ssh(
        shepr_config::MachineLabel::parse("Other").expect("test precondition"),
    );
    let mut other_snapshot = snapshot();
    other_snapshot.boot_id = crate::tests::test_boot_id("remote-boot");
    other_snapshot.workspaces[0].label = "other-workspace".into();
    let client_prefix = prefix_key(&state);
    state.connect_endpoint_with_snapshot(&other, 1, Box::new(other_snapshot));

    // A malformed welcome fails that endpoint's handshake:
    // the loop reports it as an Attention diagnostic, like any handshake failure.
    state.set_endpoint_status(&failed, EndpointFailureStatus::Attention);
    state.set_machine_diagnostic(
        &failed,
        &shepr_launch::EndpointFailure::unclassified(
            "handshake failed: protocol error: codec error",
        ),
    );

    assert_eq!(
        state.endpoint_status(&failed),
        Some(ClientEndpointStatus::Attention)
    );
    assert!(!state.activate_endpoint_projection(&failed));
    assert_eq!(*state.active_endpoint_id(), ClientEndpointId::Local);

    assert_eq!(
        state.endpoint_status(&other),
        Some(ClientEndpointStatus::Online)
    );
    assert!(state.activate_endpoint_projection(&other));
    assert_eq!(*state.active_endpoint_id(), other);
    assert_eq!(prefix_key(&state), client_prefix);
}

fn state_with_scrollable_agents() -> (ClientShellState, ClientEndpointId) {
    let (mut state, remote) = state_with_remote();
    for endpoint_id in [ClientEndpointId::Local, remote.clone()] {
        let mut projection = state
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == endpoint_id)
            .expect("test precondition")
            .snapshot()
            .cloned()
            .expect("test precondition");
        projection.agents = (0..8)
            .map(|index| ClientShellAgent {
                pane_id: shepr_protocol::PublicPaneId::new(
                    &crate::tests::test_workspace_id("w1"),
                    shepr_protocol::PanePublicNumber::new(index + 1).expect("nonzero test number"),
                ),
                ..agent(AgentStatus::Idle, 1)
            })
            .collect();
        projection.panes = projection
            .agents
            .iter()
            .map(|agent| ClientShellPane {
                pane_id: agent.pane_id,
                ..projection.panes[0].clone()
            })
            .collect();
        state.set_endpoint_snapshot(&endpoint_id, Box::new(projection));
    }
    state.compose(100, 28).expect("test precondition");
    state.sidebar_scroll.scroll_agents_to(6);
    state.compose(100, 28).expect("test precondition");
    assert_eq!(state.sidebar_scroll.agent_start(), 6);
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
        let start = if action == KeybindAction::PreviousAgent {
            0
        } else {
            state.drawn().agent_max_scroll()
        };
        state.sidebar_scroll.scroll_agents_to(start);
        state.compose(100, 28).expect("test precondition");
        assert!(!state.drawn().agents().any(|hit| {
            hit.location.endpoint == endpoint_id
                && hit
                    .location
                    .pane_id()
                    .is_some_and(|pane| pane.to_string() == pane_id)
        }));

        let mut outcome = ClientShellInput::default();
        assert!(state.handle_endpoint_navigation(action, &mut outcome));
        assert!(outcome.repaint, "agent navigation must request a frame");
        if endpoint_id != *state.active_endpoint_id() {
            assert!(state.activate_endpoint_projection(&endpoint_id));
        }
        state.compose(100, 28).expect("test precondition");
        assert!(
            state.drawn().agents().any(|hit| {
                hit.location.endpoint == endpoint_id
                    && hit
                        .location
                        .pane_id()
                        .is_some_and(|pane| pane.to_string() == pane_id)
            }),
            "{action:?} must reveal the selected agent"
        );
    }
}

#[test]
fn agent_navigation_reveal_is_cancelled_by_another_selection() {
    for select_pane in [false, true] {
        let (mut state, remote) = state_with_scrollable_agents();
        let scroll = state.sidebar_scroll.agent_start();
        let mut outcome = ClientShellInput::default();
        assert!(state.handle_endpoint_navigation(
            shepr_termio::input::KeybindAction::PreviousAgent,
            &mut outcome,
        ));
        assert_eq!(state.sidebar_scroll.agent_start(), scroll);
        if select_pane {
            assert!(state.focus_or_activate(
                crate::shell::navigation::location::Location::pane(
                    remote.clone(),
                    test_pane_id("w1:p1"),
                ),
                &mut outcome,
            ));
        } else {
            assert!(state.activate_endpoint(remote.clone(), &mut outcome));
        }
        assert!(state.activate_endpoint_projection(&remote));
        state.compose(100, 28).expect("test precondition");
        assert_eq!(state.sidebar_scroll.agent_start(), scroll);
    }
}

#[test]
fn agent_navigation_keeps_scroll_when_target_is_visible() {
    let (mut state, _) = state_with_scrollable_agents();
    let location = &state
        .drawn()
        .agents()
        .nth(1)
        .expect("a second agent hit")
        .location;
    let endpoint_id = location.endpoint.clone();
    let pane_id = location.pane_id().expect("agent hit names a pane");
    let targets = state.endpoints.agent_panel_model.targets();
    let index = targets
        .iter()
        .position(|target| target.endpoint == endpoint_id && target.pane_id() == Some(pane_id))
        .expect("test precondition");
    let scroll = state.sidebar_scroll.agent_start();
    assert!(state.handle_endpoint_navigation(
        shepr_termio::input::KeybindAction::FocusAgent(index),
        &mut ClientShellInput::default(),
    ));
    state.compose(100, 28).expect("test precondition");
    assert_eq!(state.sidebar_scroll.agent_start(), scroll);
}

#[test]
fn agent_reveal_waits_for_a_visible_agent_body() {
    use shepr_termio::input::KeybindAction;

    let (mut state, remote) = state_with_scrollable_agents();
    state.sidebar_scroll.scroll_agents_to(0);
    state.compose(100, 28).expect("test precondition");

    // The previous agent from the first one is the last, on the remote machine, and
    // is offscreen at the top of the list.
    let mut outcome = ClientShellInput::default();
    assert!(state.handle_endpoint_navigation(KeybindAction::PreviousAgent, &mut outcome));
    assert!(state.activate_endpoint_projection(&remote));

    // The agent section keeps its header but has no room for rows: the reveal waits.
    state.compose(100, 6).expect("agent body empty");
    state.compose(100, 28).expect("full height");
    assert!(
        state.drawn().agents().any(|hit| {
            hit.location.endpoint == remote
                && hit
                    .location
                    .pane_id()
                    .is_some_and(|pane| pane.to_string() == "w1:p8")
        }),
        "the agent is revealed once the agent body is visible"
    );
}

#[test]
fn single_endpoint_agent_indices_follow_the_rendered_client_recency_order() {
    use shepr_termio::input::KeybindAction;

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_agent_panel_sort(shepr_config::AgentPanelSortConfig::Priority);
    state.chrome.set_collapsed(true);

    let mut first = snapshot_with_agent("old-boot", "w1:p1", AgentStatus::Idle, 10);
    first.agents.push(ClientShellAgent {
        pane_id: test_pane_id("w1:p2"),
        state_change_seq: shepr_test_fixtures::counter_at(5),
        ..first.agents[0].clone()
    });
    first.panes.push(ClientShellPane {
        pane_id: test_pane_id("w1:p2"),
        ..first.panes[0].clone()
    });
    state.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        crate::tests::test_generation(1),
        Box::new(first),
    );

    // The first agent keeps its old sequence number across the restart, while the
    // second changes to a lower sequence number and receives the newer client recency.
    let mut second = state
        .endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id.is_local())
        .and_then(|endpoint| endpoint.snapshot())
        .expect("first local snapshot")
        .clone();
    second.boot_id = crate::tests::test_boot_id("restarted-local");
    second.agents[1].state_change_seq = shepr_test_fixtures::counter_at(9);
    state.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        crate::tests::test_generation(2),
        Box::new(second),
    );

    state
        .compose(100, 28)
        .expect("collapsed single endpoint frame");
    let first_rendered = state
        .drawn()
        .agents()
        .next()
        .and_then(|hit| hit.location.pane_id())
        .expect("first visible agent names a pane");
    assert_eq!(first_rendered.to_string(), "w1:p2");

    let command = state
        .endpoint_command_for_action(KeybindAction::FocusAgent(0))
        .expect("first agent focus command");
    assert!(matches!(
        command.command,
        EndpointCommand::PaneFocus(params) if params.pane_id == first_rendered
    ));
}

#[test]
fn agent_indices_keep_stale_rows_and_skip_agents_the_sidebar_cannot_render() {
    use shepr_termio::input::KeybindAction;

    let other_machine = machine_named("Other", "dev@other.example");
    let other_id = ClientEndpointId::Ssh(other_machine.label.clone());
    let (mut state, stale_id) = state_with_machines(&[remote_machine(), other_machine]);
    state.set_agent_panel_sort(shepr_config::AgentPanelSortConfig::Spaces);
    state.set_endpoint_snapshot(
        &ClientEndpointId::Local,
        Box::new(snapshot_with_agent(
            "local-boot",
            "w1:p1",
            AgentStatus::Idle,
            1,
        )),
    );

    let mut stale = snapshot_with_agent("remote-boot", "w1:p2", AgentStatus::Working, 2);
    stale.agents.push(ClientShellAgent {
        pane_id: test_pane_id("w9:p8"),
        // A workspace this snapshot does not carry.
        ..stale.agents[0].clone()
    });
    stale.panes.push(ClientShellPane {
        pane_id: test_pane_id("w9:p8"),
        ..stale.panes[0].clone()
    });
    state.set_endpoint_snapshot(&stale_id, Box::new(stale));
    state.set_endpoint_status(&stale_id, EndpointFailureStatus::Reconnecting);
    state.connect_endpoint_with_snapshot(
        &other_id,
        1,
        Box::new(snapshot_with_agent(
            "shared-server-boot",
            "w1:p3",
            AgentStatus::Idle,
            3,
        )),
    );

    state.compose(100, 28).expect("aggregate endpoint frame");
    let rendered = state
        .drawn()
        .agents()
        .filter_map(|hit| {
            hit.location
                .pane_id()
                .map(|pane_id| (hit.location.endpoint.clone(), pane_id))
        })
        .collect::<Vec<_>>();
    let targets = state.endpoints.agent_panel_model.targets();
    let indexed = targets
        .iter()
        .filter_map(|target| {
            target
                .pane_id()
                .map(|pane_id| (target.endpoint.clone(), pane_id))
        })
        .collect::<Vec<_>>();
    assert_eq!(indexed, rendered);
    assert_eq!(
        indexed.len(),
        3,
        "the agent with a missing workspace is omitted"
    );
    assert_eq!(
        indexed[1].0, stale_id,
        "the stale visible row keeps its index"
    );

    let mut outcome = ClientShellInput::default();
    assert!(state.handle_endpoint_navigation(KeybindAction::FocusAgent(2), &mut outcome));
    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint(Location {
            endpoint: endpoint_id,
            target: LocationTarget::Pane(pane_id),
        })] if endpoint_id == &other_id && pane_id == &crate::tests::test_pane_id("w1:p3")
    ));
}

#[test]
fn switching_machines_preserves_aggregate_agent_scroll_and_visible_rows() {
    let (mut state, remote) = state_with_scrollable_agents();
    for endpoint_id in [remote.clone(), ClientEndpointId::Local, remote] {
        let visible = state.drawn().agents().cloned().collect::<Vec<_>>();
        let hit = visible
            .iter()
            .find(|hit| hit.location.endpoint == endpoint_id)
            .expect("destination agent remains visible");
        let click = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.rect.x + 2,
            row: hit.rect.y,
            modifiers: KeyModifiers::NONE,
        })]);
        assert!(matches!(
            click.actions.as_slice(),
            [ClientShellAction::ActivateEndpoint(Location {
                endpoint: target,
                target: LocationTarget::Pane(target_pane),
            })] if target == &endpoint_id && hit.location.pane_id() == Some(*target_pane)
        ));

        state.sidebar_scroll.scroll_workspaces_to(3);
        assert!(state.activate_endpoint_projection(&endpoint_id));
        assert_eq!(state.sidebar_scroll.agent_start(), 6);
        assert_eq!(state.sidebar_scroll.workspace_start(), 0);
        assert!(state.pane_surface().is_none());

        let mut next_surface = surface();
        next_surface.boot_id = state
            .endpoint_boot_id(&endpoint_id)
            .expect("test precondition")
            .clone();
        state.receive_pane_surface_from(
            next_surface,
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
        state.compose(100, 28).expect("test precondition");
        assert_eq!(state.sidebar_scroll.agent_start(), 6);
        assert_eq!(state.drawn().agents().cloned().collect::<Vec<_>>(), visible);
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
        let hit = state
            .drawn()
            .agents()
            .find(|hit| hit.location.endpoint.is_local())
            .expect("test precondition")
            .clone();
        let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.rect.x + 2,
            row: hit.rect.y,
            modifiers: KeyModifiers::NONE,
        })]);
        assert!(
            matches!(outcome.actions.as_slice(), [ClientShellAction::ActivateEndpoint(Location {
            endpoint: ClientEndpointId::Local,
            target: LocationTarget::Pane(target),
        })] if hit.location.pane_id() == Some(*target))
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
            .snapshot()
            .cloned()
            .expect("test precondition");
        projection.revision = projection
            .revision
            .checked_next()
            .expect("test precondition");
        projection.agents.truncate(1);
        state.set_endpoint_snapshot(&endpoint_id, Box::new(projection));
    }
    assert!(state.activate_endpoint_projection(&remote));
    state.compose(100, 28).expect("test precondition");
    assert_eq!(state.sidebar_scroll.agent_start(), 0);
    assert_eq!(state.drawn().agent_max_scroll(), 0);
    assert_eq!(state.drawn().agents().count(), 2);
}

#[test]
fn same_machine_reboot_still_resets_agent_scroll() {
    let (mut state, _) = state_with_scrollable_agents();
    let mut projection = state
        .endpoints
        .active
        .snapshot()
        .expect("test precondition")
        .clone();
    projection.boot_id = crate::tests::test_boot_id("restarted-local");
    state.cache_endpoint_snapshot(&ClientEndpointId::Local, Box::new(projection));
    assert!(state.activate_endpoint_projection(&ClientEndpointId::Local));
    assert_eq!(state.sidebar_scroll.agent_start(), 0);
}

#[test]
fn switching_machines_from_copy_mode_restores_terminal_input() {
    let (mut state, remote) = state_with_remote();
    let mut local_surface = surface();
    local_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        20,
        2,
        shepr_term::AbsRow(0),
    ));
    state.receive_pane_surface_from(
        local_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(100, 28).expect("test precondition");
    assert!(state.enter_copy_mode(&mut ClientShellInput::default()));
    assert_eq!(state.mode.kind(), ClientShellMode::Copy);

    assert!(state.activate_endpoint_projection(&remote));
    let mut remote_surface = surface();
    remote_surface.boot_id = crate::tests::test_boot_id("remote-boot");
    state.receive_pane_surface_from(
        remote_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(100, 28).expect("test precondition");

    assert!(state.copy.is_none());
    assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
    let input = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_term::key::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::NONE),
    )]);
    assert!(matches!(
        input.requests.as_slice(),
        [ClientShellRequest::Shown(ClientMessage::ClientShellPaneInput { pane_id, events })]
            if pane_id == &crate::tests::test_pane_id("w1:p1") && events.len() == 1
    ));
}

#[test]
fn configured_machines_start_connecting_without_a_snapshot() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    let machine = remote_machine();
    let remote = ClientEndpointId::Ssh(machine.label.clone());
    state.set_machines(&[machine]);
    assert_eq!(remote.display_label(), "Build");
    assert_eq!(
        state.endpoint_status(&remote),
        Some(ClientEndpointStatus::Connecting)
    );
    assert!(!state.endpoint_has_snapshot(&remote));
    assert!(state.endpoint_is_active(&ClientEndpointId::Local));
}

#[test]
fn machine_navigation_does_not_require_a_local_snapshot_or_surface() {
    for (cols, rows) in [(100, 28), (36, 18)] {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        let machine = remote_machine();
        let remote = ClientEndpointId::Ssh(machine.label.clone());
        state.set_machines(&[machine]);
        state.set_endpoint_status(
            &ClientEndpointId::Local,
            EndpointFailureStatus::Reconnecting,
        );
        state.connect_endpoint_with_snapshot(&remote, 1, Box::new(snapshot()));
        assert!(state.endpoints.active.snapshot().is_none());
        assert!(state.pane_surface().is_none());
        let frame = state
            .compose(cols, rows)
            .expect("connection chrome without Local");
        let local = state
            .drawn()
            .machines()
            .find(|hit| hit.location.endpoint.is_local())
            .expect("test precondition")
            .rect;
        let local_row = (local.x..local.right())
            .map(|x| frame_cell(&frame, (x, local.y)).symbol.as_str())
            .collect::<String>();
        assert!(!local_row.contains("reconnecting"));
        assert!(
            !local_row.contains('◐'),
            "Local never gets a connection badge"
        );
        let hit = state
            .drawn()
            .machines()
            .find(|hit| hit.location.endpoint == remote)
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
            matches!(outcome.actions.as_slice(), [ClientShellAction::ActivateEndpoint(Location { endpoint: endpoint_id, .. })] if endpoint_id == &remote)
        );
        assert!(
            state.endpoints.active.snapshot().is_none(),
            "selection is committed only by coherent activation"
        );
    }
}

#[test]
fn unselected_endpoint_snapshot_keeps_server_idle_status() {
    use shepr_protocol::AgentStatus;

    let (mut state, endpoint_id) = state_with_remote();
    let mut remote = snapshot();
    remote.boot_id = crate::tests::test_boot_id("remote-boot");
    remote.agents = vec![agent(AgentStatus::Working, 2)];
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote.clone()));
    remote.revision = shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(2);
    remote.agents = vec![agent(AgentStatus::Idle, 3)];

    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote));

    let status = state
        .endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        .and_then(|endpoint| endpoint.snapshot())
        .and_then(|snapshot| snapshot.agents.first())
        .map(|agent| agent.agent_status);
    assert_eq!(status, Some(AgentStatus::Idle));
    assert_eq!(*state.active_endpoint_id(), ClientEndpointId::Local);
}

#[test]
fn clicking_remote_machine_name_requests_activation_without_mutating_projection() {
    let (mut state, endpoint_id) = state_with_remote();
    state.compose(100, 28).expect("combined endpoint frame");
    let hit = state
        .drawn()
        .machines()
        .find(|hit| hit.location.endpoint == endpoint_id)
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
        [ClientShellAction::ActivateEndpoint(Location {
            endpoint: activated,
            target: LocationTarget::Machine,
        })] if activated == &endpoint_id
    ));
    assert_eq!(*state.active_endpoint_id(), ClientEndpointId::Local);
    assert_eq!(
        state
            .endpoints
            .active
            .snapshot()
            .map(|snapshot| &snapshot.boot_id),
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
        assert_eq!(*state.active_endpoint_id(), ClientEndpointId::Local);
        let rect = if workspace {
            state
                .drawn()
                .workspaces()
                .find(|hit| hit.location.endpoint.is_local())
                .expect("test precondition")
                .rect
        } else {
            state
                .drawn()
                .machines()
                .find(|hit| hit.location.endpoint.is_local())
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
        if workspace {
            assert!(matches!(
                outcome.actions.as_slice(),
                [ClientShellAction::ActivateEndpoint(Location {
                    endpoint: ClientEndpointId::Local,
                    target,
                })] if *target != LocationTarget::Machine
            ));
        } else {
            // The machine row of the displayed endpoint toggles its collapse
            // state and submits a targetless selection, which cancels the
            // switch away while Local is still displayed.
            assert!(matches!(
                outcome.actions.as_slice(),
                [ClientShellAction::ActivateEndpoint(Location {
                    endpoint: ClientEndpointId::Local,
                    target: LocationTarget::Machine,
                })]
            ));
            assert!(state.endpoints.collapsed.contains(&ClientEndpointId::Local));
        }
    }
}

#[test]
fn reconnecting_local_selection_still_reaches_the_runtime() {
    let (mut state, _) = state_with_remote();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    let mut outcome = ClientShellInput::default();
    state.focus_or_activate(
        crate::shell::navigation::location::Location::workspace(
            ClientEndpointId::Local,
            shepr_test_fixtures::id("w1"),
        ),
        &mut outcome,
    );
    assert!(
        matches!(outcome.actions.as_slice(), [ClientShellAction::ActivateEndpoint(Location {
        endpoint: ClientEndpointId::Local,
        target: LocationTarget::Workspace(id),
    })] if id == &test_workspace_id("w1"))
    );
}

#[test]
fn context_menu_lookup_ignores_inactive_endpoint_workspaces() {
    let (mut state, endpoint_id) = state_with_remote();
    state.compose(100, 28).expect("combined endpoint frame");
    let remote = state
        .drawn()
        .workspaces()
        .find(|hit| hit.location.endpoint == endpoint_id)
        .expect("remote workspace")
        .rect;
    let local = state
        .drawn()
        .workspaces()
        .find(|hit| hit.location.endpoint.is_local())
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
    future.projection_revision =
        shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(2);
    future.surface_revision = shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(2);
    state.receive_pane_surface_from(
        future,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    assert_eq!(
        state
            .pane_surface()
            .map(|surface| surface.projection_revision),
        Some(shepr_test_fixtures::counter_at::<
            shepr_protocol::ProjectionRevision,
        >(1))
    );
    assert_eq!(
        state
            .presentation
            .surfaces
            .waiting_baseline()
            .map(|surface| surface.projection_revision),
        Some(shepr_test_fixtures::counter_at::<
            shepr_protocol::ProjectionRevision,
        >(2))
    );

    let mut next = snapshot();
    next.revision = shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(2);
    state.set_snapshot(Box::new(next));
    assert_eq!(
        state
            .pane_surface()
            .map(|surface| surface.projection_revision),
        Some(shepr_test_fixtures::counter_at::<
            shepr_protocol::ProjectionRevision,
        >(2))
    );
    assert!(state.presentation.surfaces.waiting_baseline().is_none());
}

#[test]
fn inactive_endpoint_snapshot_cache_never_regresses_revision() {
    let (mut state, endpoint_id) = state_with_remote();
    let mut newest = snapshot();
    newest.boot_id = crate::tests::test_boot_id("remote-boot");
    newest.revision = shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(3);
    newest.workspaces[0].label = "newest".into();
    state.set_endpoint_snapshot(&endpoint_id, Box::new(newest));
    let mut delayed = snapshot();
    delayed.boot_id = crate::tests::test_boot_id("remote-boot");
    delayed.revision = shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(2);
    delayed.workspaces[0].label = "delayed".into();

    state.set_endpoint_snapshot(&endpoint_id, Box::new(delayed));

    let label = state
        .endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        .and_then(|endpoint| endpoint.snapshot())
        .and_then(|snapshot| snapshot.workspaces.first())
        .map(|workspace| workspace.label.as_str());
    assert_eq!(label, Some("newest"));
}

#[test]
fn new_connection_generation_accepts_a_lower_same_boot_projection_revision() {
    let (mut state, endpoint_id) = state_with_remote();
    let mut previous = snapshot();
    previous.boot_id = crate::tests::test_boot_id("shared-server-boot");
    previous.revision = shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(9);
    previous.workspaces[0].label = "old connection".into();
    state.cache_endpoint_snapshot_for_generation(
        &endpoint_id,
        crate::tests::test_generation(4),
        Box::new(previous),
    );
    let mut reconnected = snapshot();
    reconnected.boot_id = crate::tests::test_boot_id("shared-server-boot");
    reconnected.revision = shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(1);
    reconnected.workspaces[0].label = "new connection".into();

    state.cache_endpoint_snapshot_for_generation(
        &endpoint_id,
        crate::tests::test_generation(5),
        Box::new(reconnected),
    );

    let endpoint = state
        .endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        .expect("remote endpoint");
    assert_eq!(
        endpoint.snapshot_generation(),
        Some(crate::tests::test_generation(5))
    );
    assert_eq!(
        endpoint
            .snapshot()
            .as_ref()
            .expect("test precondition")
            .revision,
        shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(1)
    );
    assert_eq!(
        endpoint
            .snapshot()
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
        previous.revision = shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(
            previous_revision,
        );
        state.cache_endpoint_snapshot_for_generation(
            &endpoint_id,
            crate::tests::test_generation(4),
            Box::new(previous),
        );
        assert!(state.activate_endpoint_projection(&endpoint_id));
        let mut previous_surface = surface();
        previous_surface.boot_id = crate::tests::test_boot_id("shared-server-boot");
        previous_surface.projection_revision = shepr_test_fixtures::counter_at::<
            shepr_protocol::ProjectionRevision,
        >(previous_revision);
        previous_surface.surface_revision =
            shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(9);
        state.receive_pane_surface_from(
            previous_surface.clone(),
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
        previous_surface.projection_revision = previous_surface
            .projection_revision
            .checked_next()
            .expect("test precondition");
        state.receive_pane_surface_from(
            previous_surface,
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
        assert!(state.presentation.surfaces.waiting_baseline().is_some());
        state.sidebar_scroll.scroll_agents_to(7);

        state.mark_endpoint_disconnected(&endpoint_id);
        state.endpoint_connected(&endpoint_id, crate::tests::test_generation(5));
        let mut reconnected = snapshot();
        reconnected.boot_id = crate::tests::test_boot_id("shared-server-boot");
        reconnected.revision =
            shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(1);
        state.cache_endpoint_snapshot_for_generation(
            &endpoint_id,
            crate::tests::test_generation(5),
            Box::new(reconnected),
        );
        assert_eq!(
            state
                .endpoints
                .active
                .snapshot()
                .expect("test precondition")
                .revision,
            shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(
                previous_revision
            )
        );
        assert_eq!(
            state
                .pane_surface()
                .expect("test precondition")
                .surface_revision,
            shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(9)
        );

        assert!(state.activate_endpoint_projection(&endpoint_id));
        assert!(state.compose(106, 20).is_none());
        let mut reconnected_surface = surface();
        reconnected_surface.boot_id = crate::tests::test_boot_id("shared-server-boot");
        reconnected_surface.projection_revision =
            shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(1);
        reconnected_surface.surface_revision =
            shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(1);
        state.receive_pane_surface_from(
            reconnected_surface,
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );

        assert_eq!(
            state
                .endpoints
                .active
                .snapshot()
                .expect("test precondition")
                .revision,
            shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(1)
        );
        assert_eq!(
            state
                .pane_surface()
                .expect("test precondition")
                .projection_revision,
            shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(1)
        );
        assert_eq!(
            state
                .pane_surface()
                .expect("test precondition")
                .surface_revision,
            shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(1)
        );
        assert!(state.presentation.surfaces.waiting_baseline().is_none());
        assert_eq!(state.sidebar_scroll.agent_start(), 7);
        assert!(state.compose(106, 20).is_some());
    }
}

#[test]
fn reconnect_snapshot_waits_for_coherent_activation_before_replacing_projection() {
    let (mut state, endpoint_id) = state_with_remote();
    assert!(state.activate_endpoint_projection(&endpoint_id));
    assert_eq!(
        state
            .endpoints
            .active
            .snapshot()
            .expect("test precondition")
            .boot_id,
        crate::tests::test_boot_id("remote-boot")
    );

    state.mark_endpoint_disconnected(&endpoint_id);
    state.endpoint_connected(&endpoint_id, crate::tests::test_generation(2));
    let mut replacement = snapshot();
    replacement.boot_id = crate::tests::test_boot_id("replacement-boot");
    state.cache_endpoint_snapshot_for_generation(
        &endpoint_id,
        crate::tests::test_generation(2),
        Box::new(replacement),
    );
    assert_eq!(
        state
            .endpoints
            .active
            .snapshot()
            .expect("test precondition")
            .boot_id,
        crate::tests::test_boot_id("remote-boot")
    );

    assert!(state.activate_endpoint_projection(&endpoint_id));
    assert_eq!(
        state
            .endpoints
            .active
            .snapshot()
            .expect("test precondition")
            .boot_id,
        crate::tests::test_boot_id("replacement-boot")
    );
}

#[test]
fn disconnected_active_endpoint_freezes_surface_and_marks_cached_ui_stale() {
    use shepr_protocol::AgentStatus;

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
    std::sync::Arc::make_mut(endpoint.snapshot_mut().expect("remote snapshot")).agents =
        vec![agent(AgentStatus::Blocked, 1)];
    assert!(state.activate_endpoint_projection(&endpoint_id));
    let mut remote_surface = surface();
    remote_surface.boot_id = crate::tests::test_boot_id("remote-boot");
    state.receive_pane_surface_from(
        remote_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );

    state.mark_endpoint_disconnected(&endpoint_id);
    let frame = state.compose(100, 28).expect("frozen endpoint frame");
    let text = frame
        .cells()
        .chunks(frame.width() as usize)
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
    assert!(state.pane_hits().is_empty());
    assert!(frame.cursor().is_none());
    let stale_icon = frame
        .cells()
        .iter()
        .find(|cell| cell.symbol == "×")
        .expect("stale blocked icon");
    assert_eq!(stale_icon.fg.to_ratatui(), state.config.palette.overlay0);
}

#[test]
fn focus_agent_index_uses_the_rendered_aggregate_rows() {
    use shepr_protocol::AgentStatus;

    let (mut state, endpoint_id) = state_with_remote();
    let endpoint = state
        .endpoints
        .iter_mut()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        .expect("remote endpoint");
    std::sync::Arc::make_mut(endpoint.snapshot_mut().expect("remote snapshot")).agents =
        vec![agent(AgentStatus::Working, 2)];
    state.rebuild_agent_panel_model();
    let focus_agent = |index| shepr_termio::input::KeybindAction::FocusAgent(index);

    assert!(state.indexed_navigation_target_exists(&focus_agent(0)));
    assert!(!state.indexed_navigation_target_exists(&focus_agent(1)));

    // A stale machine's rows stay in the sidebar, so they keep their numbers;
    // picking one reports the machine as not ready instead of shifting the
    // numbers of every row after it.
    state.set_endpoint_status(&endpoint_id, EndpointFailureStatus::Reconnecting);
    assert!(state.indexed_navigation_target_exists(&focus_agent(0)));
    assert!(!state.indexed_navigation_target_exists(&focus_agent(1)));
}
