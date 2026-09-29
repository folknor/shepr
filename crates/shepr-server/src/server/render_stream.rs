//! Virtual rendering helpers for headless client frame streaming.

use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

use crate::app::state::AppState;
use shepr_mux::pane::PaneRuntimeRegistry;
use shepr_protocol::{
    CursorState, PaneSurfaceFrame, PaneSurfacePatch, ServerMessage, SurfaceRevision,
};

fn warn_surface_encoding_failure(
    encoding: &'static str,
    error: &impl std::fmt::Display,
    last: &PaneSurfaceFrame,
    surface: &PaneSurfaceFrame,
) {
    tracing::warn!(
        %error,
        encoding = %encoding,
        boot_id = %surface.boot_id,
        base_projection_revision = ?last.projection_revision,
        base_surface_revision = ?last.surface_revision,
        projection_revision = ?surface.projection_revision,
        surface_revision = ?surface.surface_revision,
        width = surface.frame.width,
        height = surface.frame.height,
        "failed to encode compact surface update"
    );
}

/// Per-client render baseline: the last surface sent and its revision. The
/// client-owned shell compares full frame data and skips identical frames.
pub(crate) struct ClientRenderState {
    last_surface: Option<Box<PaneSurfaceFrame>>,
    surface_revision: SurfaceRevision,
    recompute_pending: bool,
}

impl ClientRenderState {
    pub(crate) fn new() -> Self {
        Self {
            last_surface: None,
            surface_revision: SurfaceRevision::ZERO,
            recompute_pending: false,
        }
    }

    pub(crate) fn request_recompute(&mut self) {
        self.recompute_pending = true;
    }

    pub(crate) fn requires_recompute(&self) -> bool {
        self.recompute_pending
    }

    pub(crate) fn request_repaint(&mut self) {
        self.last_surface = None;
    }

    pub(crate) fn last_pane_surface(&self) -> Option<&PaneSurfaceFrame> {
        self.last_surface.as_deref()
    }

    pub(crate) fn prepare_pane_surface(
        &mut self,
        mut surface: PaneSurfaceFrame,
    ) -> Option<PreparedRender> {
        let Self {
            last_surface,
            surface_revision,
            recompute_pending,
        } = self;
        if !*recompute_pending
            && last_surface.as_deref().is_some_and(|last| {
                last.projection_revision == surface.projection_revision
                    && last.frame == surface.frame
                    && last.panes == surface.panes
                    && last.splits == surface.splits
            })
        {
            return None;
        }
        // The client accepts a surface only at its exact successor revision,
        // so an exhausted counter (one step per sent frame, unreachable in
        // practice) holds the last frame rather than repeating a revision.
        let Some(next_revision) = surface_revision.checked_next() else {
            tracing::error!("surface revisions exhausted; holding the last frame");
            return None;
        };
        surface.surface_revision = next_revision;
        let committed_surface = surface.clone();
        let mut message = ServerMessage::PaneSurface(surface);
        let delta = last_surface.as_deref().and_then(|last| {
            match shepr_protocol::surface_delta::message(last, &mut message) {
                Ok(delta) => delta,
                Err(error) => {
                    if let ServerMessage::PaneSurface(surface) = &message {
                        warn_surface_encoding_failure("delta", &error, last, surface);
                    }
                    None
                }
            }
        });
        let reused = if let ServerMessage::PaneSurface(surface) = &mut message {
            delta
                .is_none()
                .then_some(last_surface.as_deref())
                .flatten()
                .filter(|last| last.frame == surface.frame)
                .and_then(
                    |last| match shepr_protocol::surface_reuse::message(last, surface) {
                        Ok(reused) => reused,
                        Err(error) => {
                            warn_surface_encoding_failure("reuse", &error, last, surface);
                            None
                        }
                    },
                )
        } else {
            None
        };
        Some(PreparedRender::Semantic {
            message: delta.or(reused).unwrap_or(message),
            committed_surface: Box::new(committed_surface),
        })
    }

