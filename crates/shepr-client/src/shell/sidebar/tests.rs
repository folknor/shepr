//! The sidebar through the whole shell: machine and workspace rows, machine state
//! entries, the agent panel, reveals, diagnostics badges and workspace drags, on one
//! machine and across several.

use crate::endpoint::{ClientEndpointId, EndpointFailureStatus};
use crate::shell::config::ClientShellConfig;
use crate::shell::input::pointer::ClientChromeDrag;
use crate::shell::navigation::location::{Location, LocationTarget};
use crate::shell::state::{ClientShellAction, ClientShellInput, ClientShellState};
use crate::shell::tests::{
    agent, cell_bg, cell_fg, frame_cell, frame_rows, machine_named, remote_machine, snapshot,
    state_with_machines, state_with_remote, state_with_remote_config, surface,
};
use crate::tests::test_workspace_id;
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use shepr_config::{AgentSidebarToken, ClientConfig, StatusIndicatorStyle};
use shepr_protocol::{ClientShellSnapshot, ClientShellWorkspace};
use shepr_surface::ratatui_conversion::WireColorExt as _;
use shepr_termio::input::raw_input::RawInputEvent;

#[test]
fn multi_machine_sidebar_draws_the_workspace_drop_marker() {
    let (mut state, remote_id) = state_with_remote();
    state.compose(120, 40).expect("test precondition");
    let local = state
        .drawn()
        .workspaces()
        .find(|hit| hit.location.endpoint == ClientEndpointId::Local)
        .expect("local workspace row")
        .rect;
    // A drop after the active machine's last workspace marks the row below it, which here is
    // the next machine's row.
    let row = local.bottom();
    let next_machine = state
        .drawn()
        .machines()
        .find(|hit| hit.location.endpoint == remote_id)
        .expect("remote machine row")
        .rect;
    assert_eq!(next_machine.y, row);
    state.pointer.chrome_drag = Some(ClientChromeDrag::Workspace {
        source_workspace_id: test_workspace_id("w1"),
        target: Some((None, row)),
    });
    let frame = state.compose(120, 40).expect("dragging frame");
    let cells = (local.x..local.right())
        .map(|x| frame_cell(&frame, (x, row)))
        .collect::<Vec<_>>();
    let marker = cells
        .iter()
        .find(|cell| cell.symbol == "─")
        .expect("the drop marker is drawn");
    assert_eq!(
        marker.fg,
        shepr_protocol::WireColor::from_ratatui(state.palette.accent)
    );
    let drawn = cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect::<String>();
    assert!(
        drawn.contains(remote_id.display_label(&state.config.local_label)),
        "the machine name stays readable: {drawn:?}"
    );
}

/// A drop before the first workspace marks the row above it, the last row of the
/// workspace section header.
#[test]
fn single_machine_sidebar_draws_the_drop_marker_above_the_first_workspace() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface_from(surface(), shepr_protocol::ConnectionGeneration::FIRST);
    state.compose(120, 40).expect("test precondition");
    let first = state
        .drawn()
        .workspaces()
        .next()
        .expect("first workspace row")
        .rect;
    let row = first.y - 1;
    state.pointer.chrome_drag = Some(ClientChromeDrag::Workspace {
        source_workspace_id: test_workspace_id("w1"),
        target: Some((Some(test_workspace_id("w1")), row)),
    });
    let frame = state.compose(120, 40).expect("dragging frame");
    let cell = &frame.cells()[usize::from(row) * 120 + usize::from(first.x)];
    assert_eq!(cell.symbol, "─");
}

