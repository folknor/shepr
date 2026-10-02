use ratatui::layout::Position;
use ratatui::layout::Rect;

pub(in crate::shell) fn contains(rect: Rect, point: (u16, u16)) -> bool {
    rect.contains(Position {
        x: point.0,
        y: point.1,
    })
}
