//! Terminal setup and restoration for the rendered client.

use std::io::{self, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use crate::deadline::Deadline;
use crate::input::EscapeDisambiguation;
use crate::limits::{
    HOST_INPUT_READ_CHUNK_BYTES, HOST_KEYBOARD_QUERY_TIMEOUT, MAX_BUFFERED_HOST_INPUT,
};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use shepr_core::geometry::HostCell;
use shepr_term::mouse::HostMouseCapture;

// ---------------------------------------------------------------------------
// Terminal setup / restore
// ---------------------------------------------------------------------------

/// Sets up the terminal for client mode (raw mode, optional mouse, keyboard enhancements)
/// and sets its window title to `window_title`, which stays for the whole session.
///
/// Returns the output writer and a guard that restores the terminal when dropped.
pub(super) fn setup_terminal(
    mouse_capture: bool,
    modify_other_keys_mode: Option<shepr_term::ModifyOtherKeysLevel>,
    window_title: &str,
) -> io::Result<(TerminalGuard, HostTerminalWriter)> {
    let output_writer = HostTerminalWriter::from_stdout()?;
    let host_modes = HostModes::new(mouse_capture);
    // Built before raw mode so a failure anywhere below still restores through Drop. Raw mode
    // goes through crossterm; screen and mode writes use this writer directly rather than
    // `ratatui::init`, whose own panic hook would restore through `io::stdout()`.
    let mut terminal_guard = TerminalGuard {
        escape_disambiguation: EscapeDisambiguation::Inactive,
        buffered_host_input: Vec::new(),
        host_modes: host_modes.clone(),
        output_writer: output_writer.clone(),
        restored: false,
        restore_state: restore_terminal_state,
    };
    // The runtime policy can retry a mode change while the event loop is live. Startup must
    // abort if raw mode, the alternate screen, or required input modes cannot be established;
    // the armed guard restores any setup changes that reached the host.
    crossterm::terminal::enable_raw_mode()?;
    let mut output = output_writer.clone();
    write_dec_mode(&mut output, shepr_term::DecMode::AlternateScreen, true)?;
    // Seed the blitter's numeric cursor-shape cache with the host's default.
    output.write_all(shepr_termio::host_term::modes::HOST_CURSOR_SHAPE_DEFAULT_SEQUENCE)?;
    output.flush()?;
    host_modes.set_keyboard_enhancement_flags(
        &mut output,
        shepr_termio::host_term::modes::ime_compatible_keyboard_enhancement_flags(),
    )?;
    let (escape_disambiguation, buffered_host_input) =
        query_host_escape_disambiguation(&mut output);
    host_modes.reassert_mouse(&mut output, HostCell::Unknown)?;
    host_modes.enable_bracketed_paste(&mut output)?;
    host_modes.enable_focus_change(&mut output)?;
    host_modes.enable_color_scheme_reports(&mut output)?;

    if let Some(mode) = modify_other_keys_mode {
        host_modes.set_modify_other_keys(&mut output, mode)?;
    }

    host_modes.disable_line_wrap(&mut output)?;

    // The client owns the outer window title: nothing else writes it while the
    // client runs, whichever machine is presented, and the guard restores the
    // host's own title on exit. A title the host did not take is cosmetic, so
    // it does not fail the launch.
    if let Err(error) = host_modes.write_window_title(&mut output, Some(window_title)) {
        shepr_platform::structured_log!(WARN, event = terminal.title, outcome = "error", %error, "the host terminal's window title could not be set");
    }

    terminal_guard.escape_disambiguation = escape_disambiguation;
    terminal_guard.buffered_host_input = buffered_host_input;
    Ok((terminal_guard, output_writer))
}

/// Owns one duplicate of stdout. Writes use the file descriptor directly, so panic and Drop
/// restoration do not depend on the standard output lock held by another client operation.
#[derive(Clone)]
pub(super) struct HostTerminalWriter(Arc<std::fs::File>);

impl HostTerminalWriter {
    fn from_stdout() -> io::Result<Self> {
        // stdout-handoff-ok: the client owns the host terminal; this takes fd 1
        // over for escape sequences and frames, not text. `try_clone_to_owned`
        // duplicates with close-on-exec set.
        let owned_fd = io::stdout().as_fd().try_clone_to_owned()?;
        Ok(Self(Arc::new(std::fs::File::from(owned_fd))))
    }
}

impl io::Write for HostTerminalWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut file = self.0.as_ref();
        file.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut file = self.0.as_ref();
        file.flush()
    }
}