/// With several machines the row above a machine's first workspace is that machine's row,
/// so a drop before the workspace marks the machine row. The marker fills the row around
/// the machine's name instead of drawing over it.
#[test]
fn multi_machine_drop_marker_above_a_first_workspace_keeps_the_machine_name() {
    let (mut state, _) = state_with_remote();
    state.compose(120, 40).expect("test precondition");
    let first = state
        .drawn()
        .workspaces()
        .find(|hit| hit.location.endpoint.is_local())
        .expect("first local workspace row")
        .rect;
    let machine = state
        .drawn()
        .machines()
        .find(|hit| hit.location.endpoint.is_local())
        .expect("local machine row")
        .rect;
    let row = first.y - 1;
    assert_eq!(
        machine.y, row,
        "the machine row is right above its first workspace"
    );
    state.pointer.chrome_drag = Some(ClientChromeDrag::Workspace {
        source_workspace_id: test_workspace_id("w1"),
        target: Some((Some(test_workspace_id("w1")), row)),
    });
    let frame = state.compose(120, 40).expect("dragging frame");
    let drawn = (machine.x..machine.right())
        .map(|x| frame_cell(&frame, (x, row)).symbol.as_str())
        .collect::<String>();
    assert!(
        drawn.contains(ClientEndpointId::Local.display_label(&state.config.local_label)),
        "the machine name stays readable: {drawn:?}"
    );
    assert!(drawn.contains('─'), "the drop marker is drawn: {drawn:?}");
}

#[test]
fn collapsed_sidebar_workspace_rows_accept_drag_targets() {
    let (mut state, _) = state_with_remote_config(&ClientConfig::default());
    let mut local = state
        .endpoints
        .active
        .snapshot()
        .expect("local snapshot")
        .clone();
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
    state.chrome.set_collapsed(true);
    state.compose(100, 28).expect("collapsed sidebar");

    let first = state
        .drawn()
        .workspaces()
        .find(|hit| {
            hit.location.endpoint.is_local()
                && hit.location.workspace_id() == Some(crate::tests::test_workspace_id("w1"))
        })
        .expect("first local workspace")
        .rect;
    let second = state
        .drawn()
        .workspaces()
        .find(|hit| {
            hit.location.endpoint.is_local()
                && hit.location.workspace_id() == Some(crate::tests::test_workspace_id("w2"))
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
        &state.pointer.chrome_drag,
        Some(ClientChromeDrag::Workspace {
            target: Some(_),
            ..
        })
    ));
}

#[test]
fn revealing_an_active_workspace_ignores_a_same_id_on_another_endpoint() {
    let (mut state, remote_id) = state_with_remote();
    let mut local = state
        .endpoints
        .active
        .snapshot()
        .expect("local snapshot")
        .clone();
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
    state.sidebar_scroll.clear_reveals();
    state.sidebar_scroll.scroll_workspaces_to(usize::MAX);
    state.compose(100, 22).expect("bottom of combined sidebar");
    assert!(
        state
            .drawn()
            .workspaces()
            .any(|hit| hit.location.endpoint == remote_id
                && hit.location.workspace_id() == Some(crate::tests::test_workspace_id("w5")))
    );
    assert!(
        !state
            .drawn()
            .workspaces()
            .any(|hit| hit.location.endpoint.is_local()
                && hit.location.workspace_id() == Some(crate::tests::test_workspace_id("w5")))
    );
    let bottom = state.sidebar_scroll.workspace_start();

    state.request_workspace_reveal(&test_workspace_id("w5"));
    assert_eq!(state.sidebar_scroll.workspace_start(), bottom);
    state.compose(100, 22).expect("active workspace revealed");

    assert!(
        state
            .drawn()
            .workspaces()
            .any(|hit| hit.location.endpoint.is_local()
                && hit.location.workspace_id() == Some(crate::tests::test_workspace_id("w5")))
    );
    assert!(state.sidebar_scroll.workspace_start() < bottom);
}

#[test]
fn machine_diagnostic_badge_reopens_its_notice() {
    let (mut state, id) = state_with_remote();
    state.set_endpoint_status(&id, EndpointFailureStatus::Attention);
    // ssh exits 255 for its own failures; this is how an auth prompt failure arrives.
    state.set_machine_diagnostic(
        &id,
        &shepr_launch::EndpointFailure::ssh(
            shepr_launch::SshFailureClass::Authentication,
            "Permission denied (keyboard-interactive)",
        ),
    );
    for _ in 0..2 {
        state.compose(120, 40).expect("test precondition");
        let hit = state
            .drawn()
            .machines()
            .find(|hit| hit.location.endpoint == id)
            .expect("test precondition");
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.status_badge.x,
            row: hit.status_badge.y,
            modifiers: KeyModifiers::NONE,
        };
        let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
        assert!(outcome.repaint);
        assert!(
            outcome.actions.is_empty(),
            "the badge only shows the reason"
        );
        let notice = state.notices.visible().expect("test precondition");
        assert!(notice.body.contains("Permission denied"));
        assert!(
            notice
                .title
                .contains("Build: restart shepr to authenticate")
        );
    }
    // A successful handshake clears the diagnostic.
    state.endpoint_connected(&id, crate::tests::test_generation(2));
    state.compose(120, 40).expect("test precondition");
    assert!(!state.machine_diagnostics.has(&id));
}

