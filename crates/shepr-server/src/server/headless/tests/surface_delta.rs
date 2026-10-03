use super::*;

fn receive_message(receiver: &RenderLaneReceiver) -> (Vec<u8>, ServerMessage) {
    let bytes = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("render message");
    let message = read_server_message(bytes.clone());
    (bytes, message)
}

fn decode_surface_message(
    decoder: &mut shepr_protocol::surface_reuse::Decoder,
    message: ServerMessage,
) -> shepr_protocol::PaneSurfaceFrame {
    match decoder.decode(message).expect("decode surface message") {
        shepr_protocol::surface_reuse::DecodedServerMessage::Wire(ServerMessage::PaneSurface(
            surface,
        )) => surface,
        other => panic!("expected decoded pane surface, got {other:?}"),
    }
}

#[tokio::test]
async fn surface_delta_reconstructs_metadata_text_and_hyperlinks() {
    let (mut server, _control_rx, render_rx, pane_id) =
        retained_test_server_with_control(b"initial text");
    server.render_now();
    let (initial_bytes, initial_message) = receive_message(&render_rx);
    let mut decoder = shepr_protocol::surface_reuse::Decoder::default();
    let initial = decode_surface_message(&mut decoder, initial_message);
    assert_eq!(
        &initial,
        server.clients[&1]
            .render_state
            .last_pane_surface()
            .expect("initial baseline")
    );

    let initial_projection_revision = initial.projection_revision;
    server.app.state.workspaces[0].custom_name = Some("renamed workspace".into());
    server.app.state.mark_shell_projection_dirty();
    write_shared_test_pane(
        &mut server,
        pane_id,
        b"\rupdated text \x1b]8;;https://example.test/path\x1b\\linked\x1b]8;;\x1b\\",
    );
    server
        .clients
        .get_mut(&1)
        .expect("test precondition")
        .request_recompute();
    assert!(!server.try_render_patches(&HashSet::from([pane_id])));
    server.render_now();
    let (delta_bytes, delta_message) = receive_message(&render_rx);
    assert!(delta_bytes.len() < initial_bytes.len());
    assert!(matches!(&delta_message, ServerMessage::SurfaceUpdate(_)));
    let decoded = decode_surface_message(&mut decoder, delta_message);
    assert_eq!(
        &decoded,
        server.clients[&1]
            .render_state
            .last_pane_surface()
            .expect("updated baseline")
    );
    assert!(decoded.projection_revision > initial_projection_revision);
    assert!(frame_text(&decoded.frame).contains("updated text"));
    assert_eq!(decoded.frame.hyperlinks, vec!["https://example.test/path"]);
    shutdown_test_runtimes(&mut server);
}

struct Pair {
    server: HeadlessServer,
    pane: shepr_core::layout::PaneId,
    control: [std::sync::mpsc::Receiver<Vec<u8>>; 2],
    render: [RenderLaneReceiver; 2],
}

impl Pair {
    fn new() -> Self {
        Self::with_slow_size(80, 23)
    }

    fn with_slow_size(cols: u16, rows: u16) -> Self {
        let mut server = test_headless_server();
        let pane = install_shared_view_test_runtime(&mut server);
        let (control7, render7) = connect_matching_test_shell(&mut server, 7);
        let (control8, render8) = connect_test_shell(&mut server, 8, cols, rows);
        let plan = server.render_plan(false);
        server.render_pass(&plan, &HashSet::new());
        render7.recv().expect("baseline 7");
        render8.recv().expect("baseline 8");
        while control7.try_recv().is_ok() {}
        while control8.try_recv().is_ok() {}
        Self {
            server,
            pane,
            control: [control7, control8],
            render: [render7, render8],
        }
    }
    fn damage(&mut self, bytes: &[u8]) {
        write_shared_test_pane(&mut self.server, self.pane, bytes);
    }
    fn pass(&mut self, dirty: bool) -> render::PassReport {
        let plan = self.server.render_plan(dirty);
        let sources = if dirty {
            HashSet::from([self.pane])
        } else {
            HashSet::new()
        };
        self.server.render_pass(&plan, &sources)
    }
}
impl Drop for Pair {
    fn drop(&mut self) {
        shutdown_test_runtimes(&mut self.server);
    }
}

