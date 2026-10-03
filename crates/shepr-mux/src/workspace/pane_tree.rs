//! A workspace's pane tree: the layout, the pane records and the operations
//! that keep the two in agreement.

use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use super::{PaneGeometry, Workspace};
use crate::pane::{PaneRuntime, PaneRuntimeRegistry, PaneState};
use crate::terminal::TerminalState;
use shepr_core::layout::{Direction, NavDirection, PaneId, TileLayout};
use shepr_protocol::TerminalId;

/// One pane's state and stable public number. Keeping both in the workspace
/// record makes its pane map the source of pane identity metadata.
pub struct WorkspacePane {
    pub pane_state: PaneState,
    pub public_number: shepr_protocol::PanePublicNumber,
}

impl Deref for WorkspacePane {
    type Target = PaneState;

    fn deref(&self) -> &Self::Target {
        &self.pane_state
    }
}

impl DerefMut for WorkspacePane {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.pane_state
    }
}

impl WorkspacePane {
    pub fn new(pane_state: PaneState, public_number: shepr_protocol::PanePublicNumber) -> Self {
        Self {
            pane_state,
            public_number,
        }
    }
}

/// A split planned on a cloned layout: plain data, no child. The caller
/// launches the pane from `geometry`, `public_id` and the terminal's cwd, then
/// commits this same plan with `commit_new_pane` in the same synchronous handler.
///
/// Only `prepare_split` builds one and its fields are read-only outside the
/// workspace module, so the plan a commit installs is the one a launch read:
/// nothing can pair a launched child with another geometry, cwd or number.
pub struct PreparedSplit {
    pub(super) pane_id: PaneId,
    pub(super) terminal: TerminalState,
    /// The new pane's PTY size in the tiled layout, since a split unzooms.
    pub(super) geometry: shepr_core::geometry::PaneGeometry,
    /// The id exported to the child as `SHEPR_PANE_ID`; its number is also
    /// registered when the split is committed.
    pub(super) public_id: shepr_protocol::PublicPaneId,
    pub(super) prepared_layout: TileLayout,
}

impl PreparedSplit {
    pub fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    /// The new pane's terminal state; its cwd is where the child starts.
    pub fn terminal(&self) -> &TerminalState {
        &self.terminal
    }

    /// The new pane's PTY size in the tiled layout, since a split unzooms.
    pub fn geometry(&self) -> shepr_core::geometry::PaneGeometry {
        self.geometry
    }

    /// The id to export to the child as `SHEPR_PANE_ID`.
    pub fn public_id(&self) -> shepr_protocol::PublicPaneId {
        self.public_id
    }
}

impl Workspace {
    pub fn root_pane(&self) -> PaneId {
        self.root_pane
    }

    pub fn layout(&self) -> &TileLayout {
        &self.layout
    }

    pub fn panes(&self) -> &HashMap<PaneId, WorkspacePane> {
        &self.panes
    }

    pub fn zoomed(&self) -> bool {
        self.zoomed
    }

    /// Whether the layout and the pane records name exactly the same panes,
    /// with the root and the focused pane among them.
    pub(super) fn has_consistent_panes(&self) -> bool {
        let layout_ids = self.layout.pane_ids();
        let layout_set: HashSet<_> = layout_ids.iter().copied().collect();
        layout_ids.len() == layout_set.len()
            && layout_set.len() == self.panes.len()
            && layout_set.contains(&self.root_pane)
            && layout_set.contains(&self.layout.focused())
            && self.panes.keys().all(|id| layout_set.contains(id))
    }

    pub fn contains_pane(&self, pane_id: PaneId) -> bool {
        self.panes.contains_key(&pane_id)
    }

    /// Pane IDs a workspace surface presents, in layout order.
    pub fn visible_pane_ids(&self) -> Vec<PaneId> {
        self.zoomed_pane_id()
            .map_or_else(|| self.layout.pane_ids(), |pane_id| vec![pane_id])
    }

    /// Whether `pane_id` is on screen: in the layout, and the focused pane
    /// when the workspace is zoomed.
    pub fn shows_pane(&self, pane_id: PaneId) -> bool {
        self.panes.contains_key(&pane_id)
            && self
                .zoomed_pane_id()
                .is_none_or(|visible_id| visible_id == pane_id)
    }

