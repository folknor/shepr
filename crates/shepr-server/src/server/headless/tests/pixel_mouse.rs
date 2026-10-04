use super::*;
use shepr_core::geometry::{CellPx, GridSize, HostCell, HostGeometry, PanePixelExtent};
use shepr_term::mouse::{HostMouseCapture, PanePixelMouse, PixelReport};

/// The child asks for any-motion reporting, SGR encoding and pixel reports.
const CHILD_PIXEL_MOUSE: &[u8] = b"\x1b[?1003h\x1b[?1006h\x1b[?1016h";

fn pixel_server() -> (
    HeadlessServer,
    shepr_core::layout::PaneId,
    tokio::sync::mpsc::Receiver<Bytes>,
) {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("pixel-mouse");
    let pane_id = workspace.tree().focused();
    let (runtime, input_rx) = shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
        80,
        23,
        0,
        CHILD_PIXEL_MOUSE,
        8,
    );
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.insert_test_runtime(pane_id, runtime);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    (server, pane_id, input_rx)
}

/// Connects a shell whose host measured its cell exactly.
fn connect_exact(
    server: &mut HeadlessServer,
    client_id: u64,
    surface: (u16, u16),
    cell: (u32, u32),
) -> (std::sync::mpsc::Receiver<Vec<u8>>, RenderLaneReceiver) {
    let (writer, control, render) = test_client_writer();
    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id: client_id.into(),
            geometry: HostGeometry::new(
                GridSize::clamped(surface.0, surface.1),
                HostCell::from_host(cell.0, cell.1, true),
            ),
            mouse_capture: false,
            surface_active: true,
            outbox: writer,
        })
    );
    (control, render)
}

fn pane_pixel_mouse(
    server: &HeadlessServer,
    pane_id: shepr_core::layout::PaneId,
) -> PanePixelMouse {
    server
        .app
        .pane_runtime(pane_id)
        .expect("pane runtime")
        .read()
        .pixel_mouse()
}

/// The newest host mouse capture mode the client was told, if any.
fn told_mouse_capture(control: &std::sync::mpsc::Receiver<Vec<u8>>) -> Option<HostMouseCapture> {
    let mut told = None;
    // The control lane is drained by a writer thread: wait out its quiet.
    while let Ok(bytes) = control.recv_timeout(Duration::from_millis(100)) {
        if let ServerMessage::MouseCapture { mode } = read_server_message(bytes) {
            told = Some(mode);
        }
    }
    told
}

fn full_render(server: &mut HeadlessServer) -> render::PassReport {
    let plan = server.render_plan(false);
    server.render_pass(&plan, &HashSet::new())
}

fn pane_surface(render: &RenderLaneReceiver) -> shepr_protocol::PaneSurfaceFrame {
    match read_server_message(render.recv().expect("pane surface")) {
        ServerMessage::PaneSurface(surface) => surface,
        other => panic!("expected pane surface, got {other:?}"),
    }
}

fn pixel_report_event(
    column: u16,
    row: u16,
    x: u32,
    y: u32,
    extent: PanePixelExtent,
) -> shepr_protocol::ClientPaneInputEvent {
    shepr_protocol::ClientPaneInputEvent::Mouse {
        kind: shepr_protocol::ClientMouseKind::Moved,
        position: shepr_protocol::ClientMousePosition::Pixels {
            column,
            row,
            report: PixelReport::new(x, y, extent),
        },
        modifiers: shepr_protocol::WireModifiers::NONE,
        lines: 0,
    }
}

fn send_pane_input(
    server: &mut HeadlessServer,
    client_id: u64,
    events: Vec<shepr_protocol::ClientPaneInputEvent>,
) {
    let pane_id = focused_test_pane(server);
    server.test_handle_server_event(ServerEvent::ShellPaneInput {
        client_id: ClientId::test_new(client_id),
        pane_id,
        events,
    });
}

#[tokio::test]
async fn surfaces_publish_the_pty_extent_not_the_viewers_cell() {
    let (mut server, pane_id, _input_rx) = pixel_server();
    let (_control_7, render_7) = connect_exact(&mut server, 7, (80, 23), (8, 16));
    let (_control_8, render_8) = connect_exact(&mut server, 8, (80, 23), (10, 20));

    let report = full_render(&mut server);
    assert_eq!(
        report.surface_renders, 1,
        "clients with the same area share one render whatever their cells"
    );

    let published = pane_pixel_mouse(&server, pane_id);
    assert!(published.requested());
    let extent = published.extent().expect("the PTY was told an extent");
    let (pitch_width, pitch_height) = extent.cell_pitch();
    assert_eq!(
        (pitch_width.get(), pitch_height.get()),
        (8, 16),
        "the first client to connect is the geometry source"
    );
    for render in [&render_7, &render_8] {
        let surface = pane_surface(render);
        assert_eq!(surface.panes[0].pixel_mouse, published);
    }
}