#[tokio::test]
async fn a_drained_slow_client_is_rendered_alone() {
    let mut pair = Pair::with_slow_size(79, 22);
    pair.damage(b"\rONE");
    assert_eq!(
        pair.pass(true).patched,
        vec![ClientId::test_new(7), ClientId::test_new(8)]
    );
    pair.render[0].recv().expect("responsive first patch");
    pair.damage(b"\rTWO");
    let report = pair.pass(true);
    assert_eq!(report.patched, vec![ClientId::test_new(7)]);
    assert_eq!(report.owed, vec![ClientId::test_new(8)]);
    // The writer has taken this peer's second patch. Keep those bytes in
    // flight while its now-free slot makes unnecessary peer rendering visible.
    let responsive_queued = pair.render[0]
        .recv()
        .expect("responsive queued second patch");
    let responsive = pair.server.clients[&7]
        .render_state
        .last_pane_surface()
        .cloned();
    pair.render[1].recv().expect("slow first patch");
    let report = pair.pass(false);
    assert_eq!(report.surface_renders, 1);
    assert_eq!(report.full, vec![ClientId::test_new(8)]);
    assert_eq!(
        pair.server.clients[&7].render_state.last_pane_surface(),
        responsive.as_ref()
    );
    assert!(pair.server.clients[&7].outbox.surface_slot_free());
    assert!(pair.render[0].try_recv().is_err());
    assert!(matches!(
        read_server_message(responsive_queued),
        ServerMessage::SurfaceUpdate(_)
    ));
}

#[tokio::test]
async fn a_pty_change_reaches_every_client_that_can_take_it_as_a_patch() {
    let mut pair = Pair::new();
    pair.damage(b"\rPATCH");
    let report = pair.pass(true);
    assert_eq!(
        report.patched,
        vec![ClientId::test_new(7), ClientId::test_new(8)]
    );
    assert!(report.full.is_empty());
    assert_eq!(report.surface_renders, 0);
}

#[tokio::test]
async fn a_view_change_owes_every_presenting_client_one_pass_and_settles_them() {
    let mut pair = Pair::new();
    pair.server.mark_view_changed();
    assert_eq!(
        pair.pass(false).full,
        vec![ClientId::test_new(7), ClientId::test_new(8)]
    );
    assert!(!pair.server.render_plan(false).has_full());
}

#[tokio::test]
async fn a_resize_renders_only_the_resized_client_when_no_workspace_resizes() {
    let mut pair = Pair::new();
    let before = pair.server.view_epoch;
    pair.server.handle_server_event(ServerEvent::ShellResize {
        client_id: ClientId::test_new(8),
        surface_cols: 79,
        surface_rows: 23,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
    });
    assert_eq!(pair.server.view_epoch, before);
    assert_eq!(pair.pass(false).full, vec![ClientId::test_new(8)]);
}

#[tokio::test]
async fn a_scroll_renders_only_the_viewers_of_the_scrolled_pane() {
    let mut pair = Pair::new();
    pair.server.app.insert_test_runtime(
        pair.pane,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(80, 23, 10_000, b"BASE"),
    );
    let workspace_id = pair.server.app.state.workspaces[0].id.clone();
    pair.server.apply_workspace_geometry(&workspace_id);
    let other = shepr_mux::workspace::Workspace::test_new("other");
    let other_id = other.id.clone();
    pair.server.app.state.workspaces.push(other);
    pair.server
        .place_test_client_on_workspace(ClientId::test_new(8), &other_id);
    pair.pass(false);
    pair.render[1].recv().expect("navigation surface");
    pair.damage(&b"history\r\n".repeat(40));
    pair.pass(true);
    pair.render[0].recv().expect("history patch");
    assert!(
        pair.server
            .app
            .test_runtime(pair.pane)
            .scroll_metrics()
            .is_some_and(|metrics| metrics.max_offset_from_bottom > 0)
    );
    let before = pair.server.view_epoch;
    let pane_id = pair
        .server
        .app
        .public_pane_id(0, pair.pane)
        .expect("pane id")
        .parse()
        .expect("typed id");
    pair.server
        .handle_client_shell_command(
            ClientId::test_new(7),
            EndpointCommand::PaneScroll(shepr_protocol::command::PaneScrollParams {
                pane_id,
                offset_from_bottom: 1,
            }),
        )
        .expect("scroll command");
    assert_eq!(pair.server.view_epoch, before);
    assert_eq!(pair.pass(false).full, vec![ClientId::test_new(7)]);
}