#[test]
fn machine_diagnostic_card_replaces_tabs_and_preserves_lines() {
    let (mut state, id) = state_with_remote();
    state.set_endpoint_status(&id, EndpointFailureStatus::Attention);
    state.set_machine_diagnostic(
        &id,
        &shepr_launch::EndpointFailure::unclassified("failure\twith fields\nretry\twith a key"),
    );

    state.compose(120, 40).expect("diagnostic badge");
    let badge = state
        .drawn()
        .machines()
        .find(|hit| hit.location.endpoint == id)
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

#[test]
fn sidebar_renders_local_and_saved_ssh_endpoints_with_status() {
    let (mut state, _) = state_with_remote();
    let frame = state.compose(100, 28).expect("combined endpoint frame");
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
    // The local server is shown by its label, never as "Local".
    assert!(text.contains(shepr_test_fixtures::FIXTURE_LOCAL_LABEL));
    assert!(!text.contains("Local"));
    assert!(text.contains("Build"));
    // The server's workspace number takes the leading column, so the name is
    // clipped at this sidebar width.
    assert!(text.contains("1  ○ remote-workspa"), "{text}");
    let local = state
        .drawn()
        .machines()
        .find(|hit| hit.location.endpoint.is_local())
        .expect("local machine row")
        .rect;
    let remote = state
        .drawn()
        .machines()
        .find(|hit| !hit.location.endpoint.is_local())
        .expect("remote machine row")
        .rect;
    assert_ne!(frame_cell(&frame, (local.right() - 1, local.y)).symbol, "●");
    assert_eq!(
        frame_cell(&frame, (remote.right() - 1, remote.y)).symbol,
        "●"
    );
    assert_eq!(
        cell_fg(&frame, (remote.right() - 1, remote.y)),
        state.palette.green
    );

    state.chrome.set_collapsed(true);
    let frame = state.compose(100, 28).expect("collapsed endpoint frame");
    let local = state
        .drawn()
        .machines()
        .find(|hit| hit.location.endpoint.is_local())
        .expect("collapsed local machine row")
        .rect;
    let remote = state
        .drawn()
        .machines()
        .find(|hit| !hit.location.endpoint.is_local())
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
    let mut local = state
        .endpoints
        .active
        .snapshot()
        .expect("local snapshot")
        .clone();
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
            .cells()
            .chunks(frame.width() as usize)
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for endpoint in [ClientEndpointId::Local, endpoint_id.clone()] {
            let row = state
                .drawn()
                .workspaces()
                .find(|hit| hit.location.endpoint == endpoint)
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
    assert!(state.drawn().workspace_max_scroll() > 0);

    let mut update = state.endpoints.active.snapshot().expect("snapshot").clone();
    update.revision = shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(2);
    update.workspaces.push(ClientShellWorkspace {
        workspace_id: test_workspace_id("w13"),
        label: "new-space".into(),
        ..template
    });
    update.focused_workspace_id = Some(test_workspace_id("w13"));
    state.set_snapshot(Box::new(update));
    let mut updated_surface = surface();
    updated_surface.projection_revision =
        shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(2);
    state.receive_pane_surface_from(
        updated_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 2).expect("zero-height workspace body");
    assert!(state.sidebar_scroll.workspace_reveal().focused_pending());
    state.compose(106, 20).expect("new workspace revealed");
    assert!(state.drawn().workspaces().any(|hit| {
        hit.location.endpoint == ClientEndpointId::Local
            && hit.location.workspace_id() == Some(crate::tests::test_workspace_id("w13"))
    }));

    state.sidebar_scroll.scroll_workspaces_to(0);
    state.compose(106, 20).expect("manual scroll");
    assert_eq!(state.sidebar_scroll.workspace_start(), 0);
    assert!(!state.drawn().workspaces().any(|hit| {
        hit.location.endpoint == ClientEndpointId::Local
            && hit.location.workspace_id() == Some(crate::tests::test_workspace_id("w13"))
    }));
    let unchanged = state.endpoints.active.snapshot().expect("snapshot").clone();
    state.set_snapshot(Box::new(unchanged));
    state
        .compose(106, 20)
        .expect("unchanged focus preserves scroll");
    assert_eq!(state.sidebar_scroll.workspace_start(), 0);
}

#[test]
fn expanded_machine_sidebar_applies_space_row_gap_within_each_machine() {
    let mut config = ClientConfig::default();
    config.ui.sidebar.spaces.row_gap = 1;
    let (mut state, remote_id) = state_with_remote_config(&config);

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
        .drawn()
        .workspaces()
        .filter(|hit| hit.location.endpoint.is_local())
        .collect::<Vec<_>>();
    assert_eq!(local_workspaces.len(), 2);
    assert_eq!(
        local_workspaces[1].rect.y,
        local_workspaces[0].rect.bottom() + 1
    );

    let local_machine = state
        .drawn()
        .machines()
        .find(|hit| hit.location.endpoint.is_local())
        .expect("local machine");
    let remote_machine = state
        .drawn()
        .machines()
        .find(|hit| hit.location.endpoint == remote_id)
        .expect("remote machine");
    assert_eq!(local_workspaces[0].rect.y, local_machine.rect.bottom());
    assert_eq!(remote_machine.rect.y, local_workspaces[1].rect.bottom());

    let remote_workspaces = state
        .drawn()
        .workspaces()
        .filter(|hit| hit.location.endpoint == remote_id)
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
    state.sidebar_scroll.scroll_workspaces_to(usize::MAX);
    state.compose(100, 22).expect("scrolled endpoint frame");
    let metrics = state
        .drawn()
        .workspace_scroll_metrics()
        .expect("workspace scroll metrics");
    assert!(metrics.max_start() > 0);
    assert_eq!(metrics.start(), metrics.max_start());
    assert_eq!(state.sidebar_scroll.workspace_start(), metrics.max_start());
    let visible_remote = state
        .drawn()
        .workspaces()
        .filter(|hit| hit.location.endpoint == remote_id)
        .collect::<Vec<_>>();
    assert_eq!(visible_remote.len(), 3);
    let gap_y = visible_remote[1].rect.bottom();
    assert_eq!(visible_remote[2].rect.y, gap_y + 1);
    assert!(visible_remote[2].rect.bottom() <= state.drawn().workspace_body().bottom());
    assert!(
        state
            .drawn()
            .workspaces()
            .all(|hit| gap_y < hit.rect.top() || gap_y >= hit.rect.bottom())
    );
}

#[test]
fn active_workspace_is_the_only_highlight_on_its_machine() {
    let (mut state, endpoint_id) = state_with_remote();
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

    let frame = state.compose(100, 28).expect("combined endpoint frame");
    let machine = state
        .drawn()
        .machines()
        .find(|hit| hit.location.endpoint == endpoint_id)
        .expect("remote machine hit")
        .rect;
    let workspace = state
        .drawn()
        .workspaces()
        .find(|hit| hit.location.endpoint == endpoint_id)
        .expect("remote workspace hit")
        .rect;
    assert_ne!(
        cell_bg(&frame, (machine.x, machine.y)),
        state.palette.active_row_bg
    );
    assert_eq!(
        cell_bg(&frame, (workspace.x + 2, workspace.y)),
        state.palette.active_row_bg
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
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    let mut remote = snapshot();
    remote.boot_id = crate::tests::test_boot_id("remote-boot");
    remote.agents = vec![agent(AgentStatus::Blocked, 1)];
    state.connect_endpoint_with_snapshot(&endpoint_id, 1, Box::new(remote));

    let frame = state.compose(100, 28).expect("combined endpoint frame");
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
    assert!(text.contains("○ Desk · pi"), "frame: {text}");
    assert!(text.contains("× Build · pi"), "frame: {text}");
    assert!(text.contains("grouped"), "frame: {text}");
    let toggle = state.drawn().agent_sort_toggle();
    assert!(!toggle.is_empty());
    let click = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: toggle.x,
        row: toggle.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert_eq!(
        state.agent_panel_sort_chrome.value(),
        shepr_config::AgentPanelSortConfig::Priority
    );
    assert!(click.actions.is_empty());

    assert!(
        frame
            .cells()
            .iter()
            .any(|cell| cell.symbol == "×" && cell.fg.to_ratatui() == state.palette.red)
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
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
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
            .cells()
            .chunks(frame.width() as usize)
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
        text.find("Desk · pi").expect("local agent")
            < text.find("Build · pi").expect("remote agent")
    );

    remote.agents = vec![agent(AgentStatus::Working, 2)];
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote.clone()));
    let text = frame_text(&mut state);
    assert!(
        text.find("Build · pi").expect("remote agent")
            < text.find("Desk · pi").expect("local agent")
    );

    remote.agents = vec![agent(AgentStatus::Idle, 3)];
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote));
    let text = frame_text(&mut state);
    assert!(
        text.find("Build · pi").expect("remote agent")
            < text.find("Desk · pi").expect("local agent")
    );
    let mut outcome = ClientShellInput::default();
    assert!(state.handle_endpoint_navigation(
        shepr_termio::input::KeybindAction::FocusAgent(0),
        &mut outcome,
    ));
    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint(Location {
            endpoint: activated,
            target: LocationTarget::Pane(pane_id),
        })] if activated == &endpoint_id && pane_id == &crate::tests::test_pane_id("w1:p1")
    ));
}

