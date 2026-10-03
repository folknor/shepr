use super::*;

#[derive(Clone, Copy)]
pub struct PaneRead<'a> {
    pub(super) terminal: &'a Arc<PaneTerminal>,
}

impl PaneRead<'_> {
    pub fn scroll_metrics(&self) -> Option<ScrollMetrics> {
        self.terminal.scroll_metrics()
    }

    pub fn search_text_window(
        &self,
        search: crate::pane::TerminalTextSearch<'_>,
    ) -> crate::pane::TerminalSearchWindow {
        self.terminal.search_text_window(search)
    }

    /// Resolve every copy-mode motion against the same stable cursor point.
    /// Line motions need retained row text; word and paragraph motions stay at
    /// the cursor when the requested target is unavailable.
    pub fn copy_motion(
        &self,
        cursor: crate::pane::TerminalTextPoint,
        motion: crate::pane::TerminalCopyMotion,
    ) -> Result<crate::pane::TerminalTextPoint, crate::pane::TerminalCopyMotionError> {
        use crate::pane::{TerminalCopyMotion, TerminalLineMotion};

        match motion {
            TerminalCopyMotion::Line(motion) => {
                let selection =
                    shepr_vt::selection::Selection::line_range((), cursor.row, cursor.row);
                let Some(text) = self.extract_selection(&selection) else {
                    return Err(crate::pane::TerminalCopyMotionError::RowUnavailable);
                };
                let width = self.terminal.dimensions().map_or(1, |grid| grid.cols.get());
                let col = match motion {
                    TerminalLineMotion::End => {
                        shepr_term::width::last_character_col(&text).unwrap_or(0)
                    }
                    TerminalLineMotion::FirstNonBlank => {
                        shepr_term::width::first_non_blank_col(&text).unwrap_or(0)
                    }
                };
                Ok(shepr_vt::Point::new(
                    cursor.row,
                    col.min(width.saturating_sub(1)),
                ))
            }
            TerminalCopyMotion::Word(motion) => Ok(self
                .terminal
                .word_motion_target(cursor, motion)
                .unwrap_or(cursor)),
            TerminalCopyMotion::Paragraph(motion) => Ok(self
                .terminal
                .paragraph_motion_target(cursor, motion)
                .unwrap_or(cursor)),
        }
    }

    pub fn terminal_dimensions(&self) -> Option<shepr_core::geometry::GridSize> {
        self.terminal.dimensions()
    }

    pub fn bracketed_paste_enabled(&self) -> bool {
        self.terminal.bracketed_paste_enabled()
    }

    /// Capture all pane input modes together for one routing and encoding
    /// decision.
    pub fn input_modes(&self) -> Option<shepr_vt::InputModes> {
        self.terminal.input_modes()
    }

    pub fn focus_reporting_enabled(&self) -> bool {
        self.terminal.focus_reporting_enabled()
    }

    pub fn mouse_reporting_enabled(&self) -> bool {
        self.terminal.mouse_reporting_enabled()
    }

    pub fn sgr_pixel_mouse_enabled(&self) -> bool {
        self.terminal.sgr_pixel_mouse_enabled()
    }

    pub fn plain_page_keys_use_host_scrollback(&self) -> Option<bool> {
        self.terminal.plain_page_keys_use_host_scrollback()
    }

    pub fn alternate_screen_active(&self) -> bool {
        self.terminal.alternate_screen_active()
    }

    /// Whether output has flipped the active screen since the last
    /// `take_screen_flip`. A lock-free load, for the server's per-plan check.
    pub fn screen_flip_pending(&self) -> bool {
        self.terminal.screen_flip_pending()
    }

    /// Clip to the pane content area before translating into surface coordinates.
    pub fn cursor_state(&self, area: Rect) -> Option<TerminalCursorState> {
        let cursor = self.terminal.cursor_state()?;
        if cursor.x >= area.width || cursor.y >= area.height {
            return None;
        }
        Some(TerminalCursorState {
            x: area.x + cursor.x,
            y: area.y + cursor.y,
            visible: cursor.visible,
            shape: cursor.shape,
        })
    }

    pub fn synchronized_output_active(&self) -> bool {
        self.terminal.synchronized_output_active()
    }

    /// Returns the synchronized-output flag and generation together. `None`
    /// means the terminal core is poisoned, so render callers must defer the
    /// frame instead of treating an invented state as a successful read.
    pub fn synchronized_output_state(&self) -> Option<(bool, u64)> {
        self.terminal.synchronized_output_state()
    }

    /// Live text returned by the server's detect capture API.
    pub fn detection_text(&self) -> String {
        self.terminal.detection_text()
    }

    pub fn terminal_title(&self) -> Option<String> {
        self.terminal.terminal_title()
    }

    /// The screen text, OSC title and OSC progress the detector evaluates,
    /// read together under one terminal lock like the live detection tick.
    /// Unchanged seeded history rows are excluded from the screen text.
    pub fn agent_detection_inputs(&self) -> super::AgentDetectionInputs {
        self.terminal.agent_detection_inputs()
    }

    /// A handle that reads this pane's history from any thread, so a save
    /// can take it on the event loop and format the history off it.
    pub fn history_source(&self) -> super::PaneHistorySource {
        super::PaneHistorySource::new(Arc::clone(self.terminal))
    }

    pub fn extract_selection<P>(
        &self,
        selection: &shepr_vt::selection::Selection<P>,
    ) -> Option<String> {
        self.terminal.extract_selection(selection)
    }

    /// Draws the visible screen into `area` of a wire frame; see
    /// [`PaneTerminal::render_into`].
    pub fn render_into(&self, frame: &mut shepr_protocol::FrameData, area: Rect) {
        self.terminal.render_into(frame, area);
    }

    pub fn collect_dirty_patch_snapshot(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> Result<TerminalDirtyPatchSnapshot, crate::pane::PatchUnavailable> {
        // Patch, revision and metadata are read in one terminal-core hold.
        self.terminal
            .collect_dirty_patch_snapshot(area_width, area_height)
    }

    /// Odd means unavailable or torn; it must never certify a stable surface.
    pub fn content_seq(&self) -> u64 {
        self.terminal
            .core
            .lock()
            .map_or(1, |core| core.content_revision)
    }
}
