use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use shepr_config::theme::Palette;
use shepr_term::scroll::{
    ListScroll, ScrollTrack, ScrollbarMetrics, ScrollbarPart, scrollbar_rows,
};

pub(in crate::shell) fn list_scroll_metrics(
    row_heights: &[u16],
    gaps_after: &[u16],
    body_height: u16,
    requested_start: usize,
) -> ListScroll {
    if row_heights.is_empty() || body_height == 0 {
        return ListScroll::new(0, 0, 0);
    }

    let mut used = 0u16;
    let mut max_start = row_heights.len();
    for index in (0..row_heights.len()).rev() {
        let height = row_heights[index].max(1).min(body_height);
        let gap = gaps_after.get(index).copied().unwrap_or(0);
        if used.saturating_add(height).saturating_add(gap) > body_height {
            break;
        }
        used = used.saturating_add(height).saturating_add(gap);
        max_start = index;
    }
    max_start = max_start.min(row_heights.len().saturating_sub(1));
    let start = requested_start.min(max_start);

    let mut viewport_rows = 0usize;
    let mut used = 0u16;
    for (index, row_height) in row_heights.iter().enumerate().skip(start) {
        let height = (*row_height).max(1).min(body_height);
        if used.saturating_add(height) > body_height {
            break;
        }
        used = used.saturating_add(height);
        viewport_rows += 1;
        let gap = gaps_after.get(index).copied().unwrap_or(0);
        if used.saturating_add(gap) > body_height {
            break;
        }
        used = used.saturating_add(gap);
    }

    ListScroll::new(start, max_start, viewport_rows)
}

pub(in crate::shell) fn list_scroll_start_to_reveal(
    row_heights: &[u16],
    gaps_after: &[u16],
    body_height: u16,
    requested_start: usize,
    target: usize,
) -> usize {
    let mut metrics = list_scroll_metrics(row_heights, gaps_after, body_height, requested_start);
    let mut start = metrics.start();
    if target < start {
        return target;
    }
    while target >= start.saturating_add(metrics.viewport_rows()) && start < metrics.max_start() {
        start = start.saturating_add(1);
        metrics = list_scroll_metrics(row_heights, gaps_after, body_height, start);
    }
    start
}

pub(in crate::shell) fn render_list_scrollbar(
    buffer: &mut Buffer,
    track: Rect,
    metrics: ListScroll,
    palette: &Palette,
) {
    render_scrollbar_buffer(
        buffer,
        metrics,
        track,
        "▕",
        Style::default().fg(palette.surface_dim),
        "▕",
        Style::default().fg(palette.overlay0),
    );
}

/// The rows a scrollbar drawn in `rect` occupies.
pub(in crate::shell) fn scroll_track(rect: Rect) -> ScrollTrack {
    ScrollTrack::new(rect.y, rect.height)
}

pub(in crate::shell) fn render_scrollbar_buffer(
    buffer: &mut Buffer,
    metrics: impl ScrollbarMetrics,
    track: Rect,
    track_symbol: &str,
    track_style: Style,
    thumb_symbol: &str,
    thumb_style: Style,
) {
    for (y, part) in scrollbar_rows(metrics, scroll_track(track)) {
        let (symbol, style) = match part {
            ScrollbarPart::Track => (track_symbol, track_style),
            ScrollbarPart::Thumb => (thumb_symbol, thumb_style),
        };
        if let Some(cell) = buffer.cell_mut((track.x, y)) {
            cell.set_symbol(symbol);
            cell.set_style(track_style);
            cell.set_style(style);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::shell::navigation::scroll::list_scroll_metrics;

    #[test]
    fn list_metrics_preserve_variable_rows_and_caller_owned_gap_policy() {
        let top = list_scroll_metrics(&[1, 3, 2], &[1, 1, 0], 5, 0);
        assert_eq!(top.max_start(), 2);
        assert_eq!(top.start(), 0);
        assert_eq!(top.viewport_rows(), 2);

        let bottom = list_scroll_metrics(&[1, 3, 2], &[1, 1, 0], 5, usize::MAX);
        assert_eq!(bottom.start(), 2);
        assert_eq!(bottom.viewport_rows(), 1);

        let parent_child = list_scroll_metrics(&[2, 2, 2], &[0, 1, 0], 5, 0);
        assert_eq!(parent_child.max_start(), 1);
        assert_eq!(parent_child.viewport_rows(), 2);
    }
}
