use super::*;
use crate::test_support::IsolatedEnv;

#[test]
fn resize_signal_reports_even_when_polled_size_is_unchanged() {
    let size = (120, 40, 8, 16, true);
    assert!(resize_report_required(true, size, size));
    assert!(!resize_report_required(false, size, size));
    assert!(resize_report_required(false, (120, 41, 8, 16, true), size));
    assert!(resize_report_required(false, (120, 40, 9, 18, true), size));
    assert!(resize_report_required(false, (120, 40, 8, 16, false), size));
}

#[test]
fn unavailable_terminal_grid_is_not_fabricated() {
    let reported_cell_size = AtomicU64::new(0);
    let err = current_terminal_geometry_with(false, false, &reported_cell_size, None, None, || {
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
    let reported_cell_size = AtomicU64::new(0);
    let geometry = current_terminal_geometry_with(
        true,
        true,
        &reported_cell_size,
        Some((9, 18)),
        None,
        || Ok((80, 24)),
    )
    .expect("grid geometry remains valid without pixel dimensions");

    assert_eq!(geometry, (80, 24, 9, 18, false));
}

#[test]
fn client_host_size_clamps_only_client_shell_grids() {
    let shell = terminal_geometry::ClientHostSize::new(0, u16::MAX, true);
    assert_eq!(shell.cols, 1);
    assert!((1..=crate::protocol::MAX_SURFACE_DIMENSION).contains(&shell.rows));
    assert!(
        usize::from(shell.cols) * usize::from(shell.rows) <= crate::protocol::MAX_SURFACE_CELLS
    );
    assert_eq!(
        terminal_geometry::ClientHostSize::new(0, u16::MAX, false),
        terminal_geometry::ClientHostSize {
            cols: 0,
            rows: u16::MAX,
        },
    );
}

#[test]
fn cell_geometry_is_bounded_before_wire_use_and_disables_inexact_pixel_mouse() {
    let (width, height, exact) = super::terminal_geometry::bounded_cell_geometry(
        crate::protocol::MAX_CELL_SIZE_PX + 1,
        crate::protocol::MAX_CELL_SIZE_PX + 2,
        true,
    );

    assert_eq!(
        (width, height, exact),
        (
            crate::protocol::MAX_CELL_SIZE_PX,
            crate::protocol::MAX_CELL_SIZE_PX,
            false,
        )
    );
}

#[test]
fn direct_notices_keep_only_the_most_recent_bounded_history() {
    let mut notices = std::collections::VecDeque::new();
    for index in 0..70 {
        remember_direct_notice(&mut notices, index.to_string());
    }

    assert_eq!(notices.len(), 64);
    assert_eq!(notices.front().map(String::as_str), Some("6"));
    assert_eq!(notices.back().map(String::as_str), Some("69"));
}

#[test]
fn remote_client_uses_extended_handshake_timeout() {
    let env = IsolatedEnv::new();
    env.set(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR, "local");

    assert_eq!(
        ClientProcessRole::from_env()
            .expect("role")
            .handshake_read_timeout(),
        REMOTE_HANDSHAKE_READ_TIMEOUT
    );
}

#[test]
fn client_process_role_keeps_launch_mode_after_environment_changes() {
    let env = IsolatedEnv::new();
    env.set(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR, "server");
    let role = ClientProcessRole::from_env().expect("valid launch role");
    env.remove(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR);
    assert_eq!(role.handshake_read_timeout(), REMOTE_HANDSHAKE_READ_TIMEOUT);
    assert_eq!(
        role.keybinding_source(),
        shell::ClientShellKeybindingSource::Endpoint
    );
}

#[test]
fn keybinding_source_refuses_unknown_values() {
    let env = IsolatedEnv::new();
    env.remove(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR);
    assert_eq!(
        ClientProcessRole::from_env().map(ClientProcessRole::keybinding_source),
        Ok(shell::ClientShellKeybindingSource::RemoteLocal)
    );
    env.set(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR, "local");
    assert_eq!(
        ClientProcessRole::from_env().map(ClientProcessRole::keybinding_source),
        Ok(shell::ClientShellKeybindingSource::RemoteLocal)
    );
    env.set(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR, "server");
    assert_eq!(
        ClientProcessRole::from_env().map(ClientProcessRole::keybinding_source),
        Ok(shell::ClientShellKeybindingSource::Endpoint)
    );
    for unknown in ["Server", "", "remote"] {
        env.set(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR, unknown);
        assert!(ClientProcessRole::from_env().is_err(), "{unknown:?}");
    }
}

#[test]
fn host_cursor_policy_auto_uses_platform_default() {
    // Both sides read the terminal environment another test changes.
    let _env = IsolatedEnv::new();
    assert_eq!(
        should_draw_host_cursor(crate::config::HostCursorModeConfig::Auto),
        crate::platform::should_draw_host_cursor_by_default()
    );
}

#[test]
fn host_cursor_policy_native_and_drawn_override_auto_detection() {
    let env = IsolatedEnv::new();
    env.set("TERM_PROGRAM", "WezTerm");

    assert!(!should_draw_host_cursor(
        crate::config::HostCursorModeConfig::Native
    ));
    assert!(should_draw_host_cursor(
        crate::config::HostCursorModeConfig::Drawn
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
        crate::host_term::theme::host_terminal_theme_query_sequence(
            crate::platform::should_query_host_terminal_palette(),
        )
        .as_bytes()
    );
    assert!(
        !output
            .windows(crate::host_term::theme::HOST_COLOR_SCHEME_QUERY_SEQUENCE.len())
            .any(|window| window
                == crate::host_term::theme::HOST_COLOR_SCHEME_QUERY_SEQUENCE.as_bytes())
    );
}

#[test]
fn write_host_color_scheme_report_mode_emits_mode_sequences() {
    let mut output = Vec::new();
    write_host_color_scheme_report_mode(&mut output, true).expect("test precondition");
    write_host_color_scheme_report_mode(&mut output, false).expect("test precondition");

    let mut expected = Vec::new();
    expected.extend_from_slice(
        crate::host_term::theme::HOST_COLOR_SCHEME_REPORT_ENABLE_SEQUENCE.as_bytes(),
    );
    expected.extend_from_slice(
        crate::host_term::theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE.as_bytes(),
    );
    assert_eq!(output, expected);
}

#[test]
fn color_scheme_change_event_requests_host_theme_query() {
    let events = crate::raw_input::parse_raw_input_bytes_sync(b"\x1b[?997;1n");

    assert!(crate::raw_input::events_require_host_terminal_theme_query(
        &events
    ));
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
    let events = crate::raw_input::parse_raw_input_bytes_sync(b"\x1b[?997;1n");
    assert_eq!(
        super::terminal_geometry::reported_cell_size_from_events(&events),
        None
    );

    let events = crate::raw_input::parse_raw_input_bytes_sync(b"\x1b[6;21;10t\x1b[6;18;9t");
    assert_eq!(
        super::terminal_geometry::reported_cell_size_from_events(&events),
        Some((9, 18))
    );
}

#[test]
fn color_scheme_reports_are_enabled_only_for_full_clients() {
    assert!(should_enable_host_color_scheme_reports(true));
    assert!(!should_enable_host_color_scheme_reports(false));
}

#[test]
fn terminal_restore_postlude_restores_visible_default_cursor() {
    let mut output = Vec::new();
    write_terminal_restore_postlude(&mut output, false).expect("test precondition");
    assert_eq!(output, b"\x1b[?25h\x1b[0 q");
}

#[test]
fn direct_attach_mouse_capture_combines_local_preference_with_child_demand() {
    assert!(effective_mouse_capture(false, true));
    assert!(effective_mouse_capture(true, false));
    assert!(!effective_mouse_capture(false, false));
    assert!(effective_sgr_pixel_mouse(true, true, true));
    assert!(!effective_sgr_pixel_mouse(true, true, false));
}

#[test]
fn terminal_restore_postlude_disables_color_scheme_reports_when_enabled() {
    let mut output = Vec::new();
    write_terminal_restore_postlude(&mut output, true).expect("test precondition");

    let mut expected = Vec::new();
    expected.extend_from_slice(
        crate::host_term::theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE.as_bytes(),
    );
    expected.extend_from_slice(b"\x1b[?25h\x1b[0 q");
    assert_eq!(output, expected);
}

#[test]
fn client_error_display_connection_failed() {
    let err = ClientError::ConnectionFailed(io::Error::new(
        io::ErrorKind::ConnectionRefused,
        "connection refused",
    ));
    let msg = err.to_string();
    assert!(
        msg.contains("failed to connect to server"),
        "should mention connection failure: {msg}"
    );
    assert!(
        msg.contains("shepr server"),
        "should suggest starting server: {msg}"
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
    assert!(!msg.contains("Is shepr server running?"));
}

#[test]
fn client_error_display_handshake_rejected() {
    let err = ClientError::HandshakeRejected {
        error: crate::protocol::HandshakeRefusal::InvalidSurface("incompatible".into()),
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
        reason: Some(crate::protocol::ShutdownReason::Message(
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
fn client_error_display_detached_default_session_reattach_hint() {
    let env = IsolatedEnv::new();
    env.remove(crate::remote::REATTACH_COMMAND_ENV_VAR);
    let err = ClientError::ServerShutdown {
        reason: Some(crate::protocol::ShutdownReason::Detached),
    };
    let paths = crate::config::AppPaths::default();
    let context = ClientErrorContext::new(
        paths.server_address().attach_command(paths.session_id()),
        std::env::var(crate::remote::REATTACH_COMMAND_ENV_VAR).ok(),
    );
    let msg = err.display_with_context(&context);
    assert!(
        msg.contains("Run `shepr` to reattach"),
        "should suggest default reattach command: {msg}"
    );
}

#[test]
fn client_error_display_detached_named_session_reattach_hint() {
    let env = IsolatedEnv::new();
    env.remove(crate::remote::REATTACH_COMMAND_ENV_VAR);
    let err = ClientError::ServerShutdown {
        reason: Some(crate::protocol::ShutdownReason::Detached),
    };
    let session = crate::session::SessionId::parse("work").expect("test precondition");
    let paths = crate::config::AppPaths::default();
    let context = ClientErrorContext::new(
        paths.server_address().attach_command(&session),
        std::env::var(crate::remote::REATTACH_COMMAND_ENV_VAR).ok(),
    );
    let msg = err.display_with_context(&context);
    assert!(
        msg.contains("Run `shepr session attach work` to reattach"),
        "should suggest named session reattach command: {msg}"
    );
}

#[test]
fn client_error_display_detached_remote_reattach_hint_takes_precedence() {
    let env = IsolatedEnv::new();
    env.set(
        crate::remote::REATTACH_COMMAND_ENV_VAR,
        "shepr --remote host --session work",
    );
    env.set(crate::session::SESSION_ENV_VAR, "work");
    let err = ClientError::ServerShutdown {
        reason: Some(crate::protocol::ShutdownReason::Detached),
    };
    let paths = crate::config::AppPaths::default();
    let context = ClientErrorContext::new(
        paths.server_address().attach_command(paths.session_id()),
        std::env::var(crate::remote::REATTACH_COMMAND_ENV_VAR).ok(),
    );
    let msg = err.display_with_context(&context);
    assert!(
        msg.contains("Run `shepr --remote host --session work` to reattach"),
        "should prefer remote reattach command: {msg}"
    );
}

#[test]
fn client_error_display_connection_lost() {
    let env = IsolatedEnv::new();
    env.remove(crate::remote::REATTACH_COMMAND_ENV_VAR);
    let err = ClientError::ConnectionLost(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe"));
    let msg = err.to_string();
    assert!(
        msg.contains("lost connection to server"),
        "should mention lost connection: {msg}"
    );
}

#[test]
fn client_error_display_remote_connection_lost_has_reattach_hint() {
    let env = IsolatedEnv::new();
    env.set(
        crate::remote::REATTACH_COMMAND_ENV_VAR,
        "shepr --remote host --session work",
    );
    let err = ClientError::ConnectionLost(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe"));
    let paths = crate::config::AppPaths::default();
    let context = ClientErrorContext::new(
        paths.server_address().attach_command(paths.session_id()),
        std::env::var(crate::remote::REATTACH_COMMAND_ENV_VAR).ok(),
    );
    let msg = err.display_with_context(&context);
    assert!(
        msg.contains("lost connection to remote Shepr"),
        "should mention remote connection loss: {msg}"
    );
    assert!(
        msg.contains("panes may still be running"),
        "should explain possible persistence: {msg}"
    );
    assert!(
        msg.contains("Run `shepr --remote host --session work` to reattach"),
        "should show remote reattach command: {msg}"
    );
}

#[test]
fn client_error_context_keeps_launch_reattach_command() {
    let env = IsolatedEnv::new();
    env.set(
        crate::remote::REATTACH_COMMAND_ENV_VAR,
        "shepr --remote first",
    );
    let paths = crate::config::AppPaths::default();
    let context = ClientErrorContext::new(
        paths.server_address().attach_command(paths.session_id()),
        std::env::var(crate::remote::REATTACH_COMMAND_ENV_VAR).ok(),
    );
    env.set(
        crate::remote::REATTACH_COMMAND_ENV_VAR,
        "shepr --remote second",
    );
    let error = ClientError::ConnectionLost(io::Error::new(io::ErrorKind::BrokenPipe, "closed"));
    let message = error.display_with_context(&context);
    assert!(message.contains("shepr --remote first"));
    assert!(!message.contains("shepr --remote second"));
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
fn forward_clipboard_uses_local_clipboard_path() {
    let env = IsolatedEnv::new();
    env.set("SSH_CONNECTION", "1 2 3 4");
    assert!(forward_clipboard("dGVzdA=="));
    assert!(!forward_clipboard("not base64"));
}
