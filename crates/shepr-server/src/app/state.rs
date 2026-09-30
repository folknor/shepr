use ratatui::layout::Rect;
use shepr_config::NewTerminalCwd;

use shepr_core::layout::PaneId;

use shepr_mux::workspace::Workspace;
use shepr_termio::host_term::theme::{HostAppearance, TerminalTheme};

pub use shepr_config::theme::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Navigate,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneFocusTarget {
    pub workspace_id: shepr_protocol::WorkspaceId,
    pub pane_id: PaneId,
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
    /// The session's focused workspace: saved with the session, where a new
    /// client starts, and what an API call without an explicit target acts
    /// on. Each client keeps its own location on the server; this is not a
    /// mirror of any client's view.
    pub active: Option<shepr_protocol::WorkspaceId>,
    pub(crate) previous_pane_focus: Option<PaneFocusTarget>,
    pub selected: Option<shepr_protocol::WorkspaceId>,
    pub mode: Mode,
    pub should_quit: bool,
    /// The area each workspace was last laid out in, keyed by
    /// `WorkspaceId::number()` (no allocation on lookup): where the server
    /// last applied that workspace's PTY geometry. A pane has one PTY size
    /// whichever client set it, so this is session data, not any client's
    /// view. Spawn sizing and API geometry (directional focus, resize steps,
    /// layout snapshots) read it, so they agree with the sizes the panes
    /// actually have. Only the server's geometry path writes it
    /// (`record_workspace_area`).
    pub(crate) workspace_areas: std::collections::HashMap<usize, Rect>,
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
#[derive(Debug, Clone)]
pub(crate) struct AppSettings {
    /// Virtual terminal size (columns, rows) used when no client is attached.
    pub(crate) headless_size: shepr_core::geometry::GridSize,
    pub(crate) sidebar_spaces: shepr_config::SpacesSidebarConfig,
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
            sidebar_spaces: ui.sidebar.spaces.clone(),
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
    pub(crate) fn active_index(&self) -> Option<usize> {
        let id = self.active.as_ref()?;
        self.workspaces
            .iter()
            .position(|workspace| &workspace.id == id)
    }

    pub(crate) fn selected_index(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.workspaces
            .iter()
            .position(|workspace| &workspace.id == id)
    }

    pub(crate) fn set_active_index(&mut self, index: Option<usize>) {
        self.active = index.and_then(|index| {
            self.workspaces
                .get(index)
                .map(|workspace| workspace.id.clone())
        });
        if self.selected.is_none() {
            self.selected = self.active.clone();
        }
    }

    /// Position of the workspace with `id`.
    pub(crate) fn workspace_index(&self, id: &shepr_protocol::WorkspaceId) -> Option<usize> {
        self.workspaces
            .iter()
            .position(|workspace| &workspace.id == id)
    }

    pub(crate) fn set_selected_index(&mut self, index: Option<usize>) {
        self.selected = index.and_then(|index| {
            self.workspaces
                .get(index)
                .map(|workspace| workspace.id.clone())
        });
    }
    pub(crate) fn mark_session_dirty(&mut self) {
        self.session_dirty = true;
    }

    pub(crate) fn mark_shell_projection_dirty(&mut self) {
        self.shell_projection_revision = self.shell_projection_revision.saturating_add(1);
    }

    /// Geometry a pane with no laid-out workspace yet (a new workspace) is
    /// spawned against: the area of the workspace the session is focused on,
    /// or the headless area before geometry has been applied to it. A
    /// client-shell request makes its own workspace the session's focus
    /// before it runs, so a client's new workspace starts at that client's
    /// size.
    pub(crate) fn pane_geometry(&self) -> shepr_mux::workspace::PaneGeometry {
        self.pane_geometry_in(self.default_layout_area())
    }

    /// Geometry of workspace `ws_idx`: its recorded layout area, or the
    /// default one when geometry has not been applied to it yet.
    pub(crate) fn pane_geometry_for_workspace(
        &self,
        ws_idx: usize,
    ) -> shepr_mux::workspace::PaneGeometry {
        self.pane_geometry_in(self.workspace_layout_area(ws_idx))
    }

    /// The area workspace `ws_idx` is laid out in: where the server last
    /// applied its PTY geometry, or the default layout area when it has not
    /// yet.
    pub(crate) fn workspace_layout_area(&self, ws_idx: usize) -> Rect {
        self.workspace_area(ws_idx)
            .unwrap_or_else(|| self.default_layout_area())
    }

    /// The recorded layout area of workspace `ws_idx`, if the server has
    /// applied geometry to it.
    pub(crate) fn workspace_area(&self, ws_idx: usize) -> Option<Rect> {
        let workspace = self.workspaces.get(ws_idx)?;
        self.workspace_areas.get(&workspace.id.number()).copied()
    }

    /// Records the area the server just applied a workspace's PTY geometry in.
    pub(crate) fn record_workspace_area(&mut self, id: &shepr_protocol::WorkspaceId, area: Rect) {
        self.workspace_areas.insert(id.number(), area);
    }

    /// Whether some workspace has had no geometry applied yet.
    pub(crate) fn has_workspace_without_area(&self) -> bool {
        self.workspaces
            .iter()
            .any(|workspace| !self.workspace_areas.contains_key(&workspace.id.number()))
    }

    /// Drops the recorded areas of workspaces that no longer exist.
    pub(crate) fn retain_live_workspace_areas(&mut self) {
        let live = self
            .workspaces
            .iter()
            .map(|workspace| workspace.id.number())
            .collect::<std::collections::HashSet<_>>();
        self.workspace_areas
            .retain(|number, _| live.contains(number));
    }

    /// The area of the workspace the session is focused on, or the headless
    /// area.
    fn default_layout_area(&self) -> Rect {
        self.active_index()
            .and_then(|ws_idx| self.workspace_area(ws_idx))
            .unwrap_or_else(|| self.settings.headless_rect())
    }

    /// The configured pane chrome applied to a workspace laid out in `area`.
    pub(crate) fn pane_geometry_in(&self, area: Rect) -> shepr_mux::workspace::PaneGeometry {
        self.settings.pane_geometry_in(area)
    }

    /// The live runtime of `pane_id` in workspace `ws_idx`: the pane's
    /// terminal id looked up in `terminal_runtimes`. `None` when the pane is
    /// not in that workspace or its terminal has no runtime (a restored pane
    /// whose shell failed to start, or one still waiting on agent resume).
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
    /// Create an AppState for testing - no channels, no PTYs.
    pub fn test_new() -> Self {
        Self {
            clock_now: super::tests::test_clock().now,
            terminals: std::collections::HashMap::new(),
            workspaces: Vec::new(),
            active: None,
            previous_pane_focus: None,
            selected: None,
            mode: Mode::Navigate,
            should_quit: false,
            workspace_areas: std::collections::HashMap::new(),
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
    /// had applied geometry to each of them in it.
    pub fn test_record_all_workspace_areas(&mut self, area: Rect) {
        let ids = self
            .workspaces
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            self.record_workspace_area(&id, area);
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
        state.set_active_index(Some(0));
        state.set_selected_index(Some(0));
        state.ensure_test_terminals();
        state
    }

    pub fn assert_invariants_for_test(&self) {
        if self.workspaces.is_empty() {
            assert!(
                self.active.is_none(),
                "empty app state must not have an active workspace"
            );
            assert!(
                self.selected.is_none(),
                "empty app state must not have a selected workspace"
            );
            assert!(
                self.previous_pane_focus.is_none(),
                "empty app state must not keep previous pane focus"
            );
            return;
        }

        assert!(
            self.selected_index().is_some(),
            "selected workspace id must resolve"
        );
        let active = self
            .active_index()
            .expect("non-empty app state must have active workspace");
        assert!(
            active < self.workspaces.len(),
            "active workspace {} out of bounds for {} workspaces",
            active,
            self.workspaces.len()
        );

        let mut workspace_ids = std::collections::HashSet::new();
        let mut workspace_id_to_idx = std::collections::HashMap::new();
        let mut pane_ids = std::collections::HashSet::new();
        let mut attached_terminal_ids = std::collections::HashSet::new();
        for (ws_idx, ws) in self.workspaces.iter().enumerate() {
            assert!(
                workspace_ids.insert(ws.id.clone()),
                "duplicate workspace id {} at workspace index {}",
                ws.id,
                ws_idx
            );
            workspace_id_to_idx.insert(ws.id.clone(), ws_idx);
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

        let assert_workspace_pane = |workspace_id: &shepr_protocol::WorkspaceId,
                                     pane_id: PaneId,
                                     context: &str| {
            let ws_idx = workspace_id_to_idx
                .get(workspace_id)
                .copied()
                .unwrap_or_else(|| panic!("{context} references missing workspace {workspace_id}"));
            assert!(
                self.workspaces[ws_idx].pane_state(pane_id).is_some(),
                "{context} references pane {pane_id:?} outside workspace {workspace_id}"
            );
        };
        if let Some(focus) = &self.previous_pane_focus {
            assert_workspace_pane(&focus.workspace_id, focus.pane_id, "previous pane focus");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crossterm::event::KeyEvent;

    #[test]
    fn pane_geometry_uses_headless_size_before_first_view() {
        let mut state = AppState::test_new();
        state.settings.headless_size = shepr_core::geometry::GridSize::clamped(132, 41);
        state.settings.pane_scrollbars = false;

        assert_eq!(state.pane_geometry().area, Rect::new(0, 0, 132, 41));
        assert_eq!(state.pane_geometry().sole_pane_size(), (41, 132));
    }

    #[test]
    fn pane_geometry_follows_the_focused_workspaces_recorded_area() {
        let mut state = AppState::test_with_adversarial_identity_state();
        state
            .workspaces
            .push(shepr_mux::workspace::Workspace::test_new("second"));
        state.ensure_test_terminals();
        let focused_area = Rect::new(0, 0, 97, 33);
        let background_area = Rect::new(0, 0, 61, 17);
        state.test_record_all_workspace_areas(background_area);
        let focused_id = state.workspaces[0].id.clone();
        state.record_workspace_area(&focused_id, focused_area);

        assert_eq!(state.pane_geometry().area, focused_area);
        assert_eq!(state.workspace_layout_area(1), background_area);
        assert_eq!(state.pane_geometry_for_workspace(1).area, background_area);

        // A workspace the server has not laid out yet starts at the focused
        // area.
        state.workspace_areas.clear();
        state.record_workspace_area(&focused_id, focused_area);
        assert!(state.has_workspace_without_area());
        assert_eq!(state.workspace_area(1), None);
        assert_eq!(state.workspace_layout_area(1), focused_area);

        // Closed workspaces drop their areas.
        state.workspaces.truncate(1);
        state.test_record_all_workspace_areas(focused_area);
        state.workspace_areas.insert(usize::MAX, background_area);
        state.retain_live_workspace_areas();
        assert!(!state.has_workspace_without_area());
        assert!(
            state
                .workspace_areas
                .values()
                .all(|area| *area == focused_area)
        );
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
