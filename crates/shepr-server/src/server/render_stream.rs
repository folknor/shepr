//! Virtual rendering helpers for headless client frame streaming.

use ratatui::layout::Rect;

use crate::app::state::AppState;
use crate::server::clients::ClientPaneIdentity;
use crate::server::committed_baseline::CommittedBaseline;
use shepr_mux::pane::PaneRuntimeRegistry;
use shepr_protocol::{
    FrameData, PaneSurfaceFrame, PaneSurfacePatch, ServerMessage, SurfaceRevision,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PatchPreparationFailure {
    RecomputePending,
    MissingBaseline,
    RevisionExhausted,
    InvalidPatch,
}

fn warn_surface_encoding_failure(
    encoding: &'static str,
    error: &impl std::fmt::Display,
    last: &PaneSurfaceFrame,
    surface: &PaneSurfaceFrame,
) {
    shepr_platform::structured_log!(
        WARN, event = surface.encode, outcome = "error",
        %error,
        encoding = %encoding,
        boot_id = %surface.boot_id,
        base_projection_revision = ?last.projection_revision,
        base_surface_revision = ?last.surface_revision,
        projection_revision = ?surface.projection_revision,
        surface_revision = ?surface.surface_revision,
        width = surface.frame.width(),
        height = surface.frame.height(),
        "failed to encode compact surface update"
    );
}

/// Moves whenever something every client's projection or surface may depend
/// on changed (application state, theme, PTY sizes, the client set). A
/// version, compared against what each client last settled at, so a client
/// that could not be served when it moved keeps that debt until it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ViewEpoch(u64);
impl ViewEpoch {
    pub(crate) const INITIAL: Self = Self(1);
    pub(crate) fn advance(&mut self) {
        self.0 = self.0.saturating_add(1);
    }
}

/// What a client is owed beyond what its baseline says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfaceDebt {
    /// Owed only what the baseline implies (missing, or recompute pending).
    Clear,
    /// A surface this client should have was not delivered.
    Owed,
    /// The last attempt produced a surface the client can never receive
    /// (oversized). Hides the baseline-implied debt until a retry is due.
    Refused,
}

/// What the delta planner made of a rendered surface.
pub(crate) enum PreparedSurface {
    /// A message to send, and the baseline to commit once it is queued.
    Ready(Box<PreparedRender>),
    /// Identical to the baseline: nothing to send.
    Unchanged,
    /// The surface revision counter is spent; the client can never be sent
    /// another surface on this connection.
    RevisionsExhausted,
}

/// Per-client render baseline: the last surface sent with its pane identities,
/// and its revision. The delta planner skips unchanged surfaces after its cell
/// comparison pass.
///
/// The baseline, `recompute_pending` and `debt` are not independent: a refusal
/// sets a recompute without dropping the baseline, a repaint drops the baseline
/// and the debt but not a pending recompute, and `surface_debt` and
/// `takes_patches` read all three together. Merging them into one state would
/// change which render each combination gets.
pub(crate) struct ClientRenderState {
    committed: Option<Box<CommittedBaseline>>,
    surface_revision: SurfaceRevision,
    recompute_pending: bool,
    /// The epoch this client last settled at; `None` is stale at any epoch
    /// (never settled, or invalidated for this client alone).
    settled: Option<ViewEpoch>,
    debt: SurfaceDebt,
}

impl ClientRenderState {
    pub(crate) fn new() -> Self {
        Self {
            committed: None,
            surface_revision: SurfaceRevision::ZERO,
            recompute_pending: false,
            settled: None,
            debt: SurfaceDebt::Clear,
        }
    }