#[tokio::test]
async fn a_navigation_renders_only_the_client_that_moved() {
    let mut pair = Pair::new();
    let other = shepr_mux::workspace::Workspace::test_new("destination");
    let other_id = other.id.clone();
    pair.server.app.state.workspaces.push(other);
    let epoch = pair.server.view_epoch;
    assert!(
        pair.server
            .navigate_shell_client(ClientId::test_new(8), &other_id)
    );
    assert_eq!(pair.server.view_epoch, epoch);
    assert_eq!(pair.pass(false).full, vec![ClientId::test_new(8)]);
}

#[tokio::test]
async fn a_host_effects_replay_renders_nobody() {
    let mut pair = Pair::new();
    let epoch = pair.server.view_epoch;
    pair.server
        .handle_server_event(ServerEvent::ShellReplayHostEffects {
            client_id: ClientId::test_new(8),
        });
    assert_eq!(pair.server.view_epoch, epoch);
    assert!(!pair.server.render_plan(false).has_full());
}

#[tokio::test]
async fn a_client_blocked_by_its_slot_is_out_of_the_plan_until_the_slot_frees() {
    let mut pair = Pair::new();
    pair.damage(b"\rONE");
    pair.pass(true);
    pair.render[0].recv().expect("responsive patch");
    pair.damage(b"\rTWO");
    pair.pass(true);
    assert!(!pair.server.render_plan(false).has_full());
    pair.render[1].recv().expect("slow slot");
    assert_eq!(
        pair.server.render_plan(false).full,
        vec![ClientId::test_new(8)]
    );
}

#[tokio::test]
async fn a_blocked_client_costs_no_surface_render_in_a_view_change_pass() {
    let mut pair = Pair::new();
    pair.damage(b"\rONE");
    pair.pass(true);
    pair.render[0].recv().expect("responsive patch");
    pair.server.mark_view_changed();
    let report = pair.pass(false);
    assert_eq!(
        report.full,
        vec![ClientId::test_new(7), ClientId::test_new(8)]
    );
    assert_eq!(report.owed, vec![ClientId::test_new(8)]);
    assert_eq!(report.surface_renders, 1);
    assert!(!pair.server.render_plan(false).has_full());
}

#[tokio::test]
async fn a_synchronized_pane_defers_the_surface_without_a_retry_loop() {
    let mut pair = Pair::new();
    pair.damage(b"\x1b[?2026hPARTIAL");
    pair.server.mark_view_changed();
    let report = pair.pass(false);
    assert_eq!(report.surface_renders, 0);
    assert_eq!(
        report.owed,
        vec![ClientId::test_new(7), ClientId::test_new(8)]
    );
    assert!(!pair.server.render_plan(false).has_full());
    pair.damage(b"\x1b[?2026l");
    assert_eq!(
        pair.server.render_plan(false).full,
        vec![ClientId::test_new(7), ClientId::test_new(8)]
    );
}

