use super::*;
use shepr_test_fixtures::*;
use shepr_test_support::IsolatedEnv;

/// A public pane id from its canonical spelling (`<workspace>:p<number>`).
/// Test ids go through the parser a server's ids go through, so a test cannot
/// build an id no server would issue.
pub(crate) fn test_pane_id(id: &str) -> shepr_protocol::PublicPaneId {
    id.parse()
        .unwrap_or_else(|_| panic!("{id:?} is not a canonical public pane id"))
}

/// A workspace id from its canonical spelling (`w<number>`).
pub(crate) fn test_workspace_id(id: &str) -> shepr_protocol::WorkspaceId {
    id.parse()
        .unwrap_or_else(|_| panic!("{id:?} is not a canonical workspace id"))
}

/// The canonical boot id of the test server named `name`. Tests name the
/// servers they talk to; each name maps to its own boot id, so two names never
/// share one and a test cannot build a boot id no server would send.
pub(crate) fn test_boot_id(name: &str) -> shepr_protocol::BootId {
    let process_id = match name {
        "boot" => 1,
        "boot-1" => 2,
        "local-boot" => 3,
        "remote-boot" => 4,
        "old-boot" => 5,
        "new-local-boot" => 6,
        "replacement-boot" => 7,
        "stale-local-boot" => 8,
        "shared-server-boot" => 9,
        "restarted-remote" => 10,
        "restarted-local" => 11,
        "restored" => 12,
        "restored-first" => 13,
        _ => panic!("{name:?} names no test server"),
    };
    fixed_boot_id(process_id)
}

#[test]
fn atomic_cell_size_keeps_width_and_height_in_one_snapshot() {
    let size = AtomicCellSize::new();
    assert_eq!(size.load(), None);
    assert!(size.store(9, 18));
    assert_eq!(size.load(), Some((9, 18)));
    assert!(!size.store(9, 18));
    assert!(size.store(0, 18));
    assert_eq!(size.load(), None);
}

#[test]
fn resize_signal_reports_even_when_polled_size_is_unchanged() {
    let size = shepr_core::geometry::HostGeometry::new(120, 40, 8, 16, true);
    assert!(resize_report_required(true, size, size));
    assert!(!resize_report_required(false, size, size));
    assert!(resize_report_required(
        false,
        shepr_core::geometry::HostGeometry::new(120, 41, 8, 16, true),
        size
    ));
    assert!(resize_report_required(
        false,
        shepr_core::geometry::HostGeometry::new(120, 40, 9, 18, true),
        size
    ));
    assert!(resize_report_required(
        false,
        shepr_core::geometry::HostGeometry::new(120, 40, 8, 16, false),
        size
    ));
}

#[test]
fn unavailable_terminal_grid_is_not_fabricated() {
    let reported_cell_size = AtomicCellSize::new();
    let err = current_terminal_geometry_with(&reported_cell_size, None, None, || {
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "terminal is gone",
        ))
    })
    .expect_err("an unavailable terminal must not produce fallback geometry");

    assert_eq!(err.kind(), io::ErrorKind::NotConnected);
}

#[test]
fn missing_pixel_geometry_keeps_a_valid_terminal_grid() {
    let reported_cell_size = AtomicCellSize::new();
    let geometry =
        current_terminal_geometry_with(&reported_cell_size, Some((9, 18)), None, || Ok((80, 24)))
            .expect("grid geometry remains valid without pixel dimensions");

    assert_eq!(
        geometry,
        shepr_core::geometry::HostGeometry::new(80, 24, 9, 18, false)
    );
}

#[test]
fn client_host_size_clamps_the_grid_to_one_surface() {
    let shell = terminal_geometry::ClientHostSize::new(0, u16::MAX);
    assert_eq!(shell.cols, 1);
    assert!((1..=shepr_protocol::MAX_SURFACE_DIMENSION).contains(&shell.rows));
    assert!(usize::from(shell.cols) * usize::from(shell.rows) <= shepr_protocol::MAX_SURFACE_CELLS);
}