    pub(crate) fn is_settled_at(&self, epoch: ViewEpoch) -> bool {
        self.settled == Some(epoch)
    }
    pub(crate) fn settle(&mut self, epoch: ViewEpoch) {
        self.settled = Some(epoch);
    }
    /// Makes this client alone stale, whatever the epoch.
    pub(crate) fn invalidate(&mut self) {
        self.settled = None;
    }
    pub(crate) fn owe(&mut self) {
        self.debt = SurfaceDebt::Owed;
    }
    /// Records a surface too large to send. Nothing was committed, so any
    /// baseline is behind what the client should see (a surface equal to it
    /// is not sent at all): a retry must send a full surface, never patch
    /// that baseline with only the newest damage.
    pub(crate) fn refuse(&mut self) {
        self.debt = SurfaceDebt::Refused;
        self.recompute_pending = true;
    }
    pub(crate) fn clear_debt(&mut self) {
        self.debt = SurfaceDebt::Clear;
    }
    /// Lets a refused client be planned again. Returns whether it was refused.
    pub(crate) fn retry_refused(&mut self) -> bool {
        if self.debt != SurfaceDebt::Refused {
            return false;
        }
        self.clear_debt();
        true
    }
    pub(crate) fn surface_debt(&self) -> bool {
        match self.debt {
            SurfaceDebt::Owed => true,
            SurfaceDebt::Refused => false,
            SurfaceDebt::Clear => self.committed.is_none() || self.recompute_pending,
        }
    }
    pub(crate) fn takes_patches(&self) -> bool {
        self.debt == SurfaceDebt::Clear && self.committed.is_some() && !self.recompute_pending
    }

    pub(crate) fn request_recompute(&mut self) {
        self.recompute_pending = true;
    }

    pub(crate) fn requires_recompute(&self) -> bool {
        self.recompute_pending
    }

    /// Forgets what was committed, surface and pane identities together.
    pub(crate) fn request_repaint(&mut self) {
        self.committed = None;
        self.clear_debt();
    }

    pub(crate) fn committed_baseline(&self) -> Option<&CommittedBaseline> {
        self.committed.as_deref()
    }

    pub(crate) fn last_pane_surface(&self) -> Option<&PaneSurfaceFrame> {
        self.committed.as_deref().map(CommittedBaseline::surface)
    }

    /// Plans the send of `surface`, whose panes are `identities`. A surface
    /// whose pane identities differ from the committed ones is a recompute
    /// even when the wire fields match: a public pane id can outlive a layout
    /// update, and retained rendering trusts the committed identities.
    pub(crate) fn prepare_surface(
        &mut self,
        mut surface: PaneSurfaceFrame,
        identities: Vec<ClientPaneIdentity>,
    ) -> PreparedSurface {
        let Self {
            committed,
            surface_revision,
            recompute_pending,
            ..
        } = self;
        if committed
            .as_deref()
            .is_some_and(|baseline| !baseline.has_identities(&identities))
        {
            *recompute_pending = true;
        }
        // The client accepts a surface only at its exact successor revision,
        // so an exhausted counter (one step per sent frame, unreachable in
        // practice) closes the connection rather than repeating a revision.
        let Some(next_revision) = surface_revision.checked_next() else {
            return PreparedSurface::RevisionsExhausted;
        };
        surface.surface_revision = next_revision;
        let plan = committed
            .as_deref()
            .map(CommittedBaseline::surface)
            .and_then(|last| match shepr_surface::delta::message(last, &surface) {
                Ok(plan) => Some(plan),
                Err(error) => {
                    warn_surface_encoding_failure("delta", &error, last, &surface);
                    None
                }
            });
        let (message, committed_surface) = match plan {
            Some(shepr_surface::delta::SurfaceDeltaPlan::Unchanged(_message))
                if !*recompute_pending =>
            {
                return PreparedSurface::Unchanged;
            }
            Some(
                shepr_surface::delta::SurfaceDeltaPlan::Unchanged(message)
                | shepr_surface::delta::SurfaceDeltaPlan::Compact(message),
            ) => {
                // Compact messages need the complete newly rendered grid for
                // the next baseline. Full messages carry that grid themselves.
                (message, Some(Box::new(surface)))
            }
            Some(shepr_surface::delta::SurfaceDeltaPlan::Full) | None => {
                (ServerMessage::PaneSurface(surface), None)
            }
        };
        PreparedSurface::Ready(Box::new(PreparedRender::Semantic {
            message,
            committed_surface,
            identities,
        }))
    }

