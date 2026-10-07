use std::collections::HashMap;

use super::*;
use crate::limits::{
    RESIZE_RECOVERY_MIN_PROBE_ROWS, RESIZE_RECOVERY_PROBE_SCREENS, SCAN_CHUNK_ROWS,
    SYNCHRONIZED_OUTPUT_FLUSH_MARGIN,
};
use crate::workspace::SurfaceChange;
use shepr_protocol::MAX_SURFACE_HYPERLINKS;

impl PaneTerminal {
    fn report_oversized_clipboard_store_effect(
        &self,
        pane_id: PaneId,
        size: shepr_vt::ClipboardStoreSize,
    ) {
        match size {
            shepr_vt::ClipboardStoreSize::Exact(bytes) => {
                self.report_oversized_clipboard_store(pane_id, bytes);
            }
            shepr_vt::ClipboardStoreSize::AtLeast(minimum_bytes) => {
                if !self
                    .oversized_clipboard_reported
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    shepr_platform::structured_log!(
                        WARN, event = clipboard.osc_store, outcome = Oversized,
                        pane = %pane_id,
                        minimum_bytes,
                        "dropped oversized OSC 52 clipboard store; decoded size is at least this many bytes"
                    );
                }
            }
        }
    }

    /// Construct a terminal that belongs to no pane: for tests, and for the
    /// `PaneRuntime::with_child_io` seam, whose runtime has no real pane.
    /// A spawned pane uses [`Self::new_with_pane_id`].
    /// It knows no local host name, so only an empty host or `localhost` in
    /// an OSC 7 `file://` report counts as this machine.
    pub(crate) fn new(terminal: shepr_vt::Terminal) -> Self {
        Self::new_inner(None, terminal, None)
    }

    /// Construct a pane terminal with the id needed by later mutation reports
    /// and the server's local host names for OSC 7 cwd reports.
    pub(crate) fn new_with_pane_id(
        pane_id: PaneId,
        terminal: shepr_vt::Terminal,
        local_host: Option<std::sync::Arc<shepr_platform::HostNames>>,
    ) -> Self {
        Self::new_inner(Some(pane_id), terminal, local_host)
    }

    fn new_inner(
        pane_id: Option<PaneId>,
        mut terminal: shepr_vt::Terminal,
        local_host: Option<std::sync::Arc<shepr_platform::HostNames>>,
    ) -> Self {
        // Replies to anything written before the pane existed have no reader.
        let _ = terminal.take_pty_responses();

        terminal.set_osc_body_capture(osc_debug::enabled());

        let mut render_state = shepr_vt::RenderState::new();
        render_state.update(&terminal);
        Self {
            core: TerminalCore::new(PaneTerminalCore {
                content_revision: ContentRevision::default(),
                detection_seq: DetectionSeq::default(),
                dirty_collection_hook: None,
                terminal,
                synchronized_output_epoch: SyncEpoch::default(),
                render_state,
                host_terminal_theme: shepr_term::host::TerminalTheme::default(),
                transient_default_color_owner_pgid: None,
                default_color_generation: DefaultColorGeneration::default(),
                agent_osc_state: AgentOscStateTracker::default(),
                local_host,
                pane_focused: false,
            }),
            pane_id,
            screen_flipped: AtomicBool::new(false),
            synchronized_output: AtomicBool::new(false),
            mutation_failure_reported: AtomicBool::new(false),
            oversized_clipboard_reported: AtomicBool::new(false),
            dirty_patch_fallback_reported: AtomicBool::new(false),
        }
    }

    /// Installs the host theme as the pane's default palette and default
    /// colours. They sit under whatever the child set itself (OSC 4/10/11),
    /// which stays in effect; nothing is written into the child's stream.
    pub(crate) fn apply_host_terminal_theme(&self, theme: shepr_term::host::TerminalTheme) {
        let Ok(mut core) = self.core.lock() else {
            self.report_terminal_mutation_failure(TerminalMutation::HostThemeUpdate);
            return;
        };
        // The server filters identical host themes before dispatching this
        // update, so each call installs a new set of pane defaults.
        self.commit_mutation(&mut core, CoreMutation::Presentation);
        core.host_terminal_theme = theme;
        if !has_default_color_override(&core.terminal) {
            core.transient_default_color_owner_pgid = None;
        }

        let mut palette = shepr_vt::default_palette();
        for (index, color) in theme.palette.iter().enumerate() {
            if let Some(color) = color {
                palette[index] = *color;
            }
        }
        core.terminal.set_default_palette(&palette);
        core.terminal
            .set_default_colors(theme.foreground, theme.background);
    }

    pub(crate) fn apply_host_terminal_appearance(
        &self,
        appearance: Option<shepr_term::host::HostAppearance>,
    ) -> Option<Bytes> {
        let mut core = match self.core.lock() {
            Ok(core) => core,
            Err(_) => {
                self.report_terminal_mutation_failure(TerminalMutation::HostAppearanceUpdate);
                return None;
            }
        };
        let color_scheme = appearance;
        let previous = core.terminal.set_color_scheme(color_scheme);
        // Only a change of the stored scheme, including one between unknown
        // and known, can change the pane; repeating the current one cannot.
        if previous != color_scheme {
            self.commit_mutation(&mut core, CoreMutation::Presentation);
        }

        let transitioned = matches!(
            (previous, color_scheme),
            (Some(previous), Some(current)) if previous != current
        );
        if !transitioned || !core.terminal.mode_get(shepr_vt::DecMode::ColorSchemeReport) {
            return None;
        }
        appearance.map(|appearance| Bytes::from_static(appearance.report()))
    }

    /// Whether a transient override can eventually be restored to a known
    /// host theme. Alternate-screen state may delay the restore probe.
    pub(crate) fn has_theme_restore_candidate(&self) -> bool {
        self.core.lock().is_ok_and(|core| {
            core.transient_default_color_owner_pgid.is_some()
                && !core.host_terminal_theme.is_empty()
        })
    }

    /// The foreground group whose default-colour overrides the runtime may
    /// restore to the host theme: one is recorded, the host theme is known and
    /// the main screen is showing. A read: a poisoned core is reported by the
    /// PTY actor, which closes the pane.
    pub(crate) fn theme_restore_owner(&self) -> Option<shepr_platform::Pgid> {
        self.core.lock().ok()?.theme_restore_owner()
    }

    /// Drops the child's OSC 10/11 default-colour overrides so the host theme
    /// shows again, provided `owner` is still the group they are recorded
    /// against and the terminal is in a state to restore
    /// ([`Self::theme_restore_owner`]). The runtime calls it once its process
    /// check shows the owner has left the foreground. This clears the core's
    /// override slots directly; nothing is written into the child's byte
    /// stream. Returns whether overrides were dropped.
    pub(crate) fn drop_default_color_overrides_if(&self, owner: shepr_platform::Pgid) -> bool {
        let Ok(mut core) = self.core.lock() else {
            self.report_terminal_mutation_failure(TerminalMutation::HostThemeRestore);
            return false;
        };
        if core.theme_restore_owner() != Some(owner) {
            return false;
        }
        core.transient_default_color_owner_pgid = None;
        core.terminal.reset_default_color_overrides();
        self.commit_mutation(&mut core, CoreMutation::Presentation);
        true
    }

    pub(crate) fn terminal_title(&self) -> Option<String> {
        self.core
            .lock()
            .ok()
            .and_then(|core| core.agent_osc_state.terminal_title().map(str::to_string))
    }

    /// Reads the three inputs to screen detection under one terminal-core
    /// lock, so they describe the same observed terminal state. The OSC title
    /// is the latest OSC 0/2 title retained for detection and the progress the
    /// latest OSC 9;4 report; each is `None` when none was seen or it was cleared.
    /// Returns `None` if either the core lock or screen read fails. Callers
    /// that need a diagnostic cause use `agent_detection_inputs_result`.
    pub(crate) fn agent_detection_inputs(&self) -> Option<AgentDetectionInputs> {
        self.agent_detection_inputs_result().ok().flatten()
    }

    /// Preserve a screen-read failure for on-demand diagnostics while keeping
    /// core poisoning distinct from a VT read error.
    pub(crate) fn agent_detection_inputs_result(
        &self,
    ) -> Result<Option<AgentDetectionInputs>, shepr_vt::ReadError> {
        let Ok(core) = self.core.lock() else {
            return Ok(None);
        };
        let screen_text = terminal_detection_text(&core.terminal)?;
        Ok(Some(AgentDetectionInputs {
            screen_text,
            osc_title: core.agent_osc_state.latest_title().map(str::to_owned),
            osc_progress: core
                .agent_osc_state
                .latest_progress()
                .map(|progress| progress.to_string()),
        }))
    }

    /// Clears retained OSC title/progress evidence when the pane's foreground
    /// agent changes, so a new agent process starts from a blank OSC slate.
    pub(crate) fn clear_agent_osc_state(&self) {
        let Ok(mut core) = self.core.lock() else {
            self.report_terminal_mutation_failure(TerminalMutation::AgentOscStateClear);
            return;
        };
        core.agent_osc_state.clear_retained();
    }

    pub(in crate::pane) fn process_pty_bytes_locked(
        &self,
        pane_id: PaneId,
        bytes: &[u8],
        now: Instant,
        mut core: std::sync::MutexGuard<'_, PaneTerminalCore>,
    ) -> ProcessBytesResult {
        // The runtime tick for a read: a synchronized update whose timeout
        // passed ends before these bytes are parsed, under the same lock, so
        // its effects are collected with theirs and its replies queue first.
        // The flush changes the screen on its own, so it marks detection
        // content changed exactly as the timer tick does, whatever the read
        // carries.
        let screen_before = core.terminal.active_screen();
        let focus_reporting_before = focus_reporting_on(&core);
        let flushed = core.terminal.tick(now);
        let synchronized_output_before = core.terminal.sync_update_buffering();
        core.terminal.write_at(bytes, now);
        self.note_screen_flip(screen_before, core.terminal.active_screen());
        let mut effects = collect_core_effects(&mut core);
        effects
            .terminal_responses
            .extend(focus_report_on_enable(&core, focus_reporting_before));

        let synchronized_output = core.terminal.sync_update_buffering();
        self.commit_mutation(
            &mut core,
            CoreMutation::Output {
                bytes,
                flushed,
                sync_changed: synchronized_output != synchronized_output_before,
            },
        );
        // A synchronized update that never ends is force-flushed by the core
        // after its timeout; schedule a render for then so the pane does not
        // stay frozen until the next PTY read.
        let render_request = if !synchronized_output {
            RenderRequest::Now
        } else {
            core.terminal
                .synchronized_output_deadline()
                .map_or(RenderRequest::None, |deadline| {
                    RenderRequest::After(
                        deadline.saturating_duration_since(now) + SYNCHRONIZED_OUTPUT_FLUSH_MARGIN,
                    )
                })
        };
        let dropped_clipboard_store_size = effects.dropped_clipboard_store_sizes.first().copied();
        drop(core);
        osc_debug::log(pane_id, &effects.osc_debug);
        if let Some(size) = dropped_clipboard_store_size {
            self.report_oversized_clipboard_store_effect(pane_id, size);
        }
        Ok(ProcessBytesEffects {
            render_request,
            terminal_title_changed: effects.terminal_title_changed,
            clipboard_writes: effects.clipboard_writes,
            reported_cwd: effects.reported_cwd,
            terminal_responses: effects.terminal_responses,
            default_color_generation: effects.default_color_generation,
        })
    }

    /// Records which foreground program group overrode a default colour, so the
    /// runtime can drop the override once that program is gone
    /// ([`Self::drop_default_color_overrides_if`]). Finding the group is the
    /// runtime's process question; this generation-checked setter records it
    /// only if the OSC 10/11 override it was looked up for is still current.
    /// Returns whether it was recorded.
    pub(crate) fn note_default_color_owner(
        &self,
        generation: DefaultColorGeneration,
        owner: shepr_platform::Pgid,
    ) -> bool {
        let Ok(mut core) = self.core.lock() else {
            self.report_terminal_mutation_failure(TerminalMutation::DefaultColorOwnerUpdate);
            return false;
        };
        if core.default_color_generation == generation && has_default_color_override(&core.terminal)
        {
            core.transient_default_color_owner_pgid = Some(owner);
            return true;
        }
        false
    }

    /// Flushes a synchronized update whose timeout has passed and returns
    /// everything the core queued for delivery. The runtime's timeout task
    /// calls this for a child that went quiet inside an update;
    /// The parser does the same before parsing new output.
    /// Readers and render paths only inspect the terminal. The render request
    /// is immediate when a frame was flushed.
    pub(crate) fn tick(&self, now: Instant) -> ProcessBytesResult {
        let Ok(mut core) = self.core.lock() else {
            // A poisoned core is noticed by the PTY actor (its per-loop
            // `core_poisoned` check, or its next read), which ends the pane;
            // this timer has no loop to stop.
            return Err(crate::pane::terminal::TerminalCorePoisoned);
        };
        let screen_before = core.terminal.active_screen();
        let focus_reporting_before = focus_reporting_on(&core);
        let flushed = core.terminal.tick(now);
        if flushed {
            self.note_screen_flip(screen_before, core.terminal.active_screen());
            self.commit_mutation(&mut core, CoreMutation::SyncFlush);
        }
        let mut effects = collect_core_effects(&mut core);
        effects
            .terminal_responses
            .extend(focus_report_on_enable(&core, focus_reporting_before));
        drop(core);
        if let Some(pane_id) = self.pane_id {
            osc_debug::log(pane_id, &effects.osc_debug);
        }
        // A synchronized update parses its buffered bytes when it flushes, so
        // an oversized OSC 52 store inside one surfaces here, not on a read.
        if let (Some(pane_id), Some(size)) = (
            self.pane_id,
            effects.dropped_clipboard_store_sizes.first().copied(),
        ) {
            self.report_oversized_clipboard_store_effect(pane_id, size);
        }
        Ok(ProcessBytesEffects {
            render_request: if flushed {
                RenderRequest::Now
            } else {
                RenderRequest::None
            },
            terminal_title_changed: effects.terminal_title_changed,
            clipboard_writes: effects.clipboard_writes,
            reported_cwd: effects.reported_cwd,
            terminal_responses: effects.terminal_responses,
            default_color_generation: effects.default_color_generation,
        })
    }

    pub(crate) fn resize(&self, geometry: shepr_core::geometry::PaneGeometry) -> Vec<Bytes> {
        let rows = geometry.rows();
        let mut core = match self.core.lock() {
            Ok(core) => core,
            Err(_) => {
                self.report_terminal_mutation_failure(TerminalMutation::Resize);
                return Vec::new();
            }
        };
        let synchronized_output_before = core.terminal.sync_update_buffering();
        let offset_from_bottom = core.terminal.scrollbar().offset_from_bottom;
        let resize_recovery_probe_lines = usize::from(rows)
            .saturating_mul(RESIZE_RECOVERY_PROBE_SCREENS)
            .max(RESIZE_RECOVERY_MIN_PROBE_ROWS);

        // Alacritty resizes and reflows the grid directly. Replaying history
        // through the parser here could split a sequence the child is still
        // writing and move its cursor behind its back.
        core.terminal.resize(geometry);
        let synchronized_output_after = core.terminal.sync_update_buffering();
        self.commit_mutation(
            &mut core,
            CoreMutation::Resize {
                sync_changed: synchronized_output_after != synchronized_output_before,
            },
        );
        let terminal_responses = drain_terminal_responses(core.terminal.take_pty_responses());

        terminal_set_scroll_offset_from_bottom(&mut core.terminal, offset_from_bottom);
        if offset_from_bottom > 0 {
            let mut remaining = offset_from_bottom.min(resize_recovery_probe_lines);
            // Check the current viewport once; each step toward live output
            // introduces just one new row at the bottom.
            let viewport = core.terminal.scrollbar();
            let mut scratch = String::new();
            let viewport_start = viewport.viewport_start().0;
            let viewport_has_text = (viewport_start
                ..viewport_start.saturating_add(viewport.viewport_rows))
                .any(|row| {
                    terminal_screen_row_has_text(&core.terminal, ScreenRow(row), &mut scratch)
                });

            if !viewport_has_text {
                while remaining > 0 {
                    let entering_row = core
                        .terminal
                        .scrollbar()
                        .viewport_start()
                        .0
                        .saturating_add(viewport.viewport_rows);
                    core.terminal
                        .scroll_viewport_delta(shepr_vt::ScrollTowards::Newer(1));
                    if terminal_screen_row_has_text(
                        &core.terminal,
                        ScreenRow(entering_row),
                        &mut scratch,
                    ) {
                        break;
                    }
                    remaining -= 1;
                }
            }
        }
        terminal_responses
    }

    fn update_scroll_position(
        &self,
        operation: TerminalMutation,
        update: impl FnOnce(&mut shepr_vt::Terminal),
    ) -> SurfaceChange {
        let Ok(mut core) = self.core.lock() else {
            self.report_terminal_mutation_failure(operation);
            return SurfaceChange::Unchanged;
        };
        let offset_before = terminal_scroll_metrics(&core.terminal).offset_from_bottom;
        update(&mut core.terminal);
        let offset_after = terminal_scroll_metrics(&core.terminal).offset_from_bottom;
        if offset_before == offset_after {
            return SurfaceChange::Unchanged;
        }
        self.commit_mutation(&mut core, CoreMutation::Presentation);
        SurfaceChange::Changed
    }

    pub(crate) fn scroll_up(&self, lines: usize) -> SurfaceChange {
        self.update_scroll_position(TerminalMutation::ScrollUp, |terminal| {
            terminal.scroll_viewport_delta(shepr_vt::ScrollTowards::Older(lines));
        })
    }

    pub(crate) fn scroll_down(&self, lines: usize) -> SurfaceChange {
        self.update_scroll_position(TerminalMutation::ScrollDown, |terminal| {
            terminal.scroll_viewport_delta(shepr_vt::ScrollTowards::Newer(lines));
        })
    }

    pub(crate) fn scroll_reset(&self) -> SurfaceChange {
        self.update_scroll_position(TerminalMutation::ScrollReset, |terminal| {
            terminal.scroll_viewport_bottom();
        })
    }

    pub(crate) fn clear_screen(&self) -> Result<SurfaceChange, PaneClearError> {
        let mut core = self
            .core
            .lock()
            .map_err(|_| PaneClearError::TerminalLockPoisoned)?;
        match core.terminal.clear_screen() {
            shepr_vt::ClearScreenOutcome::Cleared => {
                self.commit_mutation(&mut core, CoreMutation::Clear);
                Ok(SurfaceChange::Changed)
            }
            shepr_vt::ClearScreenOutcome::AlternateScreenActive => {
                Err(PaneClearError::AlternateScreenActive)
            }
        }
    }

    pub(crate) fn set_scroll_offset_from_bottom(&self, lines: usize) -> SurfaceChange {
        self.update_scroll_position(TerminalMutation::SetScrollOffset, |terminal| {
            terminal_set_scroll_offset_from_bottom(terminal, lines);
        })
    }

    pub(crate) fn scroll_metrics(&self) -> Option<ScrollMetrics> {
        let Ok(core) = self.core.lock() else {
            return None;
        };
        Some(terminal_scroll_metrics(&core.terminal))
    }

    /// Chunked copy-mode search. The terminal lock is released between
    /// chunks; rows are absolute, so output meanwhile does not move them.
    pub(crate) fn search_text_window(
        &self,
        request: TerminalTextSearch<'_>,
    ) -> TerminalSearchWindow {
        let Some(mut search) = TextSearch::new(request) else {
            return TerminalSearchWindow::empty();
        };
        let mut builder = TextBufferBuilder::new(true, false);
        let mut scratch = String::new();
        let mut next = None;
        let mut scan = None;
        loop {
            let Ok(core) = self.core.lock() else {
                break;
            };
            let terminal = &core.terminal;
            let cols = terminal.cols();
            let screen = terminal.active_screen();
            let total_rows = terminal.total_rows();
            let (scan_cols, scan_screen) = *scan.get_or_insert((cols, screen));
            if (scan_cols, scan_screen) != (cols, screen) {
                // Re-wrapped or switched screens while the lock was
                // released: the rest would not continue the same text.
                break;
            }
            let origin = terminal.history_origin();
            let end = terminal.absolute_row_for_screen(ScreenRow(total_rows));
            let mut row = next.unwrap_or(origin);
            if row < origin {
                // Lines were evicted while the lock was released, possibly
                // the start of the line being assembled.
                builder.discard_line();
                row = origin;
            }
            let chunk_end = end.min(row.saturating_add(SCAN_CHUNK_ROWS));
            while row < chunk_end {
                let Some(y) = terminal.screen_row_for_absolute(row) else {
                    break;
                };
                let Some(wrap) =
                    terminal.visit_screen_row_text(y, &mut scratch, |col, wide, text| {
                        builder.push_cell(row, col, wide, text);
                    })
                else {
                    break;
                };
                if builder.end_row(wrap.soft_wrapped) {
                    search.scan_line(&builder.line, cols, screen);
                }
                row = row.saturating_add(1);
            }
            drop(core);
            if row < chunk_end || row >= end {
                break;
            }
            next = Some(row);
            // Give the PTY reader waiting on the lock a chance to take it.
            std::thread::yield_now();
        }
        if let (Some(line), Some((cols, screen))) = (builder.trailing_line(), scan) {
            search.scan_line(line, cols, screen);
        }
        search.finish()
    }

    pub(crate) fn negotiated_keyboard_protocol(&self) -> Option<shepr_term::key::KeyboardProtocol> {
        let core = self.core.lock().ok()?;
        Some(shepr_term::key::KeyboardProtocol::from_flags(
            core.terminal.kitty_keyboard_flags(),
        ))
    }

    pub(crate) fn input_modes(&self) -> Option<shepr_vt::InputModes> {
        let core = self.core.lock().ok()?;
        Some(core.terminal.input_modes())
    }

    pub(crate) fn bracketed_paste_enabled(&self) -> bool {
        self.mode_enabled(shepr_vt::DecMode::BracketedPaste)
    }

    /// Records whether the pane holds terminal focus and answers whether the
    /// child has focus reporting on, so the caller reports `event` now. One
    /// core hold for both: a parse that turns reporting on runs either before
    /// it (and this caller reports) or after it (and the parse reports, from
    /// the recorded focus), so the child hears of the focus exactly once.
    /// The caller holds the reply-order lock across this call and the queueing
    /// of its report, as a read does across its parse and its replies, so the
    /// reports also reach the child in the order the focus was recorded.
    /// A poisoned core records nothing and reports nothing.
    pub(crate) fn note_pane_focus(&self, event: shepr_vt::FocusEvent) -> bool {
        let Ok(mut core) = self.core.lock() else {
            return false;
        };
        core.pane_focused = matches!(event, shepr_vt::FocusEvent::Gained);
        focus_reporting_on(&core)
    }

    pub(crate) fn mouse_reporting_enabled(&self) -> bool {
        self.core
            .lock()
            .is_ok_and(|core| core.terminal.mouse_tracking_enabled())
    }

    pub(crate) fn modify_other_keys_mode(&self) -> shepr_vt::ModifyOtherKeysLevel {
        self.core
            .lock()
            .map_or(shepr_vt::ModifyOtherKeysLevel::Off, |core| {
                core.terminal.modify_other_keys_level()
            })
    }

    /// The child-selected xterm modifyOtherKeys level.
    pub(crate) fn modify_other_keys_level(&self) -> shepr_vt::ModifyOtherKeysLevel {
        self.modify_other_keys_mode()
    }

    /// Mode 1016 and the extent the child was told, in one core hold. A
    /// poisoned core reads as off.
    pub(crate) fn pixel_mouse(&self) -> shepr_term::mouse::PanePixelMouse {
        self.core
            .lock()
            .map_or(shepr_term::mouse::PanePixelMouse::OFF, |core| {
                core.terminal.pixel_mouse()
            })
    }

    fn mode_enabled(&self, mode: shepr_vt::DecMode) -> bool {
        self.core
            .lock()
            .is_ok_and(|core| core.terminal.mode_get(mode))
    }

    pub(crate) fn plain_page_keys_use_host_scrollback(&self) -> Option<bool> {
        self.input_modes()
            .map(shepr_vt::InputModes::plain_page_keys_use_host_scrollback)
    }

    pub(crate) fn alternate_screen_active(&self) -> bool {
        self.core
            .lock()
            .is_ok_and(|core| core.terminal.active_screen() == shepr_vt::ActiveScreen::Alternate)
    }

    /// The cursor, with the synchronized-output gate decided in the same core
    /// hold ([`CursorRead`]), so callers need no check of their own first.
    pub(crate) fn cursor_read(&self) -> CursorRead {
        let Ok(mut core) = self.core.lock() else {
            return CursorRead::Unavailable;
        };
        if core.terminal.sync_update_buffering() {
            return CursorRead::Deferred;
        }
        current_cursor_state(&mut core).map_or(CursorRead::Unavailable, CursorRead::Shown)
    }

    /// Whether a synchronized update is open, read from the mirror without the
    /// core lock. A poisoned core reads as not active; see [`Self::surface_held`].
    pub(crate) fn synchronized_output_active(&self) -> bool {
        self.synchronized_output.load(Ordering::Acquire)
    }

    /// Whether the pane's surface cannot be drawn now: its core is poisoned or
    /// a synchronized update is open. Lock-free; the locked
    /// [`Self::synchronized_output_state`] stays the authority that defers a
    /// racing draw.
    pub(crate) fn surface_held(&self) -> bool {
        self.core.is_poisoned() || self.synchronized_output_active()
    }

    /// The synchronized-output flag and epoch read together; see
    /// [`SyncState`].
    pub(crate) fn synchronized_output_state(&self) -> SyncState {
        match self.core.lock() {
            Ok(core) => core.sync_state(),
            Err(_) => SyncState::Poisoned,
        }
    }

    pub(crate) fn detection_text(&self) -> String {
        self.core
            .lock()
            .ok()
            .and_then(|core| terminal_detection_text(&core.terminal).ok())
            .unwrap_or_default()
    }

    pub(crate) fn extract_selection<P>(
        &self,
        selection: &shepr_vt::selection::Selection<P>,
    ) -> Option<String> {
        self.core
            .lock()
            .ok()
            .and_then(|mut core| terminal_extract_selection(&mut core, selection))
    }

    /// Writes the visible screen into `area` of `frame`, cell by cell: typed
    /// underline shapes, wide-glyph tails (empty symbols) and OSC 8 links
    /// (added to the frame's link table) go straight to the wire form.
    ///
    /// Draws nothing while a synchronized update is open or the core is
    /// unreadable, and says which ([`PaneDraw::Deferred`],
    /// [`PaneDraw::Unreadable`]), decided in the same core hold as the cells,
    /// so callers need no check of their own beforehand. The part of `area`
    /// outside the frame is not drawn, and rows or columns the screen does not
    /// have are blank in the terminal's default colours.
    pub(crate) fn render_into(&self, frame: &mut FrameData, area: Rect) -> PaneDraw {
        let Ok(mut core) = self.core.lock() else {
            return PaneDraw::Unreadable;
        };
        let SyncState::Idle(sync_epoch) = core.sync_state() else {
            return PaneDraw::Deferred;
        };
        let drawn = PaneDraw::Drawn {
            sync_epoch,
            content_revision: core.content_revision,
        };

        let PaneTerminalCore {
            terminal,
            render_state,
            ..
        } = &mut *core;
        let terminal: &shepr_vt::Terminal = terminal;
        render_state.update(terminal);
        let colors = render_state.colors();
        let default_bg = terminal_default_bg(colors.background, colors.background_source);
        let default_fg = terminal_default_fg(colors.foreground, colors.foreground_source);
        let resolved_fg = Some(terminal_color(colors.foreground));
        let resolved_bg = Some(terminal_color(colors.background));
        let palette_overrides = PaletteOverrides::new(colors.palette_overrides());

        let frame_width = usize::from(frame.width());
        let area = area.intersection(Rect::new(0, 0, frame.width(), frame.height()));
        if area.is_empty() {
            return drawn;
        }
        let blank = CellPaint::blank(default_fg, default_bg);
        let mut symbol_scratch = String::new();
        let mut hyperlink_indices: Option<HashMap<String, u32>> = None;
        let mut rows_drawn = 0u16;
        for row in render_state.iter_rows().take(usize::from(area.height)) {
            let row_start = usize::from(area.y + rows_drawn) * frame_width + usize::from(area.x);
            let mut x = 0usize;
            for cell_view in row.cells().take(usize::from(area.width)) {
                let basic = cell_view.basic_data();
                let paint = terminal_cell_paint(
                    &basic,
                    default_fg,
                    default_bg,
                    resolved_fg,
                    resolved_bg,
                    palette_overrides.as_ref(),
                );
                let symbol =
                    terminal_buffer_symbol_into(&cell_view, basic.wide, &mut symbol_scratch);
                // A link that cannot be read (or a full link table) leaves the
                // cell unlinked rather than failing the frame.
                let hyperlink = if basic.has_hyperlink {
                    let x = u16::try_from(x).unwrap_or(u16::MAX);
                    terminal
                        .viewport_hyperlink_uri(x, row.y())
                        .ok()
                        .flatten()
                        .and_then(|uri| {
                            let indices = hyperlink_indices
                                .get_or_insert_with(|| seed_hyperlink_indices(frame));
                            intern_render_hyperlink(frame, indices, uri)
                        })
                } else {
                    None
                };
                let cell = &mut frame.cells_mut()[row_start + x];
                paint.write_cell(cell, symbol, terminal_grid_width(basic.wide), hyperlink);
                x += 1;
            }
            for cell in &mut frame.cells_mut()[row_start + x..row_start + usize::from(area.width)] {
                blank.write_cell(cell, " ", GridCellWidth::One, None);
            }
            // The area can be narrower than the terminal, and the emulator can
            // leave broken pairs of its own: no wide half without its other
            // half leaves this row. A blanked cell's link was already interned
            // above, so the frame's link table can keep an entry no cell
            // names; that costs one table slot and draws nothing.
            shepr_surface::pane_row::normalize_pane_row(
                &mut frame.cells_mut()[row_start..row_start + usize::from(area.width)],
            );
            rows_drawn += 1;
        }
        for y in rows_drawn..area.height {
            let row_start = usize::from(area.y + y) * frame_width + usize::from(area.x);
            for cell in &mut frame.cells_mut()[row_start..row_start + usize::from(area.width)] {
                blank.write_cell(cell, " ", GridCellWidth::One, None);
            }
        }
        // A full render draws every row whatever its dirty flag says, so it
        // leaves the flags alone: they belong to dirty-patch collection
        // alone. Clearing them here let a full frame drawn for one purpose
        // swallow rows a later patch still had to send.
        drawn
    }

    pub(in crate::pane) fn collect_dirty_patch_snapshot(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> Result<super::TerminalDirtyPatchSnapshot, PatchUnavailable> {
        let Ok(mut core) = self.core.lock() else {
            self.report_dirty_patch_fallback(PatchUnavailable::CorePoisoned);
            return Err(PatchUnavailable::CorePoisoned);
        };
        if let Some(hook) = core.dirty_collection_hook.take() {
            hook();
        }
        if core.terminal.sync_update_buffering() {
            return Err(PatchUnavailable::SynchronizedOutput);
        }
        let patch = match terminal_collect_dirty_patch(&mut core, area_width, area_height) {
            Ok(patch) => patch,
            Err(reason) => {
                drop(core);
                let unavailable = PatchUnavailable::Fallback(reason);
                self.report_dirty_patch_fallback(unavailable);
                return Err(unavailable);
            }
        };
        Ok(super::TerminalDirtyPatchSnapshot {
            patch,
            content_revision: core.content_revision,
            scroll_metrics: terminal_scroll_metrics(&core.terminal),
            mouse_reporting: core.terminal.mouse_tracking_enabled(),
            pixel_mouse: core.terminal.pixel_mouse(),
            alternate_screen_active: core.terminal.active_screen()
                == shepr_vt::ActiveScreen::Alternate,
        })
    }
}

