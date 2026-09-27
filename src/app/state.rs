#[cfg(test)]
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;
use shepr_config::NewTerminalCwdConfig;

use crate::workspace::PaneChromeInfo as PaneInfo;
use shepr_core::layout::PaneId;

use crate::host_term::theme::{HostAppearance, TerminalTheme};
use crate::workspace::Workspace;

pub use shepr_config::theme::Palette;

/// Geometry for the server-rendered active-tab pane surface.
pub struct ViewState {
    pub terminal_area: Rect,
    pub pane_infos: Vec<PaneInfo>,
}

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

/// One right-hand tab bar segment as last rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabBarStatusSegment {
    Zoom,
    Text(Option<String>),
}

/// All application state - pure data, no channels or async runtime.
/// Testable without PTYs or a tokio runtime. Live pane runtimes and the
/// channels they report through belong to `App` (`terminal_runtimes`,
/// `pane_spawn_handles`); state reaches a runtime only through the registry
/// it is handed, keyed by terminal id.
pub struct AppState {
    pub terminals:
        std::collections::HashMap<shepr_protocol::TerminalId, crate::terminal::TerminalState>,
    /// Terminal ids whose size is currently owned by a direct attach client.
    pub direct_attach_resize_locks: std::collections::HashSet<shepr_protocol::TerminalId>,
    /// Keeps a pane's pre-move public id (`<old workspace>:p<n>`) resolving
    /// after a cross-workspace pane move.
    pub(crate) public_pane_id_aliases:
        std::collections::HashMap<shepr_protocol::PublicPaneId, PaneId>,
    pub workspaces: Vec<Workspace>,
    pub active: Option<shepr_protocol::WorkspaceId>,
    pub(crate) active_tab_id: Option<shepr_protocol::PublicTabId>,
    pub(crate) previous_pane_focus: Option<PaneFocusTarget>,
    pub selected: Option<shepr_protocol::WorkspaceId>,
    pub mode: Mode,
    pub should_quit: bool,
    // Geometry of the most recently computed server pane surface.
    pub view: ViewState,
    // Client focus
    /// Last reported focus state for the outer terminal hosting shepr.
    /// When focus has not been reported, pane focus events default to Gained.
    pub outer_terminal_focus: Option<bool>,
    /// Immutable settings resolved from the launch configuration.
    pub(crate) settings: AppSettings,
    pub next_agent_state_change_seq: u64,
    pub tab_bar_right: Vec<TabBarStatusSegment>,
    pub tab_bar_right_separator: String,
    /// Last known foreground host terminal appearance.
    pub host_terminal_appearance: Option<HostAppearance>,
    /// True when the foreground host explicitly reported appearance via Mode 2031.
    pub host_terminal_appearance_explicit: bool,
    /// Cached detection manifest summaries.
    pub agent_manifest_summaries: Vec<shepr_agent::detect::manifest::AgentManifestSummary>,
    /// Resolved host terminal default colors for theming embedded panes.
    pub host_terminal_theme: TerminalTheme,
    /// Last known foreground host terminal cell size in pixels.
    pub(crate) host_cell_size: crate::host_term::cell_size::HostCellSize,
    /// Set when a persisted session snapshot would change.
    pub session_dirty: bool,
    /// Terminal runtimes that should be shut down by the app/runtime layer
    /// after state has detached their terminal metadata.
    pub(crate) terminal_runtime_shutdowns: Vec<shepr_protocol::TerminalId>,
}

/// Runtime-ready settings copied once from the immutable launch config.
#[derive(Debug, Clone)]
pub(crate) struct AppSettings {
    /// Virtual terminal size (columns, rows) used when no client is attached.
    pub(crate) headless_size: shepr_core::geometry::GridSize,
    pub(crate) sidebar_agents: shepr_config::AgentsSidebarConfig,
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
    pub(crate) new_terminal_cwd: NewTerminalCwdConfig,
    pub(crate) pane_scrollback_limit_bytes: usize,
    pub(crate) palette: Palette,
}

