use super::*;

#[tokio::test]
async fn client_shell_receives_metadata_then_shell_free_pane_surface() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("shell-only-label");
    let pane_id = workspace.tree().focused();

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(
            80,
            23,
            b"\x1b[?1003h\x1b[?1006h\x1b[?1016hCLIENT_SHELL_LIVE",
        ),
    );
    server.app.test_state_mut().seed_bookmark_index(Some(0));

    let (writer, control_rx, render_rx) = test_client_writer();
    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id: ClientId::test_new(7),
            geometry: shepr_core::geometry::HostGeometry::new(
                shepr_core::geometry::GridSize::clamped(80, 23),
                shepr_core::geometry::HostCell::from_host(10, 20, true)
            ),
            mouse_capture: false,
            surface_active: true,
            outbox: writer,
        })
    );
    let snapshot = client_shell_snapshot(&control_rx);
    assert_eq!(snapshot.workspaces.len(), 1);
    assert_eq!(snapshot.workspaces[0].label, "shell-only-label");
    server.render_now();
    let initial_surface = match read_server_message(render_rx.recv().expect("pane surface")) {
        ServerMessage::PaneSurface(surface) => {
            assert_eq!((surface.frame.width(), surface.frame.height()), (80, 23));
            let text = frame_text(&surface.frame);
            assert!(text.contains("CLIENT_SHELL_LIVE"), "surface: {text:?}");
            assert!(!text.contains("shell-only-label"), "surface: {text:?}");
            assert_eq!(surface.panes.len(), 1);
            assert_eq!(surface.panes[0].rect.x, 0);
            assert_eq!(surface.panes[0].rect.y, 0);
            assert!(surface.panes[0].pixel_mouse.requested());
            // The surface carries the pane's own pixel mouse, not a product of
            // this viewer's cell.
            assert_eq!(
                surface.panes[0].pixel_mouse,
                server
                    .app
                    .pane_runtime(pane_id)
                    .expect("pane runtime")
                    .read()
                    .pixel_mouse()
            );
            surface
        }
        other => panic!("expected pane surface, got {other:?}"),
    };

    let baseline = server.clients[&ClientId::test_new(7)]
        .render_state
        .last_pane_surface()
        .expect("initial baseline");
    let cells_ptr = baseline.frame.cells().as_ptr();
    let untouched_symbol_ptr = baseline
        .frame
        .cells()
        .last()
        .expect("test precondition")
        .symbol
        .as_ptr();

    server
        .app
        .pane_runtime(pane_id)
        .expect("pane runtime")
        .test_process_pty_bytes(b"\rPATCHED");
    let sources = std::collections::HashSet::from([pane_id]);
    assert!(server.try_render_patches(&sources));
    match read_server_message(render_rx.recv().expect("pane surface patch")) {
        ServerMessage::SurfaceUpdate(patch) => {
            assert_eq!(
                patch.base_surface_revision,
                initial_surface.surface_revision
            );
            assert_eq!(
                Some(patch.surface_revision),
                initial_surface.surface_revision.checked_next()
            );
            assert_eq!(meta_panes(&patch.meta).len(), 1);
            assert!(!patch.spans.is_empty());
            assert!(
                patch
                    .spans
                    .iter()
                    .flat_map(|row| &row.cells)
                    .any(|cell| cell.symbol == "P")
            );
        }
        other => panic!("expected pane surface patch, got {other:?}"),
    }
    let patched = server.clients[&ClientId::test_new(7)]
        .render_state
        .last_pane_surface()
        .expect("test precondition");
    assert_eq!(
        (
            patched.frame.cells().as_ptr(),
            patched
                .frame
                .cells()
                .last()
                .expect("test precondition")
                .symbol
                .as_ptr()
        ),
        (cells_ptr, untouched_symbol_ptr),
        "a text patch must preserve the frame and unchanged cell storage"
    );
    server
        .app
        .pane_runtime(pane_id)
        .expect("pane runtime")
        .test_process_pty_bytes(b"\x1b[?1003l\x1b[?1006l\x1b[?1016l");
    assert!(server.try_render_patches(&sources));
    match read_server_message(render_rx.recv().expect("metadata-only pane surface patch")) {
        ServerMessage::SurfaceUpdate(patch) => {
            assert!(patch.spans.is_empty(), "mouse modes only change metadata");
            let panes = meta_panes(&patch.meta);
            assert_eq!(panes.len(), 1);
            assert!(!panes[0].mouse_reporting);
            assert!(!panes[0].pixel_mouse.requested());
        }
        other => panic!("expected metadata-only pane surface patch, got {other:?}"),
    }
    let retained = server.clients[&ClientId::test_new(7)]
        .render_state
        .last_pane_surface()
        .expect("committed retained surface");
    assert_eq!(retained.frame.cells().as_ptr(), cells_ptr);
    assert_eq!(
        retained
            .frame
            .cells()
            .last()
            .expect("test precondition")
            .symbol
            .as_ptr(),
        untouched_symbol_ptr,
        "retained updates must not copy unchanged screen cells"
    );
    let retained = retained.clone();
    server
        .clients
        .get_mut(&ClientId::test_new(7))
        .expect("test precondition")
        .render_state
        .request_repaint();
    server.render_now();
    let full = match read_server_message(render_rx.recv().expect("full comparison surface")) {
        ServerMessage::PaneSurface(surface) => surface,
        other => panic!("expected full comparison surface, got {other:?}"),
    };
    assert!(full.surface_revision > retained.surface_revision);
    assert_eq!(retained.frame, full.frame);
    assert_eq!(retained.panes, full.panes);
    assert_eq!(retained.splits, full.splits);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn unchanged_shell_render_reuses_session_and_sends_no_snapshot() {
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    let _ = client_shell_snapshot(&control);
    server.render_now();
    assert!(control.try_recv().is_err());
    let built_at = server
        .shell_session_cache
        .as_ref()
        .expect("session cache")
        .built_at;
    let generation = server.shell_session_generation;
    let unchanged = |server: &HeadlessServer| {
        // `built_at` only moves when the session (and its /proc probes) is
        // rebuilt; an unmoved generation means no client was projected again.
        assert_eq!(
            server
                .shell_session_cache
                .as_ref()
                .map(|cache| cache.built_at),
            Some(built_at)
        );
        assert_eq!(server.shell_session_generation, generation);
        assert_eq!(
            server.clients[&ClientId::test_new(7)]
                .shell_state()
                .session_generation,
            generation
        );
    };

    server.render_now();
    unchanged(&server);
    assert!(control.try_recv().is_err());

    let root_pane = server.app.state().ws(0).tree().root();
    let pane_id = server
        .app
        .state()
        .pane(root_pane)
        .expect("pane id")
        .public_id();
    // Scrolling a pane already at the bottom changes nothing, and the claim it
    // makes records no new geometry, so it asks for no render.
    assert!(!command_through_server(
        &mut server,
        7,
        EndpointCommand::PaneScroll(shepr_protocol::command::PaneScrollParams {
            pane_id,
            offset_from_bottom: 0,
        }),
    ));
    server.render_now();
    unchanged(&server);
    assert!(control.try_recv().is_err());
    shutdown_test_runtimes(&mut server);
}

