use crossterm::event::KeyCode;
use shepr_config::AgentSidebarToken;
use shepr_config::StatusIndicatorStyle;
use shepr_config::{ClientConfig, SidebarCollapsedModeConfig};
use shepr_protocol::AgentStatus;
use shepr_protocol::ClientMessage;
use shepr_protocol::command::EndpointCommand;
use shepr_termio::input::raw_input::RawInputEvent;

use crate::endpoint::{ClientEndpointStatus, EndpointFailureStatus};
use crate::shell::endpoints::ClientEndpointFocusTarget;
use crate::shell::presentation::render;
use crate::shell::state::{
    ClientChromeDrag, ClientNavigatorTarget, ClientShellAction, ClientShellConfig,
    ClientShellInput, ClientShellMode, ClientShellOverlay, ClientShellRequest,
};
use crossterm::event::MouseButton;
use crossterm::event::MouseEventKind;
use shepr_protocol::{
    ClientShellAgent, ClientShellPane, ClientShellSnapshot, ClientShellWorkspace,
};

use crate::shell::state::ClientShellState;

use crossterm::event::MouseEvent;

use crate::shell::tests::{cell_bg, cell_fg, frame_cell, frame_rows, snapshot, surface};
use ratatui::layout::Rect;

use crate::tests::{test_pane_id, test_workspace_id};

use crate::endpoint::ClientEndpointId;
use crossterm::event::KeyModifiers;

pub(in crate::shell) fn remote_machine() -> shepr_config::MachineConfig {
    machine_named("Build", "dev@build.example")
}

fn machine_named(label: &str, ssh: &str) -> shepr_config::MachineConfig {
    shepr_config::MachineConfig {
        label: shepr_config::MachineLabel::parse(label).expect("test precondition"),
        ssh: shepr_config::SshTarget::parse(ssh).expect("test precondition"),
    }
}

/// The first argument only documents which agent a test means; agents carry
/// no name of their own.
pub(in crate::shell) fn agent(
    status: shepr_protocol::AgentStatus,
    state_change_seq: u64,
) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: "w1:p1".parse().expect("test precondition"),
        agent: Some(shepr_config::ConfigAgent::Pi),
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: status,
        state_change_seq,
    }
}

fn snapshot_with_agent(
    boot_id: &str,
    pane_id: &str,
    status: shepr_protocol::AgentStatus,
    state_change_seq: u64,
) -> ClientShellSnapshot {
    let mut value = snapshot();
    let pane_id = test_pane_id(pane_id);
    value.boot_id = crate::tests::test_boot_id(boot_id);
    value.focused_pane_id = Some(pane_id);
    value.panes[0].pane_id = pane_id;
    value.agents = vec![ClientShellAgent {
        pane_id,
        ..agent(status, state_change_seq)
    }];
    value
}

pub(in crate::shell) fn state_with_remote() -> (ClientShellState, ClientEndpointId) {
    state_with_machines(&[remote_machine()])
}

/// Online state with a remote snapshot for the first machine; any others stay Connecting.
fn state_with_machines(
    machines: &[shepr_config::MachineConfig],
) -> (ClientShellState, ClientEndpointId) {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    let endpoint_id = ClientEndpointId::Ssh(machines[0].label.clone());
    state.set_machines(machines);
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    let mut remote = snapshot();
    remote.boot_id = crate::tests::test_boot_id("remote-boot");
    remote.workspaces[0].label = "remote-workspace".into();
    state.connect_endpoint_with_snapshot(&endpoint_id, 1, Box::new(remote));
    (state, endpoint_id)
}

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
    state.config.keybinds.prefix
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
        state.mode = mode;
        let mut next = state.snapshot.as_deref().expect("snapshot").clone();
        next.revision = next.revision.checked_next().expect("revision");
        state.set_endpoint_snapshot_for_generation(&remote, 1, Box::new(next.clone()));
        assert_eq!(state.mode, mode);
        next.revision = next.revision.checked_next().expect("revision");
        state.set_endpoint_snapshot_for_generation(&remote, 2, Box::new(next));
        assert_eq!(state.mode, mode);
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
        &shepr_remote::SshFailureDiagnostic::from_message(
            "handshake failed: protocol error: codec error".to_owned(),
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
fn collapsed_sidebar_workspace_rows_accept_drag_targets() {
    let (mut state, _) = state_with_remote();
    let mut local = state.snapshot.as_deref().expect("local snapshot").clone();
    let template = local.workspaces[0].clone();
    local.workspaces = (1..=3)
        .map(|number| ClientShellWorkspace {
            workspace_id: test_workspace_id(&format!("w{number}")),
            label: format!("space-{number}"),
            ..template.clone()
        })
        .collect();
    local.focused_workspace_id = Some(test_workspace_id("w1"));
    state.set_snapshot(Box::new(local));
    state.config.sidebar_collapsed_mode = SidebarCollapsedModeConfig::Compact;
    state.chrome.set_collapsed(true);
    state.compose(100, 28).expect("collapsed sidebar");

    let first = state
        .hits
        .workspaces
        .iter()
        .find(|hit| {
            hit.endpoint_id.is_local() && hit.workspace_id == crate::tests::test_workspace_id("w1")
        })
        .expect("first local workspace")
        .rect;
    let second = state
        .hits
        .workspaces
        .iter()
        .find(|hit| {
            hit.endpoint_id.is_local() && hit.workspace_id == crate::tests::test_workspace_id("w2")
        })
        .expect("second local workspace")
        .rect;
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: first.x,
        row: first.y,
        modifiers: KeyModifiers::empty(),
    })]);
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: second.x,
        row: second.y,
        modifiers: KeyModifiers::empty(),
    })]);

    assert!(matches!(
        &state.chrome_drag,
        Some(ClientChromeDrag::Workspace {
            target: Some(_),
            ..
        })
    ));
}