#[tokio::test]
async fn non_source_client_with_matching_grid_captures_and_delivers_pixels() {
    let (mut server, pane_id, mut input_rx) = pixel_server();
    let (control_7, _render_7) = connect_exact(&mut server, 7, (80, 23), (8, 16));
    let (control_8, _render_8) = connect_exact(&mut server, 8, (80, 23), (10, 20));
    let published = pane_pixel_mouse(&server, pane_id);
    let extent = published.extent().expect("the PTY was told an extent");

    server.stream_host_mouse_capture_mode();
    assert_eq!(
        told_mouse_capture(&control_7),
        Some(HostMouseCapture::Pixels)
    );
    assert_eq!(
        told_mouse_capture(&control_8),
        Some(HostMouseCapture::Pixels),
        "a non-source client presenting the PTY's grid captures pixels"
    );

    // The report is in the geometry the child was told, whatever the cell of
    // the client that sent it.
    let (x, y) = (21, 22);
    assert!(extent.contains(x, y));
    send_pane_input(&mut server, 8, vec![pixel_report_event(2, 1, x, y, extent)]);
    assert_eq!(
        input_rx.try_recv().expect("pixel report reached the PTY"),
        Bytes::from(format!("\x1b[<35;{x};{y}M"))
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_presenting_a_pane_at_another_grid_is_told_cells() {
    let (mut server, pane_id, mut input_rx) = pixel_server();
    let (control_7, _render_7) = connect_exact(&mut server, 7, (80, 23), (8, 16));
    let (control_9, _render_9) = connect_exact(&mut server, 9, (79, 22), (10, 20));
    let published = pane_pixel_mouse(&server, pane_id);
    let extent = published.extent().expect("the PTY was told an extent");

    server.stream_host_mouse_capture_mode();
    assert_eq!(
        told_mouse_capture(&control_7),
        Some(HostMouseCapture::Pixels)
    );
    assert_eq!(
        told_mouse_capture(&control_9),
        Some(HostMouseCapture::Cells),
        "cell n of this client's screen is not cell n of the child"
    );

    // A pixel report mapped against the client's own grid is not the pane's
    // extent, so it is admitted as the cell it names: under 1016 that is the
    // cell's pitch origin in the extent the child was told.
    let other = PanePixelExtent::new(GridSize::clamped(77, 20), 770, 400).expect("nonzero");
    send_pane_input(
        &mut server,
        9,
        vec![pixel_report_event(2, 1, 21, 22, other)],
    );
    // The interacting client claims the workspace's geometry, so the child was
    // re-told its extent before the report was encoded against it.
    let told = pane_pixel_mouse(&server, pane_id)
        .extent()
        .expect("the PTY was told an extent");
    assert_ne!(told, extent, "interaction made this client the source");
    let (x, y) = told.cell_origin(2, 1);
    assert_eq!(
        input_rx.try_recv().expect("cell report reached the PTY"),
        Bytes::from(format!("\x1b[<35;{x};{y}M"))
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn oversized_pane_publishes_the_clamped_extent_and_admits_pixels() {
    let (mut server, pane_id, mut input_rx) = pixel_server();
    let (control, render) = connect_exact(
        &mut server,
        7,
        (80, 23),
        (CellPx::MAX_DIMENSION, CellPx::MAX_DIMENSION),
    );
    full_render(&mut server);
    let published = pane_pixel_mouse(&server, pane_id);
    let extent = published.extent().expect("the PTY was told an extent");
    assert_eq!(
        (extent.width().get(), extent.height().get()),
        (u16::MAX, u16::MAX),
        "the extent is clamped to what the winsize can carry"
    );
    assert_eq!(pane_surface(&render).panes[0].pixel_mouse, published);

    server.stream_host_mouse_capture_mode();
    assert_eq!(told_mouse_capture(&control), Some(HostMouseCapture::Pixels));
    send_pane_input(
        &mut server,
        7,
        vec![pixel_report_event(40, 10, 60_000, 50_000, extent)],
    );
    assert_eq!(
        input_rx.try_recv().expect("pixel report reached the PTY"),
        Bytes::from_static(b"\x1b[<35;60000;50000M")
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pixel_admission_ignores_the_presentation_memo() {
    let (mut server, pane_id, mut input_rx) = pixel_server();
    let (control, _render) = connect_exact(&mut server, 7, (80, 23), (10, 20));
    let extent = pane_pixel_mouse(&server, pane_id)
        .extent()
        .expect("the PTY was told an extent");
    server.stream_host_mouse_capture_mode();
    assert_eq!(told_mouse_capture(&control), Some(HostMouseCapture::Pixels));

    // A surface activation forgets what the client was told and does not tell
    // it again until the client asks for the replay.
    server
        .clients
        .get_mut(&ClientId::test_new(7))
        .expect("connected client")
        .outbox
        .forget_presentation();

    send_pane_input(
        &mut server,
        7,
        vec![pixel_report_event(2, 1, 21, 22, extent)],
    );
    assert_eq!(
        input_rx.try_recv().expect("pixel report reached the PTY"),
        Bytes::from_static(b"\x1b[<35;21;22M"),
        "admission reads the connection and the pane, not the outbox memo"
    );
    shutdown_test_runtimes(&mut server);
}