/// Renders and returns the one replacement the change must produce.
fn next_projection(
    server: &mut HeadlessServer,
    control: &std::sync::mpsc::Receiver<Vec<u8>>,
    previous: &mut shepr_protocol::ProjectionRevision,
) -> Box<shepr_protocol::ClientShellSnapshot> {
    server.render_now();
    let snapshot = client_shell_snapshot(control);
    assert!(snapshot.revision > *previous);
    *previous = snapshot.revision;
    snapshot
}

#[tokio::test]
async fn workspace_rename_reprojects_without_copying_connection_config() {
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    let first = client_shell_snapshot(&control);
    server.render_now();
    assert!(control.try_recv().is_err());

    let outcome = server.app.handle_endpoint_command_with_render(
        EndpointCommand::WorkspaceRename(shepr_protocol::command::WorkspaceRenameParams {
            workspace_id: first.workspaces[0].workspace_id,
            label: Some("renamed".into()),
        }),
        &crate::app::EndpointContext::without_geometry(),
    );
    assert!(outcome.view_changed());
    server.render_now();
    let renamed = client_shell_snapshot(&control);
    assert_eq!(renamed.workspaces[0].label, "renamed");
    assert!(renamed.revision > first.revision);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn cwd_report_and_slow_probe_refresh_shell_projection() {
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    let pane_id = server.app.state().ws(0).tree().root();
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    let _ = client_shell_snapshot(&control);
    server.render_now();
    assert!(control.try_recv().is_err());

    let scratch = ScratchDir::new("headless-cwd");
    let cwd = scratch.path().to_path_buf();
    let report = server.app.from_pane_runtime(
        pane_id,
        shepr_mux::events::RuntimeEvent::TerminalCwdReported {
            cwd: shepr_mux::UsableCwd::new(cwd.clone()).expect("socket directory is usable"),
        },
    );
    server.app.handle_internal_event(report);
    server.render_now();
    let reported = client_shell_snapshot(&control);
    assert_eq!(
        reported.panes[0]
            .cwd
            .as_ref()
            .map(shepr_protocol::RemotePath::as_path),
        Some(cwd.as_path())
    );

    let age_cache = |server: &mut HeadlessServer| {
        if let Some(cache) = server.shell_session_cache.as_mut() {
            cache.built_at -= super::render::SHELL_CWD_REFRESH_INTERVAL * 2;
        }
    };

    // Nothing changed: the timer re-reads the sources but neither moves the
    // generation nor asks for a render, and the next check is a full interval out.
    age_cache(&mut server);
    assert!(server.shell_cwd_refresh_due(Instant::now()));
    let generation = server.shell_session_generation;
    assert!(!server.refresh_shell_projection_sources());
    assert_eq!(server.shell_session_generation, generation);
    assert!(!server.shell_cwd_refresh_due(Instant::now()));
    server.render_now();
    assert!(control.try_recv().is_err());

    // A change no event reports (standing in for a shell's /proc cwd) is
    // found by the timer and reaches the client with the next render.
    server
        .app
        .test_state_mut()
        .ws_mut(0)
        .set_custom_name(Some("silent".into()));
    server.render_now();
    assert!(control.try_recv().is_err(), "no event reported the change");
    age_cache(&mut server);
    assert!(server.refresh_shell_projection_sources());
    assert_eq!(
        server
            .shell_session_cache
            .as_ref()
            .expect("session cache")
            .timer_projections
            .len(),
        1,
        "the changed client's projection is retained for the render pass"
    );
    server.render_now();
    assert!(
        server
            .shell_session_cache
            .as_ref()
            .expect("session cache")
            .timer_projections
            .is_empty()
    );
    assert_eq!(
        client_shell_snapshot(&control).workspaces[0].label,
        "silent"
    );

    // The timer only runs while a shell client is connected.
    assert!(server.test_handle_server_event(ServerEvent::Disconnected {
        client_id: ClientId::test_new(7),
    }));
    age_cache(&mut server);
    assert_eq!(server.shell_cwd_refresh_deadline(), None);
    shutdown_test_runtimes(&mut server);
}

/// Each change goes through the path production uses (API request, internal
/// event, title sync, metadata expiry) with no manual invalidation, and each
/// must reach the client as its own fresh projection.
#[tokio::test]
async fn each_kind_of_change_sends_a_new_projection_through_its_real_path() {
    use shepr_protocol::command::{
        PaneInputSetParams, PaneRenameParams, PaneRightClickTarget, WorkspaceRenameParams,
    };

    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    let pane_id = server.app.state().ws(0).tree().root();
    let public_pane_id = server
        .app
        .state()
        .pane(pane_id)
        .expect("pane exists")
        .public_id();
    let pane = |snapshot: &shepr_protocol::ClientShellSnapshot| {
        snapshot
            .panes
            .iter()
            .find(|pane| pane.pane_id == public_pane_id)
            .cloned()
            .expect("projected pane")
    };
    let workspace_id = server.app.state().ws(0).id();
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    let mut previous = client_shell_snapshot(&control).revision;
    server.render_now();
    assert!(control.try_recv().is_err());

    assert!(command_through_server(
        &mut server,
        7,
        EndpointCommand::PaneRename(PaneRenameParams {
            pane_id: public_pane_id,
            label: Some("manual".into()),
        }),
    ));
    let renamed = next_projection(&mut server, &control, &mut previous);
    assert_eq!(pane(&renamed).label.as_deref(), Some("manual"));

    assert!(command_through_server(
        &mut server,
        7,
        EndpointCommand::WorkspaceRename(WorkspaceRenameParams {
            workspace_id,
            label: Some("named-workspace".into()),
        }),
    ));
    let workspace = next_projection(&mut server, &control, &mut previous);
    assert_eq!(workspace.workspaces[0].label, "named-workspace");

    assert!(command_through_server(
        &mut server,
        7,
        EndpointCommand::PaneInputSet(PaneInputSetParams {
            pane_id: public_pane_id,
            right_click: PaneRightClickTarget::Pane,
        }),
    ));
    assert!(pane(&next_projection(&mut server, &control, &mut previous)).right_click_passthrough);

    let working = server.app.from_pane_runtime(
        pane_id,
        shepr_mux::events::RuntimeEvent::StateChanged {
            agent: Some(shepr_agent::Agent::Pi),
            detection: shepr_detect::Detection::new(shepr_agent::AgentState::Working, false),
            process_exited: false,
            observed_at: Instant::now(),
        },
    );
    assert!(server.handle_internal_event_with_forwarding(working));
    let agent = next_projection(&mut server, &control, &mut previous);
    assert!(
        agent
            .agents
            .iter()
            .any(|entry| entry.state_change_seq != shepr_protocol::StateChangeSeq::NEVER)
    );

    server
        .app
        .pane_runtime(pane_id)
        .expect("pane runtime")
        .test_process_pty_bytes(b"\x1b]0;compiling\x07");
    let title_sync = server.sync_terminal_title_sources(&HashSet::from([pane_id]));
    assert!(title_sync.sidebar_changed);
    let titled = next_projection(&mut server, &control, &mut previous);
    assert_eq!(
        titled.agents[0].terminal_title_stripped.as_deref(),
        Some("compiling")
    );

    let workspace_state_id = server.app.state().ws(0).id();
    let cwd = server.app.state().ws(0).identity_cwd().to_path_buf();
    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::GitStatusRefreshed {
            outcome: shepr_git::RefreshOutcome {
                statuses: vec![shepr_mux::git::WorkspaceGitStatus {
                    owner: workspace_state_id,
                    status: shepr_git::GitStatus {
                        cwd: cwd.clone(),
                        key: shepr_git::GitStatusKey::Checkout(cwd),
                        label: "focus-reporting".into(),
                        branch: shepr_git::GitBranch::Named("feature".into()),
                        ahead_behind: None,
                    },
                }],
                new_read_errors: Vec::new(),
            },
        })
    );
    assert_eq!(
        next_projection(&mut server, &control, &mut previous).workspaces[0]
            .branch
            .as_deref(),
        Some("feature")
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_reconnecting_shell_is_seeded_again_and_gets_later_changes() {
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    let _ = client_shell_snapshot(&control);
    server.render_now();
    assert!(server.test_handle_server_event(ServerEvent::Disconnected {
        client_id: ClientId::test_new(7),
    }));

    // The shared cache outlives the connection; the new one is seeded fresh
    // and still receives subsequent changes.
    let (control, _render) = connect_matching_test_shell(&mut server, 8);
    let seed = client_shell_snapshot(&control);
    let mut previous = seed.revision;
    server.render_now();
    assert!(control.try_recv().is_err());
    assert!(command_through_server(
        &mut server,
        8,
        EndpointCommand::WorkspaceRename(shepr_protocol::command::WorkspaceRenameParams {
            workspace_id: seed.workspaces[0].workspace_id,
            label: Some("after-reconnect".into()),
        }),
    ));
    assert_eq!(
        next_projection(&mut server, &control, &mut previous).workspaces[0].label,
        "after-reconnect"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn every_client_of_a_partly_restored_boot_gets_the_notice_in_its_seed() {
    let mut server = test_headless_server();
    let notice = shepr_protocol::SessionRestoreNotice {
        loss: shepr_protocol::SessionRestoreLoss::Workspaces {
            dropped: std::num::NonZeroUsize::MIN,
            panes_pruned: true,
        },
        backup_dir: "/state/shepr/session-backups".into(),
    };
    server.app.test_set_restore_notice(Some(notice.clone()));

    for client_id in [7, 8] {
        let (control, _render) = connect_matching_test_shell(&mut server, client_id);
        // The notice is part of the snapshot, so it is keyed to this boot and
        // repeated on every projection rather than sent once beside it.
        let seed = client_shell_snapshot(&control);
        assert_eq!(
            seed.restore_notice,
            Some(notice.clone()),
            "client {client_id}"
        );
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_fully_restored_boot_sends_no_restore_notice() {
    let mut server = test_headless_server();
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    let seed = client_shell_snapshot(&control);
    assert_eq!(seed.restore_notice, None);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_new_shell_seed_uses_the_shared_session_cache_for_cwd() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("cached-cwd-first");
    let first_pane = first.tree().root();
    let second = shepr_mux::workspace::Workspace::test_new("cached-cwd-second");
    let second_workspace_id = second.id();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![first, second]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));

    let older_cwd = shepr_test_support::ScratchDir::new("seed-cwd-older");
    let newer_cwd = shepr_test_support::ScratchDir::new("seed-cwd-newer");
    let older_cwd_text = older_cwd.path().to_str().expect("older cwd utf8");
    let newer_cwd_text = newer_cwd.path().to_str().expect("newer cwd utf8");
    server
        .app
        .test_state_mut()
        .terminal_mut(first_pane)
        .set_cwd(
            shepr_mux::UsableCwd::new(older_cwd.path().to_path_buf()).expect("older cwd is usable"),
        );
    let public_pane_id = server
        .app
        .state()
        .pane(first_pane)
        .expect("pane exists")
        .public_id();

    let (first_control, _first_render) = connect_matching_test_shell(&mut server, 7);
    let first_seed = client_shell_snapshot(&first_control);
    assert_eq!(
        first_seed
            .panes
            .iter()
            .find(|pane| pane.pane_id == public_pane_id)
            .expect("first pane snapshot")
            .cwd
            .as_ref()
            .map(shepr_protocol::RemotePath::as_path),
        Some(std::path::Path::new(older_cwd_text))
    );
    server.render_now();
    assert!(first_control.try_recv().is_err());

    let cache_revision = server
        .shell_session_cache
        .as_ref()
        .expect("session cache")
        .revision;
    assert_eq!(
        cache_revision,
        server.app.state().shell_projection_revision()
    );

    // Model an unreported cwd source moving forward without an application
    // revision, like the foreground cwd read from `/proc` between timer runs.
    // The live session now reads a newer cwd while the shared cache still has
    // the older value.
    server
        .app
        .test_state_mut()
        .terminal_mut(first_pane)
        .set_cwd(
            shepr_mux::UsableCwd::new(newer_cwd.path().to_path_buf()).expect("newer cwd is usable"),
        );
    assert_eq!(
        server.app.projection_input().panes[0]
            .cwd
            .as_ref()
            .map(shepr_protocol::RemotePath::as_path),
        Some(std::path::Path::new(newer_cwd_text))
    );
    assert_eq!(
        server
            .shell_session_cache
            .as_ref()
            .expect("session cache")
            .session
            .panes[0]
            .cwd
            .as_ref()
            .map(shepr_protocol::RemotePath::as_path),
        Some(std::path::Path::new(older_cwd_text))
    );

    let (control, _render) = connect_matching_test_shell(&mut server, 8);
    let seed = client_shell_snapshot(&control);
    assert_eq!(
        seed.panes
            .iter()
            .find(|pane| pane.pane_id == public_pane_id)
            .expect("seed pane snapshot")
            .cwd
            .as_ref()
            .map(shepr_protocol::RemotePath::as_path),
        Some(std::path::Path::new(older_cwd_text))
    );

    assert!(server.place_test_client_on_workspace(ClientId::test_new(8), &second_workspace_id));
    server.render_now();
    let location_projection = client_shell_snapshot(&control);
    assert_eq!(
        location_projection
            .panes
            .iter()
            .find(|pane| pane.pane_id == public_pane_id)
            .expect("projected pane snapshot")
            .cwd
            .as_ref()
            .map(shepr_protocol::RemotePath::as_path),
        Some(std::path::Path::new(older_cwd_text)),
        "a location projection must not move cwd backwards from its seed"
    );

    assert!(server.refresh_shell_projection_sources());
    let mut previous = location_projection.revision;
    let refreshed = next_projection(&mut server, &control, &mut previous);
    assert_eq!(
        refreshed
            .panes
            .iter()
            .find(|pane| pane.pane_id == public_pane_id)
            .expect("refreshed pane snapshot")
            .cwd
            .as_ref()
            .map(shepr_protocol::RemotePath::as_path),
        Some(std::path::Path::new(newer_cwd_text))
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn create_default_workspace_invalidates_the_shell_projection() {
    let mut server = test_headless_server();
    let revision = server.app.state().shell_projection_revision();
    assert!(server.app.state().workspaces().is_empty());
    let geometry = server.app.headless_spawn_geometry();
    assert_eq!(
        server.app.create_default_workspace(geometry),
        crate::app::DefaultWorkspace::Created
    );
    assert_ne!(server.app.state().shell_projection_revision(), revision);
    shutdown_test_runtimes(&mut server);
}

#[test]
fn unchanged_git_refresh_does_not_request_headless_render() {
    let mut server = test_headless_server();
    server.app.test_mark_git_refresh_in_flight();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("one");
    let workspace_id = workspace.id();
    let cwd = workspace.identity_cwd().to_path_buf();
    let status = shepr_git::GitStatus {
        cwd: cwd.clone(),
        key: shepr_git::GitStatusKey::Outside(cwd.clone()),
        label: "cached".into(),
        branch: shepr_git::GitBranch::OutsideRepository,
        ahead_behind: None,
    };
    workspace.apply_git_status(status.clone(), Some(&cwd));
    server.app.test_state_mut().test_push_workspace(workspace);

    let changed = server.handle_internal_event_with_forwarding(AppEvent::GitStatusRefreshed {
        outcome: shepr_git::RefreshOutcome {
            statuses: vec![shepr_mux::git::WorkspaceGitStatus {
                owner: workspace_id,
                status,
            }],
            new_read_errors: Vec::new(),
        },
    });

    assert!(!changed);
    assert!(!server.app.git_refresh_in_flight());
}

#[test]
fn changed_git_refresh_requests_headless_render() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("one");
    let workspace_id = workspace.id();
    let cwd = workspace.identity_cwd().to_path_buf();
    server.app.test_state_mut().test_push_workspace(workspace);

    let changed = server.handle_internal_event_with_forwarding(AppEvent::GitStatusRefreshed {
        outcome: shepr_git::RefreshOutcome {
            statuses: vec![shepr_mux::git::WorkspaceGitStatus {
                owner: workspace_id,
                status: shepr_git::GitStatus {
                    cwd: cwd.clone(),
                    key: shepr_git::GitStatusKey::Checkout(cwd),
                    label: "one".into(),
                    branch: shepr_git::GitBranch::Named("changed".into()),
                    ahead_behind: None,
                },
            }],
            new_read_errors: Vec::new(),
        },
    });

    assert!(changed);
}

#[tokio::test]
async fn unchanged_internal_events_leave_projection_and_sources_clean() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("event-effects");
    let pane_id = workspace.tree().root();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    let cwd = server
        .app
        .state()
        .terminal(pane_id)
        .expect("terminal")
        .cwd()
        .to_path_buf();
    // Every event comes from the pane's live runtime, so an unchanged result
    // below is the reducer finding nothing new, not admission dropping it.
    server.app.insert_idle_test_runtime(pane_id);
    let from_runtime = |server: &HeadlessServer, event: shepr_mux::events::RuntimeEvent| {
        server.app.from_pane_runtime(pane_id, event)
    };
    let working = |server: &HeadlessServer| {
        from_runtime(
            server,
            shepr_mux::events::RuntimeEvent::StateChanged {
                agent: Some(shepr_agent::Agent::Codex),
                detection: shepr_detect::Detection::new(shepr_agent::AgentState::Working, false),
                process_exited: false,
                observed_at: server.app.clock().now,
            },
        )
    };
    server.immediate_pty_sources_dirty = false;
    server.host_input_modes_dirty = false;
    let before = server.app.state().shell_projection_revision();
    let same_cwd = from_runtime(
        &server,
        shepr_mux::events::RuntimeEvent::TerminalCwdReported {
            cwd: shepr_mux::UsableCwd::new(cwd).expect("absolute cwd"),
        },
    );
    assert!(!server.handle_internal_event_with_forwarding(same_cwd));
    assert_eq!(server.app.state().shell_projection_revision(), before);
    assert!(!server.immediate_pty_sources_dirty);
    assert!(!server.host_input_modes_dirty);

    // The same events do invalidate once they change what a client sees.
    let moved = shepr_test_support::ScratchDir::new("event-effects-cwd");
    let moved_cwd = from_runtime(
        &server,
        shepr_mux::events::RuntimeEvent::TerminalCwdReported {
            cwd: shepr_mux::UsableCwd::new(moved.path().to_path_buf()).expect("absolute cwd"),
        },
    );
    assert!(server.handle_internal_event_with_forwarding(moved_cwd));
    assert_ne!(server.app.state().shell_projection_revision(), before);
    let before = server.app.state().shell_projection_revision();
    let event = working(&server);
    assert!(server.handle_internal_event_with_forwarding(event));
    assert_ne!(server.app.state().shell_projection_revision(), before);
    let before = server.app.state().shell_projection_revision();
    let event = working(&server);
    assert!(!server.handle_internal_event_with_forwarding(event));
    assert_eq!(server.app.state().shell_projection_revision(), before);
    assert!(!server.immediate_pty_sources_dirty);
    assert!(!server.host_input_modes_dirty);
}

#[tokio::test]
async fn missing_pane_exit_has_no_invalidation() {
    let mut server = test_headless_server();
    server.immediate_pty_sources_dirty = false;
    server.host_input_modes_dirty = false;
    let before = server.app.state().shell_projection_revision();
    // A late exit from the runtime of a pane already gone from the layout: the
    // runtime went with the pane, so admission finds no producer for it.
    let pane_id = shepr_core::layout::PaneId::alloc();
    assert!(
        !server.handle_internal_event_with_forwarding(
            shepr_mux::events::RuntimeEvent::PaneDied {
                ending: shepr_mux::pane::PaneEnding::new(shepr_mux::pane::PaneEndReason::Exited),
                ended_at: std::time::Instant::now(),
            }
            .enveloped(pane_id, shepr_mux::events::RuntimeGeneration::alloc())
        )
    );
    assert_eq!(server.app.state().shell_projection_revision(), before);
    assert!(!server.immediate_pty_sources_dirty);
    assert!(!server.host_input_modes_dirty);
}