    pub(crate) fn prepare_pane_surface_patch(
        &self,
        mut patch: PaneSurfacePatch,
    ) -> Option<PreparedRender> {
        let Self {
            last_surface,
            surface_revision,
            ..
        } = self;
        if self.requires_recompute() {
            return None;
        }
        let last = last_surface.as_deref()?;
        let next_revision = surface_revision.checked_next()?;
        let baseline = shepr_protocol::surface_reuse::Baseline::new(
            &last.boot_id,
            last.projection_revision,
            last.surface_revision,
        );
        if !baseline.accepts(
            &patch.boot_id,
            patch.base_surface_revision,
            next_revision,
            last.projection_revision,
            patch.projection_revision,
        ) || patch.projection_revision != last.projection_revision
        {
            return None;
        }
        patch.surface_revision = next_revision;
        shepr_protocol::validate_patch_rows(last.frame.width, last.frame.height, &patch.rows)
            .ok()?;
        let mut meta = shepr_protocol::SurfaceMeta::from(last);
        meta.frame.cursor.clone_from(&patch.cursor);
        for updated in &patch.panes {
            let pane = meta
                .panes
                .iter_mut()
                .find(|pane| pane.pane_id == updated.pane_id)?;
            pane.clone_from(updated);
        }
        let message = ServerMessage::SurfaceUpdate(shepr_protocol::SurfaceUpdate {
            boot_id: patch.boot_id.clone(),
            base_projection_revision: last.projection_revision,
            base_surface_revision: patch.base_surface_revision,
            surface_revision: patch.surface_revision,
            projection_revision: patch.projection_revision,
            meta: Some(meta),
            spans: patch.rows.clone(),
        });
        let size = shepr_protocol::codec::encoded_len(&message).ok()?;
        if !shepr_protocol::frame_payload_fits(size) {
            return None;
        }
        Some(PreparedRender::SemanticPatch { message, patch })
    }

    pub(crate) fn commit_sent_frame(&mut self, prepared: PreparedRender) {
        match prepared {
            PreparedRender::Semantic {
                committed_surface, ..
            } => {
                self.surface_revision = committed_surface.surface_revision;
                self.last_surface = Some(committed_surface);
                self.recompute_pending = false;
            }
            PreparedRender::SemanticPatch { patch, .. } => {
                // Planning checked the baseline and the server does not yield
                // between planning and commit, so neither branch below should
                // run. If one does, the client holds a surface this side can no
                // longer reproduce: drop the baseline so the next render sends a
                // full surface rather than diffing against a wrong grid.
                let applied = match self.last_surface.as_deref_mut() {
                    Some(surface) => apply_pane_surface_patch(surface, &patch),
                    None => Err("no committed surface"),
                };
                if let Err(reason) = applied {
                    tracing::warn!(reason, "sent surface patch did not apply to its baseline");
                    self.last_surface = None;
                }
                self.surface_revision = patch.surface_revision;
            }
        }
    }
}

// Planning validates all rows and pane IDs before any send, so this is expected to succeed.
// It still checks every row and pane first and changes nothing on a mismatch, so a planning
// bug surfaces as an error (and a full resend) instead of a panic or a half-applied patch.
pub(super) fn apply_pane_surface_patch(
    surface: &mut PaneSurfaceFrame,
    patch: &PaneSurfacePatch,
) -> Result<(), &'static str> {
    let baseline = shepr_protocol::surface_reuse::Baseline::new(
        &surface.boot_id,
        surface.projection_revision,
        surface.surface_revision,
    );
    if !baseline.accepts(
        &patch.boot_id,
        patch.base_surface_revision,
        patch.surface_revision,
        surface.projection_revision,
        patch.projection_revision,
    ) || patch.projection_revision != surface.projection_revision
    {
        return Err("patch revision does not match the surface baseline");
    }
    shepr_protocol::validate_patch_rows(surface.frame.width, surface.frame.height, &patch.rows)?;
    let width = usize::from(surface.frame.width);
    if shepr_protocol::surface_grid_size(surface.frame.width, surface.frame.height)
        != Some(surface.frame.cells.len())
    {
        return Err("surface cell grid does not match its size");
    }
    if !patch.panes.iter().all(|updated| {
        surface
            .panes
            .iter()
            .any(|pane| pane.pane_id == updated.pane_id)
    }) {
        return Err("patch names a pane missing from the surface");
    }
    for row in &patch.rows {
        let start = usize::from(row.y) * width + usize::from(row.x);
        if let Some(cells) = surface
            .frame
            .cells
            .get_mut(start..start.saturating_add(row.cells.len()))
        {
            cells.clone_from_slice(&row.cells);
        }
    }
    for updated in &patch.panes {
        if let Some(pane) = surface
            .panes
            .iter_mut()
            .find(|pane| pane.pane_id == updated.pane_id)
        {
            pane.clone_from(updated);
        }
    }
    surface.frame.cursor.clone_from(&patch.cursor);
    surface.surface_revision = patch.surface_revision;
    Ok(())
}