#[test]
fn cell_geometry_is_bounded_before_wire_use_and_disables_inexact_pixel_mouse() {
    let (width, height, exact) = super::terminal_geometry::bounded_cell_geometry(
        shepr_protocol::MAX_CELL_SIZE_PX + 1,
        shepr_protocol::MAX_CELL_SIZE_PX + 2,
        true,
    );

    assert_eq!(
        (width, height, exact),
        (
            shepr_protocol::MAX_CELL_SIZE_PX,
            shepr_protocol::MAX_CELL_SIZE_PX,
            false,
        )
    );
}

#[test]
fn host_cursor_policy_native_and_drawn_ignore_the_terminal() {
    let env = IsolatedEnv::new();
    env.set("TERM_PROGRAM", "WezTerm");

    assert!(!should_draw_host_cursor(
        shepr_config::HostCursorModeConfig::Native
    ));
    assert!(should_draw_host_cursor(
        shepr_config::HostCursorModeConfig::Drawn
    ));
}

#[test]
fn write_host_terminal_appearance_query_emits_mode_2031_query() {
    let mut output = Vec::new();
    write_host_terminal_appearance_query(&mut output).expect("test precondition");
    assert_eq!(output, b"\x1b[?996n");
}

#[test]
fn write_host_terminal_theme_query_emits_osc_queries() {
    let mut output = Vec::new();
    write_host_terminal_theme_query(&mut output).expect("test precondition");
    assert_eq!(
        output,
        shepr_termio::host_term::theme::host_terminal_theme_query_sequence().as_bytes()
    );
    assert!(
        !output
            .windows(shepr_termio::host_term::theme::HOST_COLOR_SCHEME_QUERY_SEQUENCE.len())
            .any(|window| window
                == shepr_termio::host_term::theme::HOST_COLOR_SCHEME_QUERY_SEQUENCE.as_bytes())
    );
}

#[test]
fn write_host_color_scheme_report_mode_emits_mode_sequences() {
    let mut output = Vec::new();
    write_host_color_scheme_report_mode(&mut output, true).expect("test precondition");
    write_host_color_scheme_report_mode(&mut output, false).expect("test precondition");

    let mut expected = Vec::new();
    expected.extend_from_slice(
        shepr_termio::host_term::theme::HOST_COLOR_SCHEME_REPORT_ENABLE_SEQUENCE.as_bytes(),
    );
    expected.extend_from_slice(
        shepr_termio::host_term::theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE.as_bytes(),
    );
    assert_eq!(output, expected);
}

#[test]
fn write_host_cell_size_query_emits_xtwinops_request() {
    let mut output = Vec::new();
    write_host_cell_size_query(&mut output).expect("test precondition");

    assert_eq!(output, b"\x1b[16t");
}

#[test]
fn cell_size_fallback_prefers_reported_then_previous_size() {
    assert_eq!(cell_size_fallback(0, None), (8, 16));
    assert_eq!(cell_size_fallback(0, Some((11, 22))), (11, 22));
    assert_eq!(
        cell_size_fallback(pack_cell_size(10, 21), Some((11, 22))),
        (10, 21)
    );
    assert_eq!(cell_size_fallback(pack_cell_size(10, 0), None), (8, 16));
    assert_eq!(cell_size_fallback(pack_cell_size(0, 21), None), (8, 16));
}

#[test]
fn reported_cell_size_is_taken_from_host_cell_size_events() {
    let events = shepr_test_fixtures::parse_raw_input_bytes_sync(b"\x1b[?997;1n");
    assert_eq!(
        super::terminal_geometry::reported_cell_size_from_events(&events),
        None
    );

    let events = shepr_test_fixtures::parse_raw_input_bytes_sync(b"\x1b[6;21;10t\x1b[6;18;9t");
    assert_eq!(
        super::terminal_geometry::reported_cell_size_from_events(&events),
        Some((9, 18))
    );
}

#[test]
fn terminal_restore_postlude_restores_visible_default_cursor() {
    let mut output = Vec::new();
    write_terminal_restore_postlude(&mut output).expect("test precondition");
    assert_eq!(output, b"\x1b[?25h\x1b[0 q");
}

#[test]
fn sgr_pixel_mouse_needs_capture_a_request_and_exact_geometry() {
    assert!(effective_sgr_pixel_mouse(true, true, true));
    assert!(!effective_sgr_pixel_mouse(true, true, false));
}