fn terminal_screen_row_has_text(
    terminal: &shepr_vt::Terminal,
    row: ScreenRow,
    scratch: &mut String,
) -> bool {
    let mut has_text = false;
    terminal.visit_screen_row_text(row, scratch, |_, wide, text| {
        if wide != shepr_vt::CellWide::SpacerTail && !text.trim().is_empty() {
            has_text = true;
        }
    });
    has_text
}

fn seed_hyperlink_indices(frame: &FrameData) -> HashMap<String, u32> {
    let mut indices = HashMap::with_capacity(frame.hyperlinks().len().min(MAX_SURFACE_HYPERLINKS));
    for (index, uri) in frame.hyperlinks().iter().enumerate() {
        let Ok(index) = u32::try_from(index) else {
            break;
        };
        indices.entry(uri.clone()).or_insert(index);
    }
    indices
}

fn intern_render_hyperlink(
    frame: &mut FrameData,
    indices: &mut HashMap<String, u32>,
    uri: String,
) -> Option<u32> {
    if frame.hyperlinks().last().is_some_and(|known| known == &uri) {
        return frame
            .hyperlinks()
            .len()
            .checked_sub(1)
            .and_then(|index| u32::try_from(index).ok());
    }
    if let Some(index) = indices.get(&uri) {
        return Some(*index);
    }
    let index = frame.push_hyperlink(uri.clone())?;
    indices.insert(uri, index);
    Some(index)
}

