//! Terminal setup and restoration for the rendered client.

use std::io::{self, Write as _};
use std::os::fd::AsRawFd as _;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
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
    // Read before the terminal leaves cooked mode, so a refused variable is
    // reported on an ordinary terminal.
    let modify_other_keys_mode = if enable_client_protocols {
        shepr_termio::input::host_modify_other_keys_mode()?
    } else {
        None
    };
    ratatui::init();
    let host_modes = HostModes::new(false, false, mouse_capture);
    let mut terminal_guard = TerminalGuard {
        host_escape_disambiguation_active: false,
        buffered_host_input: Vec::new(),
        restore_claimed: Arc::new(AtomicBool::new(false)),
        host_modes: host_modes.clone(),
        restored: false,
    };
    let (host_escape_disambiguation_active, buffered_host_input) = if enable_client_protocols {
        host_modes.set_keyboard_enhancement_flags(
            &mut io::stdout(),
            shepr_termio::host_term::modes::ime_compatible_keyboard_enhancement_flags(),
        )?;
        let (active, buffered_input) = query_host_escape_disambiguation();
        host_modes.apply_mouse(true, false, true)?;
        host_modes.enable_bracketed_paste(&mut io::stdout())?;
        host_modes.enable_focus_change(&mut io::stdout())?;
        host_modes.enable_color_scheme_reports(&mut io::stdout())?;
        (active, buffered_input)
    } else {
        // Keep color-scheme reports out of the attached PTY's input. Direct
        // attach never enables them, so there is nothing to restore on exit.
        write_host_color_scheme_report_mode(&mut io::stdout(), false)?;
        host_modes.apply_mouse(false, false, true)?;
        host_modes.enable_bracketed_paste(&mut io::stdout())?;
        (false, Vec::new())
    };

    if let Some(mode) = modify_other_keys_mode {
        host_modes.set_modify_other_keys(&mut io::stdout(), mode)?;
    }

    host_modes.disable_line_wrap(&mut io::stdout())?;

    terminal_guard.host_escape_disambiguation_active = host_escape_disambiguation_active;
    terminal_guard.buffered_host_input = buffered_host_input;
    Ok(terminal_guard)
}

/// Guard that restores the terminal when dropped.
pub(super) struct TerminalGuard {
    host_escape_disambiguation_active: bool,
    buffered_host_input: Vec<u8>,
    restore_claimed: Arc<AtomicBool>,
    host_modes: HostModes,
    restored: bool,
}

const HOST_KEYBOARD_QUERY_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_BUFFERED_HOST_INPUT: usize = 64 * 1024;

