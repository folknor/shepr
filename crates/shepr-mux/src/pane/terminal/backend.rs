use super::super::teardown::ChildLiveness;
use super::*;

impl PaneTerminal {
    /// Construct a terminal that belongs to no pane: for tests, and for the
    /// `PaneRuntime::with_child_io` seam, whose runtime has no real pane.
    /// A spawned pane uses [`Self::new_with_pane_id`].
    pub(crate) fn new(terminal: shepr_vt::Terminal) -> Self {
        Self::new_inner(None, terminal)
    }

    /// Construct a pane terminal with the id needed by later mutation reports.
    pub(crate) fn new_with_pane_id(pane_id: PaneId, terminal: shepr_vt::Terminal) -> Self {
        Self::new_inner(Some(pane_id), terminal)
    }

    fn new_inner(pane_id: Option<PaneId>, mut terminal: shepr_vt::Terminal) -> Self {
        // Replies to anything written before the pane existed have no reader.
        let _ = terminal.take_pty_responses();

        let mut render_state = shepr_vt::RenderState::new();
        render_state.update(&terminal);
        let initial_colors = render_state.colors();
        let initial_default_foreground = Some(initial_colors.foreground);
        let initial_default_background = Some(initial_colors.background);
        Self {
            core: Mutex::new(PaneTerminalCore {
                dirty_collection_hook: None,
                terminal,
                synchronized_output_epoch: 0,
                history_epoch: 0,
                render_state,
                initial_default_foreground,
                initial_default_background,
                host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme::default(),
                transient_default_color_owner_pgid: None,
                default_color_generation: 0,
                osc_debug_tracker: OscDebugTracker::default(),
                agent_osc_state: AgentOscStateTracker::default(),
            }),
            pane_id,
            mutation_failure_reported: AtomicBool::new(false),
            oversized_clipboard_reported: AtomicBool::new(false),
            dirty_patch_fallback_reported: AtomicBool::new(false),
        }
    }

    /// Installs the host theme as the pane's default palette and default
    /// colours. They sit under whatever the child set itself (OSC 4/10/11),
    /// which stays in effect; nothing is written into the child's stream.
    pub(crate) fn apply_host_terminal_theme(
        &self,
        theme: shepr_termio::host_term::theme::TerminalTheme,
    ) {
        let Ok(mut core) = shepr_vt::lock_terminal_core(&self.core) else {
            self.report_terminal_mutation_failure("host theme update");
            return;
        };
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
        appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
    ) -> Option<Bytes> {
        let mut core = match shepr_vt::lock_terminal_core(&self.core) {
            Ok(core) => core,
            Err(_) => {
                self.report_terminal_mutation_failure("host appearance update");
                return None;
            }
        };
        let color_scheme = appearance;
        let previous = core.terminal.set_color_scheme(color_scheme);

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
        shepr_vt::lock_terminal_core(&self.core).is_ok_and(|core| {
            core.transient_default_color_owner_pgid.is_some()
                && !core.host_terminal_theme.is_empty()
        })
    }

    pub(in crate::pane) fn maybe_restore_host_terminal_theme(
        &self,
        pane_id: PaneId,
        child_liveness: &ChildLiveness,
    ) -> bool {
        let Some(shell_pid) = child_liveness.live_pid() else {
            return false;
        };
        {
            let Ok(core) = shepr_vt::lock_terminal_core(&self.core) else {
                self.report_terminal_mutation_failure("host theme restore");
                return false;
            };
            if !should_probe_host_terminal_theme_restore(&core) {
                return false;
            }
        }

        let foreground_job = shepr_agent::detect::foreground_job(shell_pid);
        if child_liveness.live_pid() != Some(shell_pid) {
            return false;
        }
        let Ok(mut core) = shepr_vt::lock_terminal_core(&self.core) else {
            self.report_terminal_mutation_failure("host theme restore");
            return false;
        };

        let alternate_screen = core.terminal.active_screen() == shepr_vt::ActiveScreen::Alternate;
        restore_host_terminal_theme_if_needed(
            &mut core,
            pane_id,
            shell_pid,
            alternate_screen,
            foreground_job.as_ref(),
        )
    }

    pub(crate) fn terminal_title(&self) -> Option<String> {
        shepr_vt::lock_terminal_core(&self.core)
            .ok()
            .and_then(|core| core.agent_osc_state.terminal_title().map(str::to_string))
    }

    /// Reads the three inputs to screen detection under one terminal-core
    /// lock, so they describe the same observed terminal state. The OSC title
    /// is the latest OSC 0/2 title retained for detection and the progress the
    /// latest OSC 9;4 payload; each is `""` when none was seen or it was cleared.
    pub(crate) fn agent_detection_inputs(&self) -> AgentDetectionInputs {
        let Ok(core) = shepr_vt::lock_terminal_core(&self.core) else {
            self.report_terminal_mutation_failure("agent detection read");
            return AgentDetectionInputs::default();
        };
        AgentDetectionInputs {
            screen_text: terminal_detection_text(&core.terminal).unwrap_or_default(),
            osc_title: core.agent_osc_state.latest_title().to_owned(),
            osc_progress: core.agent_osc_state.latest_progress().to_owned(),
        }
    }