#[test]
fn host_modes_restore_color_scheme_reports_when_enabled() {
    let mut output = Vec::new();
    let host_modes = HostModes::new(false, false);
    host_modes
        .enable_color_scheme_reports(&mut output)
        .expect("test precondition");
    output.clear();
    host_modes.restore(&mut output).expect("test precondition");

    assert_eq!(
        output,
        shepr_termio::host_term::theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE.as_bytes()
    );
}

#[test]
fn client_error_display_connection_failed() {
    let err = ClientError::ConnectionFailed(io::Error::new(
        io::ErrorKind::ConnectionRefused,
        "connection refused",
    ));
    let msg = err.to_string();
    assert!(
        msg.contains("connection") && msg.contains("connection refused"),
        "should mention the connection failure and its cause: {msg}"
    );
    // The variant also wraps setup failures after a connect succeeded (a
    // stream clone, a thread spawn), so it must not claim no server runs.
    assert!(
        !msg.contains("starts one") && !msg.contains("server running"),
        "should not guess at a missing server: {msg}"
    );
}

#[test]
fn client_error_display_host_terminal_does_not_claim_server_connection_failed() {
    let err = ClientError::HostTerminal(io::Error::new(
        io::ErrorKind::BrokenPipe,
        "terminal output was closed",
    ));
    let msg = err.to_string();

    assert!(
        msg.contains("host terminal error"),
        "should identify the host terminal: {msg}"
    );
    assert!(msg.contains("terminal output was closed"));
    assert!(!msg.contains("Is the shepr server running?"));
}

#[test]
fn client_error_display_handshake_rejected() {
    let err = ClientError::HandshakeRejected {
        error: shepr_protocol::HandshakeRefusal::InvalidSurface("incompatible".into()),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("rejected handshake"),
        "should mention rejection: {msg}"
    );
    assert!(msg.contains("incompatible"), "should include error: {msg}");
}

#[test]
fn client_error_display_server_shutdown() {
    let err = ClientError::ServerShutdown {
        reason: Some(shepr_protocol::ShutdownReason::Message(
            "maintenance".into(),
        )),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("server shut down"),
        "should mention shutdown: {msg}"
    );
    assert!(msg.contains("maintenance"), "should include reason: {msg}");
}

#[test]
fn client_error_display_server_shutdown_no_reason() {
    let err = ClientError::ServerShutdown { reason: None };
    let msg = err.to_string();
    assert!(
        msg.contains("server shut down"),
        "should mention shutdown: {msg}"
    );
}

#[test]
fn client_error_display_connection_lost() {
    let err = ClientError::ConnectionLost(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe"));
    let msg = err.to_string();
    assert!(
        msg.contains("lost connection to server"),
        "should mention lost connection: {msg}"
    );
}

#[test]
fn decode_clipboard_payload_decodes_base64() {
    assert_eq!(decode_clipboard_payload("dGVzdA=="), Some(b"test".to_vec()));
}

#[test]
fn ioctl_cell_size_accepts_fractional_terminal_geometry() {
    assert_eq!(ioctl_cell_size(80, 24, 800, 480), Some((10, 20)));
    assert_eq!(ioctl_cell_size(80, 24, 805, 480), Some((10, 20)));
    assert_eq!(ioctl_cell_size(80, 24, 800, 485), Some((10, 20)));
    assert_eq!(ioctl_cell_size(80, 24, 0, 485), None);
}

#[test]
fn decode_clipboard_payload_rejects_invalid_base64() {
    assert_eq!(decode_clipboard_payload("not-base64!!!"), None);
}

#[test]
fn forward_clipboard_writes_osc52_to_the_supplied_test_sink() {
    let mut output = Vec::new();
    clipboard_forwarding::forward_clipboard("dGVzdA==", true, &mut output)
        .expect("valid base64 is written through OSC 52");
    assert_eq!(output, b"\x1b]52;c;dGVzdA==\x07");
    let error = clipboard_forwarding::forward_clipboard("not base64", true, &mut output)
        .expect_err("invalid base64 is rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

pub(crate) mod endpoint_choice;

mod surface_baseline;
