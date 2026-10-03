pub use shepr_vt::ScrollMetrics;

/// Top-based scrolling for chrome lists; it carries no terminal history identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListScroll {
    start: usize,
    max_start: usize,
    viewport_rows: usize,
}

impl ListScroll {
    pub fn new(start: usize, max_start: usize, viewport_rows: usize) -> Self {
        Self {
            start: start.min(max_start),
            max_start,
            viewport_rows,
        }
    }

    pub fn start(self) -> usize {
        self.start
    }

    pub fn max_start(self) -> usize {
        self.max_start
    }

    pub fn viewport_rows(self) -> usize {
        self.viewport_rows
    }
}

/// Geometry shared by list and terminal scrollbar drawing and hit testing.
pub trait ScrollbarMetrics: Copy {
    fn start(self) -> usize;
    fn max_start(self) -> usize;
    fn viewport_rows(self) -> usize;
}

impl ScrollbarMetrics for ListScroll {
    fn start(self) -> usize {
        self.start
    }
    fn max_start(self) -> usize {
        self.max_start
    }
    fn viewport_rows(self) -> usize {
        self.viewport_rows
    }
}

impl ScrollbarMetrics for ScrollMetrics {
    fn start(self) -> usize {
        self.viewport_start()
    }
    fn max_start(self) -> usize {
        self.max_offset_from_bottom
    }
    fn viewport_rows(self) -> usize {
        self.viewport_rows
    }
}

use ratatui::{buffer::Buffer, layout::Rect, style::Style};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarThumb {
    pub top: u16,
    pub len: u16,
}

pub fn scrollbar_thumb(metrics: impl ScrollbarMetrics, track: Rect) -> Option<ScrollbarThumb> {
    if metrics.max_start() == 0 || track.height == 0 {
        return None;
    }

    let track_height = track.height as usize;
    let total_rows = metrics.max_start().saturating_add(metrics.viewport_rows());
    if total_rows == 0 {
        return None;
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the ratio is clamped to [1.0, track_height] just below, so the rounded result always fits in usize without truncation or sign loss"
    )]
    let thumb_len = (metrics.viewport_rows() as f32 * track_height as f32 / total_rows as f32)
        .round()
        .max(1.0)
        .min(track_height as f32) as usize;
    let max_thumb_top = track_height.saturating_sub(thumb_len);
    let scrolled_from_top = metrics.start();
    let thumb_top = if max_thumb_top == 0 || metrics.max_start() == 0 {
        0
    } else {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to [0.0, max_thumb_top] just above, so this always fits in usize"
        )]
        let clamped = (scrolled_from_top as f32 * max_thumb_top as f32 / metrics.max_start() as f32)
            .round()
            .clamp(0.0, max_thumb_top as f32) as usize;
        clamped
    };

    #[expect(
        clippy::cast_possible_truncation,
        reason = "track.y is a small terminal row coordinate and thumb_len/thumb_top are bounded by track_height above, so these narrow to u16 without loss"
    )]
    let thumb = ScrollbarThumb {
        top: track.y + thumb_top as u16,
        len: thumb_len as u16,
    };
    Some(thumb)
}

pub fn scrollbar_thumb_grab_offset(
    metrics: impl ScrollbarMetrics,
    track: Rect,
    row: u16,
) -> Option<u16> {
    let thumb = scrollbar_thumb(metrics, track)?;
    (row >= thumb.top && row < thumb.top + thumb.len).then(|| row - thumb.top)
}

fn scrollbar_start_from_thumb_top(
    metrics: impl ScrollbarMetrics,
    track: Rect,
    thumb_top: usize,
) -> usize {
    if metrics.max_start() == 0 {
        return 0;
    }

    let thumb_len = scrollbar_thumb(metrics, track).map_or(1, |thumb| thumb.len as usize);
    let max_thumb_top = track.height as usize - thumb_len.min(track.height as usize);
    if max_thumb_top == 0 {
        return 0;
    }

    let desired_top = thumb_top.min(max_thumb_top);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "desired_top / max_thumb_top <= 1, so the scaled result stays within metrics.max_start() and fits in usize"
    )]
    let scrolled_from_top =
        (desired_top as f32 * metrics.max_start() as f32 / max_thumb_top as f32).round() as usize;
    scrolled_from_top.min(metrics.max_start())
}

pub fn scrollbar_start_from_row(metrics: impl ScrollbarMetrics, track: Rect, row: u16) -> usize {
    let thumb = match scrollbar_thumb(metrics, track) {
        Some(thumb) => thumb,
        None => return 0,
    };
    if track.height == 0 {
        return 0;
    }
    let clamped_row = row.clamp(track.y, track.y + track.height.saturating_sub(1));
    let row_offset = clamped_row.saturating_sub(track.y) as usize;
    let thumb_center = (thumb.len as usize) / 2;
    let desired_top = row_offset.saturating_sub(thumb_center);
    scrollbar_start_from_thumb_top(metrics, track, desired_top)
}

pub fn scrollbar_start_from_drag_row(
    metrics: impl ScrollbarMetrics,
    track: Rect,
    row: u16,
    grab_row_offset: u16,
) -> usize {
    if track.height == 0 {
        return 0;
    }
    let clamped_row = row.clamp(track.y, track.y + track.height.saturating_sub(1));
    let row_offset = clamped_row.saturating_sub(track.y) as usize;
    let desired_top = row_offset.saturating_sub(grab_row_offset as usize);
    scrollbar_start_from_thumb_top(metrics, track, desired_top)
}

pub fn scrollbar_offset_from_row(metrics: ScrollMetrics, track: Rect, row: u16) -> usize {
    if scrollbar_thumb(metrics, track).is_none() {
        return 0;
    }
    metrics
        .max_offset_from_bottom
        .saturating_sub(scrollbar_start_from_row(metrics, track, row))
}

pub fn scrollbar_offset_from_drag_row(
    metrics: ScrollMetrics,
    track: Rect,
    row: u16,
    grab_row_offset: u16,
) -> usize {
    if track.height == 0 {
        return 0;
    }
    metrics
        .max_offset_from_bottom
        .saturating_sub(scrollbar_start_from_drag_row(
            metrics,
            track,
            row,
            grab_row_offset,
        ))
}

pub fn render_scrollbar_buffer(
    buffer: &mut Buffer,
    metrics: impl ScrollbarMetrics,
    track: Rect,
    track_symbol: &str,
    track_style: Style,
    thumb_symbol: &str,
    thumb_style: Style,
) {
    let Some(thumb) = scrollbar_thumb(metrics, track) else {
        return;
    };

    for y in track.y..track.bottom() {
        if let Some(cell) = buffer.cell_mut((track.x, y)) {
            cell.set_symbol(track_symbol);
            cell.set_style(track_style);
        }
    }
    for y in thumb.top..thumb.top.saturating_add(thumb.len) {
        if let Some(cell) = buffer.cell_mut((track.x, y)) {
            cell.set_symbol(thumb_symbol);
            cell.set_style(thumb_style);
        }
    }
}