fn query_host_escape_disambiguation() -> (bool, Vec<u8>) {
    let mut buffered_input = Vec::new();
    if let Err(err) = io::stdout()
        .write_all(shepr_termio::host_term::modes::HOST_KEYBOARD_QUERY_SEQUENCE)
        .and_then(|()| io::stdout().flush())
    {
        tracing::debug!(%err, "host keyboard enhancement query unavailable");
        return (false, buffered_input);
    }

    // Bypass StdinLock's shared buffer so poll and read observe the same bytes.
    let stdin = io::stdin();
    let stdin_fd = stdin.as_raw_fd();
    let deadline = Instant::now() + HOST_KEYBOARD_QUERY_TIMEOUT;
    let mut responses = shepr_termio::input::raw_input::HostKeyboardProbeResponses::default();
    while !responses.primary_device_attributes && buffered_input.len() < MAX_BUFFERED_HOST_INPUT {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        let timeout_ms = i32::try_from(remaining.as_millis())
            .unwrap_or(i32::MAX)
            .max(1);
        match shepr_platform::poll_fd_readable(stdin_fd, timeout_ms) {
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
        match shepr_platform::read_fd(stdin_fd, &mut scratch[..read_limit]) {
            Ok(0) => break,
            Ok(read) => {
                buffered_input.extend_from_slice(&scratch[..read]);
                shepr_termio::input::raw_input::consume_host_keyboard_probe_responses(
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
    responses: &shepr_termio::input::raw_input::HostKeyboardProbeResponses,
) -> bool {
    responses.primary_device_attributes
        && responses.flags.is_some_and(|flags| {
            flags & shepr_protocol::KittyKeyboardFlags::DISAMBIGUATE.bits() != 0
        })
}

pub(super) fn write_host_color_scheme_report_mode(
    writer: &mut impl io::Write,
    enabled: bool,
) -> io::Result<()> {
    let sequence = if enabled {
        shepr_termio::host_term::theme::HOST_COLOR_SCHEME_REPORT_ENABLE_SEQUENCE
    } else {
        shepr_termio::host_term::theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE
    };
    writer.write_all(sequence.as_bytes())?;
    writer.flush()
}

pub(super) fn write_terminal_restore_postlude(writer: &mut impl io::Write) -> io::Result<()> {
    // Restore a visible cursor and reset DECSCUSR back to the terminal default.
    writer.write_all(shepr_termio::host_term::modes::HOST_CURSOR_AND_SHAPE_RESTORE_SEQUENCE)?;
    writer.flush()
}

pub(super) fn should_draw_host_cursor(mode: shepr_config::HostCursorModeConfig) -> bool {
    match mode {
        shepr_config::HostCursorModeConfig::Native => false,
        shepr_config::HostCursorModeConfig::Drawn => true,
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

/// Tracks endpoint mouse requests, local preferences, and mirrors read by stdin.
/// The containing `HostModes` owner performs terminal teardown.
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

const RESTORE_KITTY_KEYBOARD_ENTRY: u8 = 1 << 0;
const RESTORE_MODIFY_OTHER_KEYS: u8 = 1 << 1;
const RESTORE_COLOR_SCHEME_REPORTS: u8 = 1 << 2;
const RESTORE_FOCUS_CHANGE: u8 = 1 << 3;
const RESTORE_BRACKETED_PASTE: u8 = 1 << 4;
const RESTORE_LINE_WRAP: u8 = 1 << 5;
const RESTORE_MOUSE_CAPTURE: u8 = 1 << 6;
const RESTORE_KEYBOARD_MASK: u8 = RESTORE_KITTY_KEYBOARD_ENTRY | RESTORE_MODIFY_OTHER_KEYS;

struct HostModesState {
    mouse: HostMouseMode,
    keyboard: shepr_termio::host_term::modes::DirectHostKeyboardState,
    pane_keyboard_report_all: bool,
    keyboard_report_all_active: bool,
}

struct HostModesInner {
    state: Mutex<HostModesState>,
    restore_state: AtomicU8,
    window_title_written: AtomicBool,
    title_stack_pushed: AtomicBool,
}

/// Shared owner for host modes changed while a client session is active.
/// `TerminalGuard` keeps the restoring handle; `ClientState` uses a clone to
/// apply endpoint requests without owning a separate copy of restoration state.
#[derive(Clone)]
pub(super) struct HostModes {
    inner: Arc<HostModesInner>,
}

impl HostModes {
    pub(super) fn new(
        direct_preference: bool,
        shell_preference: bool,
        initially_active: bool,
    ) -> Self {
        Self {
            inner: Arc::new(HostModesInner {
                state: Mutex::new(HostModesState {
                    mouse: HostMouseMode::new(
                        direct_preference,
                        shell_preference,
                        initially_active,
                    ),
                    keyboard: shepr_termio::host_term::modes::DirectHostKeyboardState::default(),
                    pane_keyboard_report_all: false,
                    keyboard_report_all_active: false,
                }),
                restore_state: AtomicU8::new(0),
                window_title_written: AtomicBool::new(false),
                title_stack_pushed: AtomicBool::new(false),
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, HostModesState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn record_keyboard_restore_state(&self, kitty_entry: bool, modify_other_keys: bool) {
        let mut state = 0;
        if kitty_entry {
            state |= RESTORE_KITTY_KEYBOARD_ENTRY;
        }
        if modify_other_keys {
            state |= RESTORE_MODIFY_OTHER_KEYS;
        }
        self.inner
            .restore_state
            .update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current & !RESTORE_KEYBOARD_MASK) | state
            });
    }

    fn record_keyboard_entry(&self) {
        self.inner
            .restore_state
            .fetch_or(RESTORE_KITTY_KEYBOARD_ENTRY, Ordering::AcqRel);
    }

    fn record_restore_flag(&self, flag: u8) {
        self.inner.restore_state.fetch_or(flag, Ordering::AcqRel);
    }

    pub(super) fn enable_bracketed_paste(&self, writer: &mut impl io::Write) -> io::Result<()> {
        self.record_restore_flag(RESTORE_BRACKETED_PASTE);
        execute!(writer, EnableBracketedPaste)
    }

    pub(super) fn enable_focus_change(&self, writer: &mut impl io::Write) -> io::Result<()> {
        self.record_restore_flag(RESTORE_FOCUS_CHANGE);
        execute!(writer, EnableFocusChange)
    }

    pub(super) fn disable_line_wrap(&self, writer: &mut impl io::Write) -> io::Result<()> {
        self.record_restore_flag(RESTORE_LINE_WRAP);
        execute!(writer, DisableLineWrap)
    }

    pub(super) fn enable_color_scheme_reports(
        &self,
        writer: &mut impl io::Write,
    ) -> io::Result<()> {
        self.record_restore_flag(RESTORE_COLOR_SCHEME_REPORTS);
        write_host_color_scheme_report_mode(writer, true)
    }

    pub(super) fn configure_mouse_mode(&self, mouse: HostMouseMode) {
        self.state().mouse = mouse;
    }

    pub(super) fn mouse_input_mirrors(&self) -> (Arc<AtomicBool>, Arc<AtomicBool>) {
        self.state().mouse.input_mirrors()
    }

    pub(super) fn mouse_shell_preference(&self) -> bool {
        self.state().mouse.shell_preference()
    }

    pub(super) fn set_mouse_endpoint_request(&self, enabled: bool, sgr_pixels: bool) {
        self.state().mouse.set_endpoint_request(enabled, sgr_pixels);
    }

    pub(super) fn clear_mouse_endpoint_request(&self) {
        self.state().mouse.clear_endpoint_request();
    }

    pub(super) fn apply_mouse(
        &self,
        client_shell: bool,
        exact_geometry: bool,
        reassert: bool,
    ) -> io::Result<()> {
        if reassert {
            self.record_restore_flag(RESTORE_MOUSE_CAPTURE);
        }
        self.state()
            .mouse
            .apply(client_shell, exact_geometry, reassert)
    }

    pub(super) fn set_keyboard_enhancement_flags(
        &self,
        writer: &mut impl io::Write,
        flags: crossterm::event::KeyboardEnhancementFlags,
    ) -> io::Result<()> {
        let flags = shepr_protocol::KittyKeyboardFlags::from_bits_retain(u16::from(flags.bits()));
        let mut state = self.state();
        let kitty_entry = !flags.is_empty();
        self.record_keyboard_restore_state(
            state.keyboard.has_kitty_keyboard_entry() || kitty_entry,
            state.keyboard.modify_other_keys_active(),
        );
        let result = shepr_termio::host_term::modes::set_direct_host_keyboard_protocol(
            writer,
            &mut state.keyboard,
            flags,
            shepr_vt::ModifyOtherKeysLevel::Off,
        );
        if result.is_ok() {
            self.record_keyboard_restore_state(kitty_entry, false);
        }
        result
    }

    pub(super) fn set_direct_keyboard_protocol(
        &self,
        writer: &mut impl io::Write,
        flags: shepr_protocol::KittyKeyboardFlags,
        modify_other_keys_level: shepr_vt::ModifyOtherKeysLevel,
    ) -> io::Result<()> {
        let mut state = self.state();
        let kitty_entry = !flags.is_empty();
        let modify_other_keys = modify_other_keys_level != shepr_vt::ModifyOtherKeysLevel::Off;
        self.record_keyboard_restore_state(
            state.keyboard.has_kitty_keyboard_entry() || kitty_entry,
            state.keyboard.modify_other_keys_active() || modify_other_keys,
        );
        let result = shepr_termio::host_term::modes::set_direct_host_keyboard_protocol(
            writer,
            &mut state.keyboard,
            flags,
            modify_other_keys_level,
        );
        if result.is_ok() {
            self.record_keyboard_restore_state(kitty_entry, modify_other_keys);
        }
        result
    }

    pub(super) fn set_modify_other_keys(
        &self,
        writer: &mut impl io::Write,
        level: shepr_vt::ModifyOtherKeysLevel,
    ) -> io::Result<()> {
        let mut state = self.state();
        let modify_other_keys = level != shepr_vt::ModifyOtherKeysLevel::Off;
        let kitty_entry = state.keyboard.has_kitty_keyboard_entry();
        self.record_keyboard_restore_state(
            kitty_entry,
            state.keyboard.modify_other_keys_active() || modify_other_keys,
        );
        let result = shepr_termio::host_term::modes::set_host_modify_other_keys(
            writer,
            &mut state.keyboard,
            level,
        );
        if result.is_ok() {
            self.record_keyboard_restore_state(kitty_entry, modify_other_keys);
        }
        result
    }

    pub(super) fn set_pane_keyboard_report_all(
        &self,
        writer: &mut impl io::Write,
        enabled: bool,
        shell_requests_report_all: bool,
    ) -> io::Result<()> {
        let mut state = self.state();
        state.pane_keyboard_report_all = enabled;
        let desired = state.pane_keyboard_report_all || shell_requests_report_all;
        if desired == state.keyboard_report_all_active {
            return Ok(());
        }
        // Report-all replaces the client's current entry. The helper tracks
        // whether that entry exists, so its first use never pops an outer one.
        self.record_keyboard_entry();
        shepr_termio::host_term::modes::set_host_kitty_keyboard_report_all(
            writer,
            &mut state.keyboard,
            desired,
        )?;
        state.keyboard_report_all_active = desired;
        Ok(())
    }

    /// Whether the host was last told to report every key as an escape code,
    /// so text keys also send their repeats and releases.
    pub(super) fn keyboard_report_all_active(&self) -> bool {
        self.state().keyboard_report_all_active
    }

    pub(super) fn sync_shell_keyboard_report_all(
        &self,
        writer: &mut impl io::Write,
        shell_requests_report_all: bool,
    ) -> io::Result<()> {
        let mut state = self.state();
        let desired = state.pane_keyboard_report_all || shell_requests_report_all;
        if desired == state.keyboard_report_all_active {
            return Ok(());
        }
        self.record_keyboard_entry();
        shepr_termio::host_term::modes::set_host_kitty_keyboard_report_all(
            writer,
            &mut state.keyboard,
            desired,
        )?;
        state.keyboard_report_all_active = desired;
        Ok(())
    }

    pub(super) fn write_window_title(
        &self,
        writer: &mut impl io::Write,
        title: Option<&str>,
    ) -> io::Result<()> {
        // Save the host's own title once, before the first write, so exit can
        // put it back (XTWINOPS 22/23; terminals without a title stack ignore
        // both and keep the "shepr" reset instead).
        if !self.inner.title_stack_pushed.swap(true, Ordering::AcqRel) {
            writer.write_all(shepr_termio::host_term::modes::HOST_WINDOW_TITLE_PUSH_SEQUENCE)?;
        }
        // Mark before writing so a partial write still gets a reset attempt.
        self.inner
            .window_title_written
            .store(true, Ordering::Release);
        shepr_termio::host_term::title::write_window_title(writer, title)
    }

    pub(super) fn reset_window_title(&self, writer: &mut impl io::Write) -> io::Result<()> {
        if self
            .inner
            .window_title_written
            .swap(false, Ordering::AcqRel)
        {
            shepr_termio::host_term::title::write_window_title(writer, None)?;
        }
        Ok(())
    }

    pub(super) fn restore(&self, writer: &mut impl io::Write) -> io::Result<()> {
        // These atomics let the panic hook restore without locking state that
        // may still be on the panicking thread's stack.
        let restore_state = self.inner.restore_state.swap(0, Ordering::AcqRel);
        let mut result = Ok(());
        let next = shepr_termio::host_term::modes::restore_host_keyboard_protocol(
            writer,
            restore_state & RESTORE_MODIFY_OTHER_KEYS != 0,
            restore_state & RESTORE_KITTY_KEYBOARD_ENTRY != 0,
        );
        if result.is_ok() {
            result = next;
        }
        if restore_state & RESTORE_COLOR_SCHEME_REPORTS != 0 {
            let next = write_host_color_scheme_report_mode(writer, false);
            if result.is_ok() {
                result = next;
            }
        }
        if restore_state & RESTORE_FOCUS_CHANGE != 0 {
            let next = execute!(writer, DisableFocusChange);
            if result.is_ok() {
                result = next;
            }
        }
        if restore_state & RESTORE_BRACKETED_PASTE != 0 {
            let next = execute!(writer, DisableBracketedPaste);
            if result.is_ok() {
                result = next;
            }
        }
        if restore_state & RESTORE_LINE_WRAP != 0 {
            let next = execute!(writer, EnableLineWrap);
            if result.is_ok() {
                result = next;
            }
        }
        if restore_state & RESTORE_MOUSE_CAPTURE != 0 {
            let next = set_mouse_capture_with_writer(writer, false, false);
            if result.is_ok() {
                result = next;
            }
        }
        let next = self.reset_window_title(writer);
        if result.is_ok() {
            result = next;
        }
        if self.inner.title_stack_pushed.swap(false, Ordering::AcqRel) {
            let next =
                writer.write_all(shepr_termio::host_term::modes::HOST_WINDOW_TITLE_POP_SEQUENCE);
            if result.is_ok() {
                result = next;
            }
        }
        let next = writer.flush();
        if result.is_ok() {
            result = next;
        }
        result
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
    set_mouse_capture_with_writer(&mut io::stdout(), enabled, sgr_pixels)
}

fn set_mouse_capture_with_writer(
    writer: &mut impl io::Write,
    enabled: bool,
    sgr_pixels: bool,
) -> io::Result<()> {
    shepr_termio::host_term::modes::clear_host_mouse_reporting(writer)?;
    if enabled {
        execute!(writer, EnableMouseCapture)?;
        if sgr_pixels {
            shepr_termio::host_term::modes::enable_host_sgr_pixel_mouse_reporting(writer)?;
        }
        Ok(())
    } else {
        execute!(writer, DisableMouseCapture)
    }
}

fn restore_terminal_state_once(
    restore_claimed: &AtomicBool,
    host_modes: &HostModes,
) -> io::Result<()> {
    if restore_claimed.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    restore_terminal_state(host_modes)
}

/// Restores every host mode, the raw mode and the screen, running each step
/// even after an earlier one fails. Each failure is logged here because the
/// panic hook and `Drop` have no caller to hand an error to; the first failure
/// is also returned.
fn restore_terminal_state(host_modes: &HostModes) -> io::Result<()> {
    // Runs first so the kitty keyboard pop reaches the host before the screen
    // is torn down; a failure here leaves the host terminal encoding keys.
    let modes_result = host_modes.restore(&mut io::stdout());
    if let Err(error) = &modes_result {
        tracing::warn!(
            error = %error,
            "failed to restore host terminal modes; keyboard protocol, mouse or paste modes may stay enabled"
        );
    }

    let restore_result = ratatui::try_restore();
    if let Err(error) = &restore_result {
        tracing::warn!(error = %error, "failed to restore host terminal screen and raw mode");
    }
    let postlude_result = write_terminal_restore_postlude(&mut io::stdout());
    if let Err(error) = &postlude_result {
        tracing::warn!(error = %error, "failed to write host terminal restore postlude");
    }

    modes_result.and(restore_result).and(postlude_result)
}

impl TerminalGuard {
    pub(super) fn host_escape_disambiguation_active(&self) -> bool {
        self.host_escape_disambiguation_active
    }

    pub(super) fn take_buffered_host_input(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buffered_host_input)
    }

    pub(super) fn host_modes(&self) -> HostModes {
        self.host_modes.clone()
    }

    /// Captures the restoration state for use by the process panic hook.
    pub(super) fn panic_restore(&self) -> impl Fn() + Send + Sync + 'static {
        let restore_claimed = Arc::clone(&self.restore_claimed);
        let host_modes = self.host_modes.clone();
        move || {
            // A panic has nowhere to report a restore failure, and
            // restore_terminal_state already logged each failed step.
            restore_terminal_state_once(&restore_claimed, &host_modes).ok();
        }
    }

    pub(super) fn restore(mut self) -> io::Result<()> {
        self.restored = true;
        restore_terminal_state_once(&self.restore_claimed, &self.host_modes)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.restored {
            // Drop cannot return the error, and restore_terminal_state already
            // logged each failed step.
            restore_terminal_state_once(&self.restore_claimed, &self.host_modes).ok();
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
            let mut responses =
                shepr_termio::input::raw_input::HostKeyboardProbeResponses::default();
            buffered.extend_from_slice(&stream[..split]);
            shepr_termio::input::raw_input::consume_host_keyboard_probe_responses(
                &mut buffered,
                &mut responses,
            );
            buffered.extend_from_slice(&stream[split..]);
            shepr_termio::input::raw_input::consume_host_keyboard_probe_responses(
                &mut buffered,
                &mut responses,
            );

            assert_eq!(responses.flags, Some(7), "split {split}");
            assert!(responses.primary_device_attributes, "split {split}");
            assert_eq!(buffered, b"before-middle-after", "split {split}");
        }
    }

    #[test]
    fn host_keyboard_probe_preserves_typed_input_before_responses() {
        let mut buffered = b"aPtyped\x1b[?7u\x1b[?1;2c".to_vec();
        let mut responses = shepr_termio::input::raw_input::HostKeyboardProbeResponses::default();

        shepr_termio::input::raw_input::consume_host_keyboard_probe_responses(
            &mut buffered,
            &mut responses,
        );

        assert!(host_escape_disambiguation_confirmed(&responses));
        assert_eq!(buffered, b"aPtyped");
    }

    #[test]
    fn host_keyboard_probe_requires_disambiguation_bit_and_device_attributes() {
        for (flags, expected) in [(0, false), (2, false), (7, true)] {
            let mut buffered = format!("\x1b[?{flags}u\x1b[?1;2c").into_bytes();
            let mut responses =
                shepr_termio::input::raw_input::HostKeyboardProbeResponses::default();

            shepr_termio::input::raw_input::consume_host_keyboard_probe_responses(
                &mut buffered,
                &mut responses,
            );

            assert_eq!(host_escape_disambiguation_confirmed(&responses), expected);
            assert!(buffered.is_empty());
        }
    }

    #[test]
    fn host_keyboard_probe_requires_flags_before_device_attributes() {
        let mut buffered = b"\x1b[?1;2c\x1b[?7uinput".to_vec();
        let mut responses = shepr_termio::input::raw_input::HostKeyboardProbeResponses::default();

        shepr_termio::input::raw_input::consume_host_keyboard_probe_responses(
            &mut buffered,
            &mut responses,
        );

        assert_eq!(responses.flags, None);
        assert!(responses.primary_device_attributes);
        assert_eq!(buffered, b"input");
    }

    #[test]
    fn host_keyboard_probe_preserves_response_shaped_payloads() {
        let opaque = b"\x1b[200~paste \x1b[?1u \x1b[?1;2c\x1b[201~-\x1bPdata \x1b[?7u\x1b\\";
        let mut buffered = [opaque.as_slice(), b"\x1b[?7u\x1b[?1;2c"].concat();
        let mut responses = shepr_termio::input::raw_input::HostKeyboardProbeResponses::default();

        shepr_termio::input::raw_input::consume_host_keyboard_probe_responses(
            &mut buffered,
            &mut responses,
        );

        assert!(host_escape_disambiguation_confirmed(&responses));
        assert_eq!(buffered, opaque);
    }

    #[test]
    fn host_keyboard_probe_preserves_malformed_responses() {
        let mut buffered = b"a\x1b[?7;1ub\x1b[?65536uc".to_vec();
        let mut responses = shepr_termio::input::raw_input::HostKeyboardProbeResponses::default();

        shepr_termio::input::raw_input::consume_host_keyboard_probe_responses(
            &mut buffered,
            &mut responses,
        );

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
    fn host_modes_restore_undoes_direct_keyboard_protocol_and_title_once() {
        let modes = HostModes::new(false, false, false);
        let mut output = Vec::new();
        modes
            .set_direct_keyboard_protocol(
                &mut output,
                shepr_protocol::KittyKeyboardFlags::from_bits_retain(3),
                shepr_vt::ModifyOtherKeysLevel::from_parameter(2),
            )
            .expect("write to a Vec");
        modes
            .write_window_title(&mut output, Some("agent"))
            .expect("write to a Vec");
        assert!(output.starts_with(b"\x1b[>3u\x1b[>4;2m\x1b[22;0t"));

        output.clear();
        modes.restore(&mut output).expect("write to a Vec");
        assert_eq!(output, b"\x1b[>4;0m\x1b[<1u\x1b]0;shepr\x07\x1b[23;0t");

        output.clear();
        modes.restore(&mut output).expect("write to a Vec");
        assert!(output.is_empty(), "restore runs once per change");
    }

    #[test]
    fn host_modes_report_all_without_a_prior_entry_pushes_and_restore_pops_it() {
        let modes = HostModes::new(false, false, false);
        let mut output = Vec::new();
        modes
            .set_pane_keyboard_report_all(&mut output, true, false)
            .expect("write to a Vec");
        assert_eq!(output, b"\x1b[>31u");

        output.clear();
        modes.restore(&mut output).expect("write to a Vec");
        assert_eq!(output, b"\x1b[<1u");
    }

    /// The bytes a shell client writes on setup and on restore (the panic hook
    /// and `Drop` both restore through `HostModes::restore`).
    #[test]
    fn host_modes_setup_and_restore_bytes() {
        for (level, set) in [
            (shepr_vt::ModifyOtherKeysLevel::All, b"\x1b[>4;2m"),
            (
                shepr_vt::ModifyOtherKeysLevel::ExceptWellDefined,
                b"\x1b[>4;1m",
            ),
        ] {
            let modes = HostModes::new(false, false, false);
            let mut output = Vec::new();
            modes
                .set_keyboard_enhancement_flags(
                    &mut output,
                    shepr_termio::host_term::modes::ime_compatible_keyboard_enhancement_flags(),
                )
                .expect("write to a Vec");
            modes
                .enable_bracketed_paste(&mut output)
                .expect("write to a Vec");
            modes
                .enable_focus_change(&mut output)
                .expect("write to a Vec");
            modes
                .enable_color_scheme_reports(&mut output)
                .expect("write to a Vec");
            modes
                .set_modify_other_keys(&mut output, level)
                .expect("write to a Vec");
            modes
                .disable_line_wrap(&mut output)
                .expect("write to a Vec");
            modes
                .write_window_title(&mut output, Some("agent"))
                .expect("write to a Vec");
            let expected_setup = [
                b"\x1b[>7u".as_slice(),
                b"\x1b[?2004h",
                b"\x1b[?1004h",
                b"\x1b[?2031h",
                set,
                b"\x1b[?7l",
                b"\x1b[22;0t",
            ]
            .concat();
            assert!(output.starts_with(&expected_setup), "{output:?}");

            // Mouse capture is enabled on stdout; mark it as the setup does.
            modes.record_restore_flag(RESTORE_MOUSE_CAPTURE);
            output.clear();
            modes.restore(&mut output).expect("write to a Vec");
            let expected_restore = [
                b"\x1b[>4;0m\x1b[<1u".as_slice(),
                b"\x1b[?2031l",
                b"\x1b[?1004l",
                b"\x1b[?2004l",
                b"\x1b[?7h",
                b"\x1b[?1006l\x1b[?1016l\x1b[?1015l\x1b[?1005l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?9l",
                b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l",
                b"\x1b]0;shepr\x07",
                b"\x1b[23;0t",
            ]
            .concat();
            assert_eq!(output, expected_restore);
        }

        let mut output = Vec::new();
        write_terminal_restore_postlude(&mut output).expect("write to a Vec");
        assert_eq!(output, b"\x1b[?25h\x1b[0 q");
        output.clear();
        set_mouse_capture_with_writer(&mut output, true, true).expect("write to a Vec");
        assert!(output.ends_with(b"\x1b[?1016h"), "{output:?}");
        assert_eq!(
            shepr_termio::host_term::modes::HOST_KEYBOARD_QUERY_SEQUENCE,
            b"\x1b[?u\x1b[c"
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