#[tokio::test]
async fn a_changed_deferral_owes_its_client_without_a_view_change() {
    let mut pair = Pair::new();
    let epoch = pair.server.view_epoch;
    pair.server.app.render_dirty.take();
    pair.server
        .clients
        .get_mut(&8)
        .expect("client")
        .render_state
        .owe();
    let plan = pair.server.render_plan(false);
    let report = pair.server.render_pass_with_boundary(
        &plan,
        &HashSet::new(),
        render::SurfaceBoundary {
            render: |_, _, _, _| Err(crate::server::client_shell::SurfaceRenderDeferred::Changed),
            ..render::SurfaceBoundary::default()
        },
    );
    assert_eq!(report.owed, vec![ClientId::test_new(8)]);
    assert_eq!(report.surface_renders, 1);
    assert_eq!(pair.server.view_epoch, epoch);
    assert!(!pair.server.app.render_dirty.is_pending());
    assert_eq!(
        pair.server.render_plan(false).full,
        vec![ClientId::test_new(8)]
    );
    pair.pass(false);
    assert!(!pair.server.render_plan(false).has_full());
}

#[tokio::test]
async fn a_refused_client_retries_on_pty_damage_to_a_pane_it_shows() {
    let mut pair = Pair::new();
    pair.server
        .clients
        .get_mut(&8)
        .expect("client")
        .render_state
        .refuse();
    assert!(
        !pair
            .server
            .render_plan(true)
            .full
            .contains(&ClientId::test_new(8))
    );
    assert!(!pair.server.retry_refused_viewers(&HashSet::new()));
    assert!(
        pair.server
            .retry_refused_viewers(&HashSet::from([pair.pane]))
    );
    // Nothing was committed when the surface was refused, so the retry is a
    // full surface, not a patch of the baseline it left behind.
    let plan = pair.server.render_plan(true);
    assert_eq!(plan.full, vec![ClientId::test_new(8)]);
    assert!(!plan.patch.contains(&ClientId::test_new(8)));
}

#[tokio::test]
async fn a_patch_for_a_client_with_an_occupied_slot_owes_it_a_full_surface() {
    let mut pair = Pair::new();
    pair.damage(b"\rONE");
    pair.pass(true);
    pair.render[0].recv().expect("responsive patch");
    pair.damage(b"\rTWO");
    let outcome = pair.server.render_patches(
        &[ClientId::test_new(7), ClientId::test_new(8)],
        &HashSet::from([pair.pane]),
    );
    assert_eq!(outcome.owed, vec![ClientId::test_new(8)]);
    assert!(outcome.promote.is_empty());
    assert!(pair.server.clients[&8].render_state.surface_debt());
}

#[tokio::test]
async fn a_retained_check_failure_promotes_only_its_client() {
    let mut pair = Pair::new();
    pair.server
        .clients
        .get_mut(&8)
        .expect("client")
        .render_state
        .last_surface_mut()
        .expect("baseline")
        .boot_id = shepr_test_fixtures::fixed_boot_id(999);
    pair.damage(b"\rNEXT");
    let outcome = pair.server.render_patches(
        &[ClientId::test_new(7), ClientId::test_new(8)],
        &HashSet::from([pair.pane]),
    );
    assert_eq!(outcome.promote, vec![ClientId::test_new(8)]);
    assert_eq!(outcome.sent, vec![ClientId::test_new(7)]);
}

#[tokio::test]
async fn a_failed_source_collection_promotes_only_clients_viewing_that_pane() {
    let mut pair = Pair::new();
    let other = shepr_mux::workspace::Workspace::test_new("other");
    let other_id = other.id.clone();
    pair.server.app.state.workspaces.push(other);
    pair.server
        .place_test_client_on_workspace(ClientId::test_new(8), &other_id);
    pair.pass(false);
    pair.render[1].recv().expect("other baseline");
    let terminal = pair.server.app.state.workspaces[0]
        .terminal_id(pair.pane)
        .expect("terminal")
        .clone();
    pair.server.app.terminal_runtimes.remove(&terminal);
    let outcome = pair.server.render_patches(
        &[ClientId::test_new(7), ClientId::test_new(8)],
        &HashSet::from([pair.pane]),
    );
    assert_eq!(outcome.promote, vec![ClientId::test_new(7)]);
    assert!(outcome.sent.is_empty());
}