#[test]
fn revealing_an_active_workspace_ignores_a_same_id_on_another_endpoint() {
    let (mut state, remote_id) = state_with_remote();
    let mut local = state.snapshot.as_deref().expect("local snapshot").clone();
    let template = local.workspaces[0].clone();
    local.workspaces = (1..=30)
        .map(|number| ClientShellWorkspace {
            workspace_id: test_workspace_id(&format!("w{number}")),
            label: format!("space-{number}"),
            ..template.clone()
        })
        .collect();
    local.focused_workspace_id = Some(test_workspace_id("w5"));
    state.set_snapshot(Box::new(local));

    let mut remote = state
        .endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == remote_id)
        .and_then(|endpoint| endpoint.snapshot())
        .expect("remote snapshot")
        .clone();
    remote.workspaces[0].workspace_id = test_workspace_id("w5");
    remote.workspaces[0].label = "remote-w5".into();
    remote.focused_workspace_id = Some(test_workspace_id("w5"));
    state.set_endpoint_snapshot(&remote_id, Box::new(remote));

    // Start at the bottom, where the remote's same-id workspace is shown and
    // the local one is not, with no reveal of the new snapshot pending.
    state.reveal_focused_workspace = false;
    state.workspace_scroll = usize::MAX;
    state.compose(100, 22).expect("bottom of combined sidebar");
    assert!(
        state
            .hits
            .workspaces
            .iter()
            .any(|hit| hit.endpoint_id == remote_id
                && hit.workspace_id == crate::tests::test_workspace_id("w5"))
    );
    assert!(
        !state
            .hits
            .workspaces
            .iter()
            .any(|hit| hit.endpoint_id.is_local()
                && hit.workspace_id == crate::tests::test_workspace_id("w5"))
    );
    let bottom = state.workspace_scroll;

    state.reveal_workspace(&test_workspace_id("w5"));
    assert_eq!(state.workspace_scroll, bottom);
    state.compose(100, 22).expect("active workspace revealed");

    assert!(
        state
            .hits
            .workspaces
            .iter()
            .any(|hit| hit.endpoint_id.is_local()
                && hit.workspace_id == crate::tests::test_workspace_id("w5"))
    );
    assert!(state.workspace_scroll < bottom);
}

#[test]
fn machine_diagnostic_badge_reopens_notice_without_collapsing_machine() {
    let (mut state, id) = state_with_remote();
    state.set_endpoint_status(&id, EndpointFailureStatus::Attention);
    // ssh exits 255 for its own failures; this is how an auth prompt failure arrives.
    state.set_machine_diagnostic(
        &id,
        &shepr_remote::SshFailureDiagnostic::from_ssh_output(
            Some(255),
            "Permission denied (keyboard-interactive)",
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
        let notice = state.notices.visible().expect("test precondition");
        assert!(notice.body.contains("Permission denied"));
        assert!(
            notice
                .title
                .contains("Build: restart shepr to authenticate")
        );
    }
    // A successful handshake clears the diagnostic.
    state.endpoint_connected(&id, 2);
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

#[test]
fn machine_diagnostic_card_replaces_tabs_and_preserves_lines() {
    let (mut state, id) = state_with_remote();
    state.set_endpoint_status(&id, EndpointFailureStatus::Attention);
    state.set_machine_diagnostic(
        &id,
        &shepr_remote::SshFailureDiagnostic::from_message(
            "failure\twith fields\nretry\twith a key".to_owned(),
        ),
    );

    state.compose(120, 40).expect("diagnostic badge");
    let badge = state
        .hits
        .machines
        .iter()
        .find(|hit| hit.endpoint_id == id)
        .expect("diagnostic badge hit");
    let mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: badge.status_badge.x,
        row: badge.status_badge.y,
        modifiers: KeyModifiers::NONE,
    };
    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert!(outcome.repaint);
    assert_eq!(
        state.notices.visible().expect("diagnostic card").body,
        "failure with fields\nretry with a key"
    );

    let frame = state.compose(120, 40).expect("rendered diagnostic card");
    let rows = frame_rows(&frame);
    assert!(rows.iter().any(|row| row.contains("failure with fields")));
    assert!(rows.iter().any(|row| row.contains("retry with a key")));
    assert!(rows.iter().all(|row| !row.contains('\t')));
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
                    endpoint == &endpoint_id && pane.to_string() == pane_id
                })
        );

        let mut outcome = ClientShellInput::default();
        assert!(state.handle_endpoint_navigation(action, &mut outcome));
        assert!(outcome.repaint, "agent navigation must request a frame");
        if endpoint_id != *state.active_endpoint_id() {
            assert!(state.activate_endpoint_projection(&endpoint_id));
        }
        state.compose(100, 28).expect("test precondition");
        assert!(
            state
                .hits
                .endpoint_agents
                .iter()
                .any(|(_, endpoint, pane)| {
                    endpoint == &endpoint_id && pane.to_string() == pane_id
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
    let targets = state.agent_panel_model.targets();
    let index = targets
        .iter()
        .position(|target| target.endpoint_id == endpoint_id && target.pane_id == pane_id)
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
fn single_endpoint_agent_indices_follow_the_rendered_client_recency_order() {
    use shepr_termio::input::KeybindAction;

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.config.agent_panel_sort = shepr_config::AgentPanelSortConfig::Priority;
    state.chrome.set_collapsed(true);

    let mut first = snapshot_with_agent("old-boot", "w1:p1", AgentStatus::Idle, 10);
    first.agents.push(ClientShellAgent {
        pane_id: test_pane_id("w1:p2"),
        state_change_seq: 5,
        ..first.agents[0].clone()
    });
    first.panes.push(ClientShellPane {
        pane_id: test_pane_id("w1:p2"),
        ..first.panes[0].clone()
    });
    state.set_endpoint_snapshot_for_generation(&ClientEndpointId::Local, 1, Box::new(first));

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
    second.agents[1].state_change_seq = 9;
    state.set_endpoint_snapshot_for_generation(&ClientEndpointId::Local, 2, Box::new(second));

    state
        .compose(100, 28)
        .expect("collapsed single endpoint frame");
    let first_rendered = state.hits.agents.first().expect("first visible agent").1;
    assert_eq!(first_rendered.to_string(), "w1:p2");

    let command = state
        .endpoint_command_for_action(KeybindAction::FocusAgent(0))
        .expect("first agent focus command");
    assert!(matches!(
        command,
        EndpointCommand::PaneFocus(params) if params.pane_id == first_rendered
    ));
}

#[test]
fn agent_indices_keep_stale_rows_and_skip_agents_the_sidebar_cannot_render() {
    use shepr_termio::input::KeybindAction;

    let other_machine = machine_named("Other", "dev@other.example");
    let other_id = ClientEndpointId::Ssh(other_machine.label.clone());
    let (mut state, stale_id) = state_with_machines(&[remote_machine(), other_machine]);
    state.config.agent_panel_sort = shepr_config::AgentPanelSortConfig::Spaces;
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
        .hits
        .endpoint_agents
        .iter()
        .map(|(_, endpoint_id, pane_id)| (endpoint_id.clone(), *pane_id))
        .collect::<Vec<_>>();
    let targets = state.agent_panel_model.targets();
    let indexed = targets
        .iter()
        .map(|target| (target.endpoint_id.clone(), target.pane_id))
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
        [ClientShellAction::ActivateEndpoint {
            endpoint_id,
            target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
        }] if endpoint_id == &other_id && pane_id == &crate::tests::test_pane_id("w1:p3")
    ));
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
            }] if target == &endpoint_id && target_pane == pane_id
        ));

        state.workspace_scroll = 3;
        assert!(state.activate_endpoint_projection(&endpoint_id));
        assert_eq!(state.agent_scroll, 6);
        assert_eq!(state.workspace_scroll, 0);
        assert!(state.pane_surface().is_none());

        let mut next_surface = surface();
        next_surface.boot_id = state
            .endpoint_boot_id(&endpoint_id)
            .expect("test precondition")
            .clone();
        state.receive_pane_surface(next_surface);
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
        }] if target == &pane_id)
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
    assert_eq!(state.agent_scroll, 0);
    assert_eq!(state.hits.agent_max_scroll, 0);
    assert_eq!(state.hits.endpoint_agents.len(), 2);
}