/// Guard that restores the terminal when dropped.
pub(super) struct TerminalGuard {
    escape_disambiguation: EscapeDisambiguation,
    buffered_host_input: Vec<u8>,
    host_modes: HostModes,
    output_writer: HostTerminalWriter,
    restored: bool,
    /// `restore_terminal_state`; tests put a panicking one here.
    restore_state: fn(&HostModes, &mut HostTerminalWriter) -> io::Result<()>,
}

fn query_host_escape_disambiguation(
    writer: &mut impl io::Write,
) -> (EscapeDisambiguation, Vec<u8>) {
    let mut buffered_input = Vec::new();
    if let Err(err) = writer
        .write_all(shepr_termio::host_term::modes::HOST_KEYBOARD_QUERY_SEQUENCE)
        .and_then(|()| writer.flush())
    {
        tracing::debug!(error = %err, "host keyboard enhancement query unavailable");
        return (EscapeDisambiguation::Inactive, buffered_input);
    }

    // Bypass StdinLock's shared buffer so poll and read observe the same bytes.
    let stdin = io::stdin();
    let stdin_fd = stdin.as_raw_fd();
    // clock-io-ok: bound the host terminal query and its poll/read loop.
    let deadline = Deadline::after(Instant::now(), HOST_KEYBOARD_QUERY_TIMEOUT);
    let mut responses = shepr_termio::input::raw_input::HostKeyboardProbeResponses::default();
    while !responses.primary_device_attributes && buffered_input.len() < MAX_BUFFERED_HOST_INPUT {
        // clock-io-ok: account for elapsed poll and read time in the query budget.
        let Some(remaining) = deadline.remaining(Instant::now()) else {
            break;
        };
        match shepr_platform::poll_fd_readable(stdin_fd, remaining) {
            Ok(true) => {}
            Ok(false) => break,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                tracing::debug!(error = %err, "host keyboard enhancement query read unavailable");
                break;
            }
        }

        let mut scratch = [0u8; HOST_INPUT_READ_CHUNK_BYTES];
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
                tracing::debug!(error = %err, "host keyboard enhancement query read failed");
                break;
            }
        }
    }

    let escape_disambiguation = if host_escape_disambiguation_confirmed(&responses) {
        EscapeDisambiguation::Active
    } else {
        EscapeDisambiguation::Inactive
    };
    (escape_disambiguation, buffered_input)
}

fn host_escape_disambiguation_confirmed(
    responses: &shepr_termio::input::raw_input::HostKeyboardProbeResponses,
) -> bool {
    responses.primary_device_attributes
        && responses
            .flags
            .is_some_and(|flags| flags.contains(shepr_protocol::KittyKeyboardFlags::DISAMBIGUATE))
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
    // Restore a visible cursor and reset DECSCUSR back to the terminal default,
    // in one write.
    let show_cursor = shepr_term::seq::DecModeSequence::new(shepr_term::DecMode::ShowCursor, true);
    let shape = shepr_termio::host_term::modes::HOST_CURSOR_SHAPE_DEFAULT_SEQUENCE;
    let mut postlude = Vec::with_capacity(show_cursor.as_bytes().len() + shape.len());
    postlude.extend_from_slice(show_cursor.as_bytes());
    postlude.extend_from_slice(shape);
    writer.write_all(&postlude)?;
    writer.flush()
}

/// Sets or resets one DEC private mode with a single write, then flushes.
fn write_dec_mode(
    writer: &mut impl io::Write,
    mode: shepr_term::DecMode,
    enabled: bool,
) -> io::Result<()> {
    writer.write_all(shepr_term::seq::DecModeSequence::new(mode, enabled).as_bytes())?;
    writer.flush()
}

/// Whether applying the mouse mode writes only a change or repeats the mode
/// the host already has (after a host event that may have reset it).
#[derive(Clone, Copy, PartialEq, Eq)]
enum MouseWrite {
    OnChange,
    Always,
}

#[derive(Clone, Copy)]
enum MouseSource {
    Initial,
    Preference,
    Endpoint(HostMouseCapture),
}

#[derive(Clone)]
pub(super) struct HostMouseInputProbe {
    capture_active: Arc<AtomicBool>,
    sgr_pixels_active: Arc<AtomicBool>,
}

impl HostMouseInputProbe {
    pub(super) fn capture_active(&self) -> bool {
        self.capture_active.load(Ordering::Acquire)
    }

