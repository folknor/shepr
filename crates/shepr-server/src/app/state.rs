use ratatui::layout::Rect;
use shepr_config::NewTerminalCwd;

use shepr_mux::workspace::Workspace;
use shepr_termio::host_term::cell_size::HostCellSize;
use shepr_termio::host_term::theme::{HostAppearance, TerminalTheme};

pub use shepr_config::theme::Palette;

/// The area a workspace's panes are laid out in and the pixel size of one cell
/// there: everything besides the pane tree that decides what size a pane's PTY
/// gets, and so what a pane spawned into that workspace starts at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpawnGeometry {
    pub(crate) area: Rect,
    pub(crate) cell_size: HostCellSize,
}

impl SpawnGeometry {
    /// The pixel size of one cell, `None` when the host never reported one.
    pub(crate) fn cell_px(&self) -> Option<shepr_core::geometry::CellPx> {
        shepr_core::geometry::CellPx::new(self.cell_size.width_px, self.cell_size.height_px)
    }
}

/// All application state - pure data, no channels or async runtime.
/// Testable without PTYs or a tokio runtime. Live pane runtimes and the
/// channels they report through belong to `App` (`terminal_runtimes`,
/// `pane_spawn_handles`); state reaches a runtime only through the registry
/// it is handed, keyed by terminal id.
pub struct AppState {
    pub(crate) clock_now: std::time::Instant,
    pub terminals:
        std::collections::HashMap<shepr_protocol::TerminalId, shepr_mux::terminal::TerminalState>,
    pub workspaces: Vec<Workspace>,
    /// The session's bookmark: the workspace saved with the session and where
    /// a new client starts. It is set only from the navigation of a client
    /// whose surface is active, and no request acts on it: each client keeps
    /// its own location on the server, and this is not a mirror of any
    /// client's view. Every write goes through `set_bookmark` and
    /// `set_bookmark_index`, which keep `bookmark_position` with it.
    pub bookmark: Option<shepr_protocol::WorkspaceId>,
    /// The index the bookmarked workspace had when it was last seen, kept
    /// current on every change of workspace order. When the bookmarked
    /// workspace vanishes, the workspace now at this index takes its place.
    pub(super) bookmark_position: usize,
    pub should_quit: bool,
    /// The geometry each workspace was last laid out in, keyed by
    /// `WorkspaceId::number()` (no allocation on lookup): the area and cell
    /// size the server last applied to that workspace's PTYs, or spawned its
    /// first pane at. A pane has one PTY size whichever client set it, so this
    /// is session data, not any client's view. Spawn sizing and API geometry
    /// (directional focus, resize steps, layout snapshots) read it, so they
    /// agree with the sizes the panes actually have. Only creation and the
    /// server's geometry path write it (`record_workspace_geometry`).
    pub(crate) workspace_geometry: std::collections::HashMap<usize, SpawnGeometry>,
    /// Immutable settings resolved from the launch configuration.
    pub(crate) settings: AppSettings,
    pub next_agent_state_change_seq: u64,
    /// Last known foreground host terminal appearance.
    pub host_terminal_appearance: Option<HostAppearance>,
    /// True when the foreground host explicitly reported appearance via Mode 2031.
    pub host_terminal_appearance_explicit: bool,
    /// Resolved host terminal default colors for theming embedded panes.
    pub host_terminal_theme: TerminalTheme,
    /// Set when a persisted session snapshot would change.
    pub session_dirty: bool,
    /// Invalidates the shell projection after state changes that can affect chrome.
    pub(crate) shell_projection_revision: u64,
}