#[test]
fn same_machine_reboot_still_resets_agent_scroll() {
    let (mut state, _) = state_with_scrollable_agents();
    let mut projection = state
        .snapshot
        .as_deref()
        .expect("test precondition")
        .clone();
    projection.boot_id = crate::tests::test_boot_id("restarted-local");
    state.cache_endpoint_snapshot(&ClientEndpointId::Local, Box::new(projection));
    assert!(state.activate_endpoint_projection(&ClientEndpointId::Local));
    assert_eq!(state.agent_scroll, 0);
}

#[test]
fn switching_machines_from_copy_mode_restores_terminal_input() {
    let (mut state, remote) = state_with_remote();
    let mut local_surface = surface();
    local_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        20,
        2,
        shepr_vt::AbsRow(0),
    ));
    state.receive_pane_surface(local_surface);
    state.compose(100, 28).expect("test precondition");
    assert!(state.enter_copy_mode(&mut ClientShellInput::default()));
    assert_eq!(state.mode, ClientShellMode::Copy);

    assert!(state.activate_endpoint_projection(&remote));
    let mut remote_surface = surface();
    remote_surface.boot_id = crate::tests::test_boot_id("remote-boot");
    state.receive_pane_surface(remote_surface);
    state.compose(100, 28).expect("test precondition");

    assert!(state.copy_mode.is_none());
    assert_eq!(state.mode, ClientShellMode::Terminal);
    let input = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::NONE),
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
        assert!(state.snapshot.is_none());
        assert!(state.pane_surface().is_none());
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
        let local_row = (local.x..local.right())
            .map(|x| frame_cell(&frame, (x, local.y)).symbol.as_str())
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
    assert_ne!(frame_cell(&frame, (local.right() - 1, local.y)).symbol, "●");
    assert_eq!(
        frame_cell(&frame, (remote.right() - 1, remote.y)).symbol,
        "●"
    );
    assert_eq!(
        cell_fg(&frame, (remote.right() - 1, remote.y)),
        state.config.palette.green
    );

    state.chrome.set_collapsed(true);
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
    assert_ne!(frame_cell(&frame, (local.right() - 1, local.y)).symbol, "●");
    assert_eq!(
        frame_cell(&frame, (remote.right() - 1, remote.y)).symbol,
        "●"
    );
}