    /// Clears retained OSC title/progress evidence when the pane's foreground
    /// agent changes, so a new agent process starts from a blank OSC slate.
    pub(crate) fn clear_agent_osc_state(&self) {
        let Ok(mut core) = shepr_vt::lock_terminal_core(&self.core) else {
            self.report_terminal_mutation_failure("agent OSC state clear");
            return;
        };
        core.agent_osc_state.clear_retained();
    }

    /// Processes one chunk of child output. `now` is the read's timestamp: it
    /// decides whether a pending synchronized update has expired, and it is
    /// the parser's clock for any synchronized update this chunk begins.
    pub(crate) fn process_pty_bytes_at(
        &self,
        pane_id: PaneId,
        bytes: &[u8],
        now: Instant,
    ) -> ProcessBytesResult {
        let mut core = match shepr_vt::lock_terminal_core(&self.core) {
            Ok(core) => core,
            Err(shepr_vt::TerminalCorePoisoned) => {
                // The PTY actor logs and closes the pane for this result. It
                // also owns the idle-poison path, so logging here would
                // duplicate the same failure on reader-detected poison.
                return ProcessBytesResult {
                    core_poisoned: true,
                    ..ProcessBytesResult::default()
                };
            }
        };

        core.osc_debug_tracker.observe(bytes);
        for event in core.osc_debug_tracker.drain_pending() {
            debug!(
                pane = pane_id.raw(),
                osc_command = %event.command,
                osc_payload = ?event.payload,
                "agent OSC evidence observed"
            );
        }

        // The runtime tick for a read: a synchronized update whose timeout
        // passed ends before these bytes are parsed, under the same lock, so
        // its effects are collected with theirs and its replies queue first.
        if core.terminal.tick(now) {
            core.synchronized_output_epoch = core.synchronized_output_epoch.wrapping_add(1);
        }
        let synchronized_output_before = core
            .terminal
            .mode_get(shepr_vt::DecMode::SynchronizedOutput);
        core.terminal.write_at(bytes, now);
        let effects = collect_core_effects(&mut core);
        let default_color_generation = core.default_color_generation;

        let synchronized_output = core
            .terminal
            .mode_get(shepr_vt::DecMode::SynchronizedOutput);
        if synchronized_output != synchronized_output_before {
            core.synchronized_output_epoch = core.synchronized_output_epoch.wrapping_add(1);
        }
        let request_render = !synchronized_output;
        // A synchronized update that never ends is force-flushed by the core
        // after its timeout; schedule a render for then so the pane does not
        // stay frozen until the next PTY read.
        let render_delay = if synchronized_output {
            core.terminal
                .synchronized_output_deadline()
                .map(|deadline| {
                    deadline.saturating_duration_since(now) + SYNCHRONIZED_OUTPUT_FLUSH_MARGIN
                })
        } else {
            None
        };
        let dropped_clipboard_store_bytes = effects.dropped_clipboard_store_bytes.first().copied();
        drop(core);
        if let Some(bytes) = dropped_clipboard_store_bytes {
            self.report_oversized_clipboard_store(pane_id, bytes);
        }
        ProcessBytesResult {
            request_render,
            render_delay,
            terminal_title_changed: effects.terminal_title_changed,
            clipboard_writes: effects.clipboard_writes,
            reported_cwd: effects.reported_cwd,
            terminal_responses: effects.terminal_responses,
            default_color_owner_pending: effects.default_color_owner_pending,
            default_color_generation,
            core_poisoned: false,
        }
    }

    /// Records which foreground program overrode a default colour, so the
    /// detection tick can drop the override once that program is gone.
    ///
    /// Finding the program means scanning `/proc`. The caller releases the
    /// terminal, content and reply-order locks before this scan, then this
    /// method takes the terminal lock briefly to store the answer. The
    /// generation check drops an answer if another OSC colour write arrived
    /// during the scan.
    pub(in crate::pane) fn resolve_default_color_owner(
        &self,
        pane_id: PaneId,
        child_liveness: &ChildLiveness,
        generation: u64,
    ) {
        let Some(shell_pid) = child_liveness.live_pid() else {
            return;
        };
        let Some(owner_pgid) = current_transient_default_color_owner(shell_pid) else {
            return;
        };
        if child_liveness.live_pid() != Some(shell_pid) {
            return;
        }
        let Ok(mut core) = shepr_vt::lock_terminal_core(&self.core) else {
            self.report_terminal_mutation_failure("default color owner update");
            return;
        };
        if core.default_color_generation == generation && has_default_color_override(&core.terminal)
        {
            core.transient_default_color_owner_pgid = Some(owner_pgid);
            debug!(
                pane = pane_id.raw(),
                owner_pgid, "tracked transient default color override"
            );
        }
    }

