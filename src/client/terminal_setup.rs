//! Terminal setup and restoration for the rendered client.

use std::io::{self, Write as _};
use std::os::fd::AsRawFd as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{DisableLineWrap, EnableLineWrap};

// ---------------------------------------------------------------------------
// Terminal setup / restore
// ---------------------------------------------------------------------------

/// Sets up the terminal for client mode (raw mode, optional mouse, keyboard enhancements).
///
/// Returns a guard that restores the terminal when dropped.
pub(super) fn setup_terminal(mouse_capture: bool) -> io::Result<TerminalGuard> {
    setup_terminal_with_capabilities(true, mouse_capture)
}

/// Sets up a direct attach terminal.
///
/// Direct attach forwards stdin to the attached PTY. When configured, mouse
/// capture lets wheel events drive the attached viewport or reach child
/// programs that requested mouse input.
pub(super) fn setup_direct_attach_terminal(mouse_capture: bool) -> io::Result<TerminalGuard> {
    setup_terminal_with_capabilities(false, mouse_capture)
}

pub(super) fn setup_terminal_with_capabilities(
    enable_client_protocols: bool,
    mouse_capture: bool,
) -> io::Result<TerminalGuard> {
    ratatui::init();
    let mut terminal_guard = TerminalGuard {
        host_escape_disambiguation_active: false,
        buffered_host_input: Vec::new(),
        reset_keyboard_enhancements: false,
        reset_modify_other_keys: false,
        reset_host_color_scheme_reports: false,
        restore_claimed: Arc::new(AtomicBool::new(false)),
        restored: false,
    };
    crate::host_term::modes::clear_host_mouse_reporting(&mut io::stdout())?;
    let host_color_scheme_reports =
        should_enable_host_color_scheme_reports(enable_client_protocols);

    let (host_escape_disambiguation_active, buffered_host_input) = if enable_client_protocols {
        terminal_guard.reset_keyboard_enhancements = true;
        push_keyboard_enhancement_flags()?;
        let (active, buffered_input) = query_host_escape_disambiguation();
        set_mouse_capture(mouse_capture, false)?;
        execute!(io::stdout(), EnableBracketedPaste, EnableFocusChange)?;
        if host_color_scheme_reports {
            terminal_guard.reset_host_color_scheme_reports = true;
            write_host_color_scheme_report_mode(&mut io::stdout(), true)?;
        }
        (active, buffered_input)
    } else {
        write_host_color_scheme_report_mode(&mut io::stdout(), false)?;
        set_mouse_capture(mouse_capture, false)?;
        execute!(io::stdout(), EnableBracketedPaste)?;
        (false, Vec::new())
    };

    let modify_other_keys_mode = enable_client_protocols
        .then(crate::input::host_modify_other_keys_mode)
        .flatten();
    if let Some(mode) = modify_other_keys_mode {
        terminal_guard.reset_modify_other_keys = true;
        io::stdout().write_all(mode.set_sequence())?;
        io::stdout().flush()?;
    }

    execute!(io::stdout(), DisableLineWrap)?;

    terminal_guard.host_escape_disambiguation_active = host_escape_disambiguation_active;
    terminal_guard.buffered_host_input = buffered_host_input;
    Ok(terminal_guard)
}

pub(super) fn should_enable_host_color_scheme_reports(enable_client_protocols: bool) -> bool {
    enable_client_protocols
}

/// Guard that restores the terminal when dropped.
pub(super) struct TerminalGuard {
    host_escape_disambiguation_active: bool,
    buffered_host_input: Vec<u8>,
    reset_keyboard_enhancements: bool,
    reset_modify_other_keys: bool,
    reset_host_color_scheme_reports: bool,
    restore_claimed: Arc<AtomicBool>,
    restored: bool,
}

const HOST_KEYBOARD_QUERY_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_BUFFERED_HOST_INPUT: usize = 64 * 1024;