#[test]
fn clicking_an_offline_active_machine_row_changes_nothing() {
    let (mut state, endpoint_id) = state_with_remote();
    assert!(state.activate_endpoint_projection(&endpoint_id));
    state.set_endpoint_status(&endpoint_id, EndpointFailureStatus::Reconnecting);
    state.compose(100, 28).expect("active remote frame");
    let hit = state
        .drawn()
        .machines()
        .find(|hit| hit.location.endpoint == endpoint_id)
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
    assert!(state.notices.visible().is_none());
}

/// Every machine is listed expanded; one that is not connected shows its state entry
/// in place of its workspaces, and neither workspaces nor agents of its last snapshot.
#[test]
fn a_machine_that_is_not_connected_shows_its_state_entry_and_no_stale_rows() {
    use shepr_protocol::AgentStatus;

    let (mut state, endpoint_id) = state_with_remote();
    state.edit_endpoint_snapshot(&endpoint_id, |snapshot| {
        snapshot.agents = vec![agent(AgentStatus::Blocked, 1)];
    });
    state.compose(100, 28).expect("connected remote frame");
    assert!(
        state
            .drawn()
            .workspaces()
            .any(|hit| hit.location.endpoint == endpoint_id)
    );
    assert!(state.drawn().machine_entries().next().is_none());

    state.set_endpoint_status(&endpoint_id, EndpointFailureStatus::Reconnecting);
    state.set_machine_state(&endpoint_id, crate::shell::MachineState::NotRunning);
    let frame = state.compose(100, 28).expect("remote with no server");
    assert!(
        !state
            .drawn()
            .workspaces()
            .any(|hit| hit.location.endpoint == endpoint_id),
        "no stale workspace rows"
    );
    assert!(
        state
            .endpoints
            .agent_panel_model
            .rows
            .iter()
            .all(|row| row.endpoint_id != endpoint_id),
        "no stale agent rows"
    );
    let entry = state
        .drawn()
        .machine_entries()
        .find(|hit| hit.location.endpoint == endpoint_id)
        .expect("the remote's entry")
        .clone();
    assert!(entry.actionable);
    let machine = state
        .drawn()
        .machines()
        .find(|hit| hit.location.endpoint == endpoint_id)
        .expect("remote machine row")
        .rect;
    assert_eq!(
        entry.rect.y,
        machine.bottom(),
        "the entry sits under its machine"
    );
    let text = frame_rows(&frame).join("\n");
    assert!(text.contains("Connect"), "{text}");

    // The collapsed strip shows the state as the machine row's glyph, with no entry row.
    state.chrome.set_collapsed(true);
    let frame = state.compose(100, 28).expect("collapsed strip");
    assert!(state.drawn().machine_entries().next().is_none());
    let machine = state
        .drawn()
        .machines()
        .find(|hit| hit.location.endpoint == endpoint_id)
        .expect("collapsed remote machine row")
        .rect;
    assert_eq!(
        frame_cell(&frame, (machine.right() - 1, machine.y)).symbol,
        "○"
    );
}