/// Runtime-ready settings copied once from the immutable launch config.
///
/// The sidebar settings are deliberately absent: each client draws its
/// sidebar from its own config, and the Git refresh always computes both the
/// branch and ahead/behind whatever any sidebar shows.
#[derive(Debug, Clone)]
pub(crate) struct AppSettings {
    /// Virtual terminal size (columns, rows) used when no client is attached.
    pub(crate) headless_size: shepr_core::geometry::GridSize,
    pub(crate) pane_borders: shepr_config::PaneBordersConfig,
    pub(crate) pane_outer_borders: bool,
    pub(crate) pane_scrollbars: bool,
    pub(crate) pane_gaps: bool,
    pub(crate) show_agent_labels_on_pane_borders: bool,
    /// Expose the focused pane's cursor anchor to the outer terminal even when
    /// the pane requested `?25l`.
    pub(crate) reveal_hidden_cursor_for_cjk_ime: bool,
    /// Restrict cursor reveal to focused panes whose detected agent matches
    /// one of these. An empty vector applies to any focused pane.
    pub(crate) cjk_ime_agents: Vec<shepr_config::ConfigAgent>,
    /// DECSCUSR shape parameter (1-6) for the IME anchor cursor.
    pub(crate) cjk_ime_cursor_shape: u8,
    pub(crate) default_shell: String,
    pub(crate) login_shell: bool,
    pub(crate) new_terminal_cwd: NewTerminalCwd,
    pub(crate) pane_scrollback_limit_bytes: usize,
    pub(crate) palette: Palette,
}

impl AppSettings {
    pub(crate) fn from_config(config: &shepr_config::ValidatedConfig) -> Self {
        let ui = config.ui();
        let experimental = config.experimental();
        let terminal = config.terminal();
        Self {
            headless_size: config.headless_size(),
            pane_borders: ui.pane_borders,
            pane_outer_borders: ui.pane_outer_borders,
            pane_scrollbars: ui.pane_scrollbars,
            pane_gaps: ui.pane_gaps,
            show_agent_labels_on_pane_borders: ui.show_agent_labels_on_pane_borders,
            reveal_hidden_cursor_for_cjk_ime: experimental.reveal_hidden_cursor_for_cjk_ime,
            cjk_ime_agents: experimental.cjk_ime_agents.clone(),
            cjk_ime_cursor_shape: experimental.cjk_ime_cursor_shape.to_decscusr(),
            default_shell: terminal.default_shell.clone(),
            login_shell: terminal.login_shell,
            new_terminal_cwd: terminal.new_cwd.clone(),
            pane_scrollback_limit_bytes: config.advanced().scrollback_limit_bytes,
            palette: config.palette().clone(),
        }
    }

    pub(crate) fn headless_rect(&self) -> Rect {
        Rect::new(
            0,
            0,
            self.headless_size.cols.get(),
            self.headless_size.rows.get(),
        )
    }

    pub(crate) fn pane_geometry_in(&self, area: Rect) -> shepr_mux::workspace::PaneGeometry {
        shepr_mux::workspace::PaneGeometry {
            area,
            pane_borders: self.pane_borders,
            pane_gaps: self.pane_gaps,
            pane_outer_borders: self.pane_outer_borders,
            pane_scrollbars: self.pane_scrollbars,
        }
    }
}

impl AppState {
    /// The current position of the bookmarked workspace, if it is still there.
    pub(crate) fn bookmark_index(&self) -> Option<usize> {
        let id = self.bookmark.as_ref()?;
        self.workspace_index(id)
    }

    /// Bookmarks the workspace at `index` (or nothing), without marking the
    /// session changed: startup and tests seed it this way.
    pub(crate) fn set_bookmark_index(&mut self, index: Option<usize>) {
        self.bookmark = index.and_then(|index| {
            self.workspaces
                .get(index)
                .map(|workspace| workspace.id.clone())
        });
        self.bookmark_position = index.filter(|_| self.bookmark.is_some()).unwrap_or(0);
    }

    /// Bookmarks workspace `id`, the navigation of an active client. Saved
    /// with the session when it moved the bookmark; false when it did not (the
    /// workspace is already bookmarked, or is not a workspace).
    pub(crate) fn set_bookmark(&mut self, id: &shepr_protocol::WorkspaceId) -> bool {
        let Some(index) = self.workspace_index(id) else {
            return false;
        };
        let moved = self.bookmark.as_ref() != Some(id);
        self.bookmark = Some(id.clone());
        self.bookmark_position = index;
        if moved {
            self.mark_session_dirty();
        }
        moved
    }