    pub(super) fn sgr_pixels_active(&self) -> bool {
        self.sgr_pixels_active.load(Ordering::Acquire)
    }
}

/// Tracks endpoint mouse requests, local preferences, and mirrors read by stdin.
/// The containing `HostModes` owner performs terminal teardown.
pub(super) struct HostMouseMode {
    shell_preference: bool,
    source: MouseSource,
    capture_active: Arc<AtomicBool>,
    sgr_pixels_active: Arc<AtomicBool>,
}

impl HostMouseMode {
    /// A mouse mode whose preference is `shell_preference`, with capture
    /// already active exactly when the preference asks for it (startup enables
    /// the configured capture before anything else decides).
    pub(super) fn new(shell_preference: bool) -> Self {
        Self {
            shell_preference,
            source: MouseSource::Initial,
            capture_active: Arc::new(AtomicBool::new(shell_preference)),
            sgr_pixels_active: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn input_probe(&self) -> HostMouseInputProbe {
        HostMouseInputProbe {
            capture_active: Arc::clone(&self.capture_active),
            sgr_pixels_active: Arc::clone(&self.sgr_pixels_active),
        }
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

    pub(super) fn set_endpoint_request(&mut self, mode: HostMouseCapture) {
        self.source = MouseSource::Endpoint(mode);
    }

    pub(super) fn clear_endpoint_request(&mut self) {
        self.source = MouseSource::Preference;
    }

    pub(super) fn desired(&self) -> HostMouseCapture {
        match self.source {
            MouseSource::Initial => {
                HostMouseCapture::new(self.capture_active(), self.sgr_pixels_active())
            }
            MouseSource::Preference => HostMouseCapture::new(self.shell_preference, false),
            MouseSource::Endpoint(mode) => mode,
        }
    }

    fn apply(
        &self,
        writer: &mut impl io::Write,
        host: HostCell,
        write: MouseWrite,
    ) -> io::Result<()> {
        let requested = self.desired();
        let current = HostMouseCapture::new(self.capture_active(), self.sgr_pixels_active());
        let changed = host_mouse_capture_update(current, requested, host);
        let mode = requested.effective(host);
        if changed.is_some() || write == MouseWrite::Always {
            set_mouse_capture_with_writer(writer, mode)?;
        }
        self.capture_active.store(mode.enabled(), Ordering::Release);
        self.sgr_pixels_active
            .store(mode.pixels(), Ordering::Release);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostRestoreFlag {
    KittyKeyboardEntry,
    ModifyOtherKeys,
    ColorSchemeReports,
    FocusChange,
    BracketedPaste,
    LineWrap,
    MouseCapture,
}

impl HostRestoreFlag {
    const fn bit(self) -> u8 {
        match self {
            Self::KittyKeyboardEntry => 1 << 0,
            Self::ModifyOtherKeys => 1 << 1,
            Self::ColorSchemeReports => 1 << 2,
            Self::FocusChange => 1 << 3,
            Self::BracketedPaste => 1 << 4,
            Self::LineWrap => 1 << 5,
            Self::MouseCapture => 1 << 6,
        }
    }
}

/// Atomic restoration uses a compact bitset; named flags keep those bits local to this owner.
#[derive(Clone, Copy, Debug, Default)]
struct HostRestoreMask(u8);

impl HostRestoreMask {
    fn contains(self, flag: HostRestoreFlag) -> bool {
        self.0 & flag.bit() != 0
    }

    fn replace_keyboard(self, restore: KeyboardRestore) -> Self {
        let keyboard_bits =
            HostRestoreFlag::KittyKeyboardEntry.bit() | HostRestoreFlag::ModifyOtherKeys.bit();
        let mut next = self.0 & !keyboard_bits;
        if restore.kitty_entry {
            next |= HostRestoreFlag::KittyKeyboardEntry.bit();
        }
        if restore.modify_other_keys {
            next |= HostRestoreFlag::ModifyOtherKeys.bit();
        }
        Self(next)
    }

    fn bits(self) -> u8 {
        self.0
    }
}

struct HostModesState {
    mouse: HostMouseMode,
    keyboard: shepr_termio::host_term::modes::HostKeyboardState,
    pane_keyboard_report_all: bool,
    keyboard_report_all_active: bool,
}

enum HostKeyboardUpdate {
    EnhancementFlags(shepr_protocol::KittyKeyboardFlags),
    ModifyOtherKeys(shepr_term::ModifyOtherKeysLevel),
}

/// Which keyboard protocol modes the host may hold and shepr must restore.
#[derive(Clone, Copy)]
struct KeyboardRestore {
    kitty_entry: bool,
    modify_other_keys: bool,
}

impl HostKeyboardUpdate {
    fn restore_state(
        &self,
        keyboard: &shepr_termio::host_term::modes::HostKeyboardState,
    ) -> KeyboardRestore {
        match self {
            Self::EnhancementFlags(flags) => KeyboardRestore {
                kitty_entry: !flags.is_empty(),
                modify_other_keys: false,
            },
            Self::ModifyOtherKeys(level) => KeyboardRestore {
                kitty_entry: keyboard.has_kitty_keyboard_entry(),
                modify_other_keys: *level != shepr_term::ModifyOtherKeysLevel::Off,
            },
        }
    }

    fn apply<W: io::Write>(
        self,
        writer: &mut W,
        keyboard: &mut shepr_termio::host_term::modes::HostKeyboardState,
    ) -> io::Result<()> {
        match self {
            Self::EnhancementFlags(flags) => {
                shepr_termio::host_term::modes::set_host_keyboard_protocol(
                    writer,
                    keyboard,
                    flags,
                    shepr_term::ModifyOtherKeysLevel::Off,
                )
            }
            Self::ModifyOtherKeys(level) => {
                shepr_termio::host_term::modes::set_host_modify_other_keys(writer, keyboard, level)
            }
        }
    }
}

enum HostRestoreAction<W> {
    IfSet(HostRestoreFlag, fn(&HostModes, &mut W) -> io::Result<()>),
    Always(fn(&HostModes, &mut W) -> io::Result<()>),
}

impl<W> HostRestoreAction<W> {
    fn action(self, mask: HostRestoreMask) -> Option<fn(&HostModes, &mut W) -> io::Result<()>> {
        match self {
            Self::IfSet(flag, action) if mask.contains(flag) => Some(action),
            Self::Always(action) => Some(action),
            _ => None,
        }
    }
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
    pub(super) fn new(mouse_capture: bool) -> Self {
        Self {
            inner: Arc::new(HostModesInner {
                state: Mutex::new(HostModesState {
                    mouse: HostMouseMode::new(mouse_capture),
                    keyboard: shepr_termio::host_term::modes::HostKeyboardState::default(),
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

    fn record_keyboard_restore_state(&self, restore: KeyboardRestore) {
        self.inner
            .restore_state
            .update(Ordering::AcqRel, Ordering::Acquire, |current| {
                HostRestoreMask(current).replace_keyboard(restore).bits()
            });
    }

    fn record_keyboard_entry(&self) {
        self.inner
            .restore_state
            .fetch_or(HostRestoreFlag::KittyKeyboardEntry.bit(), Ordering::AcqRel);
    }

    fn record_restore_flag(&self, flag: HostRestoreFlag) {
        self.inner
            .restore_state
            .fetch_or(flag.bit(), Ordering::AcqRel);
    }

    pub(super) fn enable_bracketed_paste(&self, writer: &mut impl io::Write) -> io::Result<()> {
        self.record_restore_flag(HostRestoreFlag::BracketedPaste);
        write_dec_mode(writer, shepr_term::DecMode::BracketedPaste, true)
    }

    pub(super) fn enable_focus_change(&self, writer: &mut impl io::Write) -> io::Result<()> {
        self.record_restore_flag(HostRestoreFlag::FocusChange);
        write_dec_mode(writer, shepr_term::DecMode::FocusEvents, true)
    }

    pub(super) fn disable_line_wrap(&self, writer: &mut impl io::Write) -> io::Result<()> {
        self.record_restore_flag(HostRestoreFlag::LineWrap);
        write_dec_mode(writer, shepr_term::DecMode::LineWrap, false)
    }

    pub(super) fn enable_color_scheme_reports(
        &self,
        writer: &mut impl io::Write,
    ) -> io::Result<()> {
        self.record_restore_flag(HostRestoreFlag::ColorSchemeReports);
        write_host_color_scheme_report_mode(writer, true)
    }

    pub(super) fn mouse_input_probe(&self) -> HostMouseInputProbe {
        self.state().mouse.input_probe()
    }

    pub(super) fn mouse_shell_preference(&self) -> bool {
        self.state().mouse.shell_preference()
    }

    pub(super) fn set_mouse_endpoint_request(&self, mode: HostMouseCapture) {
        self.state().mouse.set_endpoint_request(mode);
    }

    pub(super) fn clear_mouse_endpoint_request(&self) {
        self.state().mouse.clear_endpoint_request();
    }

    /// Applies the desired mouse mode, writing only what changed.
    pub(super) fn apply_mouse(
        &self,
        writer: &mut impl io::Write,
        host: HostCell,
    ) -> io::Result<()> {
        self.write_mouse(writer, host, MouseWrite::OnChange)
    }

    /// Applies the desired mouse mode and writes it even when unchanged, for
    /// a host that may have reset it.
    pub(super) fn reassert_mouse(
        &self,
        writer: &mut impl io::Write,
        host: HostCell,
    ) -> io::Result<()> {
        self.write_mouse(writer, host, MouseWrite::Always)
    }

    fn write_mouse(
        &self,
        writer: &mut impl io::Write,
        host: HostCell,
        write: MouseWrite,
    ) -> io::Result<()> {
        let state = self.state();
        if state.mouse.desired().enabled() {
            self.record_restore_flag(HostRestoreFlag::MouseCapture);
        }
        state.mouse.apply(writer, host, write)
    }

    pub(super) fn set_keyboard_enhancement_flags(
        &self,
        writer: &mut impl io::Write,
        flags: shepr_protocol::KittyKeyboardFlags,
    ) -> io::Result<()> {
        self.set_keyboard_protocol(writer, HostKeyboardUpdate::EnhancementFlags(flags))
    }

    pub(super) fn set_modify_other_keys(
        &self,
        writer: &mut impl io::Write,
        level: shepr_term::ModifyOtherKeysLevel,
    ) -> io::Result<()> {
        self.set_keyboard_protocol(writer, HostKeyboardUpdate::ModifyOtherKeys(level))
    }

    fn set_keyboard_protocol(
        &self,
        writer: &mut impl io::Write,
        update: HostKeyboardUpdate,
    ) -> io::Result<()> {
        let mut state = self.state();
        // The restore mask is raised before the write and narrowed only when it
        // succeeds (the report-all paths raise it and never narrow it), so a
        // write that fails part way leaves it a superset of what shepr owns. The keyboard state
        // helper does not update its own record on failure; restoration reads this mask, never
        // that record. A transient mode write can be retried, while a permanent one ends the
        // session, so either path restores every mode the write may have reached.
        let pending = update.restore_state(&state.keyboard);
        self.record_keyboard_restore_state(KeyboardRestore {
            kitty_entry: state.keyboard.has_kitty_keyboard_entry() || pending.kitty_entry,
            modify_other_keys: state.keyboard.modify_other_keys_active()
                || pending.modify_other_keys,
        });
        let result = update.apply(writer, &mut state.keyboard);
        if result.is_ok() {
            self.record_keyboard_restore_state(KeyboardRestore {
                kitty_entry: state.keyboard.has_kitty_keyboard_entry(),
                modify_other_keys: state.keyboard.modify_other_keys_active(),
            });
        }
        result
    }

    /// Records whether the pane asks for every key to be reported. Nothing is
    /// written until [`Self::sync_shell_keyboard_report_all`] applies it.
    pub(super) fn set_pane_keyboard_report_all(&self, enabled: bool) {
        self.state().pane_keyboard_report_all = enabled;
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
        self.apply_keyboard_report_all(writer, &mut state, shell_requests_report_all)
    }

    /// Reports every key while the pane or the shell asks for it.
    fn apply_keyboard_report_all(
        &self,
        writer: &mut impl io::Write,
        state: &mut HostModesState,
        shell_requests_report_all: bool,
    ) -> io::Result<()> {
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

    pub(super) fn restore<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        // Taken, so a second restore writes nothing.
        let restore_state = HostRestoreMask(self.inner.restore_state.swap(0, Ordering::AcqRel));
        let restores: [HostRestoreAction<W>; 9] = [
            HostRestoreAction::IfSet(
                HostRestoreFlag::ModifyOtherKeys,
                restore_modify_other_keys::<W>,
            ),
            HostRestoreAction::IfSet(
                HostRestoreFlag::KittyKeyboardEntry,
                restore_kitty_keyboard_entry::<W>,
            ),
            HostRestoreAction::IfSet(
                HostRestoreFlag::ColorSchemeReports,
                restore_color_scheme_reports::<W>,
            ),
            HostRestoreAction::IfSet(HostRestoreFlag::FocusChange, restore_focus_change::<W>),
            HostRestoreAction::IfSet(
                HostRestoreFlag::BracketedPaste,
                restore_bracketed_paste::<W>,
            ),
            HostRestoreAction::IfSet(HostRestoreFlag::LineWrap, restore_line_wrap::<W>),
            HostRestoreAction::IfSet(HostRestoreFlag::MouseCapture, restore_mouse_capture::<W>),
            HostRestoreAction::Always(restore_window_title::<W>),
            HostRestoreAction::Always(restore_window_title_stack::<W>),
        ];
        let mut first_error = None;
        for restore in restores {
            let Some(action) = restore.action(restore_state) else {
                continue;
            };
            // Every action runs even after a failure; the first error wins.
            let result = action(self, writer);
            first_error = first_error.or(result.err());
        }
        let flushed = writer.flush();
        first_error = first_error.or(flushed.err());
        first_error.map_or(Ok(()), Err)
    }
}

fn restore_modify_other_keys<W: io::Write>(_: &HostModes, writer: &mut W) -> io::Result<()> {
    shepr_termio::host_term::modes::restore_host_modify_other_keys(writer)
}

fn restore_kitty_keyboard_entry<W: io::Write>(_: &HostModes, writer: &mut W) -> io::Result<()> {
    shepr_termio::host_term::modes::restore_host_kitty_keyboard_entry(writer)
}

fn restore_color_scheme_reports<W: io::Write>(_: &HostModes, writer: &mut W) -> io::Result<()> {
    write_host_color_scheme_report_mode(writer, false)
}

fn restore_focus_change<W: io::Write>(_: &HostModes, writer: &mut W) -> io::Result<()> {
    write_dec_mode(writer, shepr_term::DecMode::FocusEvents, false)
}

fn restore_bracketed_paste<W: io::Write>(_: &HostModes, writer: &mut W) -> io::Result<()> {
    write_dec_mode(writer, shepr_term::DecMode::BracketedPaste, false)
}

fn restore_line_wrap<W: io::Write>(_: &HostModes, writer: &mut W) -> io::Result<()> {
    write_dec_mode(writer, shepr_term::DecMode::LineWrap, true)
}

fn restore_mouse_capture<W: io::Write>(_: &HostModes, writer: &mut W) -> io::Result<()> {
    set_mouse_capture_with_writer(writer, HostMouseCapture::Off)
}

fn restore_window_title<W: io::Write>(host_modes: &HostModes, writer: &mut W) -> io::Result<()> {
    host_modes.reset_window_title(writer)
}

fn restore_window_title_stack<W: io::Write>(
    host_modes: &HostModes,
    writer: &mut W,
) -> io::Result<()> {
    if host_modes
        .inner
        .title_stack_pushed
        .swap(false, Ordering::AcqRel)
    {
        writer.write_all(shepr_termio::host_term::modes::HOST_WINDOW_TITLE_POP_SEQUENCE)?;
    }
    Ok(())
}

pub(super) fn host_mouse_capture_update(
    current: HostMouseCapture,
    requested: HostMouseCapture,
    host: HostCell,
) -> Option<HostMouseCapture> {
    let mode = requested.effective(host);
    (current != mode).then_some(mode)
}

fn set_mouse_capture_with_writer(
    writer: &mut impl io::Write,
    mode: HostMouseCapture,
) -> io::Result<()> {
    shepr_termio::host_term::modes::clear_host_mouse_reporting(writer)?;
    if mode.enabled() {
        execute!(writer, EnableMouseCapture)?;
        if mode.pixels() {
            shepr_termio::host_term::modes::enable_host_sgr_pixel_mouse_reporting(writer)?;
        }
        Ok(())
    } else {
        execute!(writer, DisableMouseCapture)
    }
}

/// Restores every host mode, the raw mode and the screen, running each step
/// even after an earlier one fails. Each failure is logged here because
/// `Drop` has no caller to hand an error to; the first failure is also
/// returned.
fn restore_terminal_state(
    host_modes: &HostModes,
    writer: &mut HostTerminalWriter,
) -> io::Result<()> {
    // Runs first so the kitty keyboard pop reaches the host before the screen
    // is torn down; a failure here leaves the host terminal encoding keys.
    let modes_result = host_modes.restore(writer);
    if let Err(error) = &modes_result {
        shepr_platform::structured_log!(
            WARN, event = terminal.restore_modes, outcome = "error",
            error = %error,
            "failed to restore host terminal modes; keyboard protocol, mouse or paste modes may stay enabled"
        );
    }

    // A guard whose setup failed before raw mode was entered has no saved
    // mode, and crossterm then leaves the terminal as it is.
    let raw_mode_result = crossterm::terminal::disable_raw_mode();
    if let Err(error) = &raw_mode_result {
        shepr_platform::structured_log!(WARN, event = terminal.restore_raw, outcome = "error", error = %error, "failed to restore host terminal raw mode");
    }

    let screen_result = write_dec_mode(writer, shepr_term::DecMode::AlternateScreen, false);
    if let Err(error) = &screen_result {
        shepr_platform::structured_log!(WARN, event = terminal.restore_screen, outcome = "error", error = %error, "failed to restore host terminal screen");
    }

    let postlude_result = write_terminal_restore_postlude(writer);
    if let Err(error) = &postlude_result {
        shepr_platform::structured_log!(WARN, event = terminal.restore_postlude, outcome = "error", error = %error, "failed to write host terminal restore postlude");
    }

    // Preserve the first failure while still attempting every restoration step.
    modes_result
        .and(raw_mode_result)
        .and(screen_result)
        .and(postlude_result)
}

impl TerminalGuard {
    pub(super) fn escape_disambiguation(&self) -> EscapeDisambiguation {
        self.escape_disambiguation
    }

    pub(super) fn take_buffered_host_input(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buffered_host_input)
    }

    pub(super) fn host_modes(&self) -> HostModes {
        self.host_modes.clone()
    }

    /// Restores the terminal once; `Drop` then does nothing, even if this
    /// panicked part way.
    pub(super) fn restore(mut self) -> io::Result<()> {
        self.restored = true;
        let mut output_writer = self.output_writer.clone();
        (self.restore_state)(&self.host_modes, &mut output_writer)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        let mut output_writer = self.output_writer.clone();
        let host_modes = &self.host_modes;
        let restore_state = self.restore_state;
        // This drop runs while a panic in terminal setup unwinds; a second
        // panic escaping it would abort before the client's finalization. A
        // panicked restore is not retried, and its payload is forgotten
        // because dropping one can panic too. Drop cannot return the error,
        // and restore_terminal_state already logged each failed step.
        #[expect(
            clippy::disallowed_methods,
            reason = "a terminal restore during unwinding must not panic out of drop and abort the client's finalization"
        )]
        let restored = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            restore_state(host_modes, &mut output_writer)
        }));
        if let Err(payload) = restored {
            std::mem::forget(payload);
        }
    }
}

#[cfg(test)]
impl TerminalGuard {
    /// A guard and writer over `/dev/null` with nothing to restore, standing in for
    /// `setup_terminal` where a test drives the launch without a real terminal.
    pub(super) fn detached() -> (Self, HostTerminalWriter) {
        let sink = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .expect("open /dev/null");
        let output_writer = HostTerminalWriter(Arc::new(sink));
        let guard = Self {
            escape_disambiguation: EscapeDisambiguation::Inactive,
            buffered_host_input: Vec::new(),
            host_modes: HostModes::new(false),
            output_writer: output_writer.clone(),
            restored: false,
            restore_state: |_, _| Ok(()),
        };
        (guard, output_writer)
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

            assert_eq!(
                responses.flags,
                Some(shepr_protocol::KittyKeyboardFlags::from_bits_retain(7)),
                "split {split}"
            );
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
        let cell = shepr_core::geometry::CellPx::new(9, 18).expect("valid cell");
        let estimated = HostCell::Estimated(cell);
        let exact = HostCell::Exact(cell);
        assert_eq!(
            host_mouse_capture_update(HostMouseCapture::Cells, HostMouseCapture::Pixels, estimated),
            None
        );
        assert_eq!(
            host_mouse_capture_update(HostMouseCapture::Cells, HostMouseCapture::Pixels, exact),
            Some(HostMouseCapture::Pixels)
        );
        assert_eq!(
            host_mouse_capture_update(HostMouseCapture::Pixels, HostMouseCapture::Pixels, exact),
            None
        );
    }

    static PANICKING_RESTORES: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    fn panicking_restore(_: &HostModes, _: &mut HostTerminalWriter) -> io::Result<()> {
        PANICKING_RESTORES.fetch_add(1, Ordering::SeqCst);
        panic!("terminal restore panicked");
    }

    fn guard_with_panicking_restore() -> TerminalGuard {
        let sink = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .expect("open /dev/null");
        TerminalGuard {
            escape_disambiguation: EscapeDisambiguation::Inactive,
            buffered_host_input: Vec::new(),
            host_modes: HostModes::new(false),
            output_writer: HostTerminalWriter(Arc::new(sink)),
            restored: false,
            restore_state: panicking_restore,
        }
    }

    /// Setup panics with the half-built guard alive; its drop restores while
    /// that panic unwinds, and the restore panics too. Escaping the drop would
    /// abort the test binary, so reaching the assertions is the check.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the setup panic under test unwinds through the guard; catching it is how the test sees the process survive"
    )]
    fn a_panicking_restore_during_a_setup_panic_does_not_abort_and_a_restore_runs_once() {
        let unwound = std::panic::catch_unwind(|| {
            let _guard = guard_with_panicking_restore();
            panic!("terminal setup panicked");
        });
        assert!(unwound.is_err(), "the setup panic reaches its catcher");
        assert_eq!(PANICKING_RESTORES.load(Ordering::SeqCst), 1);

        // An explicit restore that panics is not retried by the drop.
        let fatal = crate::fatal_panic::FatalPanic::default();
        let guard = guard_with_panicking_restore();
        assert!(fatal.guard(|| guard.restore()).is_none());
        assert!(fatal.is_latched());
        assert_eq!(PANICKING_RESTORES.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn host_modes_restore_undoes_keyboard_protocol_and_title_once() {
        let modes = HostModes::new(false);
        let mut output = Vec::new();
        modes
            .set_keyboard_enhancement_flags(
                &mut output,
                shepr_protocol::KittyKeyboardFlags::DISAMBIGUATE
                    | shepr_protocol::KittyKeyboardFlags::REPORT_EVENT_TYPES,
            )
            .expect("write to a Vec");
        modes
            .set_modify_other_keys(&mut output, shepr_term::ModifyOtherKeysLevel::All)
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
        let modes = HostModes::new(false);
        let mut output = Vec::new();
        modes.set_pane_keyboard_report_all(true);
        modes
            .sync_shell_keyboard_report_all(&mut output, false)
            .expect("write to a Vec");
        assert_eq!(output, b"\x1b[>31u");

        output.clear();
        modes.restore(&mut output).expect("write to a Vec");
        assert_eq!(output, b"\x1b[<1u");
    }

    #[test]
    fn host_modes_restores_mouse_capture_enabled_without_reassertion() {
        let modes = HostModes::new(false);
        modes.set_mouse_endpoint_request(HostMouseCapture::Cells);

        let mut setup_output = Vec::new();
        modes
            .apply_mouse(&mut setup_output, HostCell::Unknown)
            .expect("write to a Vec");
        assert!(!setup_output.is_empty());
        assert!(modes.state().mouse.capture_active());

        let mut restore_output = Vec::new();
        modes.restore(&mut restore_output).expect("write to a Vec");
        assert_eq!(
            restore_output,
            b"\x1b[?1006l\x1b[?1016l\x1b[?1015l\x1b[?1005l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?9l\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l"
        );
    }

    /// The bytes a shell client writes on setup and on restore (`restore` and
    /// `Drop` both restore through `HostModes::restore`).
    #[test]
    fn host_modes_setup_and_restore_bytes() {
        for (level, set) in [
            (shepr_term::ModifyOtherKeysLevel::All, b"\x1b[>4;2m"),
            (
                shepr_term::ModifyOtherKeysLevel::ExceptWellDefined,
                b"\x1b[>4;1m",
            ),
        ] {
            let modes = HostModes::new(false);
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
            modes.record_restore_flag(HostRestoreFlag::MouseCapture);
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
        set_mouse_capture_with_writer(&mut output, HostMouseCapture::Pixels)
            .expect("write to a Vec");
        assert!(output.ends_with(b"\x1b[?1016h"), "{output:?}");
        assert_eq!(
            shepr_termio::host_term::modes::HOST_KEYBOARD_QUERY_SEQUENCE,
            b"\x1b[?u\x1b[c"
        );
    }

    #[test]
    fn host_mouse_mode_owns_request_and_preference_resolution() {
        let mut mode = HostMouseMode::new(true);

        assert_eq!(mode.desired(), HostMouseCapture::Cells);
        mode.set_endpoint_request(HostMouseCapture::Off);
        assert_eq!(mode.desired(), HostMouseCapture::Off);
        mode.set_endpoint_request(HostMouseCapture::Pixels);
        assert_eq!(mode.desired(), HostMouseCapture::Pixels);
        mode.clear_endpoint_request();
        assert_eq!(mode.desired(), HostMouseCapture::Cells);
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
    fn host_modes_restore_color_scheme_reports_when_enabled() {
        let mut output = Vec::new();
        let host_modes = HostModes::new(false);
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
}