#[test]
fn every_machine_state_has_its_own_entry() {
    use crate::shell::MachineState;

    let (mut state, endpoint_id) = state_with_remote();
    state.set_endpoint_status(&endpoint_id, EndpointFailureStatus::Reconnecting);
    for (machine_state, text, actionable) in [
        (MachineState::Connecting, "Connecting...", false),
        (MachineState::NotRunning, "Connect", true),
        (MachineState::Starting, "Starting...", false),
        (MachineState::Stopping, "Stopping...", false),
        (MachineState::DifferentBuild, "Restart (other build)", true),
        (MachineState::Restarting, "Restarting...", false),
        (MachineState::Offline, "Offline", false),
        (MachineState::NeedsLogin, "Needs SSH login", false),
        (MachineState::Unavailable, "Unavailable", false),
    ] {
        state.set_machine_state(&endpoint_id, machine_state);
        let frame = state.compose(100, 28).expect("remote entry frame");
        let entry = state
            .drawn()
            .machine_entries()
            .find(|hit| hit.location.endpoint == endpoint_id)
            .expect("the remote's entry")
            .clone();
        assert_eq!(entry.actionable, actionable, "{machine_state:?}");
        let drawn = (entry.rect.x..entry.rect.right())
            .map(|x| frame_cell(&frame, (x, entry.rect.y)).symbol.as_str())
            .collect::<String>();
        assert!(drawn.contains(text), "{machine_state:?}: {drawn:?}");
        if machine_state == MachineState::NeedsLogin {
            assert_eq!(entry.rect.height, 2, "the login hint has a row of its own");
            let hint = (entry.rect.x..entry.rect.right())
                .map(|x| frame_cell(&frame, (x, entry.rect.y + 1)).symbol.as_str())
                .collect::<String>();
            assert!(hint.contains("run shepr again"), "{hint:?}");
        }
    }
}