impl AppSettings {
    pub(crate) fn from_config(config: &shepr_config::ValidatedConfig) -> Self {
        Self {
            headless_size: config.headless_size(),
            sidebar_agents: config.ui.sidebar.agents.clone(),
            sidebar_spaces: config.ui.sidebar.spaces.clone(),
            pane_borders: config.ui.pane_borders,
            pane_outer_borders: config.ui.pane_outer_borders,
            pane_scrollbars: config.ui.pane_scrollbars,
            pane_gaps: config.ui.pane_gaps,
            show_agent_labels_on_pane_borders: config.ui.show_agent_labels_on_pane_borders,
            reveal_hidden_cursor_for_cjk_ime: config.experimental.reveal_hidden_cursor_for_cjk_ime,
            cjk_ime_agents: config.experimental.cjk_ime_agents.clone(),
            cjk_ime_cursor_shape: config.experimental.cjk_ime_cursor_shape.to_decscusr(),
            default_shell: config.terminal.default_shell.clone(),
            login_shell: config.terminal.login_shell,
            new_terminal_cwd: config.terminal.new_cwd.clone(),
            pane_scrollback_limit_bytes: config.advanced.scrollback_limit_bytes,
            palette: config.palette().clone(),
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
        self.refresh_active_tab_id();
    }

    pub(crate) fn refresh_active_tab_id(&mut self) {
        self.active_tab_id = self.active_index().and_then(|index| {
            let workspace = self.workspaces.get(index)?;
            let tab = workspace.tabs.get(workspace.active_tab)?;
            Some(shepr_protocol::PublicTabId::new(
                workspace.id.to_string(),
                tab.number,
            ))
        });
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

    pub(crate) fn refresh_agent_manifest_summaries(&mut self) {
        self.agent_manifest_summaries = shepr_agent::detect::manifest::manifest_summaries();
    }

    /// Geometry a new pane's PTY is sized against: the most recently computed
    /// pane surface, or the headless size before any view has been computed
    /// (at startup, or when no client has ever attached).
    pub(crate) fn pane_geometry(&self) -> crate::workspace::PaneGeometry {
        let area = if self.view.terminal_area.is_empty() {
            Rect::new(
                0,
                0,
                self.settings.headless_size.cols.get(),
                self.settings.headless_size.rows.get(),
            )
        } else {
            self.view.terminal_area
        };
        self.pane_geometry_in(area)
    }

    /// The configured pane chrome applied to a tab laid out in `area`.
    pub(crate) fn pane_geometry_in(&self, area: Rect) -> crate::workspace::PaneGeometry {
        crate::workspace::PaneGeometry {
            area,
            pane_borders: self.settings.pane_borders,
            pane_gaps: self.settings.pane_gaps,
            pane_outer_borders: self.settings.pane_outer_borders,
            pane_scrollbars: self.settings.pane_scrollbars,
        }
    }

    /// The live runtime of `pane_id` in workspace `ws_idx`: the pane's
    /// terminal id looked up in `terminal_runtimes`. `None` when the pane is
    /// not in that workspace or its terminal has no runtime (a restored pane
    /// whose shell failed to start, or one still waiting on agent resume).
    pub(crate) fn runtime_for_pane_in_workspace<'a>(
        &'a self,
        terminal_runtimes: &'a crate::pane::PaneRuntimeRegistry,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<&'a crate::pane::PaneRuntime> {
        let terminal_id = self.workspaces.get(ws_idx)?.terminal_id(pane_id)?;
        terminal_runtimes.get(terminal_id)
    }
}

#[cfg(test)]
pub fn key_matches(
    key: &crossterm::event::KeyEvent,
    expected_code: KeyCode,
    expected_mods: KeyModifiers,
) -> bool {
    shepr_config::terminal_key_matches_combo(
        &crate::input::TerminalKey::from(*key),
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
            terminals: std::collections::HashMap::new(),
            direct_attach_resize_locks: std::collections::HashSet::new(),
            public_pane_id_aliases: std::collections::HashMap::new(),
            workspaces: Vec::new(),
            active: None,
            active_tab_id: None,
            previous_pane_focus: None,
            selected: None,
            mode: Mode::Navigate,
            should_quit: false,
            view: ViewState {
                terminal_area: Rect::default(),
                pane_infos: Vec::new(),
            },
            outer_terminal_focus: None,
            settings: AppSettings::from_config(&shepr_config::ValidatedConfig::test_default()),
            next_agent_state_change_seq: 0,
            tab_bar_right: Vec::new(),
            tab_bar_right_separator: " ".into(),
            host_terminal_appearance: None,
            host_terminal_appearance_explicit: false,
            agent_manifest_summaries: Vec::new(),
            host_terminal_theme: TerminalTheme::default(),
            host_cell_size: crate::host_term::cell_size::HostCellSize::default(),
            session_dirty: false,
            terminal_runtime_shutdowns: Vec::new(),
        }
    }

    /// Populate missing `TerminalState` entries for every pane so tests that
    /// read or write terminal metadata don't need to manually create them.
    pub fn ensure_test_terminals(&mut self) {
        use crate::terminal::TerminalState;
        for ws in &self.workspaces {
            for tab in &ws.tabs {
                for pane in tab.panes.values() {
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
    }

    pub fn test_with_adversarial_identity_state() -> Self {
        let mut state = Self::test_new();
        state.workspaces = vec![crate::workspace::Workspace::test_adversarial_identity_state()];
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
                self.active_tab_id.is_none(),
                "empty app state must not have an active tab"
            );
            assert!(
                self.selected.is_none(),
                "empty app state must not have a selected workspace"
            );
            assert!(
                self.public_pane_id_aliases.is_empty(),
                "empty app state must not keep public pane aliases"
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
        let active_workspace = &self.workspaces[active];
        let active_tab = &active_workspace.tabs[active_workspace.active_tab];
        assert_eq!(
            self.active_tab_id.as_ref(),
            Some(&shepr_protocol::PublicTabId::new(
                active_workspace.id.to_string(),
                active_tab.number
            )),
            "active tab id must follow the active workspace tab"
        );
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

            for tab in &ws.tabs {
                for (pane_id, pane) in &tab.panes {
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

        let assert_live_pane = |pane_id: PaneId, context: &str| {
            assert!(
                pane_ids.contains(&pane_id),
                "{context} references missing pane {pane_id:?}"
            );
        };
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
        for (public_id, &pane_id) in &self.public_pane_id_aliases {
            assert_live_pane(pane_id, &format!("public pane alias {public_id}"));
        }
        if let Some(focus) = &self.previous_pane_focus {
            assert_workspace_pane(&focus.workspace_id, focus.pane_id, "previous pane focus");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn split_spawn_size_is_the_new_panes_content_size_not_the_first_panes_outer_rect() {
        let mut state = AppState::test_new();
        state.view.terminal_area = Rect::new(5, 2, 120, 40);
        state.settings.pane_borders = shepr_config::PaneBordersConfig::Always;
        state.settings.pane_scrollbars = true;
        let geometry = state.pane_geometry();
        assert_eq!(geometry.area, state.view.terminal_area);

        let (mut layout, root) = shepr_core::layout::TileLayout::new();
        let new_pane = layout
            .split_pane(root, ratatui::layout::Direction::Horizontal, 0.25)
            .expect("test precondition");

        // Right three quarters (90 cols), minus left+right border and the
        // scrollbar gutter; rows minus top+bottom border.
        assert_eq!(geometry.pane_size(&layout, false, new_pane), Some((38, 87)));
    }

    #[tokio::test]
    async fn runtime_lookup_goes_through_the_registry_by_terminal_id() {
        let mut state = AppState::test_new();
        let ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let terminal_id = ws.tabs[0].panes[&pane_id].attached_terminal_id.clone();
        state.workspaces = vec![ws];
        let mut registry = crate::pane::PaneRuntimeRegistry::new();

        assert!(
            state
                .runtime_for_pane_in_workspace(&registry, 0, pane_id)
                .is_none()
        );

        registry.insert(
            terminal_id,
            crate::pane::PaneRuntime::test_with_screen_bytes(20, 5, b""),
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
            runtime.shutdown();
        }
    }

    #[test]
    fn adversarial_identity_state_satisfies_app_invariants_after_mutation() {
        let mut state = AppState::test_with_adversarial_identity_state();
        state.assert_invariants_for_test();

        let ws = &mut state.workspaces[0];
        let active_public = ws.tabs[ws.active_tab].number;
        assert_ne!(ws.active_tab + 1, active_public);
        let new_pane = ws.test_split(ratatui::layout::Direction::Horizontal);
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
}