#[tokio::test]
async fn mode_geometry_is_settled_before_the_render_plan() {
    let mut pair = Pair::new();
    pair.damage(b"\x1b[?1049h");
    pair.server
        .clients
        .get_mut(&8)
        .expect("client")
        .render_state
        .owe();
    let size_before = pair.server.app.test_runtime(pair.pane).current_size();
    let plan = pair.server.render_plan(true);
    assert_eq!(
        plan.full,
        vec![ClientId::test_new(7), ClientId::test_new(8)]
    );
    let settled_size = pair.server.app.test_runtime(pair.pane).current_size();
    assert_ne!(settled_size, size_before, "settlement precedes drawing");
    let report = pair.server.render_pass(&plan, &HashSet::new());
    assert_eq!(
        pair.server.app.test_runtime(pair.pane).current_size(),
        settled_size
    );
    assert_eq!(
        report.full,
        vec![ClientId::test_new(7), ClientId::test_new(8)]
    );
    let geometry = pair.server.app.test_runtime(pair.pane).current_size();
    let surface = pair.server.clients[&8]
        .render_state
        .last_pane_surface()
        .expect("surface");
    assert_eq!(
        geometry,
        (
            surface.panes[0].inner_rect.height,
            surface.panes[0].inner_rect.width
        )
    );
}

#[tokio::test]
async fn mode_geometry_includes_all_viewers_in_the_same_plan() {
    let mut pair = Pair::new();
    pair.damage(b"\x1b[?1049h");
    pair.server
        .clients
        .get_mut(&8)
        .expect("client")
        .render_state
        .owe();
    let plan = pair.server.render_plan(true);
    assert_eq!(
        plan.full,
        vec![ClientId::test_new(7), ClientId::test_new(8)]
    );
    pair.server.render_pass(&plan, &HashSet::new());
    assert!(!pair.server.render_plan(false).has_full());
}

#[tokio::test]
async fn a_geometry_application_that_resizes_nothing_invalidates_nobody() {
    let mut pair = Pair::new();
    let workspace_id = pair.server.app.state.workspaces[0].id.clone();
    let Some(crate::server::headless::client_views::GeometrySource::Client(source)) =
        pair.server.workspace_geometry_source(&workspace_id)
    else {
        panic!("a client sizes the shared workspace");
    };
    let other = if source == ClientId::test_new(7) {
        ClientId::test_new(8)
    } else {
        ClientId::test_new(7)
    };
    // A busy source with no baseline must not make geometry settlement
    // invalidate its co-viewer repeatedly.
    let client = pair.server.clients.get_mut(&source).expect("source");
    client.request_repaint();
    assert_eq!(
        client.outbox.offer_surface(vec![0]),
        crate::server::outbox::SurfaceOffer::Queued
    );
    pair.server
        .clients
        .get_mut(&other)
        .expect("other")
        .render_state
        .invalidate();
    assert_eq!(pair.pass(false).full, vec![other]);
    // The PTYs already had the source's size: nothing to hand back to it.
    assert!(!pair.server.render_plan(false).has_full());
}

#[tokio::test]
async fn replies_follow_the_snapshot_when_a_pass_renders_a_subset() {
    let mut pair = Pair::new();
    let other = shepr_mux::workspace::Workspace::test_new("destination");
    let other_id = other.id.clone();
    pair.server.app.state.workspaces.push(other);
    pair.server
        .navigate_shell_client(ClientId::test_new(8), &other_id);
    let reply = ServerMessage::WindowTitle {
        title: Some("reply sentinel".into()),
    };
    pair.server
        .queue_endpoint_reply(ClientId::test_new(8), &reply);
    assert_eq!(pair.pass(false).full, vec![ClientId::test_new(8)]);
    pair.server
        .release_endpoint_replies(ReleaseMode::WithinBudget);
    assert!(matches!(
        read_server_message(pair.control[1].recv().expect("snapshot")),
        ServerMessage::EndpointSnapshot(_)
    ));
    assert_eq!(
        read_server_message(pair.control[1].recv().expect("reply")),
        reply
    );
    assert!(pair.control[0].try_recv().is_err());
}

