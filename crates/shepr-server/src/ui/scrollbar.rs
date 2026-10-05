use shepr_protocol::{CellData, ChromeRole, CompactString, FrameData, WireColor};
use shepr_term::scroll::{ScrollTrack, ScrollbarPart, scrollbar_rows};

use super::PaneSurface;
use super::chrome::put_run;

/// Hands `visit` each cell of a one-column scrollbar track `height` rows
/// tall, with its row from the top: the track for `metrics`, or blanks when
/// there is none. Each cell names its chrome role and the client colours it.
/// Symbols are held inline, so nothing here allocates: the retained patch
/// runs this per scrolled pane per recipient on every pass.
pub(super) fn visit_scrollbar_track(
    height: u16,
    metrics: Option<shepr_mux::pane::ScrollMetrics>,
    focused: bool,
    mut visit: impl FnMut(u16, CellData),
) {
    let Some(metrics) = metrics else {
        for y in 0..height {
            visit(y, CellData::blank());
        }
        return;
    };
    let (track, thumb, thumb_symbol) = if focused {
        (
            ChromeRole::ScrollTrackFocused,
            ChromeRole::ScrollThumbFocused,
            "▐",
        )
    } else {
        (ChromeRole::ScrollTrack, ChromeRole::ScrollThumb, "▕")
    };
    for (y, part) in scrollbar_rows(metrics, ScrollTrack::new(0, height)) {
        let (symbol, role) = match part {
            ScrollbarPart::Track => ("▕", track),
            ScrollbarPart::Thumb => (thumb_symbol, thumb),
        };
        visit(
            y,
            CellData {
                symbol: CompactString::const_new(symbol),
                fg: WireColor::Chrome(role),
                ..CellData::blank()
            },
        );
    }
}

pub(super) fn render_pane_scrollbar(
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
    // The scrollbar draws its whole track.
    visit_scrollbar_track(track.height, Some(metrics), info.is_focused, |y, cell| {
        put_run(
            frame,
            track.x,
            track.y.saturating_add(y),
            std::slice::from_ref(&cell),
        );
    });
}