#[test]
fn local_and_ssh_sidebars_derive_workspace_positions_from_list_order() {
    let (mut state, endpoint_id) = state_with_remote();
    let mut local = state.snapshot.as_deref().expect("local snapshot").clone();
    local.workspaces[0].workspace_id = test_workspace_id("wH");
    state.set_snapshot(Box::new(local));

    let mut remote = state
        .endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        .and_then(|endpoint| endpoint.snapshot())
        .expect("remote snapshot")
        .clone();
    remote.workspaces[0].workspace_id = test_workspace_id("w1A");
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote));

    for collapsed in [false, true] {
        state.chrome.set_collapsed(collapsed);
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
        for endpoint in [ClientEndpointId::Local, endpoint_id.clone()] {
            let row = state
                .hits
                .workspaces
                .iter()
                .find(|hit| hit.endpoint_id == endpoint)
                .expect("workspace row")
                .rect;
            let rendered = (row.x..row.right())
                .map(|x| frame_cell(&frame, (x, row.y)).symbol.as_str())
                .collect::<String>();
            assert!(
                rendered.trim_start().starts_with('1'),
                "list position: {rendered}; {text}"
            );
        }
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
            label: format!("space-{number}"),
            ..template.clone()
        })
        .collect();
    // Reuse workspace IDs across machines so revealing must be endpoint-scoped.
    let mut remote = initial.clone();
    remote.boot_id = crate::tests::test_boot_id("remote-boot");
    remote.workspaces.push(ClientShellWorkspace {
        workspace_id: test_workspace_id("w13"),
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
        label: "new-space".into(),
        ..template
    });
    update.focused_workspace_id = Some(test_workspace_id("w13"));
    state.set_snapshot(Box::new(update));
    let mut updated_surface = surface();
    updated_surface.projection_revision = shepr_protocol::ProjectionRevision::new(2);
    state.receive_pane_surface(updated_surface);
    state.compose(106, 2).expect("zero-height workspace body");
    assert!(state.reveal_focused_workspace);
    state.compose(106, 20).expect("new workspace revealed");
    assert!(state.hits.workspaces.iter().any(|hit| {
        hit.endpoint_id == ClientEndpointId::Local
            && hit.workspace_id == crate::tests::test_workspace_id("w13")
    }));

    state.workspace_scroll = 0;
    state.compose(106, 20).expect("manual scroll");
    assert_eq!(state.workspace_scroll, 0);
    assert!(!state.hits.workspaces.iter().any(|hit| {
        hit.endpoint_id == ClientEndpointId::Local
            && hit.workspace_id == crate::tests::test_workspace_id("w13")
    }));
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
        workspace.label = "second-workspace".into();
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
    assert!(metrics.max_start() > 0);
    assert_eq!(metrics.start(), metrics.max_start());
    assert_eq!(state.workspace_scroll, metrics.max_start());
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
    state.receive_pane_surface(remote_surface);

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
    assert_ne!(
        cell_bg(&frame, (machine.x, machine.y)),
        state.config.palette.active_row_bg
    );
    assert_eq!(
        cell_bg(&frame, (workspace.x + 2, workspace.y)),
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
    assert_eq!(
        cell_bg(&frame, (machine.x, machine.y)),
        state.config.palette.active_row_bg
    );
}

#[test]
fn aggregate_agents_use_configured_rows_machine_token_and_status_colors() {
    use shepr_protocol::AgentStatus;

    let mut config = ClientConfig::default();
    config.ui.status_indicators = StatusIndicatorStyle::Symbols;
    config.ui.sidebar.agents.rows = vec![vec![
        AgentSidebarToken::StateIcon,
        AgentSidebarToken::Machine,
        AgentSidebarToken::Agent,
    ]];
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    let machine = remote_machine();
    let endpoint_id = ClientEndpointId::Ssh(machine.label.clone());
    state.set_machines(&[machine]);

    let mut local = snapshot();
    local.agents = vec![agent(AgentStatus::Idle, 1)];
    state.set_snapshot(Box::new(local));
    state.receive_pane_surface(surface());
    let mut remote = snapshot();
    remote.boot_id = crate::tests::test_boot_id("remote-boot");
    remote.agents = vec![agent(AgentStatus::Blocked, 1)];
    state.connect_endpoint_with_snapshot(&endpoint_id, 1, Box::new(remote));

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

    assert!(
        frame
            .cells
            .iter()
            .any(|cell| cell.symbol == "×" && cell.fg.to_ratatui() == state.config.palette.red)
    );
}

#[test]
fn aggregate_priority_uses_client_observed_recency_across_machines() {
    use shepr_config::AgentSidebarToken;
    use shepr_protocol::AgentStatus;

    let mut config = ClientConfig::default();
    config.ui.agent_panel_sort = Some(shepr_config::AgentPanelSortConfig::Priority);
    config.ui.sidebar.agents.rows =
        vec![vec![AgentSidebarToken::Machine, AgentSidebarToken::Agent]];
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    let machine = remote_machine();
    let endpoint_id = ClientEndpointId::Ssh(machine.label.clone());
    state.set_machines(&[machine]);

    let mut local = snapshot();
    local.agents = vec![agent(AgentStatus::Idle, 1)];
    state.set_snapshot(Box::new(local));
    state.receive_pane_surface(surface());
    let mut remote = snapshot();
    remote.boot_id = crate::tests::test_boot_id("remote-boot");
    remote.agents = vec![agent(AgentStatus::Idle, 1)];
    state.connect_endpoint_with_snapshot(&endpoint_id, 1, Box::new(remote.clone()));

    let mut local = snapshot();
    local.agents = vec![agent(AgentStatus::Idle, 2)];
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

    remote.agents = vec![agent(AgentStatus::Working, 2)];
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote.clone()));
    let text = frame_text(&mut state);
    assert!(
        text.find("Build · pi").expect("remote agent")
            < text.find("Local · pi").expect("local agent")
    );

    remote.agents = vec![agent(AgentStatus::Idle, 3)];
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
        }] if activated == &endpoint_id && pane_id == &crate::tests::test_pane_id("w1:p1")
    ));
}

#[test]
fn unselected_endpoint_snapshot_keeps_server_idle_status() {
    use shepr_protocol::AgentStatus;

    let (mut state, endpoint_id) = state_with_remote();
    let mut remote = snapshot();
    remote.boot_id = crate::tests::test_boot_id("remote-boot");
    remote.agents = vec![agent(AgentStatus::Working, 2)];
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote.clone()));
    remote.revision = shepr_protocol::ProjectionRevision::new(2);
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
    assert_eq!(*state.active_endpoint_id(), ClientEndpointId::Local);
    assert_eq!(
        state.snapshot.as_deref().map(|snapshot| &snapshot.boot_id),
        Some(&crate::tests::test_boot_id("boot-1"))
    );
}

