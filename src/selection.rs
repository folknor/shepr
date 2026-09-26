//! Text selection and clipboard support.
//!
//! Selection lifecycle:
//!
//!   MouseDown in pane → Anchor recorded (no visual yet)
//!   MouseDrag         → Selection becomes active, cells highlighted
//!   MouseUp           → Selection finalized; optionally copied by the caller
//!   Next click / key  → A retained selection is cleared
//!
//! Double-click selects a word; the caller decides whether to copy it immediately.
//!
//! Rows are stored as buffer rows, not viewport rows, so the selection stays
//! on its text while the viewport scrolls. Which buffer rows depends on what
//! the caller hands in to place the viewport:
//!
//! * `Option<ScrollMetrics>` (the methods without a suffix): screen rows, 0
//!   being the oldest retained line. They do not survive output once the
//!   pane's history is full: each new line evicts the oldest one and every
//!   screen row then names the line below the one it named before.
//! * the absolute row of the viewport's top row (the `_at` methods; the pane
//!   terminal's `ScrollPosition::viewport_top_row`): absolute rows,
//!   `history_origin + screen row`, which keep naming the same line while
//!   history is trimmed. A selection held or dragged during output needs
//!   these, and its text is read with `extract_selection_absolute`, which
//!   refuses rows that have been evicted.
//!
//! A selection must be built and read in one row space throughout.

use ratatui::layout::Rect;
use std::{ffi::OsStr, io::Write};

use crate::{layout::PaneId, pane::ScrollMetrics};

/// Current phase of a selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Mouse is down but hasn't moved yet. If released without
    /// moving, this was just a click - no selection created.
    Anchored,
    /// Mouse has moved from the anchor point. Cells are being highlighted.
    Dragging,
    /// Mouse released after dragging. Selection is visible and the gesture is
    /// complete. Clipboard policy is owned by the caller.
    Done,
}

/// A text selection within a terminal pane.
#[derive(Debug, Clone)]
pub struct Selection<P = PaneId> {
    /// Which pane the selection belongs to.
    pub pane_id: P,
    /// Anchor position in buffer rows (row, col); see the module docs.
    anchor: (u64, u16),
    /// Current/final position in buffer rows (row, col).
    cursor: (u64, u16),
    /// Selection phase.
    phase: Phase,
}

impl<P> Selection<P> {
    /// Start a potential selection. This records the anchor but doesn't
    /// make anything visible yet - the user might just be clicking.
    pub fn anchor(pane_id: P, viewport_row: u16, col: u16, metrics: Option<ScrollMetrics>) -> Self {
        Self::anchor_at(pane_id, viewport_row, col, viewport_top_row(metrics))
    }

    /// [`Selection::anchor`] in absolute rows: `viewport_top` is the
    /// absolute row of the viewport's top row.
    pub fn anchor_at(pane_id: P, viewport_row: u16, col: u16, viewport_top: u64) -> Self {
        let anchor = (row_below(viewport_top, viewport_row), col);
        Self {
            pane_id,
            anchor,
            cursor: anchor,
            phase: Phase::Anchored,
        }
    }

    pub(crate) fn absolute_anchor<R: Into<u64>>(pane_id: P, anchor: (R, u16)) -> Self {
        let anchor = (anchor.0.into(), anchor.1);
        Self {
            pane_id,
            anchor,
            cursor: anchor,
            phase: Phase::Anchored,
        }
    }

    pub(crate) fn absolute_range<R: Into<u64>>(
        pane_id: P,
        anchor: (R, u16),
        cursor: (R, u16),
    ) -> Self {
        Self {
            pane_id,
            anchor: (anchor.0.into(), anchor.1),
            cursor: (cursor.0.into(), cursor.1),
            phase: Phase::Dragging,
        }
    }