fn query_host_escape_disambiguation() -> (bool, Vec<u8>) {
    const QUERY: &[u8] = b"\x1b[?u\x1b[c";

    let mut buffered_input = Vec::new();
    if let Err(err) = io::stdout()
        .write_all(QUERY)
        .and_then(|()| io::stdout().flush())
    {
        tracing::debug!(%err, "host keyboard enhancement query unavailable");
        return (false, buffered_input);
    }

    // Bypass StdinLock's shared buffer so poll and read observe the same bytes.
    let stdin = io::stdin();
    let stdin_fd = stdin.as_raw_fd();
    let deadline = Instant::now() + HOST_KEYBOARD_QUERY_TIMEOUT;
    let mut responses = crate::raw_input::HostKeyboardProbeResponses::default();
    while !responses.primary_device_attributes && buffered_input.len() < MAX_BUFFERED_HOST_INPUT {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        let timeout_ms = i32::try_from(remaining.as_millis())
            .unwrap_or(i32::MAX)
            .max(1);
        match crate::platform::poll_fd_readable(stdin_fd, timeout_ms) {
            Ok(true) => {}
            Ok(false) => break,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                tracing::debug!(%err, "host keyboard enhancement query read unavailable");
                break;
            }
        }

        let mut scratch = [0u8; 4096];
        let capacity = MAX_BUFFERED_HOST_INPUT - buffered_input.len();
        let read_limit = capacity.min(scratch.len());
        match crate::platform::read_fd(stdin_fd, &mut scratch[..read_limit]) {
            Ok(0) => break,
            Ok(read) => {
                buffered_input.extend_from_slice(&scratch[..read]);
                crate::raw_input::consume_host_keyboard_probe_responses(
                    &mut buffered_input,
                    &mut responses,
                );
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                tracing::debug!(%err, "host keyboard enhancement query read failed");
                break;
            }
        }
    }

    (
        host_escape_disambiguation_confirmed(&responses),
        buffered_input,
    )
}

fn host_escape_disambiguation_confirmed(
    responses: &crate::raw_input::HostKeyboardProbeResponses,
) -> bool {
    responses.primary_device_attributes
        && responses
            .flags
            .is_some_and(|flags| flags & 0b0000_0001 != 0)
}

pub(super) fn write_host_color_scheme_report_mode(
    writer: &mut impl io::Write,
    enabled: bool,
) -> io::Result<()> {
    let sequence = if enabled {
        crate::host_term::theme::HOST_COLOR_SCHEME_REPORT_ENABLE_SEQUENCE
    } else {
        crate::host_term::theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE
    };
    writer.write_all(sequence.as_bytes())?;
    writer.flush()
}

pub(super) fn write_terminal_restore_postlude(
    writer: &mut impl io::Write,
    reset_host_color_scheme_reports: bool,
) -> io::Result<()> {
    if reset_host_color_scheme_reports {
        writer.write_all(
            crate::host_term::theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE.as_bytes(),
        )?;
    }
    // Restore a visible cursor and reset DECSCUSR back to the terminal default.
    writer.write_all(b"\x1b[?25h\x1b[0 q")?;
    writer.flush()
}

pub(super) fn should_draw_host_cursor(mode: crate::config::HostCursorModeConfig) -> bool {
    match mode {
        crate::config::HostCursorModeConfig::Auto => {
            crate::platform::should_draw_host_cursor_by_default()
        }
        crate::config::HostCursorModeConfig::Native => false,
        crate::config::HostCursorModeConfig::Drawn => true,
    }
}

pub(super) fn effective_mouse_capture(
    server_enabled: bool,
    direct_attach_preference: bool,
) -> bool {
    server_enabled || direct_attach_preference
}

pub(super) fn effective_sgr_pixel_mouse(
    enabled: bool,
    requested: bool,
    exact_geometry: bool,
) -> bool {
    enabled && requested && exact_geometry
}