#[test]
fn clicking_an_offline_active_machine_row_only_toggles_its_collapse_state() {
    let (mut state, endpoint_id) = state_with_remote();
    assert!(state.activate_endpoint_projection(&endpoint_id));
    state.set_endpoint_status(&endpoint_id, EndpointFailureStatus::Reconnecting);
    state.compose(100, 28).expect("active remote frame");
    let hit = state
        .hits
        .machines
        .iter()
        .find(|hit| hit.endpoint_id == endpoint_id)
        .expect("active remote endpoint hit")
        .rect;

    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.x + 3,
        row: hit.y,
        modifiers: KeyModifiers::empty(),
    })]);

    assert!(outcome.actions.is_empty());
    assert!(outcome.repaint);
    assert!(state.collapsed_endpoints.contains(&endpoint_id));
    assert!(state.notices.visible().is_none());
}

#[test]
fn selecting_an_offline_active_machine_in_the_navigator_is_silent() {
    let (mut state, endpoint_id) = state_with_remote();
    assert!(state.activate_endpoint_projection(&endpoint_id));
    state.set_endpoint_status(&endpoint_id, EndpointFailureStatus::Reconnecting);
    state.open_navigator_overlay();
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("expected navigator");
    };
    navigator.selected = Some(ClientNavigatorTarget::Machine {
        endpoint_id: endpoint_id.clone(),
    });

    let mut outcome = ClientShellInput::default();
    state.accept_navigator_selection(&mut outcome);

    assert!(outcome.actions.is_empty());
    assert!(state.notices.visible().is_none());
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Navigator(_))
    ));
}

#[test]
fn clicking_an_online_active_machine_row_toggles_its_collapse_state_and_reselects_it() {
    let (mut state, endpoint_id) = state_with_remote();
    assert!(state.activate_endpoint_projection(&endpoint_id));
    state.compose(100, 28).expect("active remote frame");
    let hit = state
        .hits
        .machines
        .iter()
        .find(|hit| hit.endpoint_id == endpoint_id)
        .expect("active remote endpoint hit")
        .rect;

    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.x + 3,
        row: hit.y,
        modifiers: KeyModifiers::empty(),
    })]);

    // The targetless selection reaches the runtime, where selecting the shown
    // endpoint changes nothing.
    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint {
            endpoint_id: selected,
            target: None,
        }] if *selected == endpoint_id
    ));
    assert!(state.collapsed_endpoints.contains(&endpoint_id));
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
        if workspace {
            assert!(matches!(
                outcome.actions.as_slice(),
                [ClientShellAction::ActivateEndpoint {
                    endpoint_id: ClientEndpointId::Local,
                    target: Some(_),
                }]
            ));
        } else {
            // The machine row of the displayed endpoint toggles its collapse
            // state and submits a targetless selection, which cancels the
            // switch away while Local is still displayed.
            assert!(matches!(
                outcome.actions.as_slice(),
                [ClientShellAction::ActivateEndpoint {
                    endpoint_id: ClientEndpointId::Local,
                    target: None,
                }]
            ));
            assert!(state.collapsed_endpoints.contains(&ClientEndpointId::Local));
        }
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
    }] if id == &test_workspace_id("w1"))
    );
}

