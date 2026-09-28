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
        ServerMessage::PaneSurface(surface) => surface,
        other => panic!("expected decoded pane surface, got {other:?}"),
    }
}

#[tokio::test]
async fn surface_delta_reconstructs_metadata_text_and_hyperlinks() {
    let (mut server, _control_rx, render_rx, pane_id) =
        retained_test_server_with_control(b"initial text");
    server.render_and_stream();
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
    assert!(!server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));
    server.render_and_stream();
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