/// A prepared client render message plus any baseline state needed after send.
pub(crate) enum PreparedRender {
    Semantic {
        message: ServerMessage,
        committed_surface: Box<PaneSurfaceFrame>,
    },
    SemanticPatch {
        message: ServerMessage,
        patch: PaneSurfacePatch,
    },
}

impl PreparedRender {
    pub(crate) fn message(&self) -> &ServerMessage {
        match self {
            Self::Semantic { message, .. } | Self::SemanticPatch { message, .. } => message,
        }
    }
}

pub(crate) type RenderedTabSurface = (
    ratatui::buffer::Buffer,
    Option<CursorState>,
    Vec<((u16, u16), String, String)>,
    crate::ui::TabSurfaceLayout,
);

/// Renders only the active tab's pane surface at an origin-relative client viewport.
pub(crate) fn render_tab_surface_virtual(
    app_state: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    layout: crate::ui::TabSurfaceLayout,
    area: Rect,
) -> RenderedTabSurface {
    let surface = crate::ui::TabSurfaceView {
        target: layout.target.as_ref(),
        pane_infos: &layout.pane_infos,
        split_borders: &layout.split_borders,
    };
    let cursor = crate::ui::tab_surface_cursor(app_state, terminal_runtimes, surface);
    let hyperlinks = crate::ui::tab_surface_hyperlinks(app_state, terminal_runtimes, surface);

    let backend = TestBackend::new(area.width, area.height);
    // The backend's error type is `Infallible`, so these patterns are irrefutable.
    let Ok(mut terminal) = ratatui::Terminal::new(backend);
    let Ok(_) = terminal.draw(|frame| {
        crate::ui::render_tab_surface(app_state, terminal_runtimes, surface, frame);
    });

    (
        terminal.backend().buffer().clone(),
        cursor,
        hyperlinks,
        layout,
    )
}

#[cfg(test)]
impl ClientRenderState {
    /// The last sent surface, mutable, so tests can stage a stale baseline.
    pub(crate) fn last_surface_mut(&mut self) -> Option<&mut PaneSurfaceFrame> {
        self.last_surface.as_deref_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_protocol::FrameData;

    fn test_surface(content: &str) -> PaneSurfaceFrame {
        let pane = ratatui::buffer::Buffer::with_lines([content]);
        PaneSurfaceFrame {
            boot_id: shepr_test_fixtures::fixed_boot_id(1),
            projection_revision: shepr_protocol::ProjectionRevision::new(1),
            surface_revision: shepr_protocol::SurfaceRevision::new(1),
            frame: FrameData::from_ratatui_buffer_with_hyperlinks(&pane, None, &[]),
            panes: Vec::new(),
            splits: Vec::new(),
        }
    }

    #[test]
    fn surface_delta_recompute_preserves_wire_baseline_but_epoch_reset_drops_it() {
        let mut state = ClientRenderState::new();
        let mut surface = test_surface("popup");
        surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(
            &ratatui::buffer::Buffer::empty(Rect::new(0, 0, 120, 40)),
            None,
            &[],
        );
        let initial = state
            .prepare_pane_surface(surface.clone())
            .expect("test precondition");
        state.commit_sent_frame(initial);
        state.request_recompute();
        assert!(state.last_pane_surface().is_some());
        assert!(state.requires_recompute());
        // A freshness request still emits a new revision when every cell is equal.
        let fresh = state
            .prepare_pane_surface(surface.clone())
            .expect("test precondition");
        assert!(matches!(fresh.message(), ServerMessage::SurfaceUpdate(_)));
        assert!(state.requires_recompute(), "prepare must not commit");
        state.commit_sent_frame(fresh);
        assert!(!state.requires_recompute());
        assert_eq!(
            state
                .last_pane_surface()
                .expect("test precondition")
                .surface_revision,
            2
        );
        state.request_repaint();
        assert!(state.last_pane_surface().is_none());
        let recovery = state
            .prepare_pane_surface(surface)
            .expect("test precondition");
        assert!(
            matches!(recovery.message(), ServerMessage::PaneSurface(frame) if frame.surface_revision == 3)
        );
    }

    #[test]
    fn surface_encodings_preserve_projection_and_patch_baselines() {
        let mut state = ClientRenderState::new();
        let mut decoder = shepr_protocol::surface_reuse::Decoder::default();
        let mut surface = test_surface("popup");
        let buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 240, 100));
        surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
        let initial = state
            .prepare_pane_surface(surface.clone())
            .expect("test precondition");
        decoder
            .decode(initial.message().clone())
            .expect("test precondition");
        state.commit_sent_frame(initial);