#[test]
fn machine_arrow_toggles_inactive_machine_without_switching() {
    for sidebar_collapsed in [false, true] {
        // None leaves the remote Online.
        for failure in [None, Some(EndpointFailureStatus::Reconnecting)] {
            let other_machine = machine_named("Other", "dev@other.example");
            let other_id = ClientEndpointId::Ssh(other_machine.label.clone());
            let (mut state, remote_id) = state_with_machines(&[remote_machine(), other_machine]);
            state.connect_endpoint_with_snapshot(&other_id, 1, Box::new(snapshot()));
            if let Some(failure) = failure {
                state.set_endpoint_status(&remote_id, failure);
            }
            state.chrome.set_collapsed(sidebar_collapsed);

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
                assert_eq!(
                    frame_cell(&frame, (column, machine.y)).symbol,
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
                assert_eq!(*state.active_endpoint_id(), ClientEndpointId::Local);
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
                        .as_ref(),
                    Some(&crate::tests::test_workspace_id("w1"))
                );
                assert_eq!(state.collapsed_endpoints.contains(&remote_id), collapsed);
                assert!(!state.collapsed_endpoints.contains(&ClientEndpointId::Local));
                assert!(!state.collapsed_endpoints.contains(&other_id));
                assert!(state.endpoint_error.message().is_none());

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
    state.receive_pane_surface(future);
    assert_eq!(
        state
            .pane_surface()
            .map(|surface| surface.projection_revision),
        Some(shepr_protocol::ProjectionRevision::new(1))
    );
    assert_eq!(
        state
            .surfaces
            .waiting_baseline()
            .map(|surface| surface.projection_revision),
        Some(shepr_protocol::ProjectionRevision::new(2))
    );

    let mut next = snapshot();
    next.revision = shepr_protocol::ProjectionRevision::new(2);
    state.set_snapshot(Box::new(next));
    assert_eq!(
        state
            .pane_surface()
            .map(|surface| surface.projection_revision),
        Some(shepr_protocol::ProjectionRevision::new(2))
    );
    assert!(state.surfaces.waiting_baseline().is_none());
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
    assert_eq!(endpoint.snapshot_generation(), Some(5));
    assert_eq!(
        endpoint
            .snapshot()
            .as_ref()
            .expect("test precondition")
            .revision,
        1
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
        previous.revision = shepr_protocol::ProjectionRevision::new(previous_revision);
        state.cache_endpoint_snapshot_for_generation(&endpoint_id, 4, Box::new(previous));
        assert!(state.activate_endpoint_projection(&endpoint_id));
        let mut previous_surface = surface();
        previous_surface.boot_id = crate::tests::test_boot_id("shared-server-boot");
        previous_surface.projection_revision =
            shepr_protocol::ProjectionRevision::new(previous_revision);
        previous_surface.surface_revision = shepr_protocol::SurfaceRevision::new(9);
        state.receive_pane_surface(previous_surface.clone());
        previous_surface.projection_revision = previous_surface
            .projection_revision
            .checked_next()
            .expect("test precondition");
        state.receive_pane_surface(previous_surface);
        assert!(state.surfaces.waiting_baseline().is_some());
        state.agent_scroll = 7;

        state.mark_endpoint_disconnected(&endpoint_id);
        state.endpoint_connected(&endpoint_id, 5);
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
                .pane_surface()
                .expect("test precondition")
                .surface_revision,
            9
        );

        assert!(state.activate_endpoint_projection(&endpoint_id));
        assert!(state.compose(106, 20).is_none());
        let mut reconnected_surface = surface();
        reconnected_surface.boot_id = crate::tests::test_boot_id("shared-server-boot");
        reconnected_surface.projection_revision = shepr_protocol::ProjectionRevision::new(1);
        reconnected_surface.surface_revision = shepr_protocol::SurfaceRevision::new(1);
        state.receive_pane_surface(reconnected_surface);

        assert_eq!(
            state.snapshot.as_ref().expect("test precondition").revision,
            1
        );
        assert_eq!(
            state
                .pane_surface()
                .expect("test precondition")
                .projection_revision,
            1
        );
        assert_eq!(
            state
                .pane_surface()
                .expect("test precondition")
                .surface_revision,
            1
        );
        assert!(state.surfaces.waiting_baseline().is_none());
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
    state.endpoint_connected(&endpoint_id, 2);
    let mut replacement = snapshot();
    replacement.boot_id = crate::tests::test_boot_id("replacement-boot");
    state.cache_endpoint_snapshot_for_generation(&endpoint_id, 2, Box::new(replacement));
    assert_eq!(
        state
            .snapshot
            .as_deref()
            .expect("test precondition")
            .boot_id,
        crate::tests::test_boot_id("remote-boot")
    );

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
    state.receive_pane_surface(remote_surface);

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
    let stale_icon = frame
        .cells
        .iter()
        .find(|cell| cell.symbol == "×")
        .expect("stale blocked icon");
    assert_eq!(stale_icon.fg.to_ratatui(), state.config.palette.overlay0);
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
        render::client_navigator_rows(&state.endpoints, state.active_endpoint_id(), navigator);
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

    let mut local = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    local.set_snapshot(Box::new(snapshot()));
    local.receive_pane_surface(surface());
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
        render::client_navigator_rows(&local.endpoints, local.active_endpoint_id(), navigator);
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
    endpoint.state = crate::shell::endpoints::EndpointState::Connecting {
        last: None,
        connected: false,
        generation: None,
    };
    state.open_navigator_overlay();
    let ClientShellOverlay::Navigator(navigator) = state.overlay.as_ref().expect("navigator")
    else {
        panic!("expected navigator");
    };

    let rows =
        render::client_navigator_rows(&state.endpoints, state.active_endpoint_id(), navigator);

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
        render::client_navigator_rows(&state.endpoints, state.active_endpoint_id(), navigator)
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
        render::client_navigator_rows(&state.endpoints, state.active_endpoint_id(), navigator)
            .iter()
            .find(|row| {
                matches!(
                    &row.target,
                    ClientNavigatorTarget::Pane {
                        endpoint_id: target_endpoint,
                        pane_id,
                    } if target_endpoint == &endpoint_id && pane_id == &crate::tests::test_pane_id("w1:p1")
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
    local.workspaces.push(inserted);
    state.set_snapshot(Box::new(local));

    let mut outcome = ClientShellInput::default();
    state.accept_navigator_selection(&mut outcome);

    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint {
            endpoint_id: activated,
            target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
        }] if activated == &endpoint_id && pane_id == &crate::tests::test_pane_id("w1:p1")
    ));
    assert!(state.overlay.is_none());
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
    use shepr_protocol::AgentStatus;

    let (mut state, endpoint_id) = state_with_remote();
    let endpoint = state
        .endpoints
        .iter_mut()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        .expect("remote endpoint");
    std::sync::Arc::make_mut(endpoint.snapshot_mut().expect("remote snapshot")).workspaces[0]
        .agent_status = AgentStatus::Blocked;
    state.chrome.set_collapsed(true);

    let frame = state.compose(100, 28).expect("collapsed aggregate sidebar");
    let workspace = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.endpoint_id == endpoint_id)
        .expect("remote workspace")
        .rect;
    assert_eq!(
        cell_fg(&frame, (workspace.x.saturating_add(2), workspace.y)),
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
        render::client_navigator_rows(&state.endpoints, state.active_endpoint_id(), navigator)
            .iter()
            .find(|row| {
                matches!(
                    &row.target,
                    ClientNavigatorTarget::Workspace {
                        endpoint_id: target_endpoint,
                        workspace_id,
                    } if target_endpoint == &endpoint_id && workspace_id == &crate::tests::test_workspace_id("w1")
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
        }] if activated == &endpoint_id && workspace_id == &crate::tests::test_workspace_id("w1")
    ));
}

mod surface_baseline {
    use crate::shell::presentation::surface_patch::ClientPaneSurfacePatchOutcome;
    use crate::shell::presentation::surface_patch::PatchPresentation;
    use crate::shell::presentation::surfaces::PaneSurfaces;
    use crate::shell::presentation::surfaces::PatchRejection;
    use crate::shell::state::ClientShellConfig;
    use crate::shell::state::ClientShellInput;
    use crate::shell::state::ClientShellMode;
    use shepr_config::ClientConfig;

    use crate::endpoint::ClientEndpointId;

    use crate::shell::state::ClientShellState;
    use shepr_protocol::PaneSurfaceFrame;

    use crate::shell::tests::{snapshot, surface};
    use shepr_protocol::PaneSurfacePatch;
    fn state() -> ClientShellState {
        let mut s = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        s.set_snapshot(Box::new(snapshot()));
        s.receive_pane_surface(surface());
        s
    }
    fn patch(s: &PaneSurfaceFrame) -> PaneSurfacePatch {
        PaneSurfacePatch {
            boot_id: s.boot_id.clone(),
            projection_revision: s.projection_revision,
            base_surface_revision: s.surface_revision,
            surface_revision: s.surface_revision.checked_next().expect("next"),
            rows: vec![],
            panes: vec![],
            cursor: None,
        }
    }
    fn changed_patch(surface: &PaneSurfaceFrame, marker: &str) -> PaneSurfacePatch {
        let mut p = patch(surface);
        p.panes.push(surface.panes[0].clone());
        let mut cell = shepr_protocol::CellData::blank();
        cell.symbol = marker.into();
        p.rows.push(shepr_protocol::PaneSurfacePatchRow {
            x: 0,
            y: 0,
            cells: vec![cell],
        });
        p
    }
    #[test]
    fn a_patch_on_a_surface_ahead_of_the_snapshot_advances_that_baseline() {
        let mut s = state();
        let mut next = surface();
        next.projection_revision = 2.into();
        s.receive_pane_surface(next);
        let p = changed_patch(s.surfaces.baseline().expect("baseline"), "X");
        assert!(matches!(
            s.apply_pane_surface_patch(&p),
            ClientPaneSurfacePatchOutcome::Applied(PatchPresentation::Held)
        ));
        assert_eq!(s.pane_surface().expect("held").projection_revision, 1);
        assert_ne!(s.pane_surface().expect("held").frame.cells[0].symbol, "X");
        let mut next = snapshot();
        next.revision = 2.into();
        s.set_snapshot(Box::new(next));
        assert_eq!(s.pane_surface().expect("paired").frame.cells[0].symbol, "X");
        assert_eq!(
            s.pane_surface().expect("paired").surface_revision,
            p.surface_revision
        );
    }
    #[test]
    fn a_patch_after_the_snapshot_passed_the_surface_advances_the_baseline() {
        let mut s = state();
        let mut next = snapshot();
        next.revision = 2.into();
        s.set_snapshot(Box::new(next));
        let p = patch(s.surfaces.baseline().expect("baseline"));
        assert!(matches!(
            s.apply_pane_surface_patch(&p),
            ClientPaneSurfacePatchOutcome::Applied(PatchPresentation::Held)
        ));
        assert_eq!(
            s.surfaces.baseline().expect("baseline").surface_revision,
            p.surface_revision
        );
        assert_ne!(
            s.pane_surface().expect("held").surface_revision,
            p.surface_revision
        );
        let second = changed_patch(s.surfaces.baseline().expect("baseline"), "X");
        assert!(matches!(
            s.apply_pane_surface_patch(&second),
            ClientPaneSurfacePatchOutcome::Applied(PatchPresentation::Held)
        ));
        assert_eq!(
            s.surfaces.baseline().expect("baseline").frame.cells[0].symbol,
            "X"
        );
        assert_ne!(s.pane_surface().expect("held").frame.cells[0].symbol, "X");
        let mut next = surface();
        next.projection_revision = 2.into();
        s.receive_pane_surface(next);
        assert!(s.surfaces.is_paired());
    }
    #[test]
    fn a_new_generation_loses_the_baseline_but_keeps_the_held_pair() {
        let mut s = state();
        s.set_endpoint_snapshot_for_generation(&ClientEndpointId::Local, 1, Box::new(snapshot()));
        s.receive_pane_surface(surface());
        s.set_endpoint_snapshot_for_generation(&ClientEndpointId::Local, 2, Box::new(snapshot()));
        assert!(s.surfaces.baseline().is_none());
        assert!(s.pane_surface().is_some());
    }
    #[test]
    fn a_surface_before_the_first_snapshot_stays_the_baseline() {
        let mut s = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        s.receive_pane_surface_from(surface(), 1);
        s.set_endpoint_snapshot_for_generation(&ClientEndpointId::Local, 1, Box::new(snapshot()));
        assert!(s.surfaces.is_paired());
        let p = patch(s.surfaces.baseline().expect("baseline"));
        assert!(matches!(
            s.apply_pane_surface_patch_from(&p, 1),
            ClientPaneSurfacePatchOutcome::Applied(_)
        ));
    }
    #[test]
    fn a_reconnected_connections_surface_before_its_snapshot_is_kept_and_patched() {
        let mut s = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        s.set_endpoint_snapshot_for_generation(&ClientEndpointId::Local, 1, Box::new(snapshot()));
        s.receive_pane_surface_from(surface(), 1);
        assert!(s.surfaces.is_paired());
        // Same boot and projection revision as the old connection's snapshot: it
        // must wait for its own snapshot rather than pair with the old one.
        s.receive_pane_surface_from(surface(), 2);
        assert!(!s.surfaces.is_paired());
        assert!(s.pane_surface().is_some(), "the old pair stays held");
        s.set_endpoint_snapshot_for_generation(&ClientEndpointId::Local, 2, Box::new(snapshot()));
        assert!(s.surfaces.is_paired());
        let p = patch(s.surfaces.baseline().expect("baseline"));
        assert!(matches!(
            s.apply_pane_surface_patch_from(&p, 2),
            ClientPaneSurfacePatchOutcome::Applied(_)
        ));
        // A late full surface from the old connection cannot replace it.
        s.receive_pane_surface_from(surface(), 1);
        assert!(s.surfaces.is_paired());
    }
    #[test]
    fn a_rebooted_servers_surface_before_its_snapshot_survives_the_reset() {
        let mut s = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        s.set_endpoint_snapshot_for_generation(&ClientEndpointId::Local, 1, Box::new(snapshot()));
        s.receive_pane_surface_from(surface(), 1);
        let rebooted = crate::tests::test_boot_id("restarted-local");
        let mut next_surface = surface();
        next_surface.boot_id = rebooted.clone();
        s.receive_pane_surface_from(next_surface, 2);
        let mut next_snapshot = snapshot();
        next_snapshot.boot_id = rebooted;
        s.set_endpoint_snapshot_for_generation(
            &ClientEndpointId::Local,
            2,
            Box::new(next_snapshot),
        );
        assert!(s.surfaces.is_paired());
        let p = patch(s.surfaces.baseline().expect("baseline"));
        assert!(matches!(
            s.apply_pane_surface_patch_from(&p, 2),
            ClientPaneSurfacePatchOutcome::Applied(_)
        ));
    }
    #[test]
    fn a_slow_path_patch_applies_in_place_and_composes() {
        let mut s = state();
        s.mode = ClientShellMode::Navigate;
        let ptr = s
            .surfaces
            .baseline()
            .expect("baseline")
            .frame
            .cells
            .as_ptr();
        let p = changed_patch(s.surfaces.baseline().expect("baseline"), "X");
        assert!(matches!(
            s.apply_pane_surface_patch(&p),
            ClientPaneSurfacePatchOutcome::Applied(PatchPresentation::Compose)
        ));
        assert_eq!(
            s.surfaces
                .baseline()
                .expect("baseline")
                .frame
                .cells
                .as_ptr(),
            ptr
        );
        assert_eq!(s.pane_surface().expect("paired").frame.cells[0].symbol, "X");
    }
    #[test]
    fn a_rejected_patch_reports_its_reason() {
        let mut s = state();
        let mut p = patch(s.surfaces.baseline().expect("baseline"));
        p.base_surface_revision = p.surface_revision;
        assert!(matches!(
            s.apply_pane_surface_patch(&p),
            ClientPaneSurfacePatchOutcome::Rejected(PatchRejection::DoesNotFollow)
        ));
        p = patch(s.surfaces.baseline().expect("baseline"));
        let mut pane = s.surfaces.baseline().expect("baseline").panes[0].clone();
        pane.inner_rect.width += 1;
        p.panes.push(pane);
        assert!(matches!(
            s.apply_pane_surface_patch(&p),
            ClientPaneSurfacePatchOutcome::Rejected(PatchRejection::PaneGeometry)
        ));
        p.panes.clear();
        p.rows.push(shepr_protocol::PaneSurfacePatchRow {
            x: 0,
            y: 0,
            cells: vec![],
        });
        assert!(matches!(
            s.apply_pane_surface_patch(&p),
            ClientPaneSurfacePatchOutcome::Rejected(PatchRejection::RowOutsideFrame)
        ));
        s.surfaces = PaneSurfaces::Empty;
        assert!(matches!(
            s.apply_pane_surface_patch(&p),
            ClientPaneSurfacePatchOutcome::Rejected(PatchRejection::NoBaseline)
        ));
    }
    #[test]
    fn a_surface_ahead_of_the_snapshot_keeps_the_pane_hits_live() {
        let mut s = state();
        s.compose(106, 20).expect("compose");
        let count = s.hits.panes.len();
        assert!(count > 0);
        let mut future = surface();
        future.projection_revision = 3.into();
        s.receive_pane_surface(future);
        assert_eq!(s.hits.panes.len(), count);
        assert_eq!(s.pane_surface().expect("held").projection_revision, 1);
    }
    #[test]
    fn compose_holds_the_last_frame_while_unpaired_and_draws_the_placeholder_when_nothing_was_presented()
     {
        let mut s = state();
        s.compose(106, 20).expect("compose");
        let mut future = surface();
        future.projection_revision = 3.into();
        s.receive_pane_surface(future.clone());
        assert!(s.compose(106, 20).is_none());
        s.invalidate_pane_surface();
        s.receive_pane_surface(future);
        assert!(s.compose(106, 20).is_some());
        assert!(s.pane_surface().is_none());
    }
    #[test]
    fn pairing_a_waiting_baseline_runs_the_selection_and_copy_mode_effects() {
        let mut s = state();
        s.compose(106, 20).expect("compose");
        let mut input = ClientShellInput::default();
        s.enter_copy_mode(&mut input);
        let hit = s.hits.panes[0].clone();
        let metrics = hit.scroll.expect("scroll");
        s.request_word_selection(&hit, metrics, 0, 0, &mut input);
        s.copy_mode.as_mut().expect("copy").cursor.row = shepr_vt::AbsRow(0);
        let mut future = surface();
        future.projection_revision = 2.into();
        future.panes[0].inner_rect.width = 10;
        {
            let metrics = future.panes[0].scroll.as_mut().expect("scroll");
            *metrics = shepr_vt::ScrollMetrics::new(
                metrics.offset_from_bottom,
                metrics.max_offset_from_bottom,
                metrics.viewport_rows,
                shepr_vt::AbsRow(100),
            );
        }
        s.receive_pane_surface(future);
        assert!(s.mouse_selection.word_gesture.is_some());
        let mut next = snapshot();
        next.revision = 2.into();
        s.set_snapshot(Box::new(next));
        assert!(s.mouse_selection.word_gesture.is_none());
        assert_ne!(
            s.copy_mode.as_ref().expect("copy").cursor.row,
            shepr_vt::AbsRow(0)
        );
    }
}