#[tokio::test]
async fn with_no_client_attached_planning_lays_out_a_workspace_without_owing_a_pass() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    assert!(!server.render_plan(false).has_full());
    assert!(server.app.state.workspace_area(0).is_some());
    server.mark_view_changed();
    assert!(!server.render_plan(false).has_full());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_reaped_client_leaves_survivors_with_updated_projection_and_surface() {
    let mut pair = Pair::new();
    let epoch = pair.server.view_epoch;
    pair.server.clients[&8].outbox.close();
    assert!(pair.server.reap_closed_clients());
    assert_ne!(pair.server.view_epoch, epoch);
    assert_eq!(pair.pass(false).full, vec![ClientId::test_new(7)]);
    assert!(!pair.server.render_plan(false).has_full());
}

#[tokio::test]
async fn an_oversized_first_surface_is_refused_without_a_retry_loop() {
    let mut pair = Pair::new();
    pair.server
        .clients
        .get_mut(&8)
        .expect("client")
        .request_repaint();
    let plan = pair.server.render_plan(false);
    let report = pair.server.render_pass_with_boundary(
        &plan,
        &HashSet::new(),
        render::SurfaceBoundary {
            encode: |_| {
                Err(shepr_protocol::FramingError::LimitExceeded(
                    shepr_protocol::LimitExceeded::new(
                        shepr_protocol::Limit::new(shepr_protocol::LimitKind::MessageBytes, 1),
                        2,
                    ),
                ))
            },
            ..render::SurfaceBoundary::default()
        },
    );
    assert_eq!(report.full, vec![ClientId::test_new(8)]);
    assert_eq!(report.surface_renders, 1);
    assert!(
        pair.server.clients[&8]
            .render_state
            .last_pane_surface()
            .is_none()
    );
    assert!(!pair.server.render_plan(false).has_full());
    let ServerMessage::ClientShellError {
        kind: shepr_protocol::NoticeKind::LimitExceeded(error),
    } = read_server_message(pair.control[1].recv().expect("oversized notice"))
    else {
        panic!("expected an oversized surface notice");
    };
    assert_eq!(
        error.limit.kind(),
        shepr_protocol::LimitKind::SurfaceMessageBytes
    );
    pair.damage(b"\rSMALL");
    assert!(
        pair.server
            .retry_refused_viewers(&HashSet::from([pair.pane]))
    );
    assert_eq!(pair.pass(false).full, vec![ClientId::test_new(8)]);
    assert!(!pair.server.render_plan(false).has_full());
}

#[tokio::test]
async fn exhausted_surface_revisions_close_the_client() {
    let mut pair = Pair::new();
    let client = pair.server.clients.get_mut(&8).expect("client");
    client.render_state.exhaust_revisions();
    client.render_state.owe();
    assert_eq!(pair.pass(false).full, vec![ClientId::test_new(8)]);
    assert!(pair.server.reap_closed_clients());
    assert!(!pair.server.clients.contains_key(&ClientId::test_new(8)));
}

#[tokio::test]
async fn scrolling_preserves_concurrent_shared_projection_changes() {
    let mut pair = Pair::new();
    pair.server.app.state.ensure_test_terminals();
    pair.damage(b"\x1b]0;shared title\x07");
    pair.server
        .app
        .render_dirty
        .request_terminal_title(pair.pane);
    let epoch = pair.server.view_epoch;
    let pane_id = pair
        .server
        .app
        .public_pane_id(0, pair.pane)
        .expect("pane id")
        .parse()
        .expect("typed id");
    pair.server
        .handle_client_shell_command(
            ClientId::test_new(7),
            EndpointCommand::PaneScroll(shepr_protocol::command::PaneScrollParams {
                pane_id,
                offset_from_bottom: 0,
            }),
        )
        .expect("unchanged scroll command");
    assert_ne!(pair.server.view_epoch, epoch);
    assert_eq!(
        pair.pass(false).full,
        vec![ClientId::test_new(7), ClientId::test_new(8)]
    );
}