        surface.projection_revision = surface
            .projection_revision
            .checked_next()
            .expect("test precondition");
        let update = state
            .prepare_pane_surface(surface.clone())
            .expect("test precondition");
        let mut bytes = Vec::new();
        shepr_protocol::write_message(&mut bytes, update.message()).expect("test precondition");

        assert!(matches!(update.message(), ServerMessage::SurfaceUpdate(_)));
        assert!(
            bytes.len() < 2000,
            "metadata update was {} bytes",
            bytes.len()
        );

        let ServerMessage::PaneSurface(decoded) = decoder
            .decode(update.message().clone())
            .expect("test precondition")
        else {
            panic!("decoded full surface");
        };
        assert_eq!(decoded.frame, surface.frame);
        assert_eq!(decoded.projection_revision, surface.projection_revision);
        assert_eq!(decoded.surface_revision, 2);
        state.commit_sent_frame(update);

        let mut changed_cell = surface.frame.cells[0].clone();
        changed_cell.symbol = "x".into();
        let patch = state
            .prepare_pane_surface_patch(PaneSurfacePatch {
                boot_id: surface.boot_id.clone(),
                projection_revision: surface.projection_revision,
                base_surface_revision: shepr_protocol::SurfaceRevision::new(2),
                surface_revision: shepr_protocol::SurfaceRevision::new(0),
                rows: vec![shepr_protocol::PaneSurfacePatchRow {
                    x: 0,
                    y: 0,
                    cells: vec![changed_cell.clone()],
                }],
                panes: Vec::new(),
                cursor: None,
            })
            .expect("test precondition");
        decoder
            .decode(patch.message().clone())
            .expect("test precondition");
        state.commit_sent_frame(patch);
        surface.frame.cells[0] = changed_cell;
        surface.projection_revision = surface
            .projection_revision
            .checked_next()
            .expect("test precondition");
        let update = state
            .prepare_pane_surface(surface.clone())
            .expect("test precondition");
        let ServerMessage::PaneSurface(decoded) = decoder
            .decode(update.message().clone())
            .expect("test precondition")
        else {
            panic!("decoded surface after patch");
        };
        assert_eq!(decoded.frame, surface.frame);
        assert_eq!(decoded.surface_revision, 4);
        state.commit_sent_frame(update);

        // A changed border or terminal cell must still reach the client.
        surface.frame.cells[0].symbol = "y".into();
        let changed = state
            .prepare_pane_surface(surface.clone())
            .expect("test precondition");
        assert!(matches!(changed.message(), ServerMessage::SurfaceUpdate(_)));
        assert!(matches!(
            decoder
                .decode(changed.message().clone())
                .expect("test precondition"),
            ServerMessage::PaneSurfacePatch(_)
        ));
        let decoded = decoder.current_surface().expect("decoded changed surface");
        assert_eq!(decoded.frame, surface.frame);
        state.commit_sent_frame(changed);