#[derive(Clone, Copy)]
struct EndpointMouseRequest {
    enabled: bool,
    sgr_pixels: bool,
}

/// Owns endpoint mouse requests, the local preferences used before an endpoint
/// takes control and after it releases control, and the mirrors read by stdin.
pub(super) struct HostMouseMode {
    direct_preference: bool,
    shell_preference: bool,
    endpoint_request: Option<EndpointMouseRequest>,
    use_preference: bool,
    capture_active: Arc<AtomicBool>,
    sgr_pixels_active: Arc<AtomicBool>,
}

impl HostMouseMode {
    pub(super) fn new(
        direct_preference: bool,
        shell_preference: bool,
        initially_active: bool,
    ) -> Self {
        Self {
            direct_preference,
            shell_preference,
            endpoint_request: None,
            use_preference: false,
            capture_active: Arc::new(AtomicBool::new(initially_active)),
            sgr_pixels_active: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn input_mirrors(&self) -> (Arc<AtomicBool>, Arc<AtomicBool>) {
        (
            Arc::clone(&self.capture_active),
            Arc::clone(&self.sgr_pixels_active),
        )
    }

    pub(super) fn shell_preference(&self) -> bool {
        self.shell_preference
    }

    pub(super) fn capture_active(&self) -> bool {
        self.capture_active.load(Ordering::Acquire)
    }

    pub(super) fn sgr_pixels_active(&self) -> bool {
        self.sgr_pixels_active.load(Ordering::Acquire)
    }

    pub(super) fn set_endpoint_request(&mut self, enabled: bool, sgr_pixels: bool) {
        self.endpoint_request = Some(EndpointMouseRequest {
            enabled,
            sgr_pixels,
        });
        self.use_preference = false;
    }

    pub(super) fn clear_endpoint_request(&mut self) {
        self.endpoint_request = None;
        self.use_preference = true;
    }

    pub(super) fn desired(&self, client_shell: bool) -> (bool, bool) {
        let (enabled, sgr_pixels_requested) = if let Some(request) = self.endpoint_request {
            (
                effective_mouse_capture(request.enabled, self.direct_preference),
                request.sgr_pixels,
            )
        } else if self.use_preference {
            (
                if client_shell {
                    self.shell_preference
                } else {
                    self.direct_preference
                },
                false,
            )
        } else {
            (self.capture_active(), self.sgr_pixels_active())
        };
        (enabled, sgr_pixels_requested)
    }

    pub(super) fn apply(
        &self,
        client_shell: bool,
        exact_geometry: bool,
        reassert: bool,
    ) -> io::Result<()> {
        let (enabled, sgr_pixels_requested) = self.desired(client_shell);
        let sgr_pixels = effective_sgr_pixel_mouse(enabled, sgr_pixels_requested, exact_geometry);
        let changed = host_mouse_capture_update(
            self.capture_active(),
            self.sgr_pixels_active(),
            enabled,
            sgr_pixels_requested,
            exact_geometry,
        );
        if changed.is_some() || reassert {
            set_mouse_capture(enabled, sgr_pixels)?;
        }
        self.capture_active.store(enabled, Ordering::Release);
        self.sgr_pixels_active.store(sgr_pixels, Ordering::Release);
        Ok(())
    }
}

pub(super) fn host_mouse_capture_update(
    current_enabled: bool,
    current_sgr_pixels: bool,
    enabled: bool,
    sgr_pixels_requested: bool,
    exact_geometry: bool,
) -> Option<(bool, bool)> {
    let sgr_pixels = effective_sgr_pixel_mouse(enabled, sgr_pixels_requested, exact_geometry);
    (current_enabled != enabled || current_sgr_pixels != sgr_pixels)
        .then_some((enabled, sgr_pixels))
}

pub(super) fn set_mouse_capture(enabled: bool, sgr_pixels: bool) -> io::Result<()> {
    crate::host_term::modes::clear_host_mouse_reporting(&mut io::stdout())?;
    if enabled {
        execute!(io::stdout(), EnableMouseCapture)?;
        if sgr_pixels {
            io::stdout().write_all(b"\x1b[?1016h")?;
            io::stdout().flush()?;
        }
        Ok(())
    } else {
        execute!(io::stdout(), DisableMouseCapture)
    }
}

fn restore_terminal_state_once(
    restore_claimed: &AtomicBool,
    reset_keyboard_enhancements: bool,
    reset_modify_other_keys: bool,
    reset_host_color_scheme_reports: bool,
) -> io::Result<()> {
    if restore_claimed.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    restore_terminal_state(
        reset_keyboard_enhancements,
        reset_modify_other_keys,
        reset_host_color_scheme_reports,
    )
}

fn restore_terminal_state(
    reset_keyboard_enhancements: bool,
    reset_modify_other_keys: bool,
    reset_host_color_scheme_reports: bool,
) -> io::Result<()> {
    // Reset modifyOtherKeys if we enabled it.
    if reset_modify_other_keys {
        let _ = io::stdout().write_all(b"\x1b[>4;0m");
        let _ = io::stdout().flush();
    }

    if reset_keyboard_enhancements {
        let _ = pop_keyboard_enhancement_flags();
    }

    let _ = execute!(
        io::stdout(),
        EnableLineWrap,
        DisableFocusChange,
        DisableBracketedPaste
    );
    let _ = set_mouse_capture(false, false);

    let restore_result = ratatui::try_restore();
    let postlude_result =
        write_terminal_restore_postlude(&mut io::stdout(), reset_host_color_scheme_reports);

    restore_result.and(postlude_result)
}

fn push_keyboard_enhancement_flags() -> io::Result<()> {
    execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(crate::input::ime_compatible_keyboard_enhancement_flags())
    )
}

fn pop_keyboard_enhancement_flags() -> io::Result<()> {
    execute!(io::stdout(), PopKeyboardEnhancementFlags)
}

impl TerminalGuard {
    pub(super) fn host_escape_disambiguation_active(&self) -> bool {
        self.host_escape_disambiguation_active
    }