    fn zoomed_pane_id(&self) -> Option<PaneId> {
        PaneGeometry::zoomed_pane(&self.layout, self.zoomed)
    }

    pub fn pane_state(&self, pane_id: PaneId) -> Option<&PaneState> {
        self.panes.get(&pane_id).map(|pane| &pane.pane_state)
    }

    pub fn pane_state_mut(&mut self, pane_id: PaneId) -> Option<&mut PaneState> {
        self.panes
            .get_mut(&pane_id)
            .map(|pane| &mut pane.pane_state)
    }

    pub fn terminal_id(&self, pane_id: PaneId) -> Option<&TerminalId> {
        self.panes
            .get(&pane_id)
            .map(|pane| &pane.attached_terminal_id)
    }

    pub fn public_pane_number(&self, pane_id: PaneId) -> Option<shepr_protocol::PanePublicNumber> {
        self.panes.get(&pane_id).map(|pane| pane.public_number)
    }

    pub fn pane_id_for_public_number(
        &self,
        number: shepr_protocol::PanePublicNumber,
    ) -> Option<PaneId> {
        self.panes
            .iter()
            .find_map(|(pane_id, pane)| (pane.public_number == number).then_some(*pane_id))
    }

    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    pub fn focused_pane_id(&self) -> PaneId {
        self.layout.focused()
    }

    /// Zooms or unzooms the workspace. A zoom needs a second pane to hide:
    /// `false`, with the workspace unchanged, when asked to zoom a workspace
    /// of one pane. Unzooming always succeeds.
    pub fn set_zoomed(&mut self, zoomed: bool) -> bool {
        let next = Self::resolved_zoomed(zoomed, self.panes.len(), true);
        if zoomed && !next {
            return false;
        }
        self.zoomed = next;
        true
    }

    /// Apply the workspace zoom rule to a restored pane set. A saved zoom is
    /// retained only when its focused pane survived and there is another pane
    /// for it to hide.
    pub(crate) fn resolved_zoomed(
        requested: bool,
        pane_count: usize,
        saved_focus_survived: bool,
    ) -> bool {
        requested && saved_focus_survived && pane_count > 1
    }

    pub fn focus_pane(&mut self, pane_id: PaneId) -> bool {
        if !self.has_consistent_panes() || !self.panes.contains_key(&pane_id) {
            return false;
        }
        self.layout.focus_pane(pane_id);
        true
    }

    pub fn swap_panes(&mut self, first: PaneId, second: PaneId) -> bool {
        self.has_consistent_panes() && self.layout.swap_panes(first, second)
    }

    pub fn resize_pane(
        &mut self,
        pane_id: PaneId,
        direction: NavDirection,
        delta: shepr_core::layout::RatioDelta,
        area: shepr_core::geometry::Rect,
    ) -> bool {
        self.has_consistent_panes() && self.layout.resize_pane(pane_id, direction, delta, area)
    }

    pub fn set_split_ratio_at(
        &mut self,
        path: &[shepr_core::geometry::SplitBranch],
        ratio: shepr_core::layout::SplitRatio,
    ) -> bool {
        self.has_consistent_panes() && self.layout.set_ratio_at(path, ratio)
    }

