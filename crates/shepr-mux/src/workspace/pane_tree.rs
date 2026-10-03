//! A workspace's pane tree: the layout, the pane records and the operations
//! that keep the two in agreement.

use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use super::{PaneGeometry, PaneSpawnHandles, Workspace};
use crate::pane::{PaneLaunchEnv, PaneRuntime, PaneRuntimeRegistry, PaneState};
use crate::terminal::TerminalState;
use shepr_core::layout::{Direction, NavDirection, PaneId, TileLayout};
use shepr_protocol::TerminalId;

/// One pane's state and stable public number. Keeping both in the workspace
/// record makes its pane map the source of pane identity metadata.
pub struct WorkspacePane {
    pub pane_state: PaneState,
    pub public_number: usize,
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
    pub fn new(pane_state: PaneState) -> Self {
        Self {
            pane_state,
            public_number: 0,
        }
    }
}

pub struct NewPane {
    pub pane_id: PaneId,
    pub terminal: TerminalState,
    pub runtime: PaneRuntime,
    pub prepared_layout: TileLayout,
    /// The public pane number reserved at prepare time. The child's `SHEPR`
    /// pane id was built from it, and the commit registers the pane under it.
    pub public_number: usize,
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
        self.zoomed.then(|| self.layout.focused())
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

    pub fn public_pane_number(&self, pane_id: PaneId) -> Option<usize> {
        self.panes.get(&pane_id).map(|pane| pane.public_number)
    }

    pub fn pane_id_for_public_number(&self, number: usize) -> Option<PaneId> {
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

    pub fn resize_focused_pane(
        &mut self,
        direction: NavDirection,
        delta: f32,
        area: shepr_core::geometry::Rect,
    ) -> bool {
        if !self.has_consistent_panes() {
            return false;
        }
        self.layout.resize_focused(direction, delta, area);
        true
    }

    pub fn resize_pane(
        &mut self,
        pane_id: PaneId,
        direction: NavDirection,
        delta: f32,
        area: shepr_core::geometry::Rect,
    ) -> bool {
        self.has_consistent_panes() && self.layout.resize_pane(pane_id, direction, delta, area)
    }

    pub fn set_split_ratio_at(
        &mut self,
        path: &[shepr_core::geometry::SplitBranch],
        ratio: f32,
    ) -> bool {
        self.has_consistent_panes() && self.layout.set_ratio_at(path, ratio)
    }

    /// Prepare a shell split on a cloned layout and start its runtime. The
    /// returned layout is installed by the workspace command after startup.
    /// Focus moves only when `focus_new_pane` is set.
    #[expect(
        clippy::too_many_arguments,
        reason = "a split threads target, geometry, host context, launch policy, and render hooks"
    )]
    pub(super) fn split_pane_shell(
        &self,
        target: PaneId,
        focus_new_pane: bool,
        direction: Direction,
        geometry: &PaneGeometry,
        cell: Option<shepr_core::geometry::CellPx>,
        cwd: Option<PathBuf>,
        default_cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        public_number: usize,
        spawn: &PaneSpawnHandles,
    ) -> std::io::Result<NewPane> {
        let mut prepared_layout = self.layout.clone();
        let Some(new_id) = prepared_layout.split_pane(target, direction, 0.5) else {
            // `Workspace::split_pane` checks the pane record first. Keep this
            // guard because the pane map and layout tree are separate state;
            // disagreement must not create an unlaid-out pane record.
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "split target pane is not in the layout",
            ));
        };
        // The split un-zooms the workspace (below), so size against the tiled
        // layout.
        let spawn_geometry = geometry
            .pane_spawn_geometry(&prepared_layout, false, new_id, cell)
            .unwrap_or_else(|| geometry.sole_pane_spawn_geometry(cell));
        let actual_cwd = cwd.unwrap_or(default_cwd);
        let runtime = PaneRuntime::spawn(
            new_id,
            spawn_geometry,
            &actual_cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            launch_env,
            &spawn.events,
            &spawn.render_notify,
            &spawn.render_dirty,
            &spawn.pane_teardowns,
        )?;
        let terminal_id = TerminalId::alloc();
        let terminal = TerminalState::new(terminal_id.clone(), actual_cwd);
        if focus_new_pane {
            prepared_layout.focus_pane(new_id);
        }
        Ok(NewPane {
            pane_id: new_id,
            terminal,
            runtime,
            prepared_layout,
            public_number,
        })
    }

    /// Installs a prepared split: the new layout, an unzoomed workspace and a
    /// record for the new pane. `false`, with the workspace unchanged, when the
    /// prepared layout is not this layout plus exactly `pane_id`.
    ///
    /// Only the pane-id set (and that the prepared focus is in it) is
    /// verified, not ratios or ordering, and the public number is not checked
    /// for reuse: prepare and commit run in one synchronous handler on the app
    /// thread, so nothing can edit the layout or take a number between them.
    /// Do not add a layout generation; if the two phases ever span an await,
    /// collapse them or add one then.
    pub(super) fn commit_prepared_split(
        &mut self,
        pane_id: PaneId,
        prepared_layout: TileLayout,
        terminal_id: TerminalId,
        public_number: usize,
    ) -> bool {
        let current_ids = self.layout.pane_ids();
        let prepared_ids = prepared_layout.pane_ids();
        if public_number == 0
            || public_number.checked_add(1).is_none()
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
        let mut pane = WorkspacePane::new(PaneState::new(terminal_id));
        pane.public_number = public_number;
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
        terminal_runtimes
            .get(terminal_id)
            .and_then(PaneRuntime::cwd)
            .or_else(|| {
                terminals
                    .get(terminal_id)
                    .map(|terminal| terminal.cwd().to_path_buf())
            })
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