#[cfg(test)]
impl PaneTerminal {
    pub(crate) fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: shepr_term::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.encode_mouse_button_with_modes(self.input_modes()?, kind, position, modifiers)
    }

    pub(crate) fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: shepr_term::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.encode_mouse_motion_with_modes(self.input_modes()?, kind, position, modifiers)
    }

    // Rendering tests compare cells; production consumes the typed snapshot result.
    #[cfg(test)]
    pub(super) fn collect_dirty_patch(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> TerminalDirtyPatchOutcome {
        self.collect_dirty_patch_snapshot(area_width, area_height)
            .map_or_else(
                |_| TerminalDirtyPatchOutcome::Fallback,
                |snapshot| {
                    snapshot.patch.map_or(
                        TerminalDirtyPatchOutcome::Clean,
                        TerminalDirtyPatchOutcome::Patch,
                    )
                },
            )
    }

    /// Processes one chunk of child output. `now` is the read's timestamp: it
    /// decides whether a pending synchronized update has expired, and it is
    /// the parser's clock for any synchronized update this chunk begins.
    #[cfg(test)]
    pub(crate) fn process_pty_bytes_at(
        &self,
        pane_id: PaneId,
        bytes: &[u8],
        now: Instant,
    ) -> ProcessBytesEffects {
        self.try_process_pty_bytes_at(pane_id, bytes, now)
            .expect("test process requires a healthy terminal core")
    }

    #[cfg(test)]
    pub(crate) fn try_process_pty_bytes_at(
        &self,
        pane_id: PaneId,
        bytes: &[u8],
        now: Instant,
    ) -> ProcessBytesResult {
        let core = self.core.lock()?;
        self.process_pty_bytes_locked(pane_id, bytes, now, core)
    }

    /// The cursor without the synchronized-output gate that
    /// [`Self::cursor_read`] applies.
    pub(crate) fn cursor_state(&self) -> Option<TerminalCursorState> {
        let mut core = self.core.lock().ok()?;
        current_cursor_state(&mut core)
    }

    pub(crate) fn has_transient_default_color_override(&self) -> bool {
        self.core
            .lock()
            .is_ok_and(|core| core.transient_default_color_owner_pgid.is_some())
    }

    #[cfg(test)]
    pub(crate) fn visible_text(&self) -> String {
        self.core
            .lock()
            .map_or_default(|mut core| terminal_visible_text(&mut core))
    }

    // Test-only reads of retained content; they need only text, not
    // truncation metadata.
    #[cfg(test)]
    pub(crate) fn recent_text(&self, lines: usize) -> String {
        self.core
            .lock()
            .ok()
            .and_then(|mut core| terminal_recent_text(&mut core, lines).ok())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn recent_unwrapped_text(&self, lines: usize) -> String {
        self.core
            .lock()
            .ok()
            .and_then(|mut core| terminal_recent_text_unwrapped(&mut core, lines).ok())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn process_pty_bytes(&self, pane_id: PaneId, bytes: &[u8]) -> ProcessBytesEffects {
        // clock-io-ok: test fixture adapter; production reads supply their timestamp.
        self.process_pty_bytes_at(pane_id, bytes, Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poisoned_terminal_core_has_no_detection_observation() {
        let terminal = std::sync::Arc::new(PaneTerminal::new(shepr_vt::Terminal::new(
            shepr_core::geometry::PaneGeometry::cells_only(80, 24),
            shepr_core::scrollback::ScrollbackBudget::new(0),
        )));
        let poisoner = std::sync::Arc::clone(&terminal);
        let poison_result = std::thread::spawn(move || {
            let _core = poisoner.core.lock();
            panic!("poison the terminal core for this test");
        })
        .join();

        assert!(poison_result.is_err());
        assert!(terminal.agent_detection_inputs().is_none());
    }

    #[test]
    fn resize_returns_queued_replies_before_its_own() {
        let pane = PaneTerminal::new(shepr_vt::Terminal::new(
            shepr_core::geometry::PaneGeometry::cells_only(80, 24),
            shepr_core::scrollback::ScrollbackBudget::new(0),
        ));
        {
            let mut core = pane.core.lock().expect("terminal core");
            core.terminal.write(b"\x1b[?2048h\x1b[5n");
        }

        let replies = pane.resize(shepr_core::geometry::PaneGeometry::with_cell(
            80,
            24,
            shepr_core::geometry::CellPx::new(9, 18),
        ));

        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].as_ref(), b"\x1b[0n");
        assert_eq!(replies[1].as_ref(), b"\x1b[48;24;80;432;720t");
    }
}
