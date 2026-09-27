use super::*;
use bytes::Bytes;
use shepr_api::schema::{
    ErrorResponse, PaneMoveDestination, PaneMoveParams, PaneMoveResult, ResponseResult,
};

fn pane_move_server() -> HeadlessServer {
    let mut server = test_headless_server();
    let mut first = crate::workspace::Workspace::test_new("first");
    first.test_add_tab(Some("remaining"));
    first.switch_tab(0);
    server.app.state.workspaces = vec![first, crate::workspace::Workspace::test_new("second")];
    server.app.state.ensure_test_terminals();
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    server
}

fn public_move(
    server: &mut HeadlessServer,
    params: PaneMoveParams,
) -> Result<PaneMoveResult, ErrorResponse> {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(shepr_api::ApiRequestMessage {
        request: shepr_api::schema::Request {
            id: "move-pane".into(),
            method: shepr_api::schema::Method::PaneMove(params),
        },
        respond_to,
    });
    let response = response_rx
        .recv()
        .unwrap_or_else(|_| panic!("pane move response"));
    match response {
        Ok(ResponseResult::PaneMove { move_result }) => Ok(move_result),
        Ok(other) => panic!("expected pane move response, got {other:?}"),
        Err(error) => Err(ErrorResponse {
            id: "move-pane".into(),
            error: error.into_body(),
        }),
    }
}

