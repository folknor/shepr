//! Pane PTY sizes and their settle delay.
//!
//! Layout follows every geometry change at once: the workspace records its
//! new area, and every surface is laid out from it. The panes' terminal grids
//! and PTYs follow a change that may be one step of a continuous gesture (a
//! split border or host window being dragged, ratio commands replayed from a
//! slow link) only once the geometry has held for `PANE_RESIZE_SETTLE`, so the
//! gesture costs each pane one resize and one SIGWINCH instead of one per
//! step. Until then each surface draws the pane's current grid in the new
//! content rect: clipped where the rect shrank, padded with blanks where it
//! grew, with a cursor outside the rect left undrawn.
//!
//! The deferral is kept per workspace, each with its own deadline: a gesture
//! in one workspace never holds back another's settled resize. A workspace's
//! wait restarts only when the pane sizes it waits for change, so reapplying
//! an unchanged target (every workspace is reapplied whenever any client's
//! geometry moves) does not postpone it. When it comes due the panes are
//! sized for the geometry the workspace records then, so a pane that exited,
//! a workspace that closed or a runtime replaced in the meantime needs no
//! bookkeeping here.

use std::collections::HashMap;
use std::time::Instant;

use shepr_core::geometry::PaneGeometry;
use shepr_core::layout::PaneId;
use shepr_protocol::WorkspaceId;

use super::{App, SpawnGeometry};
use crate::limits::PANE_RESIZE_SETTLE;

/// When a geometry application reaches the panes' PTYs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneResizeTiming {
    /// Now: a discrete change (a topology change, a zoom or swap, a client
    /// arriving or taking over a workspace, a screen flip), after which
    /// nothing follows at once.
    Immediate,
    /// Once the geometry has held for `PANE_RESIZE_SETTLE`: a change that may
    /// be one step of a continuous gesture (a client's surface resize, a split
    /// ratio or pane resize command). A workspace laid out for the first time
    /// is still sized at once.
    Settled,
}

/// The pane sizes a workspace's layout gives its visible panes, in layout
/// order: what a deferred resize waits to apply.
type PaneSizes = Vec<(PaneId, PaneGeometry)>;

/// One workspace's wait: the sizes it waits to apply and when it ends.
#[derive(Debug)]
struct PendingResize {
    target: PaneSizes,
    due: Instant,
}

/// The workspaces whose panes wait for their geometry to settle, each with
/// its own deadline.
#[derive(Debug, Default)]
pub(super) struct PendingPaneResizes {
    workspaces: HashMap<WorkspaceId, PendingResize>,
}

impl PendingPaneResizes {
    /// The workspace waits to give its panes `target`. A new or changed target
    /// starts the wait over; the same target keeps the deadline it has.
    fn defer(&mut self, workspace_id: WorkspaceId, target: PaneSizes, now: Instant) {
        if self
            .workspaces
            .get(&workspace_id)
            .is_some_and(|pending| pending.target == target)
        {
            return;
        }
        self.workspaces.insert(
            workspace_id,
            PendingResize {
                target,
                due: now + PANE_RESIZE_SETTLE,
            },
        );
    }

    /// The workspace's panes have their size; it no longer waits.
    fn settle(&mut self, workspace_id: &WorkspaceId) {
        self.workspaces.remove(workspace_id);
    }

    /// The earliest deadline of any waiting workspace.
    fn deadline(&self) -> Option<Instant> {
        self.workspaces.values().map(|pending| pending.due).min()
    }

    /// The waiting workspaces whose deadline has passed; the others keep
    /// waiting.
    fn take_due(&mut self, now: Instant) -> Vec<WorkspaceId> {
        let due = self
            .workspaces
            .iter()
            .filter(|(_, pending)| pending.due <= now)
            .map(|(&workspace_id, _)| workspace_id)
            .collect::<Vec<_>>();
        for workspace_id in &due {
            self.workspaces.remove(workspace_id);
        }
        due
    }
}

impl App {
    /// Records `geometry` as the area the workspace is laid out in and sizes
    /// its visible panes for it, now or once it has settled (`timing`). A
    /// workspace with no recorded geometry yet is always sized now, so a first
    /// layout (spawn, restore, a first client) is never held back. Returns
    /// whether the recorded geometry changed; false too when the workspace is
    /// gone.
    pub(crate) fn apply_workspace_geometry(
        &mut self,
        workspace_id: &WorkspaceId,
        geometry: SpawnGeometry,
        timing: PaneResizeTiming,
    ) -> bool {
        let Some(workspace) = self.state.workspace(workspace_id) else {
            return false;
        };
        let previous = workspace.spawn_geometry();
        self.state.record_workspace_geometry(workspace_id, geometry);
        if timing == PaneResizeTiming::Immediate || previous.is_none() {
            self.resize_workspace_panes(workspace_id);
            self.pending_pane_resizes.settle(workspace_id);
        } else {
            let target = self.laid_out_pane_sizes(workspace_id);
            if self.panes_out_of_size(&target) {
                let now = self.clock.now;
                self.pending_pane_resizes.defer(*workspace_id, target, now);
            } else {
                // Back to the size the panes already have: nothing is owed.
                self.pending_pane_resizes.settle(workspace_id);
            }
        }
        previous != Some(geometry)
    }

    /// When the earliest deferred pane resize comes due.
    pub(crate) fn pane_resize_deadline(&self) -> Option<Instant> {
        self.pending_pane_resizes.deadline()
    }

