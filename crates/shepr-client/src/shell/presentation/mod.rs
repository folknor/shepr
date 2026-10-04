pub(in crate::shell) mod selection_render;
pub(in crate::shell) mod status;
pub(in crate::shell) mod surface_patch;
pub(in crate::shell) mod surfaces;
pub(in crate::shell) mod text;
pub(in crate::shell) mod topology;

use ratatui::layout::Rect;

use crate::shell::presentation::surfaces::PaneSurfaces;
use crate::shell::view::{PaneHit, ShellView};

/// Effects committed by the last successful composition. Patches must preserve both
/// the cells painted over pane output and compose's replacement of the pane cursor.
#[derive(Default)]
pub(in crate::shell) struct LastComposition {
    pub(in crate::shell) pane_cells_occluded: bool,
    pub(in crate::shell) pane_cursor_overridden: bool,
}

/// What the client shows of the active endpoint's panes: the received surfaces, the view of
/// the last frame drawn from them, and the effects that frame had on pane output. Only
/// `ClientShellState::commit_frame`, run once the host took a composed frame, replaces the
/// view, so input aims at the frame on screen.
#[derive(Default)]
pub(in crate::shell) struct Presentation {
    pub(in crate::shell) surfaces: PaneSurfaces,
    view: Option<ShellView>,
    composition: LastComposition,
    composed_at: Option<std::time::Instant>,
}

impl Presentation {
    /// Whether a frame can be drawn now. Only the exact snapshot pair is drawn: with nothing
    /// presented the placeholder layer is drawn, while a presented surface held unpaired (the
    /// snapshot passed it, a baseline waits for its snapshot, or the connection was lost)
    /// leaves the last frame on screen until the matching pair exists.
    pub(in crate::shell) fn can_draw(&self, has_snapshot: bool) -> bool {
        !(has_snapshot && self.surfaces.presented().is_some() && !self.surfaces.is_paired())
    }

    /// The last drawn frame's view, or `None` before the first frame or after a reset.
    pub(in crate::shell) fn view(&self) -> Option<&ShellView> {
        self.view.as_ref()
    }

    /// The last drawn view, or an empty one that offers no targets when nothing was drawn.
    pub(in crate::shell) fn shown(&self) -> &ShellView {
        self.view.as_ref().unwrap_or(ShellView::empty_ref())
    }

    /// The panes input can aim at in the frame on screen.
    pub(in crate::shell) fn pane_hits(&self) -> &[PaneHit] {
        self.shown().pane_hits()
    }

    fn composition(&self) -> &LastComposition {
        &self.composition
    }

    pub(in crate::shell) fn composed_at(&self) -> Option<std::time::Instant> {
        self.composed_at
    }

    /// Drops the view and the composition effects with it: nothing is on screen to aim at.
    /// The surfaces are reset by their own rule.
    pub(in crate::shell) fn reset_view(&mut self) {
        self.view = None;
        self.composition = LastComposition::default();
        self.composed_at = None;
    }

    /// Keeps the frame just drawn: its view, its effects and when it was composed.
    pub(in crate::shell) fn commit(
        &mut self,
        view: ShellView,
        composition: LastComposition,
        now: std::time::Instant,
    ) {
        self.view = Some(view);
        self.composition = composition;
        self.composed_at = Some(now);
    }

    /// Replaces the hit of the pane `updated` describes after a patch changed it in place.
    /// Returns whether the frame on screen has that pane. It runs when the patch is
    /// received, before its rows are written; the caller in `surface_patch` says why
    /// that is sound.
    fn patch_pane_hit(&mut self, updated: &shepr_protocol::PaneSurfacePane, area: Rect) -> bool {
        let Some(hit) = self
            .view
            .as_mut()
            .and_then(|view| view.pane_hit_mut(&updated.pane_id))
        else {
            return false;
        };
        if let Some(updated_hit) = PaneHit::from_wire(updated, (area.x, area.y), area) {
            *hit = updated_hit;
        }
        true
    }
}

#[cfg(test)]
impl Presentation {
    pub(in crate::shell) fn view_mut(&mut self) -> Option<&mut ShellView> {
        self.view.as_mut()
    }

    fn composition_mut(&mut self) -> &mut LastComposition {
        &mut self.composition
    }

    pub(in crate::shell) fn set_composed_at(&mut self, at: std::time::Instant) {
        self.composed_at = Some(at);
    }

    /// Records an empty frame of `size` as the last drawn one.
    fn set_composed_size(&mut self, size: (u16, u16)) {
        self.view = Some(ShellView::empty_at(size));
    }

    pub(in crate::shell) fn set_view(&mut self, view: ShellView) {
        self.view = Some(view);
    }
}

#[cfg(test)]
mod tests;
