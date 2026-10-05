use super::*;

/// Why the detector could not read one coherent snapshot from the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDetectionReadError {
    TerminalCorePoisoned,
    ScreenReadFailed,
}

impl std::fmt::Display for AgentDetectionReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TerminalCorePoisoned => f.write_str("terminal core lock is poisoned"),
            Self::ScreenReadFailed => f.write_str("terminal screen text could not be read"),
        }
    }
}

impl std::error::Error for AgentDetectionReadError {}

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

    pub fn mouse_reporting_enabled(&self) -> bool {
        self.terminal.mouse_reporting_enabled()
    }

    pub fn pixel_mouse(&self) -> shepr_term::mouse::PanePixelMouse {
        self.terminal.pixel_mouse()
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
    /// The synchronized-output gate is decided in the same terminal-core hold
    /// as the cursor ([`CursorRead::Deferred`]).
    pub fn cursor(&self, area: Rect) -> crate::pane::CursorRead {
        use crate::pane::CursorRead;

        match self.terminal.cursor_read() {
            CursorRead::Shown(cursor) if cursor.x >= area.width || cursor.y >= area.height => {
                CursorRead::Unavailable
            }
            CursorRead::Shown(cursor) => CursorRead::Shown(TerminalCursorState {
                x: area.x + cursor.x,
                y: area.y + cursor.y,
                visible: cursor.visible,
                shape: cursor.shape,
            }),
            other => other,
        }
    }

    /// Whether a synchronized update is open, read without the core lock.
    pub fn synchronized_output_active(&self) -> bool {
        self.terminal.synchronized_output_active()
    }

    /// Whether the pane's surface is held back: its core is poisoned or a
    /// synchronized update is open. Lock-free, so a caller planning a render
    /// never waits on a PTY reader; drawing still decides under the lock
    /// ([`Self::synchronized_output_state`]).
    pub fn surface_held(&self) -> bool {
        self.terminal.surface_held()
    }

    /// The synchronized-output flag and epoch together. A poisoned core reads
    /// as [`SyncState::Poisoned`], so render callers defer the frame instead of
    /// treating an invented state as a successful read.
    pub fn synchronized_output_state(&self) -> super::SyncState {
        self.terminal.synchronized_output_state()
    }

    /// Live text returned by the server's detect capture API.
    pub fn detection_text(&self) -> String {
        self.terminal.detection_text()
    }

    pub fn terminal_title(&self) -> Option<String> {
        self.terminal.terminal_title()
    }

    /// Snapshot of screen text, OSC title and OSC progress, read together
    /// under one terminal lock. A failed read stays an error so on-demand
    /// diagnostics cannot present fabricated empty-screen evidence.
    pub fn agent_detection_inputs(
        &self,
    ) -> Result<super::AgentDetectionInputs, AgentDetectionReadError> {
        // PaneTerminal has already collapsed lock poisoning and VT read errors
        // into `None`; retain failure and classify the actionable poison case
        // without inventing a more specific screen-read cause.
        self.terminal.agent_detection_inputs().ok_or_else(|| {
            if self.terminal.core_poisoned() {
                AgentDetectionReadError::TerminalCorePoisoned
            } else {
                AgentDetectionReadError::ScreenReadFailed
            }
        })
    }

    pub fn extract_selection<P>(
        &self,
        selection: &shepr_vt::selection::Selection<P>,
    ) -> Option<String> {
        self.terminal.extract_selection(selection)
    }

    /// Draws the visible screen into `area` of a wire frame; see
    /// [`PaneTerminal::render_into`]. The result says whether it drew and, if
    /// it did, the state it drew from.
    pub fn render_into(
        &self,
        frame: &mut shepr_protocol::FrameData,
        area: Rect,
    ) -> crate::pane::PaneDraw {
        self.terminal.render_into(frame, area)
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

    /// The render revision; `None` when the core is poisoned, which must
    /// never certify a stable surface ([`ContentRevision::certify`]).
    pub fn content_revision(&self) -> Option<super::ContentRevision> {
        self.terminal.content_revision()
    }
}