    /// Prepare a split without launching a child or changing this workspace.
    /// `None` when `target` is not laid out here, or when the next public
    /// number has no successor, so an exhausted workspace never launches a
    /// child its commit would refuse.
    pub fn prepare_split(
        &self,
        target: PaneId,
        direction: Direction,
        geometry: &PaneGeometry,
        cell: Option<shepr_core::geometry::CellPx>,
        cwd: PathBuf,
        focus_new_pane: bool,
    ) -> Option<PreparedSplit> {
        if self.next_public_pane_number.checked_next().is_none() || !self.contains_pane(target) {
            return None;
        }
        let mut prepared_layout = self.layout.clone();
        // The pane map and the layout tree are separate state; a target the
        // map has but the layout lacks must not produce an unlaid-out pane.
        let new_id =
            prepared_layout.split_pane(target, direction, shepr_core::layout::SplitRatio::EVEN)?;
        // A split unzooms the workspace, so launch against the tiled layout.
        let geometry = geometry
            .pane_spawn_geometry(&prepared_layout, false, new_id, cell)
            .unwrap_or_else(|| geometry.sole_pane_spawn_geometry(cell));
        let terminal = TerminalState::new(TerminalId::alloc(), cwd);
        if focus_new_pane {
            prepared_layout.focus_pane(new_id);
        }
        Some(PreparedSplit {
            pane_id: new_id,
            terminal,
            geometry,
            public_id: shepr_protocol::PublicPaneId::new(&self.id, self.next_public_pane_number),
            prepared_layout,
        })
    }

    /// Installs a prepared split: the new layout, an unzoomed workspace and a
    /// record for the new pane. `false`, with the workspace unchanged, when the
    /// prepared layout is not this layout plus exactly `pane_id`.
    ///
    /// Only the pane-id set (and that the prepared focus is in it) is
    /// verified, not ratios or ordering, and the public number is checked
    /// against the live counter to refuse reuse. Prepare and commit run in one
    /// synchronous handler on the app thread, so nothing can edit the layout
    /// or take a number between them.
    /// Do not add a layout generation; if the two phases ever span an await,
    /// collapse them or add one then.
    pub(super) fn commit_prepared_split(
        &mut self,
        pane_id: PaneId,
        prepared_layout: TileLayout,
        terminal_id: TerminalId,
        public_number: shepr_protocol::PanePublicNumber,
    ) -> bool {
        let current_ids = self.layout.pane_ids();
        let prepared_ids = prepared_layout.pane_ids();
        if public_number < self.next_public_pane_number
            || public_number.checked_next().is_none()
            || !self.has_consistent_panes()
            || self.panes.contains_key(&pane_id)
            || !prepared_ids.contains(&pane_id)
            || prepared_ids.len() != current_ids.len().saturating_add(1)
            || current_ids.iter().any(|id| !prepared_ids.contains(id))
            || !prepared_ids.contains(&prepared_layout.focused())
        {
            return false;
        }

        self.layout = prepared_layout;
        self.set_zoomed(false);
        let pane = WorkspacePane::new(PaneState::new(terminal_id), public_number);
        self.panes.insert(pane_id, pane);
        true
    }

    /// Detaches `pane_id` from the layout. The runtime is left to the caller.
    /// `None` when the pane is the workspace's last one (the workspace itself
    /// must go) or is not in it.
    pub(super) fn detach_pane(&mut self, pane_id: PaneId) -> Option<()> {
        if self.panes.len() <= 1
            || !self.has_consistent_panes()
            || !self.panes.contains_key(&pane_id)
        {
            return None;
        }

        let next_root = self.promoted_root_if_needed(pane_id);

        if !self.layout.close_pane(pane_id) {
            return None;
        }

        self.panes.remove(&pane_id)?;
        self.set_zoomed(false);
        if let Some(next_root) = next_root {
            self.root_pane = next_root;
        }
        Some(())
    }

    fn promoted_root_if_needed(&self, closing: PaneId) -> Option<PaneId> {
        if self.root_pane != closing {
            return None;
        }
        self.layout.pane_ids().into_iter().find(|id| *id != closing)
    }

    pub fn cwd_for_pane(
        &self,
        pane_id: PaneId,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &PaneRuntimeRegistry,
    ) -> Option<PathBuf> {
        let terminal_id = self.terminal_id(pane_id)?;
        super::terminal_cwd(
            terminal_runtimes.get(terminal_id),
            terminals.get(terminal_id),
            super::CwdPurpose::Identity,
        )
    }

    pub fn foreground_cwd_for_pane(
        &self,
        pane_id: PaneId,
        terminal_runtimes: &PaneRuntimeRegistry,
    ) -> Option<PathBuf> {
        let terminal_id = self.terminal_id(pane_id)?;
        terminal_runtimes
            .get(terminal_id)
            .and_then(PaneRuntime::foreground_cwd)
    }
}
