#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollMetrics {
    pub offset_from_bottom: usize,
    pub max_offset_from_bottom: usize,
    pub viewport_rows: usize,
    pub history_origin: shepr_vt::AbsRow,
}

impl ScrollMetrics {
    /// The stable row ID at the top of the current viewport.
    pub fn viewport_top_row(self) -> shepr_vt::AbsRow {
        let screen_row = self
            .max_offset_from_bottom
            .saturating_sub(self.offset_from_bottom);
        self.history_origin
            .saturating_add(u64::try_from(screen_row).unwrap_or(u64::MAX))
    }

    /// Convert a viewport-relative row to its stable row ID.
    pub fn absolute_row_at_viewport(self, row: shepr_vt::ViewportRow) -> shepr_vt::AbsRow {
        shepr_vt::AbsRow::from_viewport_top(self.viewport_top_row(), row)
    }
}

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarThumb {
    pub top: u16,
    pub len: u16,
}

pub fn scrollbar_thumb(metrics: ScrollMetrics, track: Rect) -> Option<ScrollbarThumb> {
    if metrics.max_offset_from_bottom == 0 || track.height == 0 {
        return None;
    }

    let track_height = track.height as usize;
    let total_rows = metrics.max_offset_from_bottom + metrics.viewport_rows;
    if total_rows == 0 {
        return None;
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the ratio is clamped to [1.0, track_height] just below, so the rounded result always fits in usize without truncation or sign loss"
    )]
    let thumb_len = ((metrics.viewport_rows * track_height) as f32 / total_rows as f32)
        .round()
        .max(1.0)
        .min(track_height as f32) as usize;
    let max_thumb_top = track_height.saturating_sub(thumb_len);
    let scrolled_from_top = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let thumb_top = if max_thumb_top == 0 || metrics.max_offset_from_bottom == 0 {
        0
    } else {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to [0.0, max_thumb_top] just above, so this always fits in usize"
        )]
        let clamped = ((scrolled_from_top * max_thumb_top) as f32
            / metrics.max_offset_from_bottom as f32)
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

pub fn scrollbar_thumb_grab_offset(metrics: ScrollMetrics, track: Rect, row: u16) -> Option<u16> {
    let thumb = scrollbar_thumb(metrics, track)?;
    (row >= thumb.top && row < thumb.top + thumb.len).then(|| row - thumb.top)
}

fn scrollbar_offset_from_thumb_top(metrics: ScrollMetrics, track: Rect, thumb_top: usize) -> usize {
    if metrics.max_offset_from_bottom == 0 {
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
        reason = "desired_top / max_thumb_top <= 1, so the scaled result stays within metrics.max_offset_from_bottom and fits in usize"
    )]
    let scrolled_from_top = ((desired_top * metrics.max_offset_from_bottom) as f32
        / max_thumb_top as f32)
        .round() as usize;
    metrics
        .max_offset_from_bottom
        .saturating_sub(scrolled_from_top)
}

pub fn scrollbar_offset_from_row(metrics: ScrollMetrics, track: Rect, row: u16) -> usize {
    let thumb = match scrollbar_thumb(metrics, track) {
        Some(thumb) => thumb,
        None => return 0,
    };
    let clamped_row = row.clamp(track.y, track.y + track.height.saturating_sub(1));
    let row_offset = clamped_row.saturating_sub(track.y) as usize;
    let thumb_center = (thumb.len as usize) / 2;
    let desired_top = row_offset.saturating_sub(thumb_center);
    scrollbar_offset_from_thumb_top(metrics, track, desired_top)
}

pub fn scrollbar_offset_from_drag_row(
    metrics: ScrollMetrics,
    track: Rect,
    row: u16,
    grab_row_offset: u16,
) -> usize {
    let clamped_row = row.clamp(track.y, track.y + track.height.saturating_sub(1));
    let row_offset = clamped_row.saturating_sub(track.y) as usize;
    let desired_top = row_offset.saturating_sub(grab_row_offset as usize);
    scrollbar_offset_from_thumb_top(metrics, track, desired_top)
}

pub fn render_scrollbar_buffer(
    buffer: &mut Buffer,
    metrics: ScrollMetrics,
    track: Rect,
    track_color: Color,
    thumb_color: Color,
    thumb_symbol: &str,
) {
    if metrics.max_offset_from_bottom == 0 {
        return;
    }

    let Some(thumb) = scrollbar_thumb(metrics, track) else {
        return;
    };

    for y in track.y..track.y + track.height {
        let cell = &mut buffer[(track.x, y)];
        cell.set_symbol("▕");
        cell.set_style(Style::default().fg(track_color));
    }
    for y in thumb.top..thumb.top + thumb.len {
        let cell = &mut buffer[(track.x, y)];
        cell.set_symbol(thumb_symbol);
        cell.set_style(Style::default().fg(thumb_color));
    }
}