        state.request_repaint();
        assert!(matches!(
            state
                .prepare_pane_surface(surface)
                .expect("test precondition")
                .message(),
            ServerMessage::PaneSurface(_)
        ));
    }

    #[test]
    fn surface_update_keeps_large_metadata_within_the_frame_limit() {
        let mut state = ClientRenderState::new();
        let mut surface = test_surface("popup");
        surface.frame.hyperlinks = vec!["\"".repeat(shepr_protocol::MAX_FRAME_SIZE / 2)];
        let initial = state
            .prepare_pane_surface(surface.clone())
            .expect("test precondition");
        state.commit_sent_frame(initial);
        surface.projection_revision = surface
            .projection_revision
            .checked_next()
            .expect("test precondition");
        let update = state
            .prepare_pane_surface(surface)
            .expect("test precondition");
        assert!(matches!(update.message(), ServerMessage::SurfaceUpdate(_)));
        let mut bytes = Vec::new();
        shepr_protocol::write_message(&mut bytes, update.message()).expect("test precondition");
        assert!(bytes.len() < shepr_protocol::MAX_FRAME_SIZE);
    }

    #[test]
    fn mismatched_patch_is_rejected_and_drops_the_baseline_without_panicking() {
        let mut surface = test_surface("abc");
        let before = surface.clone();
        let patch = PaneSurfacePatch {
            boot_id: surface.boot_id.clone(),
            projection_revision: shepr_protocol::ProjectionRevision::new(1),
            base_surface_revision: shepr_protocol::SurfaceRevision::new(1),
            surface_revision: shepr_protocol::SurfaceRevision::new(2),
            rows: vec![shepr_protocol::PaneSurfacePatchRow {
                x: 2,
                y: 0,
                cells: vec![surface.frame.cells[0].clone(); 2],
            }],
            panes: Vec::new(),
            cursor: None,
        };
        assert!(apply_pane_surface_patch(&mut surface, &patch).is_err());
        assert_eq!(surface, before, "a rejected patch changes nothing");
        let mut stale_patch = patch.clone();
        stale_patch.base_surface_revision = stale_patch
            .base_surface_revision
            .checked_next()
            .expect("test precondition");
        assert!(apply_pane_surface_patch(&mut surface, &stale_patch).is_err());
        assert_eq!(surface, before, "a stale patch changes nothing");

        let mut state = ClientRenderState::new();
        let initial = state
            .prepare_pane_surface(test_surface("abc"))
            .expect("test precondition");
        state.commit_sent_frame(initial);
        state.commit_sent_frame(PreparedRender::SemanticPatch {
            message: ServerMessage::PaneSurfacePatch(patch.clone()),
            patch: patch.clone(),
        });
        assert!(state.last_pane_surface().is_none());

        // Committing with no baseline at all is also survivable.
        state.commit_sent_frame(PreparedRender::SemanticPatch {
            message: ServerMessage::PaneSurfacePatch(patch.clone()),
            patch,
        });
        assert!(state.last_pane_surface().is_none());
    }

    #[test]
    fn changed_frame_content_is_not_deduplicated() {
        let mut state = ClientRenderState::new();
        let prepared = state
            .prepare_pane_surface(test_surface("first"))
            .expect("initial surface");
        state.commit_sent_frame(prepared);

        assert!(state.prepare_pane_surface(test_surface("second")).is_some());
    }

    #[test]
    fn forced_full_surface_keeps_the_connection_revision_monotonic() {
        let mut state = ClientRenderState::new();
        let prepared = state
            .prepare_pane_surface(test_surface("first"))
            .expect("initial surface");
        state.commit_sent_frame(prepared);
        state.request_repaint();

        let prepared = state
            .prepare_pane_surface(test_surface("replacement"))
            .expect("forced replacement surface");
        assert!(matches!(
            prepared.message(),
            ServerMessage::PaneSurface(surface) if surface.surface_revision == 2
        ));
        state.commit_sent_frame(prepared);
        assert_eq!(
            state
                .last_pane_surface()
                .expect("test precondition")
                .surface_revision,
            2
        );
    }
}