    /// Flushes a synchronized update whose timeout has passed and returns
    /// everything the core queued for delivery. The runtime's timeout task
    /// calls this for a child that went quiet inside an update;
    /// [`Self::process_pty_bytes_at`] does the same before parsing new output.
    /// Readers and render paths only inspect the terminal. `request_render`
    /// is set when a frame was flushed.
    pub(crate) fn tick(&self, now: Instant) -> ProcessBytesResult {
        let Ok(mut core) = shepr_vt::lock_terminal_core(&self.core) else {
            // A poisoned core is noticed by the PTY actor (its per-loop
            // `core_poisoned` check, or its next read), which ends the pane;
            // this timer has no loop to stop.
            return ProcessBytesResult {
                core_poisoned: true,
                ..ProcessBytesResult::default()
            };
        };
        let flushed = core.terminal.tick(now);
        if flushed {
            core.synchronized_output_epoch = core.synchronized_output_epoch.wrapping_add(1);
        }
        let effects = collect_core_effects(&mut core);
        let default_color_generation = core.default_color_generation;
        drop(core);
        // A synchronized update parses its buffered bytes when it flushes, so
        // an oversized OSC 52 store inside one surfaces here, not on a read.
        if let (Some(pane_id), Some(bytes)) = (
            self.pane_id,
            effects.dropped_clipboard_store_bytes.first().copied(),
        ) {
            self.report_oversized_clipboard_store(pane_id, bytes);
        }
        ProcessBytesResult {
            request_render: flushed,
            render_delay: None,
            terminal_title_changed: effects.terminal_title_changed,
            clipboard_writes: effects.clipboard_writes,
            reported_cwd: effects.reported_cwd,
            terminal_responses: effects.terminal_responses,
            default_color_owner_pending: effects.default_color_owner_pending,
            default_color_generation,
            core_poisoned: false,
        }
    }

    pub(crate) fn seed_history_ansi(&self, ansi: &str) {
        if ansi.is_empty() {
            return;
        }
        // Production calls happen during pane construction, before this fresh
        // core is shared with runtime tasks. Keep a diagnostic if that
        // invariant ever changes and restored history cannot be seeded.
        let Ok(mut core) = shepr_vt::lock_terminal_core(&self.core) else {
            self.report_terminal_mutation_failure("history seed");
            return;
        };
        core.terminal.write(ansi.as_bytes());
        // Saved history is trimmed, so it normally ends on the last restored
        // line with no line break. Without one the cursor stays at the end of
        // that line and the fresh shell prints its first prompt glued onto it.
        if !ansi.ends_with('\n') {
            core.terminal.write(b"\r\n");
        }
        // Mark every retained row, including blank rows. A live write
        // replaces the cell flags with the cursor template, so later output
        // into one of these blank rows becomes live detection evidence.
        for row in 0..core.terminal.total_rows() {
            core.terminal.mark_screen_row_seeded(ScreenRow(row));
        }
        // Restored history must never answer the live child, nor surface as
        // live clipboard writes, directory reports or title and colour
        // changes.
        discard_core_effects(&mut core.terminal);
    }

