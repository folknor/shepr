//! Client locations, the navigation effects of endpoint commands, the
//! session bookmark, and the automatic creation of workspaces.

use super::*;
use shepr_protocol::WorkspaceId;
use shepr_protocol::command::{
    EndpointError, EndpointReply, PaneRenameParams, PaneTarget, WorkspaceCloseParams,
    WorkspaceCreateParams, WorkspaceCreateSource, WorkspaceMoveParams, WorkspaceTarget,
};

/// A server whose session holds one workspace per name, each with a runtime,
/// and whose bookmark is on the first.
fn server_with_workspaces(names: &[&str]) -> (HeadlessServer, Vec<shepr_core::layout::PaneId>) {
    let mut server = test_headless_server();
    let workspaces = names
        .iter()
        .map(|name| shepr_mux::workspace::Workspace::test_new(name))
        .collect::<Vec<_>>();
    let panes = workspaces
        .iter()
        .map(shepr_mux::workspace::Workspace::root_pane)
        .collect::<Vec<_>>();
    server.app.state.workspaces = workspaces;
    for pane_id in &panes {
        server.app.insert_test_runtime(
            *pane_id,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"SCREEN"),
        );
    }
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    (server, panes)
}

fn workspace_id(server: &HeadlessServer, index: usize) -> WorkspaceId {
    server
        .app
        .public_workspace_id(index)
        .expect("test precondition")
}

fn connect(
    server: &mut HeadlessServer,
    client_id: u64,
) -> (std::sync::mpsc::Receiver<Vec<u8>>, RenderLaneReceiver) {
    let (control, render) = connect_matching_test_shell(server, client_id);
    let _ = client_shell_snapshot(&control);
    (control, render)
}

fn location_of(server: &HeadlessServer, client_id: u64) -> Option<WorkspaceId> {
    server.shell_target_for_client(ClientId::test_new(client_id))
}

/// Runs `command` for the client the way the loop does and returns its
/// answer.
fn run(
    server: &mut HeadlessServer,
    client_id: u64,
    command: EndpointCommand,
) -> Result<EndpointReply, EndpointError> {
    server.handle_client_shell_command(ClientId::test_new(client_id), command)
}