    /// Sizes the panes of every workspace whose deferred resize has come due,
    /// for the geometry the workspace records now. Returns the workspaces
    /// where some pane changed size; a closed workspace is skipped.
    pub(crate) fn apply_due_pane_resizes(&mut self) -> Vec<WorkspaceId> {
        let now = self.clock.now;
        self.pending_pane_resizes
            .take_due(now)
            .into_iter()
            .filter(|workspace_id| self.resize_workspace_panes(workspace_id))
            .collect()
    }

    /// The size each visible pane of the workspace is laid out for in its
    /// recorded geometry, from the description the surface draws: each pane's
    /// content rect. Empty when the workspace is gone or not laid out yet.
    fn laid_out_pane_sizes(&self, workspace_id: &WorkspaceId) -> PaneSizes {
        let Some(workspace) = self.state.workspace(workspace_id) else {
            return Vec::new();
        };
        let Some(geometry) = workspace.spawn_geometry() else {
            return Vec::new();
        };
        crate::ui::compute_pane_surfaces(
            &self.state,
            &self.terminal_runtimes,
            workspace,
            crate::ui::ratatui_rect(geometry.area),
        )
        .into_iter()
        .filter(|pane| workspace.tree().pane(pane.id).is_some())
        .map(|pane| {
            (
                pane.id,
                PaneGeometry::with_cell(
                    pane.content_rect.width,
                    pane.content_rect.height,
                    geometry.cell,
                ),
            )
        })
        .collect()
    }

    /// Gives every visible pane of the workspace its laid-out size. Returns
    /// whether any pane changed size.
    fn resize_workspace_panes(&mut self, workspace_id: &WorkspaceId) -> bool {
        let mut resized = false;
        for (pane_id, size) in self.laid_out_pane_sizes(workspace_id) {
            if let Some(runtime) = self.terminal_runtimes.get_mut(&pane_id) {
                resized |= runtime.geometry() != size;
                runtime.resize(size);
            }
        }
        resized
    }

    /// Whether some pane of `target` has a runtime of another size.
    fn panes_out_of_size(&self, target: &[(PaneId, PaneGeometry)]) -> bool {
        target.iter().any(|(pane_id, size)| {
            self.terminal_runtimes
                .get(pane_id)
                .is_some_and(|runtime| runtime.geometry() != *size)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> WorkspaceId {
        shepr_test_fixtures::id(name)
    }

    fn target(cols: u16) -> PaneSizes {
        vec![(
            shepr_test_fixtures::fixed_pane_id(1),
            PaneGeometry::cells_only(cols, 24),
        )]
    }

    #[test]
    fn nothing_is_due_before_the_settle_delay() {
        let mut pending = PendingPaneResizes::default();
        let start = Instant::now();
        pending.defer(id("w1"), target(80), start);
        assert_eq!(pending.deadline(), Some(start + PANE_RESIZE_SETTLE));
        assert!(pending.take_due(start + PANE_RESIZE_SETTLE / 2).is_empty());
        assert_eq!(pending.take_due(start + PANE_RESIZE_SETTLE), vec![id("w1")]);
        assert_eq!(pending.deadline(), None);
        assert!(pending.take_due(start + PANE_RESIZE_SETTLE * 2).is_empty());
    }

    #[test]
    fn a_changed_target_restarts_the_wait() {
        let mut pending = PendingPaneResizes::default();
        let start = Instant::now();
        pending.defer(id("w1"), target(80), start);
        let later = start + PANE_RESIZE_SETTLE / 2;
        pending.defer(id("w1"), target(70), later);
        assert!(pending.take_due(start + PANE_RESIZE_SETTLE).is_empty());
        assert_eq!(pending.take_due(later + PANE_RESIZE_SETTLE), vec![id("w1")]);
    }

    #[test]
    fn reapplying_an_unchanged_target_keeps_its_deadline() {
        let mut pending = PendingPaneResizes::default();
        let start = Instant::now();
        pending.defer(id("w1"), target(80), start);
        pending.defer(id("w1"), target(80), start + PANE_RESIZE_SETTLE / 2);
        assert_eq!(pending.deadline(), Some(start + PANE_RESIZE_SETTLE));
        assert_eq!(pending.take_due(start + PANE_RESIZE_SETTLE), vec![id("w1")]);
    }

    #[test]
    fn workspaces_settle_on_their_own_deadlines() {
        let mut pending = PendingPaneResizes::default();
        let start = Instant::now();
        pending.defer(id("w1"), target(80), start);
        // A gesture keeps changing the other workspace's target.
        let mut now = start;
        for cols in [70, 69, 68] {
            now += PANE_RESIZE_SETTLE / 3;
            pending.defer(id("w2"), target(cols), now);
        }
        assert_eq!(pending.deadline(), Some(start + PANE_RESIZE_SETTLE));
        assert_eq!(pending.take_due(start + PANE_RESIZE_SETTLE), vec![id("w1")]);
        assert_eq!(pending.deadline(), Some(now + PANE_RESIZE_SETTLE));
        assert_eq!(pending.take_due(now + PANE_RESIZE_SETTLE), vec![id("w2")]);
    }

    #[test]
    fn settling_a_workspace_drops_only_its_wait() {
        let mut pending = PendingPaneResizes::default();
        let start = Instant::now();
        pending.defer(id("w1"), target(80), start);
        pending.defer(id("w2"), target(80), start + PANE_RESIZE_SETTLE / 2);
        pending.settle(&id("w1"));
        assert_eq!(
            pending.deadline(),
            Some(start + PANE_RESIZE_SETTLE / 2 + PANE_RESIZE_SETTLE)
        );
        pending.settle(&id("w2"));
        assert_eq!(pending.deadline(), None);
        assert!(pending.take_due(start + PANE_RESIZE_SETTLE * 2).is_empty());
    }
}