#[test]
fn clicking_a_connect_entry_starts_the_machine_and_attaches() {
    let (mut state, endpoint_id) = state_with_remote();
    state.set_endpoint_status(&endpoint_id, EndpointFailureStatus::Reconnecting);
    state.set_machine_state(&endpoint_id, crate::shell::MachineState::NotRunning);
    state.compose(100, 28).expect("remote with no server");
    let entry = state
        .drawn()
        .machine_entries()
        .find(|hit| hit.location.endpoint == endpoint_id)
        .expect("the remote's entry")
        .rect;

    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: entry.x + 4,
        row: entry.y,
        modifiers: KeyModifiers::empty(),
    })]);

    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ConnectMachine(connected)] if *connected == endpoint_id
    ));
    assert_eq!(
        state.machine_state(&endpoint_id),
        Some(crate::shell::MachineState::Starting)
    );
}

/// Restart asks first: the click only opens the question, and only its answer sends the
/// Restart, whose attempt then does the conditional stop and the start.
#[test]
fn a_restart_entry_asks_before_it_restarts() {
    let (mut state, endpoint_id) = state_with_remote();
    state.set_endpoint_status(&endpoint_id, EndpointFailureStatus::Attention);
    state.set_machine_state(&endpoint_id, crate::shell::MachineState::DifferentBuild);
    state.compose(100, 28).expect("remote of another build");
    let entry = state
        .drawn()
        .machine_entries()
        .find(|hit| hit.location.endpoint == endpoint_id)
        .expect("the remote's entry")
        .rect;

    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: entry.x + 4,
        row: entry.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(outcome.actions.is_empty(), "nothing is restarted unasked");
    let Some(crate::shell::overlays::Overlay::ConfirmRestart(question)) = state.overlay.as_ref()
    else {
        panic!("the Restart question is open");
    };
    let lines = question.lines().join(" ");
    assert!(lines.contains("ends every pane process"), "{lines}");
    assert!(lines.contains("saved layout is restored"), "{lines}");
    assert!(lines.contains("agents are resumed"), "{lines}");

    // Cancelling restarts nothing.
    let cancelled =
        crate::shell::tests::press_overlay_key(&mut state, crossterm::event::KeyCode::Esc);
    assert!(cancelled.actions.is_empty());
    assert!(state.overlay.is_none());

    state.open_confirm_restart_overlay(&endpoint_id);
    let confirmed =
        crate::shell::tests::press_overlay_key(&mut state, crossterm::event::KeyCode::Enter);
    assert!(matches!(
        confirmed.actions.as_slice(),
        [ClientShellAction::RestartMachine(restarted)] if *restarted == endpoint_id
    ));
    assert!(state.overlay.is_none());
    assert_eq!(
        state.machine_state(&endpoint_id),
        Some(crate::shell::MachineState::Restarting)
    );
}