    pub(super) fn take_buffered_host_input(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buffered_host_input)
    }

    /// Captures the restoration state for use by the process panic hook.
    pub(super) fn panic_restore(&self) -> impl Fn() + Send + Sync + 'static {
        let restore_claimed = Arc::clone(&self.restore_claimed);
        let reset_keyboard_enhancements = self.reset_keyboard_enhancements;
        let reset_modify_other_keys = self.reset_modify_other_keys;
        let reset_host_color_scheme_reports = self.reset_host_color_scheme_reports;
        move || {
            let _ = restore_terminal_state_once(
                &restore_claimed,
                reset_keyboard_enhancements,
                reset_modify_other_keys,
                reset_host_color_scheme_reports,
            );
        }
    }

    pub(super) fn restore(mut self) -> io::Result<()> {
        self.restored = true;
        restore_terminal_state_once(
            &self.restore_claimed,
            self.reset_keyboard_enhancements,
            self.reset_modify_other_keys,
            self.reset_host_color_scheme_reports,
        )
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.restored {
            let _ = restore_terminal_state_once(
                &self.restore_claimed,
                self.reset_keyboard_enhancements,
                self.reset_modify_other_keys,
                self.reset_host_color_scheme_reports,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_keyboard_probe_consumes_fragmented_responses_and_preserves_input() {
        let stream = b"before\x1b[?7u-middle-\x1b[?1;2cafter";

        for split in 1..stream.len() {
            let mut buffered = Vec::new();
            let mut responses = crate::raw_input::HostKeyboardProbeResponses::default();
            buffered.extend_from_slice(&stream[..split]);
            crate::raw_input::consume_host_keyboard_probe_responses(&mut buffered, &mut responses);
            buffered.extend_from_slice(&stream[split..]);
            crate::raw_input::consume_host_keyboard_probe_responses(&mut buffered, &mut responses);

            assert_eq!(responses.flags, Some(7), "split {split}");
            assert!(responses.primary_device_attributes, "split {split}");
            assert_eq!(buffered, b"before-middle-after", "split {split}");
        }
    }

    #[test]
    fn host_keyboard_probe_preserves_typed_input_before_responses() {
        let mut buffered = b"aPtyped\x1b[?7u\x1b[?1;2c".to_vec();
        let mut responses = crate::raw_input::HostKeyboardProbeResponses::default();

        crate::raw_input::consume_host_keyboard_probe_responses(&mut buffered, &mut responses);

        assert!(host_escape_disambiguation_confirmed(&responses));
        assert_eq!(buffered, b"aPtyped");
    }

    #[test]
    fn host_keyboard_probe_requires_disambiguation_bit_and_device_attributes() {
        for (flags, expected) in [(0, false), (2, false), (7, true)] {
            let mut buffered = format!("\x1b[?{flags}u\x1b[?1;2c").into_bytes();
            let mut responses = crate::raw_input::HostKeyboardProbeResponses::default();

            crate::raw_input::consume_host_keyboard_probe_responses(&mut buffered, &mut responses);

            assert_eq!(host_escape_disambiguation_confirmed(&responses), expected);
            assert!(buffered.is_empty());
        }
    }

    #[test]
    fn host_keyboard_probe_requires_flags_before_device_attributes() {
        let mut buffered = b"\x1b[?1;2c\x1b[?7uinput".to_vec();
        let mut responses = crate::raw_input::HostKeyboardProbeResponses::default();

        crate::raw_input::consume_host_keyboard_probe_responses(&mut buffered, &mut responses);

        assert_eq!(responses.flags, None);
        assert!(responses.primary_device_attributes);
        assert_eq!(buffered, b"input");
    }

    #[test]
    fn host_keyboard_probe_preserves_response_shaped_payloads() {
        let opaque = b"\x1b[200~paste \x1b[?1u \x1b[?1;2c\x1b[201~-\x1bPdata \x1b[?7u\x1b\\";
        let mut buffered = [opaque.as_slice(), b"\x1b[?7u\x1b[?1;2c"].concat();
        let mut responses = crate::raw_input::HostKeyboardProbeResponses::default();

        crate::raw_input::consume_host_keyboard_probe_responses(&mut buffered, &mut responses);

        assert!(host_escape_disambiguation_confirmed(&responses));
        assert_eq!(buffered, opaque);
    }

    #[test]
    fn host_keyboard_probe_preserves_malformed_responses() {
        let mut buffered = b"a\x1b[?7;1ub\x1b[?65536uc".to_vec();
        let mut responses = crate::raw_input::HostKeyboardProbeResponses::default();

        crate::raw_input::consume_host_keyboard_probe_responses(&mut buffered, &mut responses);

        assert_eq!(responses.flags, None);
        assert!(!responses.primary_device_attributes);
        assert_eq!(buffered, b"a\x1b[?7;1ub\x1b[?65536uc");
    }

    #[test]
    fn mouse_capture_update_only_emits_changed_modes_and_restores_exact_pixels() {
        assert_eq!(
            host_mouse_capture_update(true, false, true, true, false),
            None
        );
        assert_eq!(
            host_mouse_capture_update(true, false, true, true, true),
            Some((true, true))
        );
        assert_eq!(
            host_mouse_capture_update(true, true, true, true, true),
            None
        );
    }

    #[test]
    fn host_mouse_mode_owns_request_and_preference_resolution() {
        let mut mode = HostMouseMode::new(false, true, true);

        assert_eq!(mode.desired(true), (true, false));
        mode.set_endpoint_request(false, true);
        assert_eq!(mode.desired(true), (false, true));
        mode.clear_endpoint_request();
        assert_eq!(mode.desired(true), (true, false));
        assert_eq!(mode.desired(false), (false, false));
    }
}