    pub(crate) fn line_range<R: Into<u64>>(
        pane_id: P,
        anchor_row: R,
        cursor_row: R,
        end_col: u16,
    ) -> Self {
        let (anchor_row, cursor_row) = (anchor_row.into(), cursor_row.into());
        let (anchor_col, cursor_col) = if anchor_row <= cursor_row {
            (0, end_col)
        } else {
            (end_col, 0)
        };
        Self {
            pane_id,
            anchor: (anchor_row, anchor_col),
            cursor: (cursor_row, cursor_col),
            phase: Phase::Dragging,
        }
    }

    /// Convert the anchor's buffer row and pane-relative column back to
    /// screen coordinates. Adds the pane origin before clamping so the
    /// returned (screen_row, screen_col) can be compared directly against
    /// mouse screen positions.
    pub fn anchor_screen_pos(
        &self,
        pane_inner: Rect,
        metrics: Option<ScrollMetrics>,
    ) -> (u16, u16) {
        self.anchor_screen_pos_at(pane_inner, viewport_top_row(metrics))
    }

    /// [`Selection::anchor_screen_pos`] in absolute rows.
    pub fn anchor_screen_pos_at(&self, pane_inner: Rect, viewport_top: u64) -> (u16, u16) {
        let viewport_row = viewport_row_for_row(self.anchor.0, viewport_top);
        // Convert pane-relative to screen coordinates, then clamp.
        let row = (viewport_row.saturating_add(pane_inner.y)).clamp(
            pane_inner.y,
            pane_inner.y + pane_inner.height.saturating_sub(1),
        );
        let col = (self.anchor.1.saturating_add(pane_inner.x)).clamp(
            pane_inner.x,
            pane_inner.x + pane_inner.width.saturating_sub(1),
        );
        (row, col)
    }

    /// Extend the selection as the mouse drags. Activates highlighting
    /// once the cursor moves to a different cell than the anchor.
    /// Screen coordinates are clamped to the pane boundary.
    pub fn drag(
        &mut self,
        screen_col: u16,
        screen_row: u16,
        pane_inner: Rect,
        metrics: Option<ScrollMetrics>,
    ) {
        self.drag_at(
            screen_col,
            screen_row,
            pane_inner,
            viewport_top_row(metrics),
        );
    }

    /// [`Selection::drag`] in absolute rows.
    pub fn drag_at(
        &mut self,
        screen_col: u16,
        screen_row: u16,
        pane_inner: Rect,
        viewport_top: u64,
    ) {
        let (viewport_row, col) = clamp_to_pane(screen_col, screen_row, pane_inner);
        self.cursor = (row_below(viewport_top, viewport_row), col);
        if self.cursor != self.anchor {
            self.phase = Phase::Dragging;
        }
    }

    /// Finalize the selection. Returns the selected range if the user
    /// actually dragged (not just clicked). Returns None for plain clicks.
    pub fn finish(&mut self) -> bool {
        if self.phase == Phase::Dragging {
            self.phase = Phase::Done;
            true
        } else {
            false
        }
    }

    /// Whether this selection should be rendered (highlight visible).
    pub fn is_visible(&self) -> bool {
        self.phase == Phase::Dragging || self.phase == Phase::Done
    }

    /// Whether this selection was already finalized.
    #[cfg(test)]
    pub fn is_finalized(&self) -> bool {
        self.phase == Phase::Done
    }

    /// Whether the user just clicked without dragging (not a selection).
    pub fn is_just_click(&self) -> bool {
        self.phase == Phase::Anchored
    }

    /// Force the selection into Dragging phase, used when the mouse
    /// has moved off the anchor cell but drag() couldn't transition
    /// because the cursor was clamped to the same cell as the anchor.
    pub fn force_dragging(&mut self) {
        if self.phase == Phase::Anchored {
            self.phase = Phase::Dragging;
        }
    }

    /// Whether the pointer is still down and the selection can keep extending.
    pub fn is_in_progress(&self) -> bool {
        matches!(self.phase, Phase::Anchored | Phase::Dragging)
    }

