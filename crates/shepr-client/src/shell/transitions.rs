//! The cross-domain transitions: each is a fixed sequence of reactions, one per domain,
//! in the order written here. The domains own what each reaction does.

use crate::shell::copy::CopySession;
use crate::shell::endpoints::{ProjectionReset, ProjectionStep};
use crate::shell::input::scroll_lanes::ScrollLanes;
use crate::shell::input::selection::{MouseSelection, PreviousPane};
use crate::shell::ledger::{DropReason, Ledger};
use crate::shell::presentation::surfaces::{Pairing, PaneSurfaces};
use crate::shell::state::{ClientShellMode, ClientShellState};
use shepr_protocol::{ClientShellSnapshot, PaneSurfaceFrame};
use std::sync::Arc;

impl ClientShellState {
    /// Presents `snapshot` of the presented endpoint, or ignores it when it is an older
    /// revision of what is presented. An endpoint switch or a reboot resets the projection
    /// first; the focus, selection, copy and navigation reactions follow.
    pub(in crate::shell) fn apply_active_snapshot(
        &mut self,
        snapshot: &Arc<ClientShellSnapshot>,
        generation: shepr_protocol::ConnectionGeneration,
    ) {
        let presented = self.endpoints.presented().clone();
        let Some(change) =
            self.endpoints
                .active
                .accept(&presented, Arc::clone(snapshot), generation)
        else {
            return;
        };
        // A new connection holds the last presented pair and keeps only a baseline it
        // sent itself: its first surface may arrive before its first snapshot.
        if change.generation_changed {
            self.presentation
                .surfaces
                .snapshot_generation_changed(generation);
        }
        // The presented view describes the last composed frame, which stays on screen until
        // the snapshot's matching surface is composed. Clicks are aimed at that frame, so its
        // hits stay live through the gap: emptying them dropped clicks, made copy-mode entry
        // fail silently, and let a click inside Help or the navigator close it. Targets the
        // new snapshot removed are rejected by the endpoint like any other stale ID. A switch
        // or a reboot is different (IDs can be reused), and the reset drops the view.
        if let ProjectionStep::Replaced(reset) = change.step {
            // A reboot must not turn Enter on a stale preview into focus on a reused ID.
            let preview = self.mode.take_preview();
            // The reset drops everything presented, but not a baseline the incoming
            // connection already sent for this boot: its next patch follows it.
            let mut surfaces = std::mem::take(&mut self.presentation.surfaces);
            surfaces.reset_for_boot(&snapshot.boot_id, generation);
            self.reset_endpoint_projection(reset);
            self.presentation.surfaces = surfaces;
            self.mode.set_preview(preview);
        }
        if change.focused_workspace_changed {
            self.sidebar_scroll.reveal_focused_workspace();
        }
        self.mouse_selection.reconcile_snapshot(snapshot);
        crate::shell::copy::reconcile_snapshot(
            &mut self.copy,
            &mut self.mode,
            &mut self.mouse_selection,
            snapshot,
        );
        // The closure reads the shell while the mode is written, so the candidate is
        // decided first.
        if self.mode.is(ClientShellMode::Navigate) && self.mode.preview().is_none() {
            let target = snapshot
                .focused_workspace_id
                .as_ref()
                .and_then(|id| self.navigation_target(&presented, id));
            self.mode.set_preview(target);
        }
        let pane_exists = |pane_id: &shepr_protocol::PublicPaneId| {
            snapshot.panes.iter().any(|pane| &pane.pane_id == pane_id)
        };
        self.scroll_lanes.retain_panes(pane_exists);
        self.reconcile_pending_workspace_highlight();
        self.pair_surfaces();
    }

    /// Drops everything the shell presented of the previous projection: requests first,
    /// while every feature's state still exists, then each feature in turn. A reboot also
    /// resets the aggregate agent list's scroll, which an endpoint switch keeps because
    /// that list belongs to the client.
    pub(in crate::shell) fn reset_endpoint_projection(&mut self, reset: ProjectionReset) {
        self.drop_all_requests(DropReason::Reset);
        self.presentation.reset_view();
        self.presentation.surfaces = PaneSurfaces::default();
        self.input_leases = Default::default();
        self.pointer.reset_for_projection();
        self.sidebar_scroll.reset_workspaces();
        if reset == ProjectionReset::Rebooted {
            self.sidebar_scroll.reset_agents();
        }
        self.mouse_selection.clear();
        self.scroll_lanes.clear();
        self.notices.reset_endpoint();
        self.endpoint_error.dismiss();
        self.mode.set_preview(None);
        if self.mode.is(ClientShellMode::Copy) {
            self.mode.set(ClientShellMode::Terminal);
        }
        self.pending_workspace_highlight = None;
        self.overlay = None;
        self.endpoints.active.reset();
        self.copy = None;
    }

    /// A full surface from the shown connection `generation`. It becomes the baseline
    /// (the reader enforces order and the shell mirrors it) and is presented once it
    /// pairs with that connection's snapshot, which may arrive after it.
    ///
    /// Routing admits only the shown connection, and generations only grow. This is the
    /// guard behind it: a surface from an older connection than the snapshot or the
    /// baseline must not replace a newer connection's baseline.
    pub(crate) fn receive_pane_surface_from(
        &mut self,
        surface: PaneSurfaceFrame,
        generation: shepr_protocol::ConnectionGeneration,
    ) {
        let newest = self
            .endpoints
            .active
            .generation()
            .max(self.presentation.surfaces.baseline_generation());
        if Some(generation) < newest {
            tracing::warn!(
                ?generation,
                ?newest,
                "dropping a pane surface from an older connection"
            );
            return;
        }
        self.presentation.surfaces.receive(surface, generation);
        self.pair_surfaces();
    }

    /// Pairs the baseline with the snapshot and, when that changes what is presented,
    /// runs the presentation effects over the paired surface.
    fn pair_surfaces(&mut self) {
        let Some(generation) = self.endpoints.active.generation() else {
            return;
        };
        let Some(snapshot) = self.endpoints.active.snapshot() else {
            return;
        };
        let pairing =
            self.presentation
                .surfaces
                .pair(&snapshot.boot_id, snapshot.revision, generation);
        if let Pairing::Presented { previous } = pairing {
            let before = self.mouse_selection.facts_in(previous.as_ref());
            if let Some(surface) = self.presentation.surfaces.paired() {
                surface_presented(
                    &mut self.mouse_selection,
                    &mut self.copy,
                    &mut self.scroll_lanes,
                    &mut self.ledger,
                    before,
                    surface,
                );
            }
        }
    }
}

/// Effects of a change to the presented surface, which the caller stores: invalidates a
/// selection or word gesture whose pane changed size or screen, drops scroll targets the
/// surface shows, and refreshes copy-mode geometry and clamping. It only mints tickets from
/// `ledger`; it never opens, answers or drops a request.
pub(in crate::shell) fn surface_presented(
    selection: &mut MouseSelection,
    copy: &mut Option<CopySession>,
    lanes: &mut ScrollLanes,
    ledger: &mut Ledger,
    before: PreviousPane,
    surface: &PaneSurfaceFrame,
) {
    selection.surface_presented(before, surface);
    for pane in &surface.panes {
        if let Some(scroll) = pane.scroll {
            lanes.shown(
                &pane.pane_id,
                scroll.offset_from_bottom,
                scroll.max_offset_from_bottom,
            );
        }
    }
    crate::shell::copy::surface_presented(copy, selection, lanes, ledger, surface);
}
