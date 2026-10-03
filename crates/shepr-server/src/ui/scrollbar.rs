use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use shepr_term::scroll::{ScrollTrack, ScrollbarMetrics, ScrollbarPart, scrollbar_rows};

use super::chrome::overlay_buffer;
use crate::app::AppState;
use shepr_mux::workspace::PaneChromeInfo as PaneInfo;
use shepr_protocol::FrameData;

pub(crate) fn pane_scrollbar_rect(info: &PaneInfo) -> Option<Rect> {
    info.scrollbar_rect
}

pub(crate) fn should_show_scrollbar(metrics: shepr_mux::pane::ScrollMetrics) -> bool {
    PaneInfo::scrollbar_visible(metrics.max_offset_from_bottom)
}

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

pub(crate) fn render_pane_scrollbar_buffer(
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

pub(super) fn render_pane_scrollbar(
    app: &AppState,
    frame: &mut FrameData,
    info: &PaneInfo,
    rt: &shepr_mux::pane::PaneRuntime,
) {
    let Some(metrics) = rt.read().scroll_metrics() else {
        return;
    };
    let Some(track) = pane_scrollbar_rect(info) else {
        return;
    };
    let mut scratch = Buffer::empty(track);
    render_pane_scrollbar_buffer(
        &mut scratch,
        metrics,
        track,
        &app.settings.palette,
        info.is_focused,
    );
    // The scrollbar draws its whole track.
    overlay_buffer(frame, &scratch, track);
}
