use super::*;
use ratatui::layout::Position;

pub(super) fn contains(rect: Rect, point: (u16, u16)) -> bool {
    rect.contains(Position {
        x: point.0,
        y: point.1,
    })
}
