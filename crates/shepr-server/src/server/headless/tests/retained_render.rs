use super::*;

#[test]
fn server_message_encoding_splits_payloads_over_the_frame_cap() {
    let small = shepr_protocol::encode_message(&ServerMessage::ClientShellError {
        kind: shepr_protocol::NoticeKind::PaneInputDropped {
            pane_id: shepr_protocol::PublicPaneId::new(
                &crate::test_support::test_workspace_id("w1"),
                shepr_protocol::PanePublicNumber::new(1).expect("nonzero test number"),
            ),
            events: 1,
        },
    })
    .expect("small message frames");
    assert!(matches!(
        read_server_message(small),
        ServerMessage::ClientShellError { kind: shepr_protocol::NoticeKind::PaneInputDropped { pane_id, events: 1 } } if pane_id == "w1:p1".parse::<shepr_protocol::PublicPaneId>().expect("id")
    ));

    // Clipboard data past one frame crosses as a continued frame and a final
    // one, and reads back whole.
    let data = vec![b'x'; MAX_FRAME_SIZE + 1];
    let large = shepr_protocol::encode_message(&ServerMessage::Clipboard { data: data.clone() })
        .expect("large message frames");
    let first_prefix = u32::from_le_bytes(large[..4].try_into().expect("test precondition"));
    assert_ne!(first_prefix & (1 << 31), 0, "the first frame is continued");
    assert!(matches!(
        read_server_message(large),
        ServerMessage::Clipboard { data: read } if read == data
    ));
}

fn recv_pane_surface_patch(
    receiver: &mut PaneSurfaceReceiver,
    context: &str,
) -> shepr_protocol::SurfaceUpdate {
    let message = read_server_message(
        receiver
            .receiver
            .recv()
            .unwrap_or_else(|error| panic!("{context}: {error}")),
    );
    receiver
        .decoder
        .decode(message.clone())
        .expect("valid surface update");
    match message {
        ServerMessage::SurfaceUpdate(patch) => patch,
        other => panic!("{context}: expected pane surface patch, got {other:?}"),
    }
}

