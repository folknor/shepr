use ratatui::{buffer::Buffer, layout::Rect};

use super::chrome::overlay_buffer;
use crate::app::AppState;
use shepr_mux::workspace::PaneChromeInfo as PaneInfo;
use shepr_protocol::FrameData;

pub(crate) fn pane_scrollbar_rect(info: &PaneInfo) -> Option<Rect> {
    info.scrollbar_rect
}

pub(crate) fn should_show_scrollbar(metrics: shepr_mux::pane::ScrollMetrics) -> bool {
    metrics.max_offset_from_bottom > 0
}

use shepr_termio::scroll::render_scrollbar_buffer;

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
    let Some(metrics) = rt.scroll_metrics() else {
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
