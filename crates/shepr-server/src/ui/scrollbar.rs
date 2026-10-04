use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use shepr_term::scroll::{ScrollTrack, ScrollbarMetrics, ScrollbarPart, scrollbar_rows};

use super::PaneSurface;
use super::chrome::overlay_buffer;
use crate::app::AppState;
use shepr_protocol::FrameData;

fn render_scrollbar_buffer(
    buffer: &mut Buffer,
    metrics: impl ScrollbarMetrics,
    track: Rect,
    track_symbol: &str,
    track_style: Style,
    thumb_symbol: &str,
    thumb_style: Style,
) {
    for (y, part) in scrollbar_rows(metrics, ScrollTrack::new(track.y, track.height)) {
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

fn render_pane_scrollbar_buffer(
    buffer: &mut Buffer,
    metrics: shepr_mux::pane::ScrollMetrics,
    track: Rect,
    palette: &shepr_config::theme::Palette,
    focused: bool,
) {
    let (track_color, thumb_color, thumb_symbol) = if focused {
        (palette.overlay0, palette.overlay1, "▐")
    } else {
        (palette.surface_dim, palette.overlay0, "▕")
    };
    render_scrollbar_buffer(
        buffer,
        metrics,
        track,
        "▕",
        ratatui::style::Style::default().fg(track_color),
        thumb_symbol,
        ratatui::style::Style::default().fg(thumb_color),
    );
}

/// A buffer over `track` holding the scrollbar for `metrics`, or blank cells
/// when there is none.
pub(super) fn scrollbar_track_buffer(
    track: Rect,
    metrics: Option<shepr_mux::pane::ScrollMetrics>,
    palette: &shepr_config::theme::Palette,
    focused: bool,
) -> Buffer {
    let mut buffer = Buffer::empty(track);
    if let Some(metrics) = metrics {
        render_pane_scrollbar_buffer(&mut buffer, metrics, track, palette, focused);
    }
    buffer
}

pub(super) fn render_pane_scrollbar(
    app: &AppState,
    frame: &mut FrameData,
    info: &PaneSurface,
    rt: &shepr_mux::pane::PaneRuntime,
) {
    let Some(metrics) = rt.read().scroll_metrics() else {
        return;
    };
    let Some(track) = info.scrollbar_rect else {
        return;
    };
    let scratch = scrollbar_track_buffer(
        track,
        Some(metrics),
        &app.settings().palette,
        info.is_focused,
    );
    // The scrollbar draws its whole track.
    overlay_buffer(frame, &scratch, track);
}
