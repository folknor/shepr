use ratatui::{layout::Rect, widgets::Borders};

use crate::layout::{PaneId, PaneInfo};

/// Layout position with the chrome and content geometry added for a view.
#[derive(Clone)]
pub(crate) struct PaneChromeInfo {
    pub id: PaneId,
    pub rect: Rect,
    pub inner_rect: Rect,
    pub scrollbar_rect: Option<Rect>,
    pub borders: Borders,
    pub is_focused: bool,
}

impl From<PaneInfo> for PaneChromeInfo {
    fn from(pane: PaneInfo) -> Self {
        Self {
            id: pane.id,
            rect: pane.rect,
            inner_rect: pane.rect,
            scrollbar_rect: None,
            borders: Borders::NONE,
            is_focused: pane.is_focused,
        }
    }
}

impl From<PaneChromeInfo> for PaneInfo {
    fn from(pane: PaneChromeInfo) -> Self {
        Self {
            id: pane.id,
            rect: pane.rect,
            is_focused: pane.is_focused,
        }
    }
}