#[tokio::test]
async fn a_location_change_invalidates_only_that_clients_projection() {
    let (mut server, _panes) = server_with_workspaces(&["first", "second"]);
    let (control_7, _render_7) = connect(&mut server, 7);
    let (control_8, _render_8) = connect(&mut server, 8);
    server.render_now();
    assert!(control_7.try_recv().is_err());
    assert!(control_8.try_recv().is_err());
    let session_generation = server.shell_session_generation;
    let projected = |server: &HeadlessServer, client_id: u64| {
        let shell = server.clients[&client_id].shell_state();
        (
            shell.location.generation(),
            shell.projected_location_generation,
        )
    };
    let (generation_7, projected_7) = projected(&server, 7);
    assert_eq!(generation_7, projected_7);
    let (generation_8, projected_8) = projected(&server, 8);
    assert_eq!(generation_8, projected_8);

    let second = workspace_id(&server, 1);
    let outcome = run(
        &mut server,
        7,
        EndpointCommand::WorkspaceFocus(WorkspaceTarget {
            workspace_id: second,
        }),
    );
    assert!(outcome.is_ok());

    // Only client 7's location moved, and only its generation is ahead of what
    // was projected.
    let (generation_7, projected_7) = projected(&server, 7);
    assert!(generation_7 > projected_7);
    assert_eq!(projected(&server, 8), (generation_8, projected_8));
    assert_eq!(location_of(&server, 7), Some(second));
    assert_eq!(location_of(&server, 8), Some(workspace_id(&server, 0)));

    server.render_now();
    let replacement = client_shell_snapshot(&control_7);
    assert_eq!(replacement.focused_workspace_id.as_ref(), Some(&second));
    assert!(
        control_8.try_recv().is_err(),
        "the other client's projection is untouched"
    );
    let (generation_7, projected_7) = projected(&server, 7);
    assert_eq!(generation_7, projected_7, "advanced after the projection");
    assert_eq!(
        server.shell_session_generation, session_generation,
        "focusing a workspace changes nothing shared"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_rejected_focus_command_moves_nobody() {
    let (mut server, _panes) = server_with_workspaces(&["first", "second"]);
    let (_control, _render) = connect(&mut server, 7);
    let before = server.clients[&7].shell_state().location.clone();
    let gone = WorkspaceId::from_number(9_999).expect("nonzero number");
    let bookmark = server.app.state.bookmark;

    let result = run(
        &mut server,
        7,
        EndpointCommand::WorkspaceFocus(WorkspaceTarget { workspace_id: gone }),
    );

    assert!(matches!(result, Err(EndpointError::Rejected(_))));
    assert_eq!(server.clients[&7].shell_state().location, before);
    assert_eq!(server.app.state.bookmark, bookmark);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn navigation_moves_the_requester_and_the_bookmark_only_from_an_active_client() {
    let (mut server, _panes) = server_with_workspaces(&["first", "second"]);
    let (_control_7, _render_7) = connect(&mut server, 7);
    let (_control_8, _render_8) = connect(&mut server, 8);
    let first = workspace_id(&server, 0);
    let second = workspace_id(&server, 1);
    assert_eq!(server.app.state.bookmark.as_ref(), Some(&first));

    // An active client's navigation moves its own location and the bookmark.
    assert!(
        run(
            &mut server,
            7,
            EndpointCommand::WorkspaceFocus(WorkspaceTarget {
                workspace_id: second,
            }),
        )
        .is_ok()
    );
    assert_eq!(location_of(&server, 7), Some(second));
    assert_eq!(location_of(&server, 8), Some(first));
    assert_eq!(server.app.state.bookmark.as_ref(), Some(&second));
    assert!(server.app.state.session_dirty, "the bookmark is saved");

    // A client whose surface is not active moves itself and nothing shared.
    assert!(
        server
            .set_client_shell_surface_active(ClientId::test_new(8), false)
            .is_some_and(|(changed, _)| changed)
    );
    assert!(server.navigate_shell_client(ClientId::test_new(8), &second));
    assert_eq!(location_of(&server, 8), Some(second));
    assert!(server.navigate_shell_client(ClientId::test_new(8), &first));
    assert_eq!(server.app.state.bookmark.as_ref(), Some(&second));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_new_client_starts_at_the_bookmark() {
    let (mut server, _panes) = server_with_workspaces(&["first", "second"]);
    server.app.state.set_bookmark_index(Some(1));

    let (_control, _render) = connect(&mut server, 7);

    assert_eq!(location_of(&server, 7), Some(workspace_id(&server, 1)));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_client_whose_workspace_vanished_lands_by_remembered_index_across_a_move() {
    let (mut server, _panes) = server_with_workspaces(&["a", "b", "c"]);
    let (_control_7, _render_7) = connect(&mut server, 7);
    let (_control_8, _render_8) = connect(&mut server, 8);
    let (a, b, c) = (
        workspace_id(&server, 0),
        workspace_id(&server, 1),
        workspace_id(&server, 2),
    );
    // Client 7 views b (index 1), and so does the bookmark.
    assert!(server.place_test_client_on_workspace(ClientId::test_new(7), &b));
    server.app.state.set_bookmark_index(Some(1));
    assert_eq!(server.clients[&7].shell_state().location.index(), 1);

    // Client 8 moves a to the end: the order is b, c, a, and b is at index 0.
    assert!(
        run(
            &mut server,
            8,
            EndpointCommand::WorkspaceMove(WorkspaceMoveParams {
                workspace_id: a,
                before_workspace_id: None,
            }),
        )
        .is_ok()
    );
    assert_eq!(server.workspace_order(), vec![b, c, a]);
    let location = &server.clients[&7].shell_state().location;
    assert_eq!(location.focused_workspace_id(), Some(&b));
    assert_eq!(
        location.index(),
        0,
        "the move refreshed the remembered index"
    );
    assert_eq!(server.app.state.bookmark_index(), Some(0));

    // Then b closes: client 7 lands on the workspace now at index 0 (c), not
    // at the index it had before the move (which would be a).
    server.app.state.session_dirty = false;
    assert!(
        run(
            &mut server,
            8,
            EndpointCommand::WorkspaceClose(WorkspaceCloseParams { workspace_id: b }),
        )
        .is_ok()
    );
    assert_eq!(location_of(&server, 7), Some(c));
    assert_eq!(server.app.state.bookmark.as_ref(), Some(&c));
    assert!(
        server.app.state.session_dirty,
        "the repaired bookmark is saved"
    );
    // Client 8 never viewed b, and keeps its workspace.
    assert_eq!(location_of(&server, 8), Some(a));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn the_last_workspace_closing_leaves_a_fresh_one_for_the_requester() {
    let (mut server, panes) = server_with_workspaces(&["only"]);
    let (_control, _render) = connect(&mut server, 7);
    let closing = workspace_id(&server, 0);
    let pane_id = server
        .app
        .public_pane_id(0, panes[0])
        .expect("test precondition");

    assert!(
        run(
            &mut server,
            7,
            EndpointCommand::PaneClose(PaneTarget { pane_id }),
        )
        .is_ok()
    );

    assert_eq!(server.app.state.workspaces.len(), 1);
    let replacement = workspace_id(&server, 0);
    assert_ne!(replacement, closing);
    assert_eq!(location_of(&server, 7), Some(replacement));
    assert_eq!(
        server.clients.geometry_controller(&replacement),
        Some(ClientId::test_new(7))
    );
    shutdown_test_runtimes(&mut server);
}

/// A client that presents a surface: active, with a writer. The `Receiver`s
/// keep the writer's channels open.
fn presenting_client(
    server: &mut HeadlessServer,
    client_id: u64,
    size: (u16, u16),
) -> (std::sync::mpsc::Receiver<Vec<u8>>, RenderLaneReceiver) {
    let (writer, control, render) = test_client_writer();
    server.insert_test_client(
        client_id,
        ClientConnection::new(
            size,
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            client_id,
            writer,
        ),
    );
    (control, render)
}

fn created_workspace(server: &HeadlessServer) -> WorkspaceId {
    assert_eq!(
        server.app.state.workspaces.len(),
        1,
        "one workspace created"
    );
    workspace_id(server, 0)
}

#[tokio::test]
async fn automatic_creation_is_controlled_by_the_trigger_when_it_presents_a_surface() {
    let mut server = test_headless_server();
    let _low = presenting_client(&mut server, 1, (80, 24));
    let _trigger = presenting_client(&mut server, 2, (100, 30));

    assert!(server.create_automatic_workspace(Some(ClientId::test_new(2))));

    let created = created_workspace(&server);
    assert_eq!(
        server.clients.geometry_controller(&created),
        Some(ClientId::test_new(2)),
        "the trigger outranks the lower id"
    );
    assert_eq!(
        server
            .app
            .state
            .workspace_spawn_geometry(0)
            .map(|geometry| geometry.area),
        Some(Rect::new(0, 0, 100, 30))
    );
    // Both clients land on the new workspace; the geometry stays with the
    // trigger through the settlement that follows.
    assert_eq!(location_of(&server, 1), Some(created));
    assert_eq!(location_of(&server, 2), Some(created));
    assert_eq!(
        server.clients.geometry_controller(&created),
        Some(ClientId::test_new(2))
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn automatic_creation_falls_back_to_the_lowest_id_presenting_client() {
    let mut server = test_headless_server();
    let _high = presenting_client(&mut server, 5, (90, 28));
    let _low = presenting_client(&mut server, 3, (70, 20));
    // The trigger holds a connection that presents nothing (no writer).
    server.insert_test_client(
        9,
        ClientConnection::new(
            (120, 40),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            9,
            crate::server::outbox::ClientOutbox::detached(),
        ),
    );

    assert!(server.create_automatic_workspace(Some(ClientId::test_new(9))));

    let created = created_workspace(&server);
    assert_eq!(
        server.clients.geometry_controller(&created),
        Some(ClientId::test_new(3))
    );
    assert_eq!(
        server
            .app
            .state
            .workspace_spawn_geometry(0)
            .map(|geometry| geometry.area),
        Some(Rect::new(0, 0, 70, 20))
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn automatic_creation_with_no_presenting_client_is_headless_with_no_controller() {
    let mut server = test_headless_server();
    // An active connection that cannot present (no writer) triggers the
    // creation: the workspace is sized for the headless area, with no
    // controller.
    server.insert_test_client(
        4,
        ClientConnection::new(
            (120, 40),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            4,
            crate::server::outbox::ClientOutbox::detached(),
        ),
    );

    assert!(server.create_automatic_workspace(Some(ClientId::test_new(4))));

    let created = created_workspace(&server);
    assert_eq!(server.clients.geometry_controller(&created), None);
    assert_eq!(
        server.app.state.workspace_spawn_geometry(0),
        Some(server.app.headless_spawn_geometry())
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn the_loop_creates_nothing_for_a_session_no_client_looks_at() {
    let mut server = test_headless_server();

    assert!(!server.create_automatic_workspace(None));
    assert!(server.app.state.workspaces.is_empty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn workspace_create_sizes_the_first_pty_for_the_requester_and_navigates_it() {
    let (mut server, _panes) = server_with_workspaces(&["existing"]);
    let (writer, control, _render) = test_client_writer();
    let client_id = ClientId::test_new(7);
    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id,
            geometry: shepr_core::geometry::HostGeometry::new(100, 30, 9, 18, false),
            mouse_capture: false,
            surface_active: true,
            outbox: writer,
        })
    );
    let _ = client_shell_snapshot(&control);

    let result = run(
        &mut server,
        7,
        EndpointCommand::WorkspaceCreate(WorkspaceCreateParams {
            source: WorkspaceCreateSource::Default,
            label: Some("fresh".into()),
        }),
    );
    assert_eq!(result, Ok(EndpointReply::Done));

    assert_eq!(server.app.state.workspaces.len(), 2);
    let created = workspace_id(&server, 1);
    let root = server.app.state.workspaces[1].root_pane();
    let runtime = server.app.test_runtime(root);
    let grid = runtime.grid_size();
    assert_eq!(
        runtime.pixel_size(),
        Some((
            u32::from(grid.cols.get()) * 9,
            u32::from(grid.rows.get()) * 18
        )),
        "the first window size carries the requester's cell size"
    );
    assert_eq!(location_of(&server, 7), Some(created));
    assert_eq!(server.app.state.bookmark.as_ref(), Some(&created));
    assert_eq!(
        server.clients.geometry_controller(&created),
        Some(client_id),
        "the requester controls what it navigated to"
    );
    assert_eq!(server.app.state.workspaces[1].display_name(), "fresh");
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pane_replies_name_the_requested_target() {
    let (mut server, panes) = server_with_workspaces(&["first", "second"]);
    let (_control, _render) = connect(&mut server, 7);
    let first_pane = server
        .app
        .public_pane_id(0, panes[0])
        .expect("test precondition");
    let second_pane = server
        .app
        .public_pane_id(1, panes[1])
        .expect("test precondition");

    // Focusing a pane of the other workspace navigates the requester there.
    let Ok(EndpointReply::PaneInfo { pane }) = run(
        &mut server,
        7,
        EndpointCommand::PaneFocus(PaneTarget {
            pane_id: second_pane,
        }),
    ) else {
        panic!("expected pane info");
    };
    assert_eq!(pane.pane_id, second_pane);
    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(7)),
        Some(*second_pane.workspace_id())
    );

    // A pane info reply names its target; focus remains in the shell snapshot.
    let Ok(EndpointReply::PaneInfo { pane }) = run(
        &mut server,
        7,
        EndpointCommand::PaneRename(PaneRenameParams {
            pane_id: first_pane,
            label: Some("elsewhere".into()),
        }),
    ) else {
        panic!("expected pane info");
    };
    assert_eq!(pane.pane_id, first_pane);
    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(7)),
        Some(*second_pane.workspace_id())
    );
    shutdown_test_runtimes(&mut server);
}
