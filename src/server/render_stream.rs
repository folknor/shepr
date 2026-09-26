//! Virtual rendering helpers for headless client frame streaming.

use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::layout::{Position, Rect, Size};

use crate::app::state::AppState;
use crate::protocol::render_ansi::{BlitEncoder, EncodedBlit};
use crate::protocol::{
    CursorState, FrameData, PaneSurfaceFrame, PaneSurfacePatch, RenderEncoding, ServerMessage,
    TerminalFrame,
};
use crate::terminal::TerminalRuntimeRegistry;

/// Per-client render baseline for the selected render encoding.
pub(crate) enum ClientRenderState {
    /// Semantic clients compare full frame data and skip identical frames.
    Semantic {
        last_surface: Option<Box<PaneSurfaceFrame>>,
        surface_revision: u64,
        recompute_pending: bool,
    },
    /// Terminal-ANSI clients keep a terminal diff encoder.
    TerminalAnsi {
        blit_encoder: BlitEncoder,
        repaint_pending: bool,
    },
}

impl ClientRenderState {
    pub(crate) fn new(render_encoding: RenderEncoding) -> Self {
        match render_encoding {
            RenderEncoding::SemanticFrame => Self::Semantic {
                last_surface: None,
                surface_revision: 0,
                recompute_pending: false,
            },
            RenderEncoding::TerminalAnsi => Self::TerminalAnsi {
                blit_encoder: BlitEncoder::new(),
                repaint_pending: false,
            },
        }
    }

    pub(crate) fn request_recompute(&mut self) {
        if let Self::Semantic {
            recompute_pending, ..
        } = self
        {
            *recompute_pending = true;
        }
    }

    pub(crate) fn requires_recompute(&self) -> bool {
        matches!(
            self,
            Self::Semantic {
                recompute_pending: true,
                ..
            }
        )
    }

    pub(crate) fn reset_baseline(&mut self) {
        match self {
            Self::Semantic { last_surface, .. } => *last_surface = None,
            Self::TerminalAnsi {
                blit_encoder,
                repaint_pending,
                ..
            } => {
                *blit_encoder = BlitEncoder::new();
                *repaint_pending = false;
            }
        }
    }

    pub(crate) fn request_repaint(&mut self) {
        match self {
            Self::Semantic { last_surface, .. } => *last_surface = None,
            Self::TerminalAnsi {
                repaint_pending, ..
            } => *repaint_pending = true,
        }
    }

    pub(crate) fn prepare_frame(&mut self, frame: FrameData) -> Option<PreparedRender> {
        match self {
            Self::Semantic { .. } => None,
            Self::TerminalAnsi {
                blit_encoder,
                repaint_pending,
            } => {
                if !*repaint_pending && blit_encoder.is_current(&frame) {
                    return None;
                }
                let encoded = blit_encoder.encode(&frame, *repaint_pending);
                Some(PreparedRender::TerminalAnsi {
                    message: ServerMessage::Terminal(TerminalFrame {
                        bytes: encoded.bytes.clone(),
                    }),
                    frame,
                    encoded: Some(encoded),
                })
            }
        }
    }

    pub(crate) fn last_pane_surface(&self) -> Option<&PaneSurfaceFrame> {
        match self {
            Self::Semantic { last_surface, .. } => last_surface.as_deref(),
            Self::TerminalAnsi { .. } => None,
        }
    }