#[test]
fn clicking_an_online_active_machine_row_reselects_it() {
    let (mut state, endpoint_id) = state_with_remote();
    assert!(state.activate_endpoint_projection(&endpoint_id));
    state.compose(100, 28).expect("active remote frame");
    let hit = state
        .drawn()
        .machines()
        .find(|hit| hit.location.endpoint == endpoint_id)
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
        [ClientShellAction::ActivateEndpoint(Location {
            endpoint: selected,
            target: LocationTarget::Machine,
        })] if *selected == endpoint_id
    ));
}

/// A machine row has no fold marker and nothing to fold: every connected machine keeps
/// its workspaces listed under it, in the expanded sidebar and the collapsed strip.
#[test]
fn every_connected_machine_lists_its_workspaces() {
    for sidebar_collapsed in [false, true] {
        let other_machine = machine_named("Other", "dev@other.example");
        let other_id = ClientEndpointId::Ssh(other_machine.label.clone());
        let (mut state, remote_id) = state_with_machines(&[remote_machine(), other_machine]);
        state.connect_endpoint_with_snapshot(&other_id, 1, Box::new(snapshot()));
        state.chrome.set_collapsed(sidebar_collapsed);
        let frame = state.compose(100, 28).expect("three machine frame");
        let text = frame_rows(&frame).join("\n");
        assert!(!text.contains('▾') && !text.contains('▸'), "{text}");
        for endpoint_id in [&ClientEndpointId::Local, &remote_id, &other_id] {
            assert!(
                state
                    .drawn()
                    .workspaces()
                    .any(|hit| &hit.location.endpoint == endpoint_id),
                "{endpoint_id}"
            );
        }
    }
}

#[test]
fn workspace_drag_rejects_foreign_endpoint_slots() {
    let (mut state, endpoint_id) = state_with_remote();
    state.compose(100, 28).expect("aggregate sidebar");
    let local = state
        .drawn()
        .workspaces()
        .find(|hit| hit.location.endpoint.is_local())
        .expect("local workspace")
        .rect;
    let remote = state
        .drawn()
        .workspaces()
        .find(|hit| hit.location.endpoint == endpoint_id)
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

    assert!(state.pointer.chrome_drag.is_none());
}

#[test]
fn collapsed_aggregate_workspace_status_uses_its_status_color() {
    use shepr_protocol::AgentStatus;

    let (mut state, endpoint_id) = state_with_remote();
    state.edit_endpoint_snapshot(&endpoint_id, |snapshot| {
        snapshot.workspaces[0].agent_status = AgentStatus::Blocked;
    });
    state.chrome.set_collapsed(true);

    let frame = state.compose(100, 28).expect("collapsed aggregate sidebar");
    let workspace = state
        .drawn()
        .workspaces()
        .find(|hit| hit.location.endpoint == endpoint_id)
        .expect("remote workspace")
        .rect;
    assert_eq!(
        cell_fg(&frame, (workspace.x.saturating_add(2), workspace.y)),
        state.palette.red
    );
}
