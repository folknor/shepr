use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use shepr_config::theme::Palette;

pub(in crate::shell) fn list_scroll_metrics(
    row_heights: &[u16],
    gaps_after: &[u16],
    body_height: u16,
    requested_start: usize,
) -> shepr_termio::ScrollMetrics {
    if row_heights.is_empty() || body_height == 0 {
        return shepr_termio::ScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 0,
            viewport_rows: 0,
            history_origin: shepr_vt::AbsRow(0),
        };
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

    shepr_termio::ScrollMetrics {
        offset_from_bottom: max_start.saturating_sub(start),
        max_offset_from_bottom: max_start,
        viewport_rows,
        history_origin: shepr_vt::AbsRow(0),
    }
}

pub(in crate::shell) fn list_scroll_start_to_reveal(
    row_heights: &[u16],
    gaps_after: &[u16],
    body_height: u16,
    requested_start: usize,
    target: usize,
) -> usize {
    let mut metrics = list_scroll_metrics(row_heights, gaps_after, body_height, requested_start);
    let mut start = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    if target < start {
        return target;
    }
    while target >= start.saturating_add(metrics.viewport_rows)
        && start < metrics.max_offset_from_bottom
    {
        start = start.saturating_add(1);
        metrics = list_scroll_metrics(row_heights, gaps_after, body_height, start);
    }
    start
}

pub(in crate::shell) fn render_list_scrollbar(
    buffer: &mut Buffer,
    track: Rect,
    metrics: shepr_termio::ScrollMetrics,
    palette: &Palette,
) {
    shepr_termio::scroll::render_scrollbar_buffer(
        buffer,
        metrics,
        track,
        "▕",
        Style::default().fg(palette.surface_dim),
        "▕",
        Style::default().fg(palette.overlay0),
    );
}

#[cfg(test)]
mod tests {
    use crate::shell::navigation::scroll::list_scroll_metrics;

    #[test]
    fn list_metrics_preserve_variable_rows_and_caller_owned_gap_policy() {
        let top = list_scroll_metrics(&[1, 3, 2], &[1, 1, 0], 5, 0);
        assert_eq!(top.max_offset_from_bottom, 2);
        assert_eq!(top.offset_from_bottom, 2);
        assert_eq!(top.viewport_rows, 2);

        let bottom = list_scroll_metrics(&[1, 3, 2], &[1, 1, 0], 5, usize::MAX);
        assert_eq!(bottom.offset_from_bottom, 0);
        assert_eq!(bottom.viewport_rows, 1);

        let parent_child = list_scroll_metrics(&[2, 2, 2], &[0, 1, 0], 5, 0);
        assert_eq!(parent_child.max_offset_from_bottom, 1);
        assert_eq!(parent_child.viewport_rows, 2);
    }
}