    pub(crate) fn prepare_pane_surface(
        &mut self,
        mut surface: PaneSurfaceFrame,
    ) -> Option<PreparedRender> {
        let Self::Semantic {
            last_surface,
            surface_revision,
            recompute_pending,
        } = self
        else {
            return None;
        };
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
        surface.surface_revision = surface_revision.saturating_add(1);
        let committed_surface = surface.clone();
        let mut message = ServerMessage::PaneSurface(surface);
        let delta = last_surface.as_deref().and_then(|last| {
            crate::protocol::surface_delta::message(last, &mut message)
                .map_err(|error| tracing::warn!(%error, "failed to encode surface delta"))
                .ok()
                .flatten()
        });
        let reused = if let ServerMessage::PaneSurface(surface) = &mut message {
            delta
                .is_none()
                .then_some(last_surface.as_deref())
                .flatten()
                .filter(|last| last.frame == surface.frame)
                .and_then(|last| {
                    crate::protocol::surface_reuse::message(last, surface)
                        .map_err(|error| tracing::warn!(%error, "failed to encode surface reuse"))
                        .ok()
                        .flatten()
                })
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
        let Self::Semantic {
            last_surface,
            surface_revision,
            ..
        } = self
        else {
            return None;
        };
        if self.requires_recompute() {
            return None;
        }
        let last = last_surface.as_deref()?;
        let next_revision = surface_revision.checked_add(1)?;
        let baseline = crate::protocol::surface_reuse::Baseline::new(
            &last.boot_id,
            last.projection_revision,
            last.surface_revision,
        );
        if !baseline.accepts(
            &patch.boot_id,
            patch.base_surface_revision,
            next_revision,
            &crate::protocol::surface_reuse::ProjectionUpdate::Patch {
                revision: patch.projection_revision,
            },
        ) {
            return None;
        }
        patch.surface_revision = next_revision;
        Some(PreparedRender::SemanticPatch {
            message: ServerMessage::PaneSurfacePatch(patch),
        })
    }

    pub(crate) fn commit_sent_frame(&mut self, prepared: PreparedRender) {
        match (self, prepared) {
            (
                Self::Semantic {
                    last_surface,
                    surface_revision,
                    recompute_pending,
                    ..
                },
                PreparedRender::Semantic {
                    committed_surface, ..
                },
            ) => {
                *surface_revision = committed_surface.surface_revision;
                *last_surface = Some(committed_surface);
                *recompute_pending = false;
            }
            (
                Self::Semantic {
                    last_surface,
                    surface_revision,
                    ..
                },
                PreparedRender::SemanticPatch {
                    message: ServerMessage::PaneSurfacePatch(patch),
                },
            ) => {
                // Planning checked the baseline and the server does not yield
                // between planning and commit, so neither branch below should
                // run. If one does, the client holds a surface this side can no
                // longer reproduce: drop the baseline so the next render sends a
                // full surface rather than diffing against a wrong grid.
                let applied = match last_surface.as_deref_mut() {
                    Some(surface) => apply_pane_surface_patch(surface, &patch),
                    None => Err("no committed surface"),
                };
                if let Err(reason) = applied {
                    tracing::warn!(reason, "sent surface patch did not apply to its baseline");
                    *last_surface = None;
                }
                *surface_revision = patch.surface_revision;
            }
            (
                Self::TerminalAnsi {
                    blit_encoder,
                    repaint_pending,
                },
                PreparedRender::TerminalAnsi {
                    frame,
                    encoded: Some(encoded),
                    ..
                },
            ) => {
                blit_encoder.commit(frame, &encoded);
                *repaint_pending = false;
            }
            _ => {}
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
    let baseline = crate::protocol::surface_reuse::Baseline::new(
        &surface.boot_id,
        surface.projection_revision,
        surface.surface_revision,
    );
    if !baseline.accepts(
        &patch.boot_id,
        patch.base_surface_revision,
        patch.surface_revision,
        &crate::protocol::surface_reuse::ProjectionUpdate::Patch {
            revision: patch.projection_revision,
        },
    ) {
        return Err("patch revision does not match the surface baseline");
    }
    let width = usize::from(surface.frame.width);
    let cell_count = surface.frame.cells.len();
    let row_fits = |row: &crate::protocol::PaneSurfacePatchRow| {
        let start = usize::from(row.y) * width + usize::from(row.x);
        usize::from(row.x) + row.cells.len() <= width
            && start.saturating_add(row.cells.len()) <= cell_count
    };
    if !patch.rows.iter().all(row_fits) {
        return Err("patch row exceeds the surface grid");
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
    },
    TerminalAnsi {
        message: ServerMessage,
        frame: FrameData,
        encoded: Option<EncodedBlit>,
    },
}

impl PreparedRender {
    pub(crate) fn message(&self) -> &ServerMessage {
        match self {
            Self::Semantic { message, .. }
            | Self::SemanticPatch { message }
            | Self::TerminalAnsi { message, .. } => message,
        }
    }
}

struct CursorTrackingBackend {
    inner: TestBackend,
    rendered_cursor: Option<Position>,
}

impl CursorTrackingBackend {
    fn new(width: u16, height: u16) -> Self {
        Self {
            inner: TestBackend::new(width, height),
            rendered_cursor: None,
        }
    }

    fn buffer(&self) -> &ratatui::buffer::Buffer {
        self.inner.buffer()
    }

    fn rendered_cursor(&self) -> Option<CursorState> {
        self.rendered_cursor.map(|pos| CursorState {
            x: pos.x,
            y: pos.y,
            visible: true,
            shape: 0,
        })
    }
}

impl Backend for CursorTrackingBackend {
    type Error = std::convert::Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()?;
        self.rendered_cursor = None;
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let position = position.into();
        self.inner.set_cursor_position(position)?;
        self.rendered_cursor = Some(position);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
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
    terminal_runtimes: &TerminalRuntimeRegistry,
    layout: crate::ui::TabSurfaceLayout,
    area: Rect,
) -> RenderedTabSurface {
    let surface = crate::ui::TabSurfaceView {
        target: layout.target,
        pane_infos: &layout.pane_infos,
        split_borders: &layout.split_borders,
    };
    let cursor = crate::ui::tab_surface_cursor(app_state, terminal_runtimes, surface);
    let hyperlinks = crate::ui::tab_surface_hyperlinks(app_state, terminal_runtimes, surface);

    let backend = CursorTrackingBackend::new(area.width, area.height);
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

/// Renders one server-owned terminal directly for `terminal attach` clients.
pub(crate) fn render_terminal_virtual(
    runtime: &crate::terminal::TerminalRuntime,
    area: Rect,
) -> (ratatui::buffer::Buffer, Option<CursorState>) {
    let suppress_cursor = runtime.synchronized_output_active();
    let backend = CursorTrackingBackend::new(area.width, area.height);
    // The backend's error type is `Infallible`, so these patterns are irrefutable.
    let Ok(mut terminal) = ratatui::Terminal::new(backend);
    let Ok(_) = terminal.draw(|frame| {
        runtime.render(frame, area, true);
    });

    let buffer = terminal.backend().buffer().clone();
    let cursor = (!suppress_cursor)
        .then(|| runtime.cursor_state(area, true))
        .flatten()
        .map(|cursor| CursorState {
            x: cursor.x,
            y: cursor.y,
            visible: cursor.visible && !crate::ui::pane_is_scrolled_back(runtime),
            shape: cursor.shape,
        })
        .or_else(|| {
            (!suppress_cursor)
                .then(|| terminal.backend().rendered_cursor())
                .flatten()
        });

    (buffer, cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_surface(content: &str) -> PaneSurfaceFrame {
        let pane = ratatui::buffer::Buffer::with_lines([content]);
        PaneSurfaceFrame {
            boot_id: "boot-1".into(),
            projection_revision: 1,
            surface_revision: 1,
            frame: FrameData::from_ratatui_buffer_with_hyperlinks(&pane, None, &[]),
            panes: Vec::new(),
            splits: Vec::new(),
        }
    }

    #[test]
    fn surface_delta_recompute_preserves_wire_baseline_but_epoch_reset_drops_it() {
        let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
        let mut surface = test_surface("popup");
        surface.frame = FrameData::from_ratatui_buffer(
            &ratatui::buffer::Buffer::empty(Rect::new(0, 0, 120, 40)),
            None,
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
        assert!(
            matches!(fresh.message(), ServerMessage::EndpointControl { kind, .. }
            if kind == crate::protocol::surface_delta::MESSAGE_KIND)
        );
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
        let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
        let mut decoder = crate::protocol::surface_reuse::Decoder::new(true);
        let mut surface = test_surface("popup");
        let buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 240, 100));
        surface.frame = FrameData::from_ratatui_buffer(&buffer, None);
        let initial = state
            .prepare_pane_surface(surface.clone())
            .expect("test precondition");
        decoder
            .decode(initial.message().clone())
            .expect("test precondition");
        state.commit_sent_frame(initial);

        surface.projection_revision += 1;
        let update = state
            .prepare_pane_surface(surface.clone())
            .expect("test precondition");
        let mut bytes = Vec::new();
        crate::protocol::write_message(&mut bytes, update.message()).expect("test precondition");

        assert!(matches!(
            update.message(),
            ServerMessage::EndpointControl { kind, .. }
                if kind == crate::protocol::surface_reuse::MESSAGE_KIND
                    || kind == crate::protocol::surface_delta::MESSAGE_KIND
        ));
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
                base_surface_revision: 2,
                surface_revision: 0,
                rows: vec![crate::protocol::PaneSurfacePatchRow {
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
        surface.projection_revision += 1;
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
        let ServerMessage::PaneSurface(decoded) = decoder
            .decode(changed.message().clone())
            .expect("test precondition")
        else {
            panic!("changed full surface");
        };
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
    fn surface_reuse_falls_back_when_json_metadata_exceeds_the_frame_limit() {
        let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
        let mut surface = test_surface("popup");
        surface.frame.hyperlinks = vec!["\"".repeat(crate::protocol::MAX_FRAME_SIZE / 2)];
        let initial = state
            .prepare_pane_surface(surface.clone())
            .expect("test precondition");
        state.commit_sent_frame(initial);
        surface.projection_revision += 1;
        let update = state
            .prepare_pane_surface(surface)
            .expect("test precondition");
        assert!(matches!(update.message(), ServerMessage::PaneSurface(_)));
        let mut bytes = Vec::new();
        crate::protocol::write_message(&mut bytes, update.message()).expect("test precondition");
        assert!(bytes.len() < crate::protocol::MAX_FRAME_SIZE);
    }

    #[test]
    fn mismatched_patch_is_rejected_and_drops_the_baseline_without_panicking() {
        let mut surface = test_surface("abc");
        let before = surface.clone();
        let patch = PaneSurfacePatch {
            boot_id: surface.boot_id.clone(),
            projection_revision: 1,
            base_surface_revision: 1,
            surface_revision: 2,
            rows: vec![crate::protocol::PaneSurfacePatchRow {
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
        stale_patch.base_surface_revision += 1;
        assert!(apply_pane_surface_patch(&mut surface, &stale_patch).is_err());
        assert_eq!(surface, before, "a stale patch changes nothing");

        let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
        let initial = state
            .prepare_pane_surface(test_surface("abc"))
            .expect("test precondition");
        state.commit_sent_frame(initial);
        state.commit_sent_frame(PreparedRender::SemanticPatch {
            message: ServerMessage::PaneSurfacePatch(patch.clone()),
        });
        assert!(state.last_pane_surface().is_none());

        // Committing with no baseline at all is also survivable.
        state.commit_sent_frame(PreparedRender::SemanticPatch {
            message: ServerMessage::PaneSurfacePatch(patch),
        });
        assert!(state.last_pane_surface().is_none());
    }

    #[test]
    fn changed_frame_content_is_not_deduplicated() {
        let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
        let prepared = state
            .prepare_pane_surface(test_surface("first"))
            .expect("initial surface");
        state.commit_sent_frame(prepared);

        assert!(state.prepare_pane_surface(test_surface("second")).is_some());
    }

    #[test]
    fn forced_full_surface_keeps_the_connection_revision_monotonic() {
        let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
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