    /// Whether the user is actively dragging (cursor moved from anchor).
    pub fn is_dragging(&self) -> bool {
        self.phase == Phase::Dragging
    }

    /// Returns (start, end) in reading order (top-left to bottom-right).
    fn ordered(&self) -> ((u64, u16), (u64, u16)) {
        let (ar, ac) = self.anchor;
        let (cr, cc) = self.cursor;
        if ar < cr || (ar == cr && ac <= cc) {
            ((ar, ac), (cr, cc))
        } else {
            ((cr, cc), (ar, ac))
        }
    }

    /// The selected range in reading order, rows in the form screen rows
    /// take (a selection built with `ScrollMetrics`). Rows of an absolute
    /// selection past `u32::MAX` saturate; read those with
    /// [`Selection::ordered_rows`].
    pub(crate) fn ordered_cells(&self) -> ((u32, u16), (u32, u16)) {
        let ((sr, sc), (er, ec)) = self.ordered();
        let narrow = |row: u64| u32::try_from(row).unwrap_or(u32::MAX);
        ((narrow(sr), sc), (narrow(er), ec))
    }

    /// The selected range in reading order, rows as stored.
    pub(crate) fn ordered_rows(&self) -> ((u64, u16), (u64, u16)) {
        self.ordered()
    }

    /// Check whether a pane-relative cell (row, col) is inside the selection.
    pub fn contains(&self, viewport_row: u16, col: u16, metrics: Option<ScrollMetrics>) -> bool {
        self.contains_at(viewport_row, col, viewport_top_row(metrics))
    }

    /// [`Selection::contains`] in absolute rows.
    pub fn contains_at(&self, viewport_row: u16, col: u16, viewport_top: u64) -> bool {
        if !self.is_visible() {
            return false;
        }
        let row = row_below(viewport_top, viewport_row);
        let ((sr, sc), (er, ec)) = self.ordered();
        if row < sr || row > er {
            return false;
        }
        if sr == er {
            col >= sc && col <= ec
        } else if row == sr {
            col >= sc
        } else if row == er {
            col <= ec
        } else {
            true
        }
    }
}

/// The screen row of the viewport's top row.
fn viewport_top_row(metrics: Option<ScrollMetrics>) -> u64 {
    let value = metrics
        .map(|metrics| {
            metrics
                .max_offset_from_bottom
                .saturating_sub(metrics.offset_from_bottom)
        })
        .unwrap_or(0);
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// The screen row of a viewport row. Despite the name, not an absolute row:
/// see [`row_for_viewport_at`] for those.
pub(crate) fn absolute_row_for_viewport(viewport_row: u16, metrics: Option<ScrollMetrics>) -> u32 {
    u32::try_from(row_below(viewport_top_row(metrics), viewport_row)).unwrap_or(u32::MAX)
}

/// The absolute row of a viewport row, `viewport_top` being the absolute row
/// of the viewport's top row.
// Not called yet: the client shell still works in screen rows (see the
// module docs); this is the absolute counterpart of
// `absolute_row_for_viewport` for when it moves over.
#[allow(dead_code)]
pub(crate) fn row_for_viewport_at(viewport_row: u16, viewport_top: u64) -> u64 {
    row_below(viewport_top, viewport_row)
}

fn row_below(viewport_top: u64, viewport_row: u16) -> u64 {
    viewport_top.saturating_add(u64::from(viewport_row))
}

fn viewport_row_for_row(row: u64, viewport_top: u64) -> u16 {
    row.saturating_sub(viewport_top).try_into().unwrap_or(0)
}

fn clamp_to_pane(screen_col: u16, screen_row: u16, pane_inner: Rect) -> (u16, u16) {
    let clamped_col = screen_col.clamp(
        pane_inner.x,
        pane_inner.x + pane_inner.width.saturating_sub(1),
    );
    let clamped_row = screen_row.clamp(
        pane_inner.y,
        pane_inner.y + pane_inner.height.saturating_sub(1),
    );
    (clamped_row - pane_inner.y, clamped_col - pane_inner.x)
}

fn osc52_sequence(bytes: &[u8]) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("\x1b]52;c;{encoded}\x07")
}