    /// Brings the bookmark in line with the workspaces after they changed. A
    /// bookmarked workspace still there only has its remembered index
    /// refreshed. One that vanished is replaced by the workspace now at that
    /// index, clamped to the last one, or by nothing when none is left; that
    /// repair schedules a save. Returns whether the bookmark moved.
    pub(crate) fn reconcile_bookmark(&mut self) -> bool {
        let Some(id) = self.bookmark.clone() else {
            return false;
        };
        if let Some(index) = self.workspace_index(&id) {
            self.bookmark_position = index;
            return false;
        }
        let landed = self
            .workspaces
            .len()
            .checked_sub(1)
            .map(|last| self.bookmark_position.min(last));
        self.set_bookmark_index(landed);
        self.mark_session_dirty();
        true
    }

    /// Position of the workspace with `id`.
    pub(crate) fn workspace_index(&self, id: &shepr_protocol::WorkspaceId) -> Option<usize> {
        self.workspaces
            .iter()
            .position(|workspace| &workspace.id == id)
    }

    pub(crate) fn mark_session_dirty(&mut self) {
        self.session_dirty = true;
    }

    pub(crate) fn mark_shell_projection_dirty(&mut self) {
        self.shell_projection_revision = self.shell_projection_revision.saturating_add(1);
    }

    /// The area workspace `ws_idx` is laid out in: where the server last
    /// applied its PTY geometry, or the headless area when it has not yet.
    pub(crate) fn workspace_layout_area(&self, ws_idx: usize) -> Rect {
        self.workspace_area(ws_idx)
            .unwrap_or_else(|| self.settings.headless_rect())
    }

    /// The recorded geometry of workspace `ws_idx`, if one was recorded.
    pub(crate) fn workspace_spawn_geometry(&self, ws_idx: usize) -> Option<SpawnGeometry> {
        let workspace = self.workspaces.get(ws_idx)?;
        self.workspace_geometry.get(&workspace.id.number()).copied()
    }

    /// The recorded layout area of workspace `ws_idx`, if the server has
    /// applied geometry to it.
    pub(crate) fn workspace_area(&self, ws_idx: usize) -> Option<Rect> {
        self.workspace_spawn_geometry(ws_idx)
            .map(|geometry| geometry.area)
    }

    /// Records the geometry the server just applied a workspace's PTYs in, or
    /// spawned its first pane at.
    pub(crate) fn record_workspace_geometry(
        &mut self,
        id: &shepr_protocol::WorkspaceId,
        geometry: SpawnGeometry,
    ) {
        self.workspace_geometry.insert(id.number(), geometry);
    }

    /// Whether some workspace has no recorded geometry yet.
    pub(crate) fn has_workspace_without_area(&self) -> bool {
        self.workspaces
            .iter()
            .any(|workspace| !self.workspace_geometry.contains_key(&workspace.id.number()))
    }

    /// Drops the recorded geometry of workspaces that no longer exist.
    pub(crate) fn retain_live_workspace_geometry(&mut self) {
        let live = self
            .workspaces
            .iter()
            .map(|workspace| workspace.id.number())
            .collect::<std::collections::HashSet<_>>();
        self.workspace_geometry
            .retain(|number, _| live.contains(number));
    }

    /// The configured pane chrome applied to a workspace laid out in `area`.
    pub(crate) fn pane_geometry_in(&self, area: Rect) -> shepr_mux::workspace::PaneGeometry {
        self.settings.pane_geometry_in(area)
    }

    /// The live runtime of `pane_id` in workspace `ws_idx`: the pane's
    /// terminal id looked up in `terminal_runtimes`. `None` when the pane is
    /// not in that workspace or its terminal has no runtime (a restored pane
    /// whose shell failed to start, or one still waiting on agent resume).
    /// This lookup only returns a borrowed runtime; it does not itself probe
    /// the process or perform I/O. App-level code should own any such probes.
    pub(crate) fn runtime_for_pane_in_workspace<'a>(
        &'a self,
        terminal_runtimes: &'a shepr_mux::pane::PaneRuntimeRegistry,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<&'a shepr_mux::pane::PaneRuntime> {
        let terminal_id = self.workspaces.get(ws_idx)?.terminal_id(pane_id)?;
        terminal_runtimes.get(terminal_id)
    }
}

#[cfg(test)]
use crate::test_support::{ValidatedConfigFixture as _, WorkspaceFixture as _};