#[tokio::test]
async fn public_pane_move_focus_follows_the_moved_pane() {
    let mut server = pane_move_server();
    let source = server.app.state.workspaces[0].tabs[0].root_pane;
    let terminal_id = server.app.state.workspaces[0].tabs[0]
        .terminal_id(source)
        .expect("test precondition")
        .clone();
    let (runtime, mut input_rx) = crate::pane::PaneRuntime::test_with_channel(80, 24);
    server.app.terminal_runtimes.insert(terminal_id, runtime);
    let source_id = server
        .app
        .public_pane_id(0, source)
        .expect("test precondition");
    let destination_id = server.app.public_workspace_id(1);
    let (control_rx, render_rx) = connect_test_shell(&mut server, 9, 80, 23);
    let mut render_rx = PaneSurfaceReceiver::new(render_rx);
    let initial = client_shell_snapshot(&control_rx);

    let moved = public_move(
        &mut server,
        PaneMoveParams {
            pane_id: source_id,
            destination: PaneMoveDestination::NewTab {
                workspace_id: Some(destination_id.clone()),
                label: None,
            },
            focus: true,
        },
    )
    .expect("test precondition");
    assert!(moved.changed);
    assert_eq!(server.app.state.active_index(), Some(1));
    assert!(moved.closed_tab_id.is_some());
    let location = server.clients[&9]
        .shell_state()
        .and_then(|shell| shell.location.as_ref())
        .expect("test precondition");
    assert_eq!(
        location.focused_workspace_id.as_deref(),
        Some(destination_id.as_str()),
        "successful public pane move with focus must move the attached client"
    );
    assert_eq!(
        location.focused_tab_id().map(ToString::to_string),
        Some(moved.pane.tab_id.clone())
    );

    server.render_and_stream();
    let snapshot = client_shell_snapshot(&control_rx);
    assert!(snapshot.revision > initial.revision);
    assert_eq!(
        snapshot.focused_workspace_id.as_deref(),
        Some(destination_id.as_str())
    );
    assert_eq!(
        snapshot.focused_tab_id.as_deref(),
        Some(moved.pane.tab_id.as_str())
    );
    assert_eq!(
        snapshot.focused_pane_id.as_deref(),
        Some(moved.pane.pane_id.as_str())
    );
    let surface = recv_pane_surface(&mut render_rx, "moved pane surface");
    assert_eq!(surface.projection_revision, snapshot.revision);

    server.handle_server_event(ServerEvent::ClientShellPaneInput {
        client_id: ClientId::test_new(9),
        pane_id: moved.pane.pane_id.into(),
        events: vec![shepr_protocol::ClientPaneInputEvent::TextCommit("x".into())],
    });
    assert_eq!(
        input_rx.try_recv().expect("input reaches moved terminal"),
        Bytes::from_static(b"x")
    );
    assert_eq!(server.app.state.active_index(), Some(1));
    assert_eq!(
        server.shell_tab_id_for_client(ClientId::test_new(9)),
        Some(moved.pane.tab_id)
    );
    for workspace in &server.app.state.workspaces {
        workspace.assert_invariants_for_test();
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_pane_move_focus_handles_source_removal_and_unchanged_server_target() {
    // Both moves leave the server's numeric workspace/tab coordinates unchanged.
    // One creates a workspace; the other moves into an already-focused split tab.
    for new_workspace in [true, false] {
        let mut server = pane_move_server();
        let source = server.app.state.workspaces[0].tabs[0].root_pane;
        let source_id = server
            .app
            .public_pane_id(0, source)
            .expect("test precondition");
        let first_tab = server.app.public_tab_id(0, 0).expect("test precondition");
        let second_tab = server.app.public_tab_id(1, 0).expect("test precondition");
        let destination = if new_workspace {
            server.app.state.workspaces.remove(1);
            server.app.state.workspaces[0].tabs.remove(1);
            PaneMoveDestination::NewWorkspace {
                label: None,
                tab_label: None,
            }
        } else {
            server.app.state.switch_workspace_tab(1, 0);
            PaneMoveDestination::Tab {
                tab_id: second_tab,
                target_pane_id: None,
                split: shepr_api::schema::SplitDirection::Right,
                ratio: None,
            }
        };
        let target_before = server.default_shell_target();
        let (_first_control, _first_render) = connect_test_shell(&mut server, 9, 80, 23);
        let (_second_control, _second_render) = connect_test_shell(&mut server, 10, 80, 23);
        assert!(server.focus_shell_client_on_tab(ClientId::test_new(9), &first_tab));
        assert!(server.focus_shell_client_on_tab(ClientId::test_new(10), &first_tab));
        let moved = public_move(
            &mut server,
            PaneMoveParams {
                pane_id: source_id,
                destination,
                focus: true,
            },
        )
        .expect("test precondition");
        assert!(moved.changed);
        if new_workspace {
            assert_eq!(
                server
                    .default_shell_target()
                    .map(|target| target.tab_id.to_string()),
                Some(moved.pane.tab_id.clone()),
            );
        } else {
            assert_eq!(server.default_shell_target(), target_before);
        }
        assert_eq!(moved.closed_workspace_id.is_some(), new_workspace);
        for client_id in [9, 10] {
            assert_eq!(
                server.shell_tab_id_for_client(client_id.into()).as_deref(),
                Some(moved.pane.tab_id.as_str())
            );
            let target = server
                .shell_focus_target(client_id.into())
                .expect("test precondition");
            assert_eq!(target.pane_id, source);
            if new_workspace {
                let location = server.clients[&client_id]
                    .shell_state()
                    .and_then(|shell| shell.location.as_ref())
                    .expect("test precondition");
                assert_eq!(location.active_tab_ids.len(), 1);
            }
        }
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn public_pane_move_without_effective_focus_preserves_client_views() {
    for case in ["no-focus", "same-tab", "zoomed", "invalid"] {
        let mut server = pane_move_server();
        let source = server.app.state.workspaces[0].tabs[0].root_pane;
        let source_id = server
            .app
            .public_pane_id(0, source)
            .expect("test precondition");
        let remaining_tab = server.app.public_tab_id(0, 1).expect("test precondition");
        let destination_tab = server.app.public_tab_id(1, 0).expect("test precondition");
        let (_control, _render) = connect_test_shell(&mut server, 9, 80, 23);
        let (_source_control, _source_render) = connect_test_shell(&mut server, 10, 80, 23);
        // Keep a valid view distinct from the server default in every case.
        assert!(server.focus_shell_client_on_tab(ClientId::test_new(9), &remaining_tab));
        let location_before = server.clients[&9]
            .shell_state()
            .and_then(|shell| shell.location.clone());
        let revision_before = server.clients[&9]
            .shell_state()
            .map_or(0, |shell| shell.projection_revision.get());
        if case == "zoomed" {
            server.app.state.workspaces[0].tabs[0].zoomed = true;
        }
        let tab_id = match case {
            "same-tab" => server.app.public_tab_id(0, 0).expect("test precondition"),
            "invalid" => "missing-tab".into(),
            _ => destination_tab,
        };
        let result = public_move(
            &mut server,
            PaneMoveParams {
                pane_id: source_id,
                destination: PaneMoveDestination::Tab {
                    tab_id,
                    target_pane_id: None,
                    split: shepr_api::schema::SplitDirection::Right,
                    ratio: None,
                },
                focus: case != "no-focus",
            },
        );
        if case == "invalid" {
            assert_eq!(
                result.expect_err("test precondition").error.code,
                "tab_not_found"
            );
        } else {
            assert_eq!(
                result.expect("test precondition").changed,
                case == "no-focus"
            );
        }
        assert_eq!(
            server.clients[&9]
                .shell_state()
                .and_then(|shell| shell.location.clone()),
            location_before,
            "{case}"
        );
        assert_eq!(
            server.clients[&9]
                .shell_state()
                .map_or(0, |shell| shell.projection_revision.get()),
            revision_before,
            "{case}"
        );
        assert_eq!(server.app.state.active_index(), Some(0));
        if case == "no-focus" {
            // A later change to the server default must not move this client.
            server.app.state.switch_workspace_tab(1, 0);
            assert_eq!(
                server
                    .shell_tab_id_for_client(ClientId::test_new(10))
                    .as_deref(),
                Some(remaining_tab.as_str()),
                "the removed source tab must be reconciled to the remaining source tab"
            );
        }
        shutdown_test_runtimes(&mut server);
    }
}