fn contains_wsl_marker(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("microsoft") || lower.contains("wsl2") || lower.contains("-wsl")
}

fn is_wsl_for_env(
    os_release: Option<&str>,
    proc_version: Option<&str>,
    wsl_distro_name: Option<&OsStr>,
    wsl_interop: Option<&OsStr>,
    runtime_marker_exists: bool,
) -> bool {
    wsl_distro_name.is_some()
        || wsl_interop.is_some()
        || os_release.is_some_and(contains_wsl_marker)
        || proc_version.is_some_and(contains_wsl_marker)
        || runtime_marker_exists
}

fn is_wsl() -> bool {
    let os_release = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
    let proc_version = std::fs::read_to_string("/proc/version").ok();
    is_wsl_for_env(
        os_release.as_deref(),
        proc_version.as_deref(),
        std::env::var_os("WSL_DISTRO_NAME").as_deref(),
        std::env::var_os("WSL_INTEROP").as_deref(),
        std::path::Path::new("/run/WSL").exists()
            || std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists(),
    )
}

fn should_prefer_osc52_for_env(
    ssh_connection: Option<&OsStr>,
    ssh_tty: Option<&OsStr>,
    vscode_ipc_hook_cli: Option<&OsStr>,
    wsl: bool,
) -> bool {
    ssh_connection.is_some() || ssh_tty.is_some() || vscode_ipc_hook_cli.is_some() || wsl
}

fn should_prefer_osc52() -> bool {
    should_prefer_osc52_for_env(
        std::env::var_os("SSH_CONNECTION").as_deref(),
        std::env::var_os("SSH_TTY").as_deref(),
        std::env::var_os("VSCODE_IPC_HOOK_CLI").as_deref(),
        is_wsl(),
    )
}