    pub(crate) fn resize(&self, geometry: shepr_core::geometry::PaneGeometry) -> Vec<Bytes> {
        let rows = geometry.rows();
        let mut core = match shepr_vt::lock_terminal_core(&self.core) {
            Ok(core) => core,
            Err(_) => {
                self.report_terminal_mutation_failure("resize");
                return Vec::new();
            }
        };
        let synchronized_output_before = core
            .terminal
            .mode_get(shepr_vt::DecMode::SynchronizedOutput);
        let offset_from_bottom = core.terminal.scrollbar();
        let offset_from_bottom = offset_from_bottom
            .total
            .saturating_sub(offset_from_bottom.offset + offset_from_bottom.len);
        let resize_recovery_probe_lines = usize::from(rows)
            .saturating_mul(8)
            .max(DEFAULT_DETECTION_ROWS);

        // Alacritty resizes and reflows the grid directly. Replaying history
        // through the parser here could split a sequence the child is still
        // writing and move its cursor behind its back.
        let grid_before = (core.terminal.cols(), core.terminal.rows());
        core.terminal.resize(geometry);
        if (core.terminal.cols(), core.terminal.rows()) != grid_before {
            core.history_epoch = core.history_epoch.wrapping_add(1);
        }
        let synchronized_output_after = core
            .terminal
            .mode_get(shepr_vt::DecMode::SynchronizedOutput);
        if synchronized_output_after != synchronized_output_before {
            core.synchronized_output_epoch = core.synchronized_output_epoch.wrapping_add(1);
        }
        let terminal_responses = drain_terminal_responses(&mut core);

        terminal_set_scroll_offset_from_bottom(&mut core.terminal, offset_from_bottom);
        if offset_from_bottom > 0 {
            let mut remaining = offset_from_bottom.min(resize_recovery_probe_lines);
            // Check the current viewport once; each step toward live output
            // introduces just one new row at the bottom.
            let viewport = core.terminal.scrollbar();
            let mut scratch = String::new();
            let viewport_has_text = (viewport.offset..viewport.offset.saturating_add(viewport.len))
                .any(|row| {
                    terminal_screen_row_has_text(&core.terminal, ScreenRow(row), &mut scratch)
                });

            if !viewport_has_text {
                while remaining > 0 {
                    let entering_row = core
                        .terminal
                        .scrollbar()
                        .offset
                        .saturating_add(viewport.len);
                    core.terminal.scroll_viewport_delta(1);
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

    pub(crate) fn scroll_up(&self, lines: usize) {
        let Ok(mut core) = shepr_vt::lock_terminal_core(&self.core) else {
            self.report_terminal_mutation_failure("scroll up");
            return;
        };
        let lines = isize::try_from(lines).unwrap_or(isize::MAX);
        core.terminal.scroll_viewport_delta(-lines);
    }

    pub(crate) fn scroll_down(&self, lines: usize) {
        let Ok(mut core) = shepr_vt::lock_terminal_core(&self.core) else {
            self.report_terminal_mutation_failure("scroll down");
            return;
        };
        let lines = isize::try_from(lines).unwrap_or(isize::MAX);
        core.terminal.scroll_viewport_delta(lines);
    }

    pub(crate) fn scroll_reset(&self) {
        let Ok(mut core) = shepr_vt::lock_terminal_core(&self.core) else {
            self.report_terminal_mutation_failure("scroll reset");
            return;
        };
        core.terminal.scroll_viewport_bottom();
    }

    pub(crate) fn clear_screen(&self) -> Result<(), PaneClearError> {
        let mut core = shepr_vt::lock_terminal_core(&self.core)
            .map_err(|_| PaneClearError::TerminalLockPoisoned)?;
        match core.terminal.clear_screen() {
            shepr_vt::ClearScreenOutcome::Cleared => Ok(()),
            shepr_vt::ClearScreenOutcome::AlternateScreenActive => {
                Err(PaneClearError::AlternateScreenActive)
            }
        }
    }

    pub(crate) fn set_scroll_offset_from_bottom(&self, lines: usize) {
        let Ok(mut core) = shepr_vt::lock_terminal_core(&self.core) else {
            self.report_terminal_mutation_failure("set scroll offset");
            return;
        };
        terminal_set_scroll_offset_from_bottom(&mut core.terminal, lines);
    }

    pub(crate) fn scroll_metrics(&self) -> Option<ScrollMetrics> {
        let Ok(core) = shepr_vt::lock_terminal_core(&self.core) else {
            return None;
        };
        Some(terminal_scroll_metrics(&core.terminal))
    }

    /// Chunked copy-mode search. The terminal lock is released between
    /// chunks; rows are absolute, so output meanwhile does not move them.
    pub(crate) fn search_text_window(
        &self,
        query: &str,
        case_sensitive: bool,
        direction: TerminalSearchDirection,
        cursor: TerminalTextPoint,
        previous: Option<(TerminalTextPoint, TerminalTextPoint)>,
        limit: usize,
    ) -> TerminalSearchWindow {
        let Some(mut search) =
            TextSearch::new(query, case_sensitive, direction, cursor, previous, limit)
        else {
            return TerminalSearchWindow::empty();
        };
        let mut builder = TextBufferBuilder::new(true, false);
        let mut scratch = String::new();
        let mut next = None;
        let mut scan = None;
        loop {
            let Ok(core) = shepr_vt::lock_terminal_core(&self.core) else {
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

    pub(crate) fn negotiated_keyboard_protocol(
        &self,
    ) -> Option<shepr_termio::input::KeyboardProtocol> {
        let Ok(core) = shepr_vt::lock_terminal_core(&self.core) else {
            return None;
        };
        Some(shepr_termio::input::KeyboardProtocol::from_kitty_flags(
            core.terminal.kitty_keyboard_flags(),
        ))
    }

    pub(crate) fn bracketed_paste_enabled(&self) -> bool {
        self.mode_enabled(shepr_vt::DecMode::BracketedPaste)
    }

    pub(crate) fn focus_reporting_enabled(&self) -> bool {
        self.mode_enabled(shepr_vt::DecMode::FocusEvents)
    }

    pub(crate) fn mouse_reporting_enabled(&self) -> bool {
        shepr_vt::lock_terminal_core(&self.core)
            .is_ok_and(|core| core.terminal.mouse_tracking_enabled())
    }

    pub(crate) fn modify_other_keys_level(&self) -> u8 {
        shepr_vt::lock_terminal_core(&self.core)
            .map_or(0, |core| core.terminal.modify_other_keys_level().as_u8())
    }

    pub(crate) fn sgr_pixel_mouse_enabled(&self) -> bool {
        self.mode_enabled(shepr_vt::DecMode::MouseSgrPixels)
    }

    fn mode_enabled(&self, mode: shepr_vt::DecMode) -> bool {
        shepr_vt::lock_terminal_core(&self.core).is_ok_and(|core| core.terminal.mode_get(mode))
    }

    pub(crate) fn plain_page_keys_use_host_scrollback(&self) -> Option<bool> {
        let core = shepr_vt::lock_terminal_core(&self.core).ok()?;
        let alternate_screen = core.terminal.active_screen() == shepr_vt::ActiveScreen::Alternate;
        let mouse_reporting = core.terminal.mouse_tracking_enabled();
        let application_cursor = core
            .terminal
            .mode_get(shepr_vt::DecMode::ApplicationCursorKeys);
        let bracketed_paste = core.terminal.mode_get(shepr_vt::DecMode::BracketedPaste);
        Some(!alternate_screen && !mouse_reporting && (!application_cursor || bracketed_paste))
    }

    pub(crate) fn alternate_screen_active(&self) -> bool {
        shepr_vt::lock_terminal_core(&self.core)
            .is_ok_and(|core| core.terminal.active_screen() == shepr_vt::ActiveScreen::Alternate)
    }

    pub(crate) fn wheel_routing(&self) -> Option<crate::pane::WheelRouting> {
        let Ok(core) = shepr_vt::lock_terminal_core(&self.core) else {
            return None;
        };
        let alternate_screen = core.terminal.active_screen() == shepr_vt::ActiveScreen::Alternate;
        let mouse_alternate_scroll = core
            .terminal
            .mode_get(shepr_vt::DecMode::MouseAlternateScroll);
        let mouse_reporting = core.terminal.mouse_tracking_enabled();
        Some(if mouse_reporting {
            crate::pane::WheelRouting::MouseReport
        } else if alternate_screen && mouse_alternate_scroll {
            crate::pane::WheelRouting::AlternateScroll
        } else {
            crate::pane::WheelRouting::HostScroll
        })
    }

    pub(crate) fn cursor_state(&self) -> Option<TerminalCursorState> {
        let mut core = shepr_vt::lock_terminal_core(&self.core).ok()?;
        current_cursor_state(&mut core)
    }

    pub(crate) fn synchronized_output_active(&self) -> bool {
        shepr_vt::lock_terminal_core(&self.core)
            .ok()
            .is_some_and(|core| {
                core.terminal
                    .mode_get(shepr_vt::DecMode::SynchronizedOutput)
            })
    }

    pub(crate) fn synchronized_output_state(&self) -> Option<(bool, u64)> {
        let core = shepr_vt::lock_terminal_core(&self.core).ok()?;
        Some((
            core.terminal
                .mode_get(shepr_vt::DecMode::SynchronizedOutput),
            core.synchronized_output_epoch,
        ))
    }

    pub(crate) fn encode_terminal_key(
        &self,
        key: shepr_termio::input::TerminalKey,
        protocol: shepr_termio::input::KeyboardProtocol,
    ) -> Vec<u8> {
        let repeat_count = key.repeat_count;
        let first = key.with_repeat_count(1);
        let mut bytes = self.encode_terminal_key_once(first.clone(), protocol);
        if repeat_count > 1 && first.kind != crossterm::event::KeyEventKind::Release {
            let repeated = first.with_kind(crossterm::event::KeyEventKind::Repeat);
            let repeated_bytes = self.encode_terminal_key_once(repeated, protocol);
            for _ in 1..repeat_count {
                bytes.extend_from_slice(&repeated_bytes);
            }
        }
        bytes
    }

    pub(super) fn encode_terminal_key_once(
        &self,
        key: shepr_termio::input::TerminalKey,
        protocol: shepr_termio::input::KeyboardProtocol,
    ) -> Vec<u8> {
        // Character keys follow the caller's protocol; every other key follows
        // the modes the child negotiated with this pane.
        if matches!(key.code, crossterm::event::KeyCode::Char(_)) {
            return shepr_termio::input::encode_terminal_key(key, protocol);
        }
        let Some(modes) = shepr_vt::lock_terminal_core(&self.core).ok().map(|core| {
            shepr_termio::input::KeyEncodeModes {
                kitty_flags: core.terminal.kitty_keyboard_flags(),
                modify_other_keys: core.terminal.modify_other_keys_level().as_u8(),
                application_cursor: core
                    .terminal
                    .mode_get(shepr_vt::DecMode::ApplicationCursorKeys),
            }
        }) else {
            return shepr_termio::input::encode_terminal_key(key, protocol);
        };
        shepr_termio::input::encode_terminal_key_with_modes(key, modes)
    }

    pub(crate) fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        use crossterm::event::MouseEventKind;
        if !matches!(
            kind,
            MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_)
        ) {
            return None;
        }
        self.encode_mouse_event(kind, position, modifiers)
    }

    pub(crate) fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        if kind != crossterm::event::MouseEventKind::Moved {
            return None;
        }
        self.encode_mouse_event(kind, position, modifiers)
    }

    pub(crate) fn encode_mouse_wheel(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        use crossterm::event::MouseEventKind;
        if !matches!(
            kind,
            MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ) {
            return None;
        }
        self.encode_mouse_event(kind, position, modifiers)
    }

    fn encode_mouse_event(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        let core = shepr_vt::lock_terminal_core(&self.core).ok()?;
        let terminal = &core.terminal;
        let mode_enabled = |mode: shepr_vt::DecMode| terminal.mode_get(mode);
        let mode = if mode_enabled(shepr_vt::DecMode::MouseAnyMotion) {
            shepr_termio::input::MouseProtocolMode::AnyMotion
        } else if mode_enabled(shepr_vt::DecMode::MouseButtonMotion) {
            shepr_termio::input::MouseProtocolMode::ButtonMotion
        } else if mode_enabled(shepr_vt::DecMode::MousePressRelease) {
            shepr_termio::input::MouseProtocolMode::PressRelease
        } else if mode_enabled(shepr_vt::DecMode::X10Mouse) {
            shepr_termio::input::MouseProtocolMode::Press
        } else {
            return None;
        };
        let cell_encoding = if mode_enabled(shepr_vt::DecMode::MouseSgr) {
            shepr_termio::input::MouseProtocolEncoding::Sgr
        } else if mode_enabled(shepr_vt::DecMode::MouseUtf8) {
            shepr_termio::input::MouseProtocolEncoding::Utf8
        } else {
            shepr_termio::input::MouseProtocolEncoding::Default
        };
        let sgr_pixels = mode_enabled(shepr_vt::DecMode::MouseSgrPixels);
        // Reports are 1-based. Pixel positions already arrive 1-based; cell
        // positions are shifted here. Under SGR-pixels (mode 1016) a cell
        // position is mapped to the top-left pixel of that cell using the same
        // integer cell pitch the pixel fallback below uses, so the child maps
        // it straight back to the cell. Only when the pane has no pixel
        // geometry at all is the cell sent as-is in SGR form: the child can't
        // know a cell size either then, and a report beats a dropped click.
        let cell_pitch = || {
            let cols = u32::from(terminal.cols());
            let rows = u32::from(terminal.rows());
            let width_px = terminal.width_px();
            let height_px = terminal.height_px();
            (cols > 0 && rows > 0 && width_px > 0 && height_px > 0)
                .then(|| ((width_px / cols).max(1), (height_px / rows).max(1)))
        };
        let (encoding, x, y) = match position {
            shepr_termio::input::mouse::Position::Cell { column, row } if sgr_pixels => {
                match cell_pitch() {
                    Some((cell_width, cell_height)) => (
                        shepr_termio::input::MouseProtocolEncoding::SgrPixels,
                        u32::from(column)
                            .saturating_mul(cell_width)
                            .saturating_add(1),
                        u32::from(row).saturating_mul(cell_height).saturating_add(1),
                    ),
                    None => (
                        shepr_termio::input::MouseProtocolEncoding::Sgr,
                        u32::from(column) + 1,
                        u32::from(row) + 1,
                    ),
                }
            }
            shepr_termio::input::mouse::Position::Cell { column, row } => {
                (cell_encoding, u32::from(column) + 1, u32::from(row) + 1)
            }
            shepr_termio::input::mouse::Position::Pixels { x, y } if sgr_pixels => {
                (shepr_termio::input::MouseProtocolEncoding::SgrPixels, x, y)
            }
            shepr_termio::input::mouse::Position::Pixels { x, y } => {
                let cols = u32::from(terminal.cols());
                let rows = u32::from(terminal.rows());
                let (cell_width, cell_height) = cell_pitch()?;
                (
                    cell_encoding,
                    (x.saturating_sub(1) / cell_width).min(cols - 1) + 1,
                    (y.saturating_sub(1) / cell_height).min(rows - 1) + 1,
                )
            }
        };
        shepr_termio::input::encode_mouse_event(kind, x, y, modifiers, mode, encoding)
    }

    pub(crate) fn detection_text(&self) -> String {
        shepr_vt::lock_terminal_core(&self.core)
            .ok()
            .and_then(|core| terminal_detection_text(&core.terminal).ok())
            .unwrap_or_default()
    }

    pub(crate) fn extract_selection<P>(
        &self,
        selection: &shepr_vt::selection::Selection<P>,
    ) -> Option<String> {
        shepr_vt::lock_terminal_core(&self.core)
            .ok()
            .and_then(|mut core| terminal_extract_selection(&mut core, selection))
    }

    /// Read primary-screen history with nothing cached, in bounded chunks.
    /// The inactive primary grid is inaccessible while the alternate screen
    /// is active, so return None rather than replacing saved history with a
    /// full-screen program frame. Saves read through a
    /// [`super::PaneHistorySource`] with a cache kept between them instead.
    pub(crate) fn primary_history_ansi(&self) -> Option<String> {
        let mut cache = super::PaneHistoryCache::default();
        self.read_primary_history(&mut cache).map(|()| cache.text())
    }

    /// Writes the visible screen into `area` of `frame`, cell by cell: typed
    /// underline shapes, wide-glyph tails (empty symbols) and OSC 8 links
    /// (added to the frame's link table) go straight to the wire form.
    ///
    /// Draws nothing while a synchronized update is open or the core is
    /// unreadable. The part of `area` outside the frame is not drawn, and rows
    /// or columns the screen does not have are blank in the terminal's default
    /// colours.
    pub(crate) fn render_into(&self, frame: &mut FrameData, area: Rect) {
        let Ok(mut core) = shepr_vt::lock_terminal_core(&self.core) else {
            return;
        };
        if core
            .terminal
            .mode_get(shepr_vt::DecMode::SynchronizedOutput)
        {
            return;
        }
        let host_theme = core.host_terminal_theme;
        let initial_default_foreground = core.initial_default_foreground;
        let initial_default_background = core.initial_default_background;
        let PaneTerminalCore {
            terminal,
            render_state,
            ..
        } = &mut *core;
        let terminal: &shepr_vt::Terminal = terminal;
        render_state.update(terminal);
        let colors = render_state.colors();
        let default_bg =
            terminal_default_bg(colors.background, host_theme, initial_default_background);
        let default_fg =
            terminal_default_fg(colors.foreground, host_theme, initial_default_foreground);
        let resolved_fg = Some(terminal_color(colors.foreground));
        let resolved_bg = Some(terminal_color(colors.background));
        let default_palette = terminal.default_palette();
        let palette_overrides = PaletteOverrides::new(&colors.palette, &default_palette);

        let frame_width = usize::from(frame.width);
        if frame.cells.len() != frame_width * usize::from(frame.height) {
            return;
        }
        let area = area.intersection(Rect::new(0, 0, frame.width, frame.height));
        if area.is_empty() {
            return;
        }
        let blank = CellPaint::blank(default_fg, default_bg);
        let mut symbol_scratch = String::new();
        let mut rows_drawn = 0u16;
        for row in render_state.iter_rows().take(usize::from(area.height)) {
            let row_start = usize::from(area.y + rows_drawn) * frame_width + usize::from(area.x);
            let mut x = 0usize;
            for cell_view in row.cells().take(usize::from(area.width)) {
                let basic = cell_view.basic_data();
                let paint = terminal_cell_paint(
                    &cell_view,
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
                        .viewport_hyperlink_uri(x, ViewportRow(row.y()))
                        .ok()
                        .flatten()
                        .and_then(|uri| frame.intern_hyperlink(&uri))
                } else {
                    None
                };
                let cell = &mut frame.cells[row_start + x];
                paint.write_cell(cell, symbol, hyperlink);
                x += 1;
            }
            for cell in &mut frame.cells[row_start + x..row_start + usize::from(area.width)] {
                blank.write_cell(cell, " ", None);
            }
            rows_drawn += 1;
        }
        for y in rows_drawn..area.height {
            let row_start = usize::from(area.y + y) * frame_width + usize::from(area.x);
            for cell in &mut frame.cells[row_start..row_start + usize::from(area.width)] {
                blank.write_cell(cell, " ", None);
            }
        }
        // A full render draws every row whatever its dirty flag says, so it
        // leaves the flags alone: they belong to dirty-patch collection
        // alone. Clearing them here let a full frame drawn for one purpose
        // swallow rows a later patch still had to send.
    }

    pub(crate) fn collect_dirty_patch(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> TerminalDirtyPatchOutcome {
        let Ok(mut core) = shepr_vt::lock_terminal_core(&self.core) else {
            self.report_dirty_patch_fallback("terminal core lock poisoned");
            return TerminalDirtyPatchOutcome::Fallback;
        };
        if let Some(hook) = core.dirty_collection_hook.take() {
            hook();
        }
        if core
            .terminal
            .mode_get(shepr_vt::DecMode::SynchronizedOutput)
        {
            return TerminalDirtyPatchOutcome::Fallback;
        }
        let collection = terminal_collect_dirty_patch(&mut core, area_width, area_height);
        let fallback_reason = collection.fallback_reason;
        let outcome = collection.outcome;
        if matches!(outcome, TerminalDirtyPatchOutcome::Fallback) {
            drop(core);
            if let Some(reason) = fallback_reason {
                self.report_dirty_patch_fallback(reason);
            }
            return outcome;
        }
        drop(core);
        outcome
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

#[cfg(test)]
impl PaneTerminal {
    pub(crate) fn has_transient_default_color_override(&self) -> bool {
        shepr_vt::lock_terminal_core(&self.core)
            .is_ok_and(|core| core.transient_default_color_owner_pgid.is_some())
    }

    pub(crate) fn visible_text(&self) -> String {
        shepr_vt::lock_terminal_core(&self.core)
            .map_or_default(|mut core| terminal_visible_text(&mut core))
    }

    pub(crate) fn visible_ansi(&self) -> String {
        shepr_vt::lock_terminal_core(&self.core)
            .ok()
            .and_then(|core| terminal_visible_ansi(&core).ok())
            .unwrap_or_default()
    }

    // Test-only reads compare retained content and replay against the chunked
    // production history reader; they need only text, not truncation metadata.
    pub(crate) fn recent_text(&self, lines: usize) -> String {
        shepr_vt::lock_terminal_core(&self.core)
            .ok()
            .and_then(|mut core| terminal_recent_text(&mut core, lines).ok())
            .unwrap_or_default()
    }

    pub(crate) fn recent_ansi(&self, lines: usize) -> String {
        shepr_vt::lock_terminal_core(&self.core)
            .ok()
            .and_then(|mut core| terminal_recent_ansi(&mut core, lines).ok())
            .unwrap_or_default()
    }

    pub(crate) fn recent_unwrapped_text(&self, lines: usize) -> String {
        shepr_vt::lock_terminal_core(&self.core)
            .ok()
            .and_then(|mut core| terminal_recent_text_unwrapped(&mut core, lines).ok())
            .unwrap_or_default()
    }

    pub(crate) fn process_pty_bytes(&self, pane_id: PaneId, bytes: &[u8]) -> ProcessBytesResult {
        self.process_pty_bytes_at(pane_id, bytes, Instant::now())
    }

    pub(crate) fn scroll_position(&self) -> Option<ScrollPosition> {
        let core = shepr_vt::lock_terminal_core(&self.core).ok()?;
        Some(ScrollPosition {
            metrics: terminal_scroll_metrics(&core.terminal),
        })
    }

    // This aggregate snapshot performs multiple terminal queries. Pane-scaled
    // callers should add a narrow accessor instead.
    pub(crate) fn input_state(&self) -> Option<InputState> {
        let Ok(core) = shepr_vt::lock_terminal_core(&self.core) else {
            return None;
        };
        let alternate_screen = core.terminal.active_screen() == shepr_vt::ActiveScreen::Alternate;
        let application_cursor = core
            .terminal
            .mode_get(shepr_vt::DecMode::ApplicationCursorKeys);
        let bracketed_paste = core.terminal.mode_get(shepr_vt::DecMode::BracketedPaste);
        let focus_reporting = core.terminal.mode_get(shepr_vt::DecMode::FocusEvents);
        let mouse_sgr = core.terminal.mode_get(shepr_vt::DecMode::MouseSgr);
        let mouse_utf8 = core.terminal.mode_get(shepr_vt::DecMode::MouseUtf8);
        let mouse_sgr_pixels = core.terminal.mode_get(shepr_vt::DecMode::MouseSgrPixels);
        let mouse_alternate_scroll = core
            .terminal
            .mode_get(shepr_vt::DecMode::MouseAlternateScroll);
        let mouse_protocol_mode = if core.terminal.mode_get(shepr_vt::DecMode::MouseAnyMotion) {
            shepr_termio::input::MouseProtocolMode::AnyMotion
        } else if core.terminal.mode_get(shepr_vt::DecMode::MouseButtonMotion) {
            shepr_termio::input::MouseProtocolMode::ButtonMotion
        } else if core.terminal.mode_get(shepr_vt::DecMode::MousePressRelease) {
            shepr_termio::input::MouseProtocolMode::PressRelease
        } else if core.terminal.mode_get(shepr_vt::DecMode::X10Mouse) {
            shepr_termio::input::MouseProtocolMode::Press
        } else {
            shepr_termio::input::MouseProtocolMode::None
        };
        let mouse_protocol_encoding = if mouse_sgr_pixels {
            shepr_termio::input::MouseProtocolEncoding::SgrPixels
        } else if mouse_sgr {
            shepr_termio::input::MouseProtocolEncoding::Sgr
        } else if mouse_utf8 {
            shepr_termio::input::MouseProtocolEncoding::Utf8
        } else {
            shepr_termio::input::MouseProtocolEncoding::Default
        };
        Some(InputState {
            alternate_screen,
            application_cursor,
            bracketed_paste,
            focus_reporting,
            mouse_protocol_mode,
            mouse_protocol_encoding,
            mouse_alternate_scroll,
            modify_other_keys: core.terminal.modify_other_keys_level()
                == shepr_vt::ModifyOtherKeysLevel::All,
            color_scheme_reporting: core.terminal.mode_get(shepr_vt::DecMode::ColorSchemeReport),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writing_into_a_blank_seeded_row_makes_it_live() {
        let pane = PaneTerminal::new(shepr_vt::Terminal::new(80, 4, 0));
        pane.seed_history_ansi("saved first row\r\n\r\nsaved third row");

        let mut core = shepr_vt::lock_terminal_core(&pane.core).expect("terminal core");
        let mut scratch = String::new();
        let mut row_text = String::new();
        let seeded = terminal_screen_row_into_with_seeded(
            &core.terminal,
            ScreenRow(1),
            &mut scratch,
            &mut row_text,
        );
        assert!(seeded, "the blank restored row is marked seeded");
        assert!(row_text.is_empty());

        core.terminal.write(b"\x1b[2;1Hlive in restored blank row");
        let seeded = terminal_screen_row_into_with_seeded(
            &core.terminal,
            ScreenRow(1),
            &mut scratch,
            &mut row_text,
        );
        assert!(!seeded, "output written into it makes the row live");
        assert_eq!(row_text, "live in restored blank row");
    }

    #[test]
    fn resize_returns_queued_replies_before_its_own() {
        let pane = PaneTerminal::new(shepr_vt::Terminal::new(80, 24, 0));
        {
            let mut core = shepr_vt::lock_terminal_core(&pane.core).expect("terminal core");
            core.terminal.write(b"\x1b[?2048h\x1b[5n");
        }

        let replies = pane.resize(shepr_core::geometry::PaneGeometry::new(80, 24, 9, 18));

        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].as_ref(), b"\x1b[0n");
        assert_eq!(replies[1].as_ref(), b"\x1b[48;24;80;432;720t");
    }
}
