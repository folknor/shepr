use super::*;
use std::ffi::OsString;
use std::sync::{Mutex, OnceLock};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

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

fn restore_env_var(key: &str, value: Option<OsString>) {
    if let Some(value) = value {
        unsafe { std::env::set_var(key, value) };
    } else {
        unsafe { std::env::remove_var(key) };
    }
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        restore_env_var(self.key, self.previous.clone());
    }
}

struct EnvVarsRemovedGuard {
    previous: Vec<(&'static str, Option<OsString>)>,
}

impl EnvVarsRemovedGuard {
    fn new(keys: &[&'static str]) -> Self {
        let previous: Vec<_> = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in keys {
            unsafe { std::env::remove_var(key) };
        }
        Self { previous }
    }
}

impl Drop for EnvVarsRemovedGuard {
    fn drop(&mut self) {
        for (key, value) in self.previous.clone() {
            restore_env_var(key, value);
        }
    }
}

#[test]
fn remote_client_uses_extended_handshake_timeout() {
    let _guard = env_lock().lock().expect("test precondition");
    let _remote = EnvVarGuard::set(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR, "local");

    assert_eq!(handshake_read_timeout(), REMOTE_HANDSHAKE_READ_TIMEOUT);
}

#[test]
fn host_cursor_policy_auto_uses_platform_default() {
    assert_eq!(
        should_draw_host_cursor(crate::config::HostCursorModeConfig::Auto),
        crate::platform::should_draw_host_cursor_by_default()
    );
}

#[test]
fn host_cursor_policy_native_and_drawn_override_auto_detection() {
    let _guard = env_lock().lock().expect("test precondition");
    let _env = EnvVarGuard::set("TERM_PROGRAM", "WezTerm");

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
        crate::terminal_theme::host_terminal_theme_query_sequence(
            crate::platform::should_query_host_terminal_palette(),
        )
        .as_bytes()
    );
    assert!(
        !output
            .windows(crate::terminal_theme::HOST_COLOR_SCHEME_QUERY_SEQUENCE.len())
            .any(|window| window
                == crate::terminal_theme::HOST_COLOR_SCHEME_QUERY_SEQUENCE.as_bytes())
    );
}

#[test]
fn write_host_color_scheme_report_mode_emits_mode_sequences() {
    let mut output = Vec::new();
    write_host_color_scheme_report_mode(&mut output, true).expect("test precondition");
    write_host_color_scheme_report_mode(&mut output, false).expect("test precondition");

    let mut expected = Vec::new();
    expected.extend_from_slice(
        crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_ENABLE_SEQUENCE.as_bytes(),
    );
    expected.extend_from_slice(
        crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE.as_bytes(),
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
fn host_terminal_theme_query_is_enabled() {
    assert!(should_query_host_terminal_theme());
}

#[test]
fn write_host_cell_size_query_emits_xtwinops_request() {
    let mut output = Vec::new();
    write_host_cell_size_query(&mut output).expect("test precondition");

    assert_eq!(output, b"\x1b[16t");
}

#[test]
fn host_cell_size_query_is_enabled() {
    assert!(should_query_host_cell_size());
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
        crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE.as_bytes(),
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
fn client_error_display_handshake_rejected() {
    let err = ClientError::HandshakeRejected {
        version: 1,
        error: "incompatible".into(),
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
        reason: Some("maintenance".into()),
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
    let _guard = env_lock().lock().expect("test precondition");
    let _env = EnvVarsRemovedGuard::new(&[
        crate::remote::REATTACH_COMMAND_ENV_VAR,
        crate::session::SESSION_ENV_VAR,
    ]);
    let err = ClientError::ServerShutdown {
        reason: Some("detached".into()),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("Run `shepr` to reattach"),
        "should suggest default reattach command: {msg}"
    );
}

#[test]
fn client_error_display_detached_named_session_reattach_hint() {
    let _guard = env_lock().lock().expect("test precondition");
    let _remote_env = EnvVarsRemovedGuard::new(&[crate::remote::REATTACH_COMMAND_ENV_VAR]);
    let _session_env = EnvVarGuard::set(crate::session::SESSION_ENV_VAR, "work");
    let err = ClientError::ServerShutdown {
        reason: Some("detached".into()),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("Run `shepr session attach work` to reattach"),
        "should suggest named session reattach command: {msg}"
    );
}

#[test]
fn client_error_display_detached_remote_reattach_hint_takes_precedence() {
    let _guard = env_lock().lock().expect("test precondition");
    let _remote_env = EnvVarGuard::set(
        crate::remote::REATTACH_COMMAND_ENV_VAR,
        "shepr --remote host --session work",
    );
    let _session_env = EnvVarGuard::set(crate::session::SESSION_ENV_VAR, "work");
    let err = ClientError::ServerShutdown {
        reason: Some("detached".into()),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("Run `shepr --remote host --session work` to reattach"),
        "should prefer remote reattach command: {msg}"
    );
}

#[test]
fn client_error_display_connection_lost() {
    let _guard = env_lock().lock().expect("test precondition");
    let _env = EnvVarsRemovedGuard::new(&[crate::remote::REATTACH_COMMAND_ENV_VAR]);
    let err = ClientError::ConnectionLost(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe"));
    let msg = err.to_string();
    assert!(
        msg.contains("lost connection to server"),
        "should mention lost connection: {msg}"
    );
}

#[test]
fn client_error_display_remote_connection_lost_has_reattach_hint() {
    let _guard = env_lock().lock().expect("test precondition");
    let _remote_env = EnvVarGuard::set(
        crate::remote::REATTACH_COMMAND_ENV_VAR,
        "shepr --remote host --session work",
    );
    let err = ClientError::ConnectionLost(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe"));
    let msg = err.to_string();
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
    let _guard = env_lock().lock().expect("test precondition");
    let _ssh = EnvVarGuard::set("SSH_CONNECTION", "1 2 3 4");
    assert!(forward_clipboard("dGVzdA=="));
    assert!(!forward_clipboard("not base64"));
}