/// Write clipboard bytes to the system clipboard via native platform tools or OSC 52.
///
/// OSC 52 format: `ESC ] 52 ; c ; <base64> BEL`
///
/// Some terminals still only honor BEL-terminated OSC 52 writes, so shepr
/// emits BEL here even though ST works in newer emulators.
pub fn write_osc52_bytes(bytes: &[u8]) {
    if !should_prefer_osc52() && crate::platform::write_clipboard(bytes) {
        return;
    }

    let sequence = osc52_sequence(bytes);
    let _ = std::io::stdout().write_all(sequence.as_bytes());
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sel(sr: u32, sc: u16, er: u32, ec: u16) -> Selection {
        let mut sel = Selection::anchor(
            PaneId::from_raw(0),
            u16::try_from(sr).unwrap_or(u16::MAX),
            sc,
            None,
        );
        sel.anchor = (u64::from(sr), sc);
        sel.cursor = (u64::from(er), ec);
        sel.phase = Phase::Dragging;
        sel
    }

    #[test]
    fn osc52_sequence_uses_bel_terminator() {
        assert_eq!(osc52_sequence(b"hello"), "\x1b]52;c;aGVsbG8=\x07");
    }

    #[test]
    fn ssh_sessions_prefer_osc52() {
        assert!(should_prefer_osc52_for_env(
            Some(OsStr::new("1 2 3 4")),
            None,
            None,
            false
        ));
        assert!(should_prefer_osc52_for_env(
            None,
            Some(OsStr::new("/dev/ttys001")),
            None,
            false
        ));
        assert!(!should_prefer_osc52_for_env(None, None, None, false));
    }

    #[test]
    fn wsl_sessions_prefer_osc52() {
        assert!(should_prefer_osc52_for_env(None, None, None, true));
    }

    #[test]
    fn vscode_remote_sessions_prefer_osc52() {
        assert!(should_prefer_osc52_for_env(
            None,
            None,
            Some(OsStr::new("/tmp/vscode-remote-cli.sock")),
            false
        ));
    }

    #[test]
    fn wsl_detection_uses_env_vars() {
        assert!(is_wsl_for_env(
            None,
            None,
            Some(OsStr::new("Ubuntu")),
            None,
            false
        ));
        assert!(is_wsl_for_env(
            None,
            None,
            None,
            Some(OsStr::new("/run/WSL/123_interop")),
            false
        ));
    }

    #[test]
    fn wsl_detection_uses_kernel_markers() {
        assert!(is_wsl_for_env(
            Some("5.15.167.4-microsoft-standard-WSL2"),
            None,
            None,
            None,
            false
        ));
        assert!(is_wsl_for_env(
            None,
            Some("Linux version 5.15.167.4-microsoft-standard-WSL2"),
            None,
            None,
            false
        ));
    }

    #[test]
    fn wsl_detection_ignores_non_wsl_kernel_strings() {
        assert!(!contains_wsl_marker("notwsl-kernel"));
        assert!(!is_wsl_for_env(
            Some("6.8.0-31-generic"),
            Some("Linux version 6.8.0-31-generic"),
            None,
            None,
            false
        ));
    }

    #[test]
    fn wsl_detection_uses_wsl_runtime_markers() {
        assert!(is_wsl_for_env(None, None, None, None, true));
        assert!(!is_wsl_for_env(None, None, None, None, false));
    }

    #[test]
    fn ordering_forward() {
        let sel = make_sel(2, 5, 4, 10);
        assert_eq!(sel.ordered(), ((2, 5), (4, 10)));
    }

    #[test]
    fn ordering_backward() {
        let sel = make_sel(4, 10, 2, 5);
        assert_eq!(sel.ordered(), ((2, 5), (4, 10)));
    }

    #[test]
    fn single_line_contains() {
        let sel = make_sel(2, 5, 2, 15);
        assert!(!sel.contains(2, 4, None));
        assert!(sel.contains(2, 5, None));
        assert!(sel.contains(2, 10, None));
        assert!(sel.contains(2, 15, None));
        assert!(!sel.contains(2, 16, None));
        assert!(!sel.contains(1, 10, None));
        assert!(!sel.contains(3, 10, None));
    }

    #[test]
    fn multi_line_contains() {
        let sel = make_sel(2, 5, 4, 10);
        assert!(!sel.contains(2, 4, None));
        assert!(sel.contains(2, 5, None));
        assert!(sel.contains(2, 79, None));
        assert!(sel.contains(3, 0, None));
        assert!(sel.contains(3, 79, None));
        assert!(sel.contains(4, 0, None));
        assert!(sel.contains(4, 10, None));
        assert!(!sel.contains(4, 11, None));
    }

    #[test]
    fn anchored_not_visible() {
        let sel = Selection::anchor(PaneId::from_raw(0), 5, 10, None);
        assert!(!sel.is_visible());
        assert!(!sel.contains(5, 10, None));
    }

    #[test]
    fn click_without_drag() {
        let mut sel = Selection::anchor(PaneId::from_raw(0), 5, 10, None);
        assert!(sel.is_just_click());
        let copied = sel.finish();
        assert!(!copied);
    }

    #[test]
    fn drag_then_finish() {
        let mut sel = Selection::anchor(PaneId::from_raw(0), 5, 10, None);
        sel.drag(20, 7, Rect::new(10, 5, 80, 24), None);
        assert!(sel.is_visible());
        assert!(!sel.is_just_click());
        let copied = sel.finish();
        assert!(copied);
    }

    #[test]
    fn drag_uses_buffer_rows_when_scrolled() {
        let mut sel = Selection::anchor(
            PaneId::from_raw(0),
            0,
            10,
            Some(ScrollMetrics {
                offset_from_bottom: 1,
                max_offset_from_bottom: 10,
                viewport_rows: 4,
            }),
        );

        sel.drag(
            10,
            5,
            Rect::new(10, 5, 80, 4),
            Some(ScrollMetrics {
                offset_from_bottom: 2,
                max_offset_from_bottom: 10,
                viewport_rows: 4,
            }),
        );

        assert_eq!(sel.ordered_cells(), ((8, 0), (9, 10)));
    }

    #[test]
    fn contains_tracks_current_viewport_after_scroll() {
        let sel = make_sel(8, 2, 10, 4);
        let metrics = Some(ScrollMetrics {
            offset_from_bottom: 2,
            max_offset_from_bottom: 10,
            viewport_rows: 4,
        });

        assert!(sel.contains(0, 2, metrics));
        assert!(sel.contains(1, 40, metrics));
        assert!(sel.contains(2, 4, metrics));
        assert!(!sel.contains(3, 4, metrics));
    }

    #[test]
    fn absolute_selection_follows_its_lines_when_the_viewport_top_moves() {
        let pane_inner = Rect::new(0, 5, 80, 10);
        // Anchored on viewport row 2 while the viewport's top is absolute
        // row 1000, dragged to viewport row 3.
        let mut sel = Selection::anchor_at(PaneId::from_raw(0), 2, 4, 1_000);
        sel.drag_at(9, 8, pane_inner, 1_000);
        assert_eq!(sel.ordered_rows(), ((1_002, 4), (1_003, 9)));

        // One line of output evicted a line and the viewport followed the
        // bottom: the selected lines now sit one viewport row higher.
        let top = 1_001;
        assert!(sel.contains_at(1, 4, top));
        assert!(sel.contains_at(2, 9, top));
        assert!(!sel.contains_at(3, 4, top));
        assert_eq!(sel.anchor_screen_pos_at(pane_inner, top), (6, 4));
        assert_eq!(row_for_viewport_at(1, top), 1_002);
    }

    #[test]
    fn absolute_rows_past_u32_keep_their_value() {
        let top = u64::from(u32::MAX) + 10;
        let sel = Selection::absolute_range(PaneId::from_raw(0), (top, 0), (top + 1, 3));
        assert_eq!(sel.ordered_rows(), ((top, 0), (top + 1, 3)));
        // The screen-row form cannot hold them.
        assert_eq!(sel.ordered_cells(), ((u32::MAX, 0), (u32::MAX, 3)));
        assert!(sel.contains_at(1, 2, top));
    }

    #[test]
    fn clamp_to_pane_bounds() {
        let (row, col) = clamp_to_pane(200, 100, Rect::new(10, 5, 80, 24));
        assert_eq!(row, 23);
        assert_eq!(col, 79);

        let (row, col) = clamp_to_pane(0, 0, Rect::new(10, 5, 80, 24));
        assert_eq!(row, 0);
        assert_eq!(col, 0);
    }

    #[test]
    fn anchor_screen_pos_adds_pane_origin() {
        // Pane offset by sidebar (x=10) and tab bar (y=5).
        // Anchor at viewport_row=3, col=5 (pane-relative).
        let sel = Selection::anchor(PaneId::from_raw(0), 3, 5, None);
        let pane_inner = Rect::new(10, 5, 80, 24);
        let (row, col) = sel.anchor_screen_pos(pane_inner, None);
        // Screen row = 3 + 5 = 8, screen col = 5 + 10 = 15
        assert_eq!(row, 8);
        assert_eq!(col, 15);
    }

    #[test]
    fn anchor_screen_pos_same_cell_as_mouse_with_offset() {
        // When the pane has a non-zero origin, anchor and mouse on the same
        // screen cell must compare equal - no false drag detection.
        let pane_inner = Rect::new(10, 5, 80, 24);
        // Mouse clicked at screen (15, 8) → anchor stored as (viewport_row=3, col=5)
        let sel = Selection::anchor(PaneId::from_raw(0), 3, 5, None);
        let (ar, ac) = sel.anchor_screen_pos(pane_inner, None);
        // Screen position of the anchor must match the mouse position
        assert_eq!((ar, ac), (8, 15));
    }
}