#[cfg(test)]
use crossterm::event::{KeyCode, KeyModifiers};

#[cfg(test)]
pub fn key_matches(
    key: &crossterm::event::KeyEvent,
    expected_code: KeyCode,
    expected_mods: KeyModifiers,
) -> bool {
    shepr_config::terminal_key_matches_combo(
        &shepr_termio::input::TerminalKey::from(*key),
        (expected_code, expected_mods),
    )
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

#[cfg(test)]
impl AppState {
    /// Chrome geometry of workspace `ws_idx`: its recorded layout area, or the
    /// headless area when geometry has not been applied to it yet.
    pub(crate) fn pane_geometry_for_workspace(
        &self,
        ws_idx: usize,
    ) -> shepr_mux::workspace::PaneGeometry {
        self.pane_geometry_in(self.workspace_layout_area(ws_idx))
    }

    /// Create an AppState for testing - no channels, no PTYs.
    pub fn test_new() -> Self {
        Self {
            clock_now: super::tests::test_clock().now,
            terminals: std::collections::HashMap::new(),
            workspaces: Vec::new(),
            bookmark: None,
            bookmark_position: 0,
            should_quit: false,
            workspace_geometry: std::collections::HashMap::new(),
            settings: AppSettings::from_config(&shepr_config::ValidatedConfig::test_default()),
            next_agent_state_change_seq: 0,
            host_terminal_appearance: None,
            host_terminal_appearance_explicit: false,
            host_terminal_theme: TerminalTheme::default(),
            session_dirty: false,
            shell_projection_revision: 0,
        }
    }

    /// Records `area` as the layout area of every workspace, as if the server
    /// had applied geometry to each of them in it, for a host that reported no
    /// cell size.
    pub fn test_record_all_workspace_areas(&mut self, area: Rect) {
        self.test_record_all_workspace_geometry(SpawnGeometry {
            area,
            cell_size: HostCellSize::default(),
        });
    }

    /// Records `geometry` for every workspace.
    pub(crate) fn test_record_all_workspace_geometry(&mut self, geometry: SpawnGeometry) {
        let ids = self
            .workspaces
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            self.record_workspace_geometry(&id, geometry);
        }
    }

    /// Populate missing `TerminalState` entries for every pane so tests that
    /// read or write terminal metadata don't need to manually create them.
    pub fn ensure_test_terminals(&mut self) {
        use shepr_mux::terminal::TerminalState;
        for ws in &self.workspaces {
            for pane in ws.panes().values() {
                if !self.terminals.contains_key(&pane.attached_terminal_id) {
                    let cwd = ws.identity_cwd.clone();
                    self.terminals.insert(
                        pane.attached_terminal_id.clone(),
                        TerminalState::new(pane.attached_terminal_id.clone(), cwd),
                    );
                }
            }
        }
    }

    pub fn test_with_adversarial_identity_state() -> Self {
        let mut state = Self::test_new();
        state.workspaces = vec![shepr_mux::workspace::Workspace::test_adversarial_identity_state()];
        state.set_bookmark_index(Some(0));
        state.ensure_test_terminals();
        state
    }

    pub fn assert_invariants_for_test(&self) {
        if self.workspaces.is_empty() {
            assert!(
                self.bookmark.is_none(),
                "empty app state must not have a bookmarked workspace"
            );
            return;
        }

        if let Some(bookmark) = &self.bookmark {
            let position = self
                .workspace_index(bookmark)
                .expect("the bookmarked workspace id must resolve");
            assert_eq!(
                position, self.bookmark_position,
                "the bookmark must remember the index it has"
            );
        }

        let mut workspace_ids = std::collections::HashSet::new();
        let mut pane_ids = std::collections::HashSet::new();
        let mut attached_terminal_ids = std::collections::HashSet::new();
        for (ws_idx, ws) in self.workspaces.iter().enumerate() {
            assert!(
                workspace_ids.insert(ws.id.clone()),
                "duplicate workspace id {} at workspace index {}",
                ws.id,
                ws_idx
            );
            ws.assert_invariants_for_test();

            for (pane_id, pane) in ws.panes() {
                assert!(
                    pane_ids.insert(*pane_id),
                    "pane {pane_id:?} appears in more than one workspace"
                );
                assert!(
                    attached_terminal_ids.insert(pane.attached_terminal_id.clone()),
                    "terminal {} is attached to more than one app pane",
                    pane.attached_terminal_id
                );
                assert!(
                    self.terminals.contains_key(&pane.attached_terminal_id),
                    "pane {:?} is attached to missing terminal {}",
                    pane_id,
                    pane.attached_terminal_id
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crossterm::event::KeyEvent;

    #[test]
    fn pane_settings_use_the_resolved_absolute_shell() {
        let env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("pane-resolved-shell");
        let shell = shepr_test_support::fixture::stand_in(scratch.path(), "zsh", &[]);
        env.set("PATH", scratch.path());
        let mut values = shepr_config::Config::default();
        values.terminal.default_shell = "zsh".into();
        let paths = shepr_config::AppPaths::rooted_at(
            scratch.path(),
            Some(scratch.path()),
            Some(scratch.path()),
        );
        let config = shepr_config::ValidatedConfig::from_values(values, None, paths)
            .expect("shell resolves");
        assert_eq!(
            AppSettings::from_config(&config).default_shell,
            shell.to_string_lossy()
        );
    }

    #[test]
    fn an_unrecorded_workspace_is_laid_out_in_the_headless_area() {
        let mut state = AppState::test_new();
        state.settings.headless_size = shepr_core::geometry::GridSize::clamped(132, 41);
        state.settings.pane_scrollbars = false;
        state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("only")];

        assert_eq!(state.workspace_spawn_geometry(0), None);
        assert_eq!(state.workspace_layout_area(0), Rect::new(0, 0, 132, 41));
        assert_eq!(
            state.pane_geometry_for_workspace(0).area,
            Rect::new(0, 0, 132, 41)
        );
        assert_eq!(
            state.pane_geometry_for_workspace(0).sole_pane_size(),
            (41, 132)
        );
    }

    #[test]
    fn a_workspaces_layout_area_is_its_own_recorded_geometry_never_another_workspaces() {
        let mut state = AppState::test_with_adversarial_identity_state();
        state
            .workspaces
            .push(shepr_mux::workspace::Workspace::test_new("second"));
        state.ensure_test_terminals();
        let first_area = Rect::new(0, 0, 97, 33);
        let first_cell = HostCellSize {
            width_px: 9,
            height_px: 18,
        };
        let first_id = state.workspaces[0].id.clone();
        state.record_workspace_geometry(
            &first_id,
            SpawnGeometry {
                area: first_area,
                cell_size: first_cell,
            },
        );

        // The bookmark names the first workspace, and the unrecorded second
        // one does not borrow its area: it falls back to the headless area.
        assert_eq!(state.bookmark_index(), Some(0));
        assert_eq!(state.workspace_layout_area(0), first_area);
        assert_eq!(
            state.workspace_spawn_geometry(0).map(|g| g.cell_size),
            Some(first_cell)
        );
        assert_eq!(state.workspace_area(1), None);
        assert!(state.has_workspace_without_area());
        assert_eq!(
            state.workspace_layout_area(1),
            state.settings.headless_rect()
        );

        // Closed workspaces drop their geometry.
        state.workspaces.truncate(1);
        state.workspace_geometry.insert(
            usize::MAX,
            SpawnGeometry {
                area: Rect::new(0, 0, 61, 17),
                cell_size: HostCellSize::default(),
            },
        );
        state.retain_live_workspace_geometry();
        assert!(!state.has_workspace_without_area());
        assert_eq!(state.workspace_geometry.len(), 1);
    }

    #[test]
    fn the_bookmark_remembers_its_index_and_repairs_by_it() {
        let mut state = AppState::test_new();
        state.workspaces = ["a", "b", "c", "d"]
            .into_iter()
            .map(shepr_mux::workspace::Workspace::test_new)
            .collect();
        let ids: Vec<_> = state.workspaces.iter().map(|w| w.id.clone()).collect();

        assert!(state.set_bookmark(&ids[2]));
        assert!(!state.set_bookmark(&ids[2]), "already bookmarked");
        state.session_dirty = false;

        // An order change refreshes the remembered index and moves nothing.
        let moved = state.workspaces.remove(0);
        state.workspaces.push(moved);
        assert!(!state.reconcile_bookmark());
        assert_eq!(state.bookmark_index(), Some(1));
        assert!(!state.session_dirty);

        // The bookmarked workspace vanishes: the one now at its index takes
        // over, and the repair schedules a save.
        state.workspaces.remove(1);
        assert!(state.reconcile_bookmark());
        assert_eq!(state.bookmark.as_ref(), Some(&ids[3]));
        assert_eq!(state.bookmark_index(), Some(1));
        assert!(state.session_dirty);

        // Past the end it clamps, and with nothing left it is none.
        state.workspaces.truncate(1);
        assert!(state.reconcile_bookmark());
        assert_eq!(state.bookmark_index(), Some(0));
        state.workspaces.clear();
        assert!(state.reconcile_bookmark());
        assert_eq!(state.bookmark, None);
    }

    #[test]
    fn split_spawn_size_is_the_new_panes_content_size_not_the_first_panes_outer_rect() {
        let mut state = AppState::test_new();
        let area = Rect::new(5, 2, 120, 40);
        state.settings.pane_borders = shepr_config::PaneBordersConfig::Always;
        state.settings.pane_scrollbars = true;
        let geometry = state.pane_geometry_in(area);
        assert_eq!(geometry.area, area);

        let (mut layout, root) = shepr_core::layout::TileLayout::new();
        let new_pane = layout
            .split_pane(root, shepr_core::layout::Direction::Horizontal, 0.25)
            .expect("test precondition");

        // Right three quarters (90 cols), minus left+right border and the
        // scrollbar gutter; rows minus top+bottom border.
        assert_eq!(geometry.pane_size(&layout, false, new_pane), Some((38, 87)));
    }

    #[tokio::test]
    async fn runtime_lookup_goes_through_the_registry_by_terminal_id() {
        let mut state = AppState::test_new();
        let ws = shepr_mux::workspace::Workspace::test_new("test");
        let pane_id = ws.root_pane();
        let terminal_id = ws.panes()[&pane_id].attached_terminal_id.clone();
        state.workspaces = vec![ws];
        let mut registry = shepr_mux::pane::PaneRuntimeRegistry::new();

        assert!(
            state
                .runtime_for_pane_in_workspace(&registry, 0, pane_id)
                .is_none()
        );

        registry.insert(
            terminal_id,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b""),
        );
        assert!(
            state
                .runtime_for_pane_in_workspace(&registry, 0, pane_id)
                .is_some()
        );
        assert!(
            state
                .runtime_for_pane_in_workspace(&registry, 1, pane_id)
                .is_none()
        );
        for (_, runtime) in registry.drain() {
            drop(runtime);
        }
    }

    #[test]
    fn adversarial_identity_state_satisfies_app_invariants_after_mutation() {
        let mut state = AppState::test_with_adversarial_identity_state();
        state.assert_invariants_for_test();

        let ws = &mut state.workspaces[0];
        let new_pane = ws.test_split(shepr_core::layout::Direction::Horizontal);
        assert!(ws.public_pane_number(new_pane).is_some());
        state.ensure_test_terminals();

        state.assert_invariants_for_test();
    }

    #[test]
    fn key_matches_requires_exact_modifiers() {
        assert!(key_matches(
            &KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
        ));

        assert!(!key_matches(
            &KeyEvent::new(
                KeyCode::Char('b'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
        ));
    }

    #[test]
    fn key_matches_letters_case_insensitively() {
        assert!(key_matches(
            &KeyEvent::new(KeyCode::Char('B'), KeyModifiers::SHIFT),
            KeyCode::Char('b'),
            KeyModifiers::SHIFT,
        ));
    }

    #[test]
    fn shell_projection_revision_is_explicit_and_monotonic() {
        let mut state = AppState::test_new();
        assert_eq!(state.shell_projection_revision, 0);
        state.mark_shell_projection_dirty();
        state.mark_shell_projection_dirty();
        assert_eq!(state.shell_projection_revision, 2);

        state.shell_projection_revision = u64::MAX;
        state.mark_shell_projection_dirty();
        assert_eq!(state.shell_projection_revision, u64::MAX);
    }
}