    pub(crate) fn prepare_pane_surface_patch(
        &self,
        mut patch: PaneSurfacePatch,
    ) -> Result<PreparedRender, PatchPreparationFailure> {
        let Self {
            committed,
            surface_revision,
            ..
        } = self;
        if self.requires_recompute() {
            return Err(PatchPreparationFailure::RecomputePending);
        }
        let last = committed
            .as_deref()
            .map(CommittedBaseline::surface)
            .ok_or(PatchPreparationFailure::MissingBaseline)?;
        let next_revision = surface_revision
            .checked_next()
            .ok_or(PatchPreparationFailure::RevisionExhausted)?;
        patch.surface_revision = next_revision;
        shepr_surface::decode::SurfaceBaseline::new(last)
            .admits(&patch)
            .map_err(|reason| {
                tracing::debug!(%reason, "retained surface patch failed baseline admission");
                PatchPreparationFailure::InvalidPatch
            })?;
        let meta = shepr_protocol::SurfaceMeta::Patch(shepr_protocol::SurfacePatchMeta {
            cursor: patch.cursor.clone(),
            panes: patch.panes.clone(),
        });
        let message = ServerMessage::SurfaceUpdate(shepr_protocol::SurfaceUpdate {
            boot_id: patch.boot_id.clone(),
            base_projection_revision: last.projection_revision,
            base_surface_revision: patch.base_surface_revision,
            surface_revision: patch.surface_revision,
            projection_revision: patch.projection_revision,
            meta: Some(meta),
            spans: patch.rows.clone(),
        });
        Ok(PreparedRender::SemanticPatch { message, patch })
    }