#[tokio::test]
async fn unrelated_render_keeps_synchronized_pane_frame_committed() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 7);
    let mut render = PaneSurfaceReceiver::new(render);
    server.render_now();
    let before = recv_pane_surface(&mut render, "baseline");
    assert!(frame_text(&before.frame).contains("BASE"));
    let projection_before = server.clients[&ClientId::test_new(7)]
        .shell_state()
        .projection_revision;
    // Drain the attach and baseline control traffic.
    while control.recv_timeout(Duration::from_millis(50)).is_ok() {}

    write_shared_test_pane(
        &mut server,
        pane_id,
        b"\x1b[?2026h\x1b[?1049h\x1b[2J\x1b[HPARTIAL",
    );
    server
        .app
        .test_state_mut()
        .ws_mut(0)
        .set_name(shepr_mux::Label::new("renamed during frame").expect("test name"));
    server.app.test_state_mut().mark_shell_projection_dirty();
    server
        .clients
        .get_mut(&ClientId::test_new(7))
        .expect("test precondition")
        .request_recompute();
    server.render_now();
    assert!(render.try_recv().is_err(), "partial frame was published");
    // Only the pane surface waits for the synchronized update: the projection
    // still goes out, so a reply flushed after this render cannot overtake it.
    let ServerMessage::EndpointSnapshot(snapshot) = read_server_message(
        control
            .recv_timeout(Duration::from_secs(1))
            .expect("the projection is sent while the surface waits"),
    ) else {
        panic!("expected the changed projection");
    };
    assert_eq!(snapshot.workspaces[0].label, "renamed during frame");
    assert!(snapshot.revision > projection_before);
    assert_eq!(
        server.clients[&ClientId::test_new(7)]
            .shell_state()
            .projection_revision,
        snapshot.revision
    );

    write_shared_test_pane(&mut server, pane_id, b"\rCOMPLETE\x1b[?2026l");
    assert!(!server.try_render_patches(&HashSet::from([pane_id])));
    server.render_now();
    let after = recv_pane_surface(&mut render, "completed frame");
    assert!(frame_text(&after.frame).contains("COMPLETE"));
    assert!(after.projection_revision > projection_before);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn sibling_retained_output_waits_for_synchronized_pane_to_finish() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("synchronized-split");
    let first = workspace.tree().root();
    let second = workspace.test_split(shepr_core::layout::Direction::Vertical);

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.insert_test_runtime(
        first,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"FIRST"),
    );
    server.app.insert_test_runtime(
        second,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"SECOND"),
    );
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let (_control, render) = connect_matching_test_shell(&mut server, 7);
    let mut render = PaneSurfaceReceiver::new(render);
    server.render_now();
    let _ = recv_pane_surface(&mut render, "split baseline");

    write_shared_test_pane(&mut server, first, b"\x1b[?2026h\rPARTIAL");
    write_shared_test_pane(&mut server, second, b"\rUPDATED");
    assert!(!server.try_render_patches(&HashSet::from([second])));
    server.render_now();
    assert!(
        render.try_recv().is_err(),
        "sibling published partial frame"
    );

    write_shared_test_pane(&mut server, first, b"\rCOMPLETE\x1b[?2026l");
    assert!(!server.try_render_patches(&HashSet::from([first])));
    server.render_now();
    let after = recv_pane_surface(&mut render, "completed split");
    let text = frame_text(&after.frame);
    assert!(
        text.contains("COMPLETE") && text.contains("UPDATED"),
        "{text}"
    );
    assert!(!text.contains("PARTIAL"));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn zoom_hidden_synchronized_pane_does_not_block_surface() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("zoomed-sync");
    let hidden = workspace.tree().root();
    let visible = workspace.test_split(shepr_core::layout::Direction::Vertical);
    assert!(workspace.set_zoomed(true));

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.insert_test_runtime(
        hidden,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"HIDDEN"),
    );
    server.app.insert_test_runtime(
        visible,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"VISIBLE"),
    );
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let (_control, render) = connect_matching_test_shell(&mut server, 7);
    let mut render = PaneSurfaceReceiver::new(render);
    write_shared_test_pane(&mut server, hidden, b"\x1b[?2026h\rPARTIAL");
    server.render_now();
    let surface = recv_pane_surface(&mut render, "zoomed visible pane");
    assert!(frame_text(&surface.frame).contains("VISIBLE"));
    assert!(!frame_text(&surface.frame).contains("PARTIAL"));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn retained_snapshot_survives_a_writer_waiting_for_the_terminal_core() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 7);
    let mut render = PaneSurfaceReceiver::new(render);
    let _ = control.recv().expect("snapshot");
    server.render_now();
    let _ = recv_pane_surface(&mut render, "initial surface");

    let (release, writer, revision) = {
        let runtime = server.app.pane_runtime(pane_id).expect("runtime");
        runtime.test_process_pty_bytes(b"\rAAAA\x1b[?1003h");
        let revision = runtime.read().content_revision().expect("healthy core");
        let (release, writer) =
            runtime.test_contend_during_dirty_collection(b"\rBBBB\x1b[?1003l".to_vec());
        (release, writer, revision)
    };
    let retained = server.try_render_patches(&HashSet::from([pane_id]));
    release.send(()).expect("release waiting writer");
    let writer_took_core = writer.join().expect("writer completed");

    assert!(
        retained,
        "a waiting writer must not invalidate the collected snapshot"
    );
    assert!(
        !writer_took_core,
        "writer must wait for the collection to release the terminal core"
    );
    let patch = recv_pane_surface_patch(&mut render, "snapshot before waiting write");
    assert_eq!(meta_panes(&patch.meta)[0].content_revision, revision);
    assert!(revision.is_stable());
    assert!(meta_panes(&patch.meta)[0].mouse_reporting);
    let surface = server.clients[&ClientId::test_new(7)]
        .render_state
        .last_pane_surface()
        .expect("surface");
    assert!(frame_text(&surface.frame).contains("AAAA"));
    assert!(!frame_text(&surface.frame).contains("BBBB"));

    assert!(server.try_render_patches(&HashSet::from([pane_id])));
    let next = recv_pane_surface_patch(&mut render, "waiting write remains dirty");
    let mut next_revision = revision;
    next_revision.advance();
    assert_eq!(meta_panes(&next.meta)[0].content_revision, next_revision);
    assert!(!meta_panes(&next.meta)[0].mouse_reporting);
    let surface = server.clients[&ClientId::test_new(7)]
        .render_state
        .last_pane_surface()
        .expect("next surface");
    assert!(frame_text(&surface.frame).contains("BBBB"));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn different_size_shells_receive_geometry_specific_patches_from_one_dirty_collection() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (large_control, large_render) = connect_test_shell(&mut server, 7, 80, 23);
    let mut large_render = PaneSurfaceReceiver::new(large_render);
    let (small_control, small_render) = connect_test_shell(&mut server, 8, 68, 17);
    let mut small_render = PaneSurfaceReceiver::new(small_render);
    let _ = large_control.recv().expect("large snapshot");
    let _ = small_control.recv().expect("small snapshot");
    server.render_now();
    let large_initial = recv_pane_surface(&mut large_render, "large initial surface");
    let small_initial = recv_pane_surface(&mut small_render, "small initial surface");
    let initial_size = server.app.test_runtime(pane_id).current_size();
    assert_eq!(
        (large_initial.frame.width(), large_initial.frame.height()),
        (80, 23)
    );
    assert_eq!(
        (small_initial.frame.width(), small_initial.frame.height()),
        (68, 17)
    );
    assert_ne!(
        large_initial.panes[0].content_rect,
        small_initial.panes[0].content_rect
    );

    write_shared_test_pane(&mut server, pane_id, b"\rMIXED");
    assert!(server.try_render_patches(&HashSet::from([pane_id])));

    let large_patch = recv_pane_surface_patch(&mut large_render, "large retained patch");
    let small_patch = recv_pane_surface_patch(&mut small_render, "small retained patch");
    assert_eq!(
        large_patch.base_surface_revision,
        large_initial.surface_revision
    );
    assert_eq!(
        small_patch.base_surface_revision,
        small_initial.surface_revision
    );
    assert!(large_patch.spans.iter().all(|row| {
        row.x
            .saturating_add(u16::try_from(row.cells.len()).unwrap_or(u16::MAX))
            <= large_initial.frame.width()
            && row.y < large_initial.frame.height()
    }));
    assert!(small_patch.spans.iter().all(|row| {
        row.x
            .saturating_add(u16::try_from(row.cells.len()).unwrap_or(u16::MAX))
            <= small_initial.frame.width()
            && row.y < small_initial.frame.height()
    }));
    assert_eq!(large_patch.spans, small_patch.spans);
    assert_ne!(
        meta_panes(&large_patch.meta)[0].content_rect,
        meta_panes(&small_patch.meta)[0].content_rect
    );
    assert!(
        frame_text(
            &server.clients[&ClientId::test_new(7)]
                .render_state
                .last_pane_surface()
                .expect("large retained surface")
                .frame
        )
        .contains("MIXED")
    );
    assert!(
        frame_text(
            &server.clients[&ClientId::test_new(8)]
                .render_state
                .last_pane_surface()
                .expect("small retained surface")
                .frame
        )
        .contains("MIXED")
    );

    write_shared_test_pane(&mut server, pane_id, b"\x1b[?1049hALT");
    assert!(!server.try_render_patches(&HashSet::from([pane_id])));
    server.render_now();
    let large_alt = recv_pane_surface(&mut large_render, "large alternate-screen surface");
    let small_alt = recv_pane_surface(&mut small_render, "small alternate-screen surface");
    assert!(large_alt.panes[0].alternate_screen_active);
    assert!(small_alt.panes[0].alternate_screen_active);
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        (initial_size.0, initial_size.1 + 1)
    );
    assert_eq!(
        large_alt.panes[0].content_rect.width,
        large_initial.panes[0].content_rect.width + 1
    );
    assert_eq!(
        small_alt.panes[0].content_rect.width,
        small_initial.panes[0].content_rect.width + 1
    );

    write_shared_test_pane(&mut server, pane_id, b"\x1b[?1049l");
    assert!(!server.try_render_patches(&HashSet::from([pane_id])));
    server.render_now();
    let large_main = recv_pane_surface(&mut large_render, "large restored main-screen surface");
    let small_main = recv_pane_surface(&mut small_render, "small restored main-screen surface");
    assert!(!large_main.panes[0].alternate_screen_active);
    assert!(!small_main.panes[0].alternate_screen_active);
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        initial_size
    );
    assert_eq!(
        large_main.panes[0].content_rect,
        large_initial.panes[0].content_rect
    );
    assert_eq!(
        small_main.panes[0].content_rect,
        small_initial.panes[0].content_rect
    );

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn retained_patches_only_reach_shells_viewing_the_dirty_workspace() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("divergent-retained");
    let first_pane = first.tree().root();
    let second = shepr_mux::workspace::Workspace::test_new("divergent-second");
    let second_pane = second.tree().root();

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![first, second]);
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"FIRST"),
    );
    server.app.insert_test_runtime(
        second_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"SECOND"),
    );
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let second_workspace_id = server.app.state().ws(1).id();

    let (first_control, first_render) = connect_matching_test_shell(&mut server, 7);
    let mut first_render = PaneSurfaceReceiver::new(first_render);
    let (second_control, second_render) = connect_matching_test_shell(&mut server, 8);
    let mut second_render = PaneSurfaceReceiver::new(second_render);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.place_test_client_on_workspace(ClientId::test_new(8), &second_workspace_id));
    // Both shells are the same size, so the claim moves the controller without
    // changing any recorded geometry.
    let _ = server
        .claim_shell_workspace_geometry(ClientId::test_new(8), client_views::PendingResumes::Defer);
    assert_eq!(
        server.clients.geometry_controller(&second_workspace_id),
        Some(ClientId::test_new(8))
    );
    assert!(
        server.pty_sources_visible_to_any_render_target(&HashSet::from([first_pane, second_pane,]))
    );
    server.render_now();
    let first_surface = recv_pane_surface(&mut first_render, "first baseline");
    let second_surface = recv_pane_surface(&mut second_render, "second baseline");
    assert_eq!(
        (first_surface.frame.width(), first_surface.frame.height()),
        (80, 23)
    );
    assert_eq!(
        (second_surface.frame.width(), second_surface.frame.height()),
        (80, 23)
    );
    assert!(frame_text(&first_surface.frame).contains("FIRST"));
    assert!(frame_text(&second_surface.frame).contains("SECOND"));

    server
        .app
        .test_runtime(first_pane)
        .test_process_pty_bytes(b"\rFIRST_PATCH");
    assert!(server.try_render_patches(&HashSet::from([first_pane])));
    let first_patch = recv_pane_surface_patch(&mut first_render, "first patch");
    assert_eq!(meta_panes(&first_patch.meta).len(), 1);
    assert!(second_render.try_recv().is_err());

    server
        .app
        .test_runtime(second_pane)
        .test_process_pty_bytes(b"\rSECOND_PATCH");
    assert!(server.try_render_patches(&HashSet::from([second_pane])));
    let second_patch = recv_pane_surface_patch(&mut second_render, "second patch");
    assert_eq!(meta_panes(&second_patch.meta).len(), 1);
    assert!(first_render.try_recv().is_err());

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn late_retained_fallback_promotes_its_client_and_commits_no_patch_for_it() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (_first_control, first_render) = connect_matching_test_shell(&mut server, 7);
    let mut first_render = PaneSurfaceReceiver::new(first_render);
    let (_second_control, second_render) = connect_matching_test_shell(&mut server, 8);
    let mut second_render = PaneSurfaceReceiver::new(second_render);
    server.render_now();
    let _ = recv_pane_surface(&mut first_render, "first baseline");
    let _ = recv_pane_surface(&mut second_render, "second baseline");

    // Foreground renders last. Its old hyperlink forces a fallback after the first plan.
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(8)));
    let linked = server
        .clients
        .get_mut(&ClientId::test_new(8))
        .expect("test precondition")
        .render_state
        .last_surface_mut()
        .expect("test precondition");
    linked
        .frame
        .push_hyperlink("https://example.com".into())
        .expect("room in the link table");
    // The first content cell, inside the pane's top-left border corner.
    let first_content_cell = usize::from(linked.frame.width()) + 1;
    linked.frame.cells_mut()[first_content_cell].hyperlink = Some(0);
    let before = [7, 8].map(|id| {
        server.clients[&ClientId::test_new(id)]
            .render_state
            .last_pane_surface()
            .expect("test precondition")
            .clone()
    });

    write_shared_test_pane(&mut server, pane_id, b"\rNEXT\x1b[?1003h");
    let outcome = server.render_patches(&[7.into(), 8.into()], &HashSet::from([pane_id]));
    assert_eq!(outcome.sent, vec![ClientId::test_new(7)]);
    assert_eq!(outcome.promote, vec![ClientId::test_new(8)]);
    assert!(first_render.try_recv().is_ok());
    assert!(second_render.try_recv().is_err());
    assert_ne!(
        server.clients[&ClientId::test_new(7)]
            .render_state
            .last_pane_surface(),
        Some(&before[0])
    );
    assert_eq!(
        server.clients[&ClientId::test_new(8)]
            .render_state
            .last_pane_surface(),
        Some(&before[1])
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn backpressured_shell_does_not_disable_retained_patches_for_responsive_peer() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (responsive_control, responsive_render) = connect_matching_test_shell(&mut server, 7);
    let (slow_control, slow_render) = connect_matching_test_shell(&mut server, 8);
    let mut slow_render = PaneSurfaceReceiver::new(slow_render);
    let _ = responsive_control.recv().expect("responsive snapshot");
    let _ = slow_control.recv().expect("slow snapshot");
    server.render_now();
    let _ = responsive_render
        .recv()
        .expect("responsive initial surface");
    let _ = slow_render.recv("slow initial surface");

    let sources = HashSet::from([pane_id]);
    write_shared_test_pane(&mut server, pane_id, b"\rONE");
    assert!(server.try_render_patches(&sources));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive first patch")),
        ServerMessage::SurfaceUpdate(_)
    ));

    let slow_baseline = server.clients[&ClientId::test_new(8)]
        .render_state
        .last_pane_surface()
        .expect("test precondition")
        .clone();
    write_shared_test_pane(&mut server, pane_id, b"\rTWO\x1b[?1003h");
    assert!(server.try_render_patches(&sources));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive second patch")),
        ServerMessage::SurfaceUpdate(_)
    ));
    assert!(
        server.clients[&ClientId::test_new(8)]
            .render_state
            .surface_debt()
    );
    assert_eq!(
        server.clients[&ClientId::test_new(8)]
            .render_state
            .last_pane_surface(),
        Some(&slow_baseline),
        "queue-full must not advance cells, metadata, cursor, or revision"
    );

    write_shared_test_pane(&mut server, pane_id, b"\rTHREE");
    assert!(server.try_render_patches(&sources));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive third patch")),
        ServerMessage::SurfaceUpdate(_)
    ));

    assert!(matches!(
        slow_render.recv("slow queued first patch"),
        DecodedServerMessage::PaneSurfacePatch(_)
    ));
    server.render_now();
    assert!(matches!(
        slow_render.recv("slow full recovery surface"),
        DecodedServerMessage::Wire(ServerMessage::PaneSurface(_))
            | DecodedServerMessage::PaneSurfacePatch(_)
    ));

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn full_render_backpressure_does_not_disable_responsive_peer_patches() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (responsive_control, responsive_render) = connect_matching_test_shell(&mut server, 7);
    let (slow_control, slow_render) = connect_matching_test_shell(&mut server, 8);
    let _ = responsive_control.recv().expect("responsive snapshot");
    let _ = slow_control.recv().expect("slow snapshot");
    server.render_now();
    let _ = responsive_render
        .recv()
        .expect("responsive initial surface");
    // Keep the slow client's initial surface queued, then force another full
    // replacement for both clients.
    server
        .clients
        .get_mut(&ClientId::test_new(7))
        .expect("test precondition")
        .request_repaint();
    server
        .clients
        .get_mut(&ClientId::test_new(8))
        .expect("test precondition")
        .request_repaint();
    server.render_now();
    let _ = responsive_render
        .recv()
        .expect("responsive full replacement");
    assert!(
        server.clients[&ClientId::test_new(8)]
            .render_state
            .surface_debt()
    );

    write_shared_test_pane(&mut server, pane_id, b"\rPATCH");
    assert!(server.try_render_patches(&HashSet::from([pane_id])));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive retained patch")),
        ServerMessage::SurfaceUpdate(_)
    ));

    let _ = slow_render.recv().expect("slow queued initial surface");
    server.render_now();
    assert!(matches!(
        read_server_message(slow_render.recv().expect("slow full recovery surface")),
        ServerMessage::PaneSurface(_)
    ));

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_surface_larger_than_one_frame_crosses_in_parts() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("oversized");
    let pane_id = workspace.tree().root();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    // One displayed grapheme large enough that the encoded surface is past
    // one frame: the surface is split, not refused.
    let mut screen = String::with_capacity(2_200_001);
    screen.push('x');
    for _ in 0..1_100_000 {
        screen.push('\u{0301}');
    }
    server.app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, screen.as_bytes()),
    );
    let (control, render_rx) = connect_test_shell(&mut server, 91, 80, 24);

    server.render_now();
    let bytes = render_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("the large surface was queued");
    assert!(bytes.len() > MAX_FRAME_SIZE);
    let first_prefix = u32::from_le_bytes(bytes[..4].try_into().expect("test precondition"));
    assert_ne!(first_prefix & (1 << 31), 0, "the first frame is continued");
    let ServerMessage::PaneSurface(surface) = read_server_message(bytes) else {
        panic!("expected a full pane surface");
    };
    assert!(
        surface
            .frame
            .cells()
            .iter()
            .any(|cell| cell.symbol.len() > MAX_FRAME_SIZE),
        "the large grapheme arrives whole"
    );
    assert!(
        std::iter::from_fn(|| control.recv_timeout(Duration::from_millis(200)).ok())
            .map(read_server_message)
            .all(|message| !matches!(message, ServerMessage::ClientShellError { .. })),
        "a split surface is not reported as too large"
    );
    assert!(
        !server
            .clients
            .get(&ClientId::test_new(91))
            .expect("client stays connected")
            .oversized_surface_reported
    );
    assert!(
        shepr_protocol::NoticeKind::LimitExceeded(shepr_protocol::LimitExceeded::new(
            shepr_protocol::Limit::new(
                shepr_protocol::LimitKind::SurfaceMessageBytes,
                shepr_protocol::MAX_MESSAGE_SIZE,
            ),
            3_000_000,
        ))
        .to_string()
        .contains("too large")
    );
    shutdown_test_runtimes(&mut server);
}