    pub(crate) fn commit_sent_frame(&mut self, prepared: PreparedRender) {
        match prepared {
            PreparedRender::Semantic {
                message,
                committed_surface,
                identities,
            } => {
                let committed_surface = match committed_surface {
                    Some(surface) => Some(surface),
                    None => match message {
                        ServerMessage::PaneSurface(surface) => Some(Box::new(surface)),
                        _ => None,
                    },
                };
                if let Some(committed_surface) = committed_surface {
                    self.surface_revision = committed_surface.surface_revision;
                    self.committed = Some(Box::new(CommittedBaseline::new(
                        *committed_surface,
                        identities,
                    )));
                    self.recompute_pending = false;
                } else {
                    shepr_platform::structured_log!(
                        ERROR,
                        event = surface.render,
                        outcome = "missing_baseline",
                        "full surface render did not contain a surface baseline"
                    );
                    self.committed = None;
                }
            }
            PreparedRender::SemanticPatch { patch, .. } => {
                // Planning checked the baseline and the server does not yield
                // between planning and commit, so neither branch below should
                // run. If one does, the client holds a surface this side can no
                // longer reproduce: drop the baseline so the next render sends a
                // full surface rather than diffing against a wrong grid.
                let applied = match self.committed.as_deref_mut() {
                    Some(baseline) => apply_pane_surface_patch(baseline.surface_mut(), &patch),
                    None => Err(shepr_surface::decode::SurfaceDecodeError::MissingBaseline),
                };
                if let Err(reason) = applied {
                    shepr_platform::structured_log!(WARN, event = surface.patch, outcome = "invalid", %reason, "sent surface patch did not apply to its baseline");
                    self.committed = None;
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
) -> Result<(), shepr_surface::decode::SurfaceDecodeError> {
    shepr_surface::decode::apply_patch_to_surface(surface, patch)
}

/// A prepared client render message plus any baseline state needed after send.
pub(crate) enum PreparedRender {
    Semantic {
        message: ServerMessage,
        /// `None` exactly when `message` is the full `PaneSurface`, which then
        /// is the baseline; `prepare_surface` is the only constructor, and
        /// `commit_sent_frame` recovers the surface from the message. A compact
        /// message needs its own complete grid, hence `Some`.
        committed_surface: Option<Box<PaneSurfaceFrame>>,
        /// The identities of the surface's panes, committed beside it.
        identities: Vec<ClientPaneIdentity>,
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

/// One virtual render: the frame, the layout it drew and how each pane's draw
/// went.
pub(crate) type VirtualSurface = (
    FrameData,
    crate::ui::SurfaceLayout,
    Vec<(shepr_core::layout::PaneId, shepr_mux::pane::PaneDraw)>,
);

/// Renders only the focused workspace's pane surface at an origin-relative
/// client viewport, straight into wire form: the frame holds the cells, the
/// links and the cursor. The third part is how each pane's draw went.
pub(crate) fn render_surface_virtual(
    app_state: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    layout: crate::ui::SurfaceLayout,
    area: Rect,
) -> Result<VirtualSurface, shepr_protocol::FrameGridError> {
    // Full rendering materializes every cell for new surfaces and retained-path
    // fallbacks; dirty-row updates take the retained renderer instead.
    let surface = crate::ui::SurfaceView {
        target: layout.target,
        panes: &layout.panes,
        split_borders: &layout.split_borders,
    };
    let mut frame = FrameData::blank(area.width, area.height)?;
    let draws = crate::ui::render_surface(app_state, terminal_runtimes, surface, &mut frame);
    frame.set_cursor(crate::ui::surface_cursor(
        app_state,
        terminal_runtimes,
        surface,
    ));
    Ok((frame, layout, draws))
}

#[cfg(test)]
impl PreparedSurface {
    fn expect(self, message: &str) -> PreparedRender {
        match self {
            Self::Ready(render) => *render,
            _ => panic!("{message}"),
        }
    }
    fn is_some(&self) -> bool {
        matches!(self, Self::Ready(_))
    }
    fn is_none(&self) -> bool {
        matches!(self, Self::Unchanged)
    }
}
#[cfg(test)]
impl ClientRenderState {
    /// Whether this client holds any settled epoch at all.
    pub(crate) fn has_settled(&self) -> bool {
        self.settled.is_some()
    }

    pub(crate) fn exhaust_revisions(&mut self) {
        self.surface_revision = shepr_test_fixtures::counter_at(u64::MAX);
    }

    /// The last sent surface, mutable, so tests can stage a stale baseline.
    pub(crate) fn last_surface_mut(&mut self) -> Option<&mut PaneSurfaceFrame> {
        self.committed
            .as_deref_mut()
            .map(CommittedBaseline::surface_mut)
    }

    /// Plans a surface with no panes, as most unit tests of the planner use.
    fn prepare_pane_surface(&mut self, surface: PaneSurfaceFrame) -> PreparedSurface {
        self.prepare_surface(surface, Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_surface::decode::DecodedServerMessage;
    use shepr_surface::ratatui_conversion::FrameDataExt as _;

    fn test_surface(content: &str) -> PaneSurfaceFrame {
        let pane = ratatui::buffer::Buffer::with_lines([content]);
        PaneSurfaceFrame {
            boot_id: shepr_test_fixtures::fixed_boot_id(1),
            projection_revision: shepr_test_fixtures::counter_at::<
                shepr_protocol::ProjectionRevision,
            >(1),
            surface_revision: shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(1),
            frame: FrameData::from_ratatui_buffer_with_hyperlinks(&pane, None, &[])
                .expect("test buffer is a valid frame"),
            panes: Vec::new(),
            splits: Vec::new(),
        }
    }

    #[test]
    fn surface_debt_tracks_missing_baselines_refusal_retries_and_repaint() {
        let mut state = ClientRenderState::new();
        assert!(state.surface_debt());
        assert!(!state.takes_patches());
        state.settle(ViewEpoch::INITIAL);
        state.refuse();
        assert!(!state.surface_debt());
        assert!(!state.takes_patches());
        assert!(state.retry_refused());
        assert!(state.surface_debt());
        let prepared = state
            .prepare_pane_surface(test_surface("baseline"))
            .expect("surface");
        state.commit_sent_frame(prepared);
        assert!(!state.surface_debt());
        assert!(state.takes_patches());
        state.owe();
        assert!(state.surface_debt());
        assert!(!state.takes_patches());
        state.clear_debt();
        state.request_recompute();
        assert!(state.surface_debt());
        state.refuse();
        assert!(!state.surface_debt());
        state.request_repaint();
        assert!(state.surface_debt());
        state.invalidate();
        assert!(!state.is_settled_at(ViewEpoch::INITIAL));
    }

    #[test]
    fn exhausted_surface_revisions_are_distinct_from_an_unchanged_surface() {
        let mut state = ClientRenderState::new();
        state.exhaust_revisions();
        assert!(matches!(
            state.prepare_pane_surface(test_surface("baseline")),
            PreparedSurface::RevisionsExhausted
        ));
    }

    #[test]
    fn surface_delta_recompute_preserves_wire_baseline_but_epoch_reset_drops_it() {
        let mut state = ClientRenderState::new();
        let mut surface = test_surface("popup");
        surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(
            &ratatui::buffer::Buffer::empty(Rect::new(0, 0, 120, 40)),
            None,
            &[],
        )
        .expect("test buffer is a valid frame");
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
            shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(2)
        );
        state.request_repaint();
        assert!(state.last_pane_surface().is_none());
        let recovery = state
            .prepare_pane_surface(surface)
            .expect("test precondition");
        assert!(
            matches!(recovery.message(), ServerMessage::PaneSurface(frame) if frame.surface_revision == shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(3))
        );
    }

    #[test]
    fn surface_encodings_preserve_projection_and_patch_baselines() {
        let mut state = ClientRenderState::new();
        let mut decoder = shepr_surface::decode::Decoder::default();
        let mut surface = test_surface("popup");
        let buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 240, 100));
        surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[])
            .expect("test buffer is a valid frame");
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

        let DecodedServerMessage::Wire(ServerMessage::PaneSurface(decoded)) = decoder
            .decode(update.message().clone())
            .expect("test precondition")
        else {
            panic!("decoded full surface");
        };
        assert_eq!(decoded.frame, surface.frame);
        assert_eq!(decoded.projection_revision, surface.projection_revision);
        assert_eq!(
            decoded.surface_revision,
            shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(2)
        );
        state.commit_sent_frame(update);

        let mut changed_cell = surface.frame.cells()[0].clone();
        changed_cell.symbol = "x".into();
        let patch =
            state
                .prepare_pane_surface_patch(PaneSurfacePatch {
                    boot_id: surface.boot_id.clone(),
                    projection_revision: surface.projection_revision,
                    base_surface_revision: shepr_test_fixtures::counter_at::<
                        shepr_protocol::SurfaceRevision,
                    >(2),
                    surface_revision: shepr_test_fixtures::counter_at::<
                        shepr_protocol::SurfaceRevision,
                    >(0),
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
        surface.frame.cells_mut()[0] = changed_cell;
        surface.projection_revision = surface
            .projection_revision
            .checked_next()
            .expect("test precondition");
        let update = state
            .prepare_pane_surface(surface.clone())
            .expect("test precondition");
        let DecodedServerMessage::Wire(ServerMessage::PaneSurface(decoded)) = decoder
            .decode(update.message().clone())
            .expect("test precondition")
        else {
            panic!("decoded surface after patch");
        };
        assert_eq!(decoded.frame, surface.frame);
        assert_eq!(
            decoded.surface_revision,
            shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(4)
        );
        state.commit_sent_frame(update);

        // A changed border or terminal cell must still reach the client.
        surface.frame.cells_mut()[0].symbol = "y".into();
        let changed = state
            .prepare_pane_surface(surface.clone())
            .expect("test precondition");
        assert!(matches!(changed.message(), ServerMessage::SurfaceUpdate(_)));
        assert!(matches!(
            decoder
                .decode(changed.message().clone())
                .expect("test precondition"),
            DecodedServerMessage::PaneSurfacePatch(_)
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
    fn surface_update_with_large_metadata_crosses_in_parts() {
        let mut state = ClientRenderState::new();
        let mut surface = test_surface("popup");
        // Metadata alone past one frame: the update still carries it, split
        // across frames, and the client decodes it against its baseline.
        surface
            .frame
            .set_hyperlinks(vec!["\"".repeat(shepr_protocol::MAX_FRAME_SIZE + 1)])
            .expect("no cell links yet");
        let mut decoder = shepr_surface::decode::Decoder::default();
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
        assert!(matches!(update.message(), ServerMessage::SurfaceUpdate(_)));
        let mut bytes = Vec::new();
        shepr_protocol::write_message(&mut bytes, update.message()).expect("test precondition");
        assert!(bytes.len() > shepr_protocol::MAX_FRAME_SIZE);
        let read: ServerMessage =
            shepr_protocol::read_message(&mut bytes.as_slice()).expect("test precondition");
        let DecodedServerMessage::Wire(ServerMessage::PaneSurface(decoded)) =
            decoder.decode(read).expect("test precondition")
        else {
            panic!("decoded full surface");
        };
        assert_eq!(decoded.frame, surface.frame);
    }

    #[test]
    fn mismatched_patch_is_rejected_and_drops_the_baseline_without_panicking() {
        let mut surface = test_surface("abc");
        let before = surface.clone();
        let patch = PaneSurfacePatch {
            boot_id: surface.boot_id.clone(),
            projection_revision: shepr_test_fixtures::counter_at::<
                shepr_protocol::ProjectionRevision,
            >(1),
            base_surface_revision: shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(
                1,
            ),
            surface_revision: shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(2),
            rows: vec![shepr_protocol::PaneSurfacePatchRow {
                x: 2,
                y: 0,
                cells: vec![surface.frame.cells()[0].clone(); 2],
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
        // Committing reads only the patch; the message stands in for the
        // update that was sent.
        state.commit_sent_frame(PreparedRender::SemanticPatch {
            message: ServerMessage::HealthPong,
            patch: patch.clone(),
        });
        assert!(state.last_pane_surface().is_none());

        // Committing with no baseline at all is also survivable.
        state.commit_sent_frame(PreparedRender::SemanticPatch {
            message: ServerMessage::HealthPong,
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
    fn unchanged_full_render_is_skipped_after_the_delta_cell_scan() {
        let mut state = ClientRenderState::new();
        let surface = test_surface("same");
        let initial = state
            .prepare_pane_surface(surface.clone())
            .expect("initial surface");
        state.commit_sent_frame(initial);

        assert!(state.prepare_pane_surface(surface).is_none());
        assert_eq!(
            state
                .last_pane_surface()
                .expect("committed surface")
                .surface_revision,
            shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(1)
        );
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
            ServerMessage::PaneSurface(surface) if surface.surface_revision == shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(2)
        ));
        state.commit_sent_frame(prepared);
        assert_eq!(
            state
                .last_pane_surface()
                .expect("test precondition")
                .surface_revision,
            shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(2)
        );
    }

    #[test]
    fn dirty_patch_does_not_resend_the_retained_hyperlink_table() {
        let mut state = ClientRenderState::new();
        let mut first = test_surface("abc");
        first
            .frame
            .set_hyperlinks(vec!["https://example.test/".repeat(4096)])
            .expect("no cell links yet");
        let initial = state.prepare_pane_surface(first.clone()).expect("initial");
        let mut decoder = shepr_surface::decode::Decoder::default();
        decoder.decode(initial.message().clone()).expect("baseline");
        state.commit_sent_frame(initial);
        let mut changed = first.frame.cells()[0].clone();
        changed.symbol = "z".into();
        let prepared = state
            .prepare_pane_surface_patch(PaneSurfacePatch {
                boot_id: first.boot_id.clone(),
                projection_revision: first.projection_revision,
                base_surface_revision: first.surface_revision,
                surface_revision: SurfaceRevision::ZERO,
                rows: vec![shepr_protocol::PaneSurfacePatchRow {
                    x: 0,
                    y: 0,
                    cells: vec![changed.clone()],
                }],
                panes: Vec::new(),
                cursor: None,
            })
            .expect("patch");
        let mut bytes = Vec::new();
        shepr_protocol::write_message(&mut bytes, prepared.message()).expect("encode");
        assert!(
            bytes.len() < 1024,
            "retained metadata must not cross the wire"
        );
        let wire = shepr_protocol::read_message(&mut bytes.as_slice()).expect("wire");
        assert!(matches!(
            decoder.decode(wire).expect("patch"),
            DecodedServerMessage::PaneSurfacePatch(_)
        ));
        state.commit_sent_frame(prepared);
        first.surface_revision =
            shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(2);
        first.frame.cells_mut()[0] = changed;
        assert_eq!(state.last_pane_surface(), Some(&first));
        assert_eq!(decoder.current_surface(), Some(first));
    }
}
