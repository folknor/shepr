// The reducers live beneath this module so they reach its private fields;
// everything else in `app` goes through accessors and named reducers.
pub(crate) mod actions;

use shepr_config::NewTerminalCwd;
use shepr_core::geometry::Rect;
use shepr_protocol::WorkspaceId;

use shepr_mux::workspace::{PaneRef, SpawnGeometry, Workspace, WorkspaceSet};
use shepr_term::host::{HostAppearance, TerminalTheme};

/// What a host terminal has told the server about its light or dark
/// appearance, and how it was learned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum HostAppearanceReport {
    /// Nothing reported yet.
    #[default]
    Unknown,
    /// Derived from the host's reported background colour.
    Inferred(HostAppearance),
    /// Reported by the host itself (Mode 2031). A colour-derived guess never
    /// replaces it.
    Explicit(HostAppearance),
}

impl HostAppearanceReport {
    pub(crate) fn appearance(self) -> Option<HostAppearance> {
        match self {
            Self::Unknown => None,
            Self::Inferred(appearance) | Self::Explicit(appearance) => Some(appearance),
        }
    }

    pub(crate) fn is_explicit(self) -> bool {
        matches!(self, Self::Explicit(_))
    }
}

/// The session's data: its workspaces (with their panes and terminals) and
/// the presentation facts every client shares, plus the bookkeeping the App
/// drains (`lifecycle_authority_dirty`, `session_dirty`). Live runtimes are
/// the App's (`terminal_runtimes`, `pane_launcher`); reducers here take
/// runtime observations as arguments instead of probing processes or the
/// filesystem, so the state is testable without PTYs or a tokio runtime.
///
/// Everything is addressed by identity: a workspace by its `WorkspaceId`, a
/// pane by its `PaneId`. Positions survive only where order is the subject
/// (moving a workspace, the bookmark, the sidebar order). The fields are
/// private to this module and its reducers (`actions`); the rest of `app`
/// reads through accessors and commits changes through named reducers.
pub(crate) struct AppState {
    /// The session's workspaces in display order, with the allocator their IDs
    /// come from, the bookmark (the workspace saved with the session and where
    /// a new client starts; set only from the navigation of a client whose
    /// surface is active, and not a mirror of any client's view) and, through
    /// each workspace, the geometry its PTYs were last laid out in and the
    /// panes with their terminal state.
    workspaces: WorkspaceSet,
    /// Immutable settings resolved from the launch configuration.
    settings: AppSettings,
    next_agent_state_change_seq: shepr_agent::StateChangeSeq,
    /// Panes whose terminal ownership an update touched since `App` last
    /// mirrored full-lifecycle authority into their runtimes. It holds ids, not
    /// values: the drain reads the live authority, so a change no mutation
    /// reported is still delivered. A set, so it stays bounded by the pane
    /// count when no `App` drains it. `App` looks the runtime up by the pane.
    lifecycle_authority_dirty: std::collections::HashSet<shepr_core::layout::PaneId>,
    /// Last known foreground host terminal appearance and how it was learned.
    host_terminal_appearance: HostAppearanceReport,
    /// Resolved host terminal default colors for theming embedded panes.
    host_terminal_theme: TerminalTheme,
    /// Set when a persisted session snapshot would change.
    session_dirty: bool,
    /// Invalidates the shell projection after state changes that can affect chrome.
    shell_projection_revision: shepr_protocol::ProjectionRevision,
    /// The session saver's last reported condition, which every projection
    /// carries. The saver owns the policy; this is its projected copy.
    session_save_status: shepr_protocol::SessionSaveStatus,
}

/// Runtime-ready settings copied once from the immutable launch config.
///
/// The sidebar settings are deliberately absent: each client draws its
/// sidebar from its own config, and the Git refresh always computes both the
/// branch and ahead/behind whatever any sidebar shows.
///
/// The pane launch settings (shell, login shell, scrollback limit) are absent
/// too: `App::open` hands them from the validated config straight to
/// the `PaneLauncher`, their one holder. A copy here would be a second source
/// that a test could change without the launcher noticing.
#[derive(Debug, Clone)]
pub(crate) struct AppSettings {
    /// Virtual terminal size (columns, rows) used when no client is attached.
    pub(crate) headless_size: shepr_core::geometry::GridSize,
    pub(crate) pane_scrollbars: bool,
    pub(crate) pane_gaps: bool,
    /// Expose the focused pane's cursor anchor to the outer terminal even when
    /// the pane requested `?25l`.
    pub(crate) reveal_hidden_cursor_for_cjk_ime: bool,
    /// Restrict cursor reveal to focused panes with a matching detected agent.
    pub(crate) cjk_ime_agents: AgentFilter,
    /// Resolved once to the protocol cursor shape used by surface rendering.
    pub(crate) cjk_ime_cursor_shape: shepr_protocol::CursorShapeParam,
    pub(crate) new_terminal_cwd: NewTerminalCwd,
}

impl AppSettings {
    pub(crate) fn from_config(config: &shepr_config::ValidatedServerConfig) -> Self {
        let ui = config.ui();
        let experimental = config.experimental();
        let terminal = config.terminal();
        Self {
            headless_size: config.headless_size(),
            pane_scrollbars: ui.pane_scrollbars,
            pane_gaps: ui.pane_gaps,
            reveal_hidden_cursor_for_cjk_ime: experimental.reveal_hidden_cursor_for_cjk_ime,
            cjk_ime_agents: AgentFilter::from_config(&experimental.cjk_ime_agents),
            cjk_ime_cursor_shape: shepr_protocol::CursorShapeParam::from_decscusr(
                experimental.cjk_ime_cursor_shape.to_decscusr(),
            ),
            new_terminal_cwd: terminal.new_cwd.clone(),
        }
    }

    pub(crate) fn headless_rect(&self) -> Rect {
        self.headless_size.rect()
    }

    pub(crate) fn chrome_in(&self, area: Rect) -> shepr_mux::workspace::WorkspaceChrome {
        shepr_mux::workspace::WorkspaceChrome {
            area,
            pane_gaps: self.pane_gaps,
            pane_scrollbars: self.pane_scrollbars,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AgentFilter(AgentFilterKind);

#[derive(Debug, Clone)]
enum AgentFilterKind {
    Any,
    Only(Vec<shepr_config::ConfigAgent>),
}

impl AgentFilter {
    pub(crate) fn from_config(agents: &[shepr_config::ConfigAgent]) -> Self {
        if agents.is_empty() {
            Self(AgentFilterKind::Any)
        } else {
            Self(AgentFilterKind::Only(agents.to_vec()))
        }
    }

    pub(crate) fn includes(&self, agent: Option<shepr_config::ConfigAgent>) -> bool {
        match &self.0 {
            AgentFilterKind::Any => true,
            AgentFilterKind::Only(agents) => agent.is_some_and(|agent| agents.contains(&agent)),
        }
    }
}

impl AppState {
    /// State around `workspaces` (restore's output, or an empty set), with
    /// nothing marked dirty.
    pub(crate) fn new(
        settings: AppSettings,
        workspaces: WorkspaceSet,
        host_theme: TerminalTheme,
    ) -> Self {
        Self {
            workspaces,
            settings,
            next_agent_state_change_seq: shepr_agent::StateChangeSeq::NEVER,
            lifecycle_authority_dirty: std::collections::HashSet::new(),
            host_terminal_appearance: HostAppearanceReport::Unknown,
            host_terminal_theme: host_theme,
            session_dirty: false,
            shell_projection_revision: shepr_protocol::ProjectionRevision::ZERO,
            session_save_status: shepr_protocol::SessionSaveStatus::Ready,
        }
    }

    /// Observes the persistence worker's policy for the shared projection.
    /// Runtime policy stays with the saver; this is its last reported state.
    pub(crate) fn record_session_save_status(&mut self, status: shepr_protocol::SessionSaveStatus) {
        if self.session_save_status != status {
            self.session_save_status = status;
            self.mark_shell_projection_dirty();
        }
    }

    pub(crate) fn session_save_status(&self) -> shepr_protocol::SessionSaveStatus {
        self.session_save_status
    }

    /// Saturation is a permanent cache miss, so exhaustion cannot hide a
    /// later mutation behind a revision a cache has already retained.
    pub(crate) fn shell_projection_is_current(
        &self,
        revision: shepr_protocol::ProjectionRevision,
    ) -> bool {
        self.shell_projection_revision.checked_next().is_some()
            && revision == self.shell_projection_revision
    }

    pub(crate) fn record_host_appearance(&mut self, report: HostAppearanceReport) -> bool {
        if self.host_terminal_appearance == report {
            return false;
        }
        self.host_terminal_appearance = report;
        true
    }

    pub(crate) fn record_host_theme(&mut self, theme: TerminalTheme) -> bool {
        if theme.is_empty() || self.host_terminal_theme == theme {
            return false;
        }
        self.host_terminal_theme = theme;
        self.mark_session_dirty();
        true
    }

    pub(crate) fn sync_terminal_titles(
        &mut self,
        observations: impl IntoIterator<Item = (shepr_core::layout::PaneId, Option<String>)>,
    ) -> bool {
        let mut projection_changed = false;
        for (pane_id, title) in observations {
            let Some(record) = self.workspaces.pane_mut(pane_id) else {
                continue;
            };
            let terminal = record.terminal_mut();
            let has_effective_agent = terminal.ownership().effective_agent().is_some();
            let change = terminal.set_terminal_title(title);
            projection_changed |=
                has_effective_agent && (change.raw_changed || change.stripped_changed);
        }
        if projection_changed {
            self.mark_shell_projection_dirty();
        }
        projection_changed
    }

    /// The session's workspaces, their order, IDs and bookmark.
    pub(crate) fn workspaces(&self) -> &WorkspaceSet {
        &self.workspaces
    }

    /// Reserves a workspace identity without exposing the mutable set to
    /// launch orchestration. Nothing is projected until creation commits.
    pub(crate) fn prepare_workspace(
        &mut self,
        cwd: &shepr_core::absolute_path::AbsolutePath,
    ) -> Option<shepr_mux::workspace::PreparedWorkspace> {
        self.workspaces.prepare_workspace(cwd)
    }

    /// The pane `pane_id` and the workspace that owns it; `None` for a closed
    /// or unknown pane, including late events.
    pub(crate) fn pane(&self, pane_id: shepr_core::layout::PaneId) -> Option<PaneRef<'_>> {
        self.workspaces.pane(pane_id)
    }

    /// The workspace `id` names; `None` for a closed or unknown workspace.
    pub(crate) fn workspace(&self, id: &WorkspaceId) -> Option<&Workspace> {
        self.workspaces.get(id)
    }

    /// The pane `public_id` names, with its workspace; `None` for a closed or
    /// unknown pane. Only the exact stable id resolves: positional forms
    /// (`w_N`, bare `N`) never parse to an id, so a mistyped or index-style id
    /// fails rather than silently targeting whichever workspace sits at that
    /// position.
    pub(crate) fn resolve_pane(
        &self,
        public_id: &shepr_protocol::PublicPaneId,
    ) -> Option<PaneRef<'_>> {
        self.workspaces.resolve(public_id)
    }

    /// The workspace that holds `pane_id`, for mutation.
    fn workspace_of_mut(&mut self, pane_id: shepr_core::layout::PaneId) -> Option<&mut Workspace> {
        let id = self.workspaces.pane(pane_id)?.workspace().id();
        self.workspaces.get_mut(&id)
    }

    /// The settings resolved from the launch configuration.
    pub(crate) fn settings(&self) -> &AppSettings {
        &self.settings
    }

    /// Resolved host terminal default colors for theming embedded panes.
    pub(crate) fn host_terminal_theme(&self) -> TerminalTheme {
        self.host_terminal_theme
    }

    /// Last known foreground host terminal appearance.
    pub(crate) fn host_terminal_appearance(&self) -> Option<HostAppearance> {
        self.host_terminal_appearance.appearance()
    }

    /// The revision of the shell projection, which every state change that can
    /// affect chrome advances.
    pub(crate) fn shell_projection_revision(&self) -> shepr_protocol::ProjectionRevision {
        self.shell_projection_revision
    }

    /// Whether a persisted session snapshot would change.
    pub(crate) fn session_dirty(&self) -> bool {
        self.session_dirty
    }

    /// Reads and clears the session-dirty flag: the session saver's one way to
    /// claim the change it is about to save.
    pub(crate) fn take_session_dirty(&mut self) -> bool {
        std::mem::take(&mut self.session_dirty)
    }

    /// The panes whose terminal ownership changed since the last drain; the
    /// App mirrors their lifecycle authority into their runtimes.
    pub(crate) fn drain_lifecycle_authority_dirty(&mut self) -> Vec<shepr_core::layout::PaneId> {
        self.lifecycle_authority_dirty.drain().collect()
    }

    /// The current position of the bookmarked workspace, if it is still there.
    pub(crate) fn bookmark_index(&self) -> Option<usize> {
        self.workspaces.bookmark_index()
    }

    /// Bookmarks workspace `id`, the navigation of an active client. Saved
    /// with the session when it moved the bookmark; false when it did not (the
    /// workspace is already bookmarked, or is not a workspace).
    pub(crate) fn set_bookmark(&mut self, id: &WorkspaceId) -> bool {
        let moved = self.workspaces.set_bookmark(id);
        if moved {
            self.mark_session_dirty();
        }
        moved
    }

    /// Records that persisted session data changed. App owns translating this
    /// state signal into one debounced save per headless loop pass.
    pub(crate) fn mark_session_dirty(&mut self) {
        self.session_dirty = true;
    }

    pub(crate) fn mark_shell_projection_dirty(&mut self) {
        // Cache users must use shell_projection_is_current: saturation forces
        // a rebuild instead of panicking in the event loop.
        self.shell_projection_revision = self
            .shell_projection_revision
            .checked_next()
            .unwrap_or(self.shell_projection_revision);
    }

    /// The area `workspace` is laid out in: the geometry the server last
    /// recorded for it, or the headless area when it has none yet.
    pub(crate) fn layout_area(&self, workspace: &Workspace) -> Rect {
        workspace
            .spawn_geometry()
            .map_or_else(|| self.settings.headless_rect(), |geometry| geometry.area)
    }

    /// Records the geometry a workspace is laid out in (its panes' PTYs
    /// follow at once or once it settles; see `App::apply_workspace_geometry`),
    /// or spawned its first pane at.
    pub(crate) fn record_workspace_geometry(&mut self, id: &WorkspaceId, geometry: SpawnGeometry) {
        if let Some(workspace) = self.workspaces.get_mut(id) {
            workspace.record_spawn_geometry(geometry);
        }
    }

    /// The terminal state of `pane_id`, wherever the pane lives. The pane's
    /// record owns it. `None` for a closed or unknown pane, including late
    /// events.
    pub(crate) fn terminal(
        &self,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<&shepr_mux::terminal::TerminalState> {
        Some(self.workspaces.pane(pane_id)?.terminal())
    }
}

#[cfg(test)]
use crate::test_support::{ValidatedServerConfigFixture as _, WorkspaceFixture as _};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

#[cfg(test)]
impl AppState {
    pub(crate) fn test_workspaces_mut(&mut self) -> &mut WorkspaceSet {
        &mut self.workspaces
    }

    /// Bookmarks the workspace at `index` (or nothing), without marking the
    /// session changed: tests seed it this way.
    pub(crate) fn seed_bookmark_index(&mut self, index: Option<usize>) {
        self.workspaces.seed_bookmark_index(index);
    }

    /// Create an AppState for testing - no channels, no PTYs.
    pub(crate) fn test_new() -> Self {
        Self::new(
            AppSettings::from_config(&shepr_config::ValidatedServerConfig::test_default()),
            WorkspaceSet::new(),
            TerminalTheme::default(),
        )
    }

    /// The settings, for a test that changes how the chrome is drawn.
    pub(crate) fn settings_mut(&mut self) -> &mut AppSettings {
        &mut self.settings
    }

    /// Whether the foreground host explicitly reported its appearance.
    pub(crate) fn host_terminal_appearance_explicit(&self) -> bool {
        self.host_terminal_appearance.is_explicit()
    }

    /// Clears the session-dirty flag, as a save would.
    pub(crate) fn test_clear_session_dirty(&mut self) {
        self.session_dirty = false;
    }

    /// The fixture workspace at display position `index`; a test names a
    /// fixture by where it put it. Panics when there is none.
    pub(crate) fn ws(&self, index: usize) -> &Workspace {
        &self.workspaces.as_slice()[index]
    }

    /// The fixture workspace at display position `index`, for mutation.
    /// Panics when there is none.
    pub(crate) fn ws_mut(&mut self, index: usize) -> &mut Workspace {
        let id = self.ws(index).id();
        self.workspaces
            .get_mut(&id)
            .expect("a fixture workspace at this position")
    }

    /// Records `area` as the layout area of every workspace, as if the server
    /// had applied geometry to each of them in it, for a host that reported no
    /// cell size.
    pub(crate) fn test_record_all_workspace_areas(&mut self, area: ratatui::layout::Rect) {
        self.test_record_all_workspace_geometry(SpawnGeometry {
            area: crate::ui::core_rect(area),
            cell: None,
        });
    }

    /// Records `geometry` for every workspace.
    pub(crate) fn test_record_all_workspace_geometry(&mut self, geometry: SpawnGeometry) {
        let ids = self
            .workspaces
            .iter()
            .map(Workspace::id)
            .collect::<Vec<_>>();
        for id in ids {
            self.record_workspace_geometry(&id, geometry);
        }
    }

    /// Replace the fixture workspace set. The set's allocator moves past the
    /// fixtures' IDs, so a workspace the state creates later never repeats
    /// one. The bookmark is cleared.
    pub(crate) fn test_set_workspaces(&mut self, workspaces: Vec<Workspace>) {
        self.workspaces = WorkspaceSet::restored(
            shepr_mux::workspace::WorkspaceIdAllocator::new(),
            workspaces,
            None,
        );
    }

    /// Add a fixture workspace the way live creation does, moving the set's
    /// allocator past its ID.
    /// Returns the workspace's ID.
    pub(crate) fn test_push_workspace(&mut self, workspace: Workspace) -> WorkspaceId {
        let id = workspace.id();
        assert!(
            self.workspaces.insert(workspace).is_ok(),
            "a fixture workspace repeats an id or a pane"
        );
        id
    }

    /// The terminal state of `pane`, for mutation. Panics when the pane is not
    /// in the state.
    pub(crate) fn terminal_mut(
        &mut self,
        pane: shepr_core::layout::PaneId,
    ) -> &mut shepr_mux::terminal::TerminalState {
        self.workspaces
            .pane_mut(pane)
            .expect("a fixture pane in the state")
            .terminal_mut()
    }

    /// Split a fixture workspace.
    pub(crate) fn test_split_workspace(
        &mut self,
        ws_idx: usize,
        direction: shepr_core::layout::Direction,
    ) -> shepr_core::layout::PaneId {
        self.ws_mut(ws_idx).test_split(direction)
    }

    pub(crate) fn test_with_adversarial_identity_state() -> Self {
        let mut state = Self::test_new();
        state.test_set_workspaces(vec![
            shepr_mux::workspace::Workspace::test_adversarial_identity_state(),
        ]);
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn an_unrecorded_workspace_is_laid_out_in_the_headless_area() {
        let mut state = AppState::test_new();
        state.settings.headless_size = shepr_core::geometry::GridSize::clamped(132, 41);
        state.settings.pane_scrollbars = false;
        let id = state.test_push_workspace(shepr_mux::workspace::Workspace::test_new("only"));
        let workspace = state.workspace(&id).expect("the pushed workspace");

        assert_eq!(workspace.spawn_geometry(), None);
        let area = state.layout_area(workspace);
        assert_eq!(area, Rect::new(0, 0, 132, 41));
        assert_eq!(
            state.settings.chrome_in(area).area,
            Rect::new(0, 0, 132, 41)
        );
        assert_eq!(
            state.settings.chrome_in(area).sole_pane_size(),
            // Framed on every side: the headless area less the border cells.
            shepr_core::geometry::GridSize::clamped(130, 39)
        );
    }

    #[test]
    fn a_workspaces_layout_area_is_its_own_recorded_geometry_never_another_workspaces() {
        let mut state = AppState::test_with_adversarial_identity_state();
        let second_id =
            state.test_push_workspace(shepr_mux::workspace::Workspace::test_new("second"));
        state.seed_bookmark_index(Some(0));
        let first_area = Rect::new(0, 0, 97, 33);
        let first_cell = shepr_core::geometry::CellPx::new(9, 18);
        let first_id = state.ws(0).id();
        state.record_workspace_geometry(
            &first_id,
            SpawnGeometry {
                area: first_area,
                cell: first_cell,
            },
        );

        // The bookmark names the first workspace, and the unrecorded second
        // one does not borrow its area: it falls back to the headless area.
        assert_eq!(state.bookmark_index(), Some(0));
        let first = state.workspace(&first_id).expect("the first workspace");
        let second = state.workspace(&second_id).expect("the second workspace");
        assert_eq!(state.layout_area(first), first_area);
        assert_eq!(first.spawn_geometry().map(|g| g.cell), Some(first_cell));
        assert_eq!(second.spawn_geometry(), None);
        assert_eq!(state.layout_area(second), state.settings.headless_rect());
    }

    #[test]
    fn lookups_by_id_survive_a_reorder() {
        let mut state = AppState::test_new();
        let first = state.test_push_workspace(shepr_mux::workspace::Workspace::test_new("first"));
        let second = state.test_push_workspace(shepr_mux::workspace::Workspace::test_new("second"));
        let second_root = state.workspace(&second).expect("second").tree().root();
        assert_eq!(state.workspaces().position(&second), Some(1));

        state.move_workspace(&second, Some(&first));

        assert_eq!(state.workspaces().position(&second), Some(0));
        assert_eq!(state.workspace(&second).map(Workspace::id), Some(second));
        assert_eq!(
            state.pane(second_root).map(|pane| pane.workspace().id()),
            Some(second)
        );
        assert_eq!(
            state.rename_workspace(&first, shepr_mux::Label::new("renamed").expect("test name")),
            Some(crate::app::actions::ViewMutation::Metadata)
        );
        assert_eq!(
            state.workspace(&first).map(Workspace::name),
            Some("renamed")
        );
    }

    #[test]
    fn a_pane_resolves_to_its_workspace_after_an_earlier_workspace_closes() {
        let mut state = AppState::test_new();
        let first = state.test_push_workspace(shepr_mux::workspace::Workspace::test_new("first"));
        let second = state.test_push_workspace(shepr_mux::workspace::Workspace::test_new("second"));
        let second_root = state.workspace(&second).expect("second").tree().root();
        let public = state
            .pane(second_root)
            .expect("the second workspace's root pane")
            .public_id();

        assert!(state.close_workspace(&first).is_some());

        assert_eq!(state.workspaces().position(&second), Some(0));
        let resolved = state
            .resolve_pane(&public)
            .expect("the pane still resolves");
        assert_eq!(resolved.workspace().id(), second);
        assert_eq!(resolved.id(), second_root);
        assert!(state.workspace(&first).is_none());
    }

    #[test]
    fn a_closed_workspaces_id_resolves_to_nothing() {
        let mut state = AppState::test_new();
        let only = state.test_push_workspace(shepr_mux::workspace::Workspace::test_new("only"));
        assert!(state.workspace(&only).is_some());

        assert!(state.close_workspace(&only).is_some());

        assert!(state.workspace(&only).is_none());
        assert!(state.close_workspace(&only).is_none());
        assert!(
            state
                .rename_workspace(&only, shepr_mux::Label::new("gone").expect("test name"))
                .is_none()
        );
    }

    #[test]
    fn public_ids_resolve() {
        let mut state = AppState::test_new();
        state.test_push_workspace(shepr_mux::workspace::Workspace::test_new("a"));
        state.test_push_workspace(shepr_mux::workspace::Workspace::test_new("b"));
        let second = state.test_split_workspace(1, shepr_core::layout::Direction::Horizontal);

        let public = state.pane(second).expect("the split pane").public_id();
        let resolved = state
            .resolve_pane(&public)
            .expect("public pane id resolves");
        assert_eq!(resolved.id(), second);
        assert_eq!(resolved.workspace().id(), state.ws(1).id());
    }

    #[test]
    fn unknown_public_pane_id_does_not_resolve() {
        let mut state = AppState::test_new();
        state.test_push_workspace(shepr_mux::workspace::Workspace::test_new("a"));
        state.test_push_workspace(shepr_mux::workspace::Workspace::test_new("b"));
        let retired_id = shepr_protocol::PublicPaneId::new(
            &retired_workspace_id(),
            shepr_protocol::PanePublicNumber::new(9).expect("number"),
        );

        assert!(state.resolve_pane(&retired_id).is_none());
    }

    #[test]
    fn positional_and_raw_ids_are_rejected() {
        let mut state = AppState::test_new();
        state.test_push_workspace(shepr_mux::workspace::Workspace::test_new("a"));
        state.test_push_workspace(shepr_mux::workspace::Workspace::test_new("b"));
        let ws_id = state.ws(0).id();
        let root = state.ws(0).tree().root();

        for id in ["1", "2", "w_1", "w_2"] {
            assert!(
                id.parse::<shepr_protocol::WorkspaceId>()
                    .ok()
                    .and_then(|id| state.workspace(&id))
                    .is_none(),
                "workspace id {id:?}"
            );
        }
        // Only the canonical text parses. Raw internal pane ids (`p_<raw>`)
        // restart every process, so after a server restart they would name a
        // different pane.
        for id in [
            format!("p_{}", root.raw()),
            format!("p_1_{}", root.raw()),
            format!("{ws_id}-1"),
            "1:p1".to_string(),
        ] {
            assert!(
                id.parse::<shepr_protocol::PublicPaneId>()
                    .ok()
                    .and_then(|id| state.resolve_pane(&id))
                    .is_none(),
                "pane id {id:?}"
            );
        }
    }

    #[test]
    fn navigating_to_a_workspace_marks_the_session_only_when_the_bookmark_moves() {
        let mut state = AppState::test_new();
        state.test_set_workspaces(
            ["a", "b"]
                .into_iter()
                .map(shepr_mux::workspace::Workspace::test_new)
                .collect(),
        );
        let ids: Vec<_> = state
            .workspaces()
            .iter()
            .map(shepr_mux::workspace::Workspace::id)
            .collect();

        assert!(state.set_bookmark(&ids[1]));
        assert!(state.session_dirty);
        state.session_dirty = false;
        assert!(!state.set_bookmark(&ids[1]), "already bookmarked");
        assert!(!state.session_dirty);
    }

    #[test]
    fn split_spawn_size_is_the_new_panes_content_size_not_the_first_panes_outer_rect() {
        let mut state = AppState::test_new();
        let area = Rect::new(5, 2, 120, 40);
        state.settings.pane_scrollbars = true;
        let geometry = state.settings.chrome_in(area);
        assert_eq!(geometry.area, area);

        let (mut layout, root) = shepr_core::layout::TileLayout::new();
        let new_pane = shepr_core::layout::PaneId::alloc();
        assert!(layout.split_pane(
            root,
            shepr_core::layout::Direction::Horizontal,
            shepr_core::layout::SplitRatio::clamped(0.25),
            new_pane,
        ));

        // Right three quarters (90 cols), minus left+right border and the
        // scrollbar gutter; rows minus top+bottom border.
        assert_eq!(
            geometry.pane_size(&layout, false, new_pane),
            Some(shepr_core::geometry::GridSize::clamped(87, 38))
        );
    }

    #[tokio::test]
    async fn runtime_lookup_is_by_pane() {
        let pane_id = shepr_core::layout::PaneId::alloc();
        let mut registry = shepr_mux::pane::PaneRuntimeRegistry::default();

        assert!(registry.get(&pane_id).is_none());
        registry.insert(
            pane_id,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b""),
        );
        assert!(registry.get(&pane_id).is_some());
        assert!(registry.get(&shepr_core::layout::PaneId::alloc()).is_none());
        for (_, runtime) in registry.drain() {
            drop(runtime);
        }
    }

    #[test]
    fn a_split_pane_has_a_terminal_in_the_state() {
        let mut state = AppState::test_with_adversarial_identity_state();

        let ws = state.ws_mut(0);
        let new_pane = ws.test_split(shepr_core::layout::Direction::Horizontal);

        assert!(ws.tree().pane(new_pane).is_some());
        assert!(state.terminal(new_pane).is_some());
        assert!(
            state
                .terminal(shepr_core::layout::PaneId::alloc())
                .is_none()
        );
    }

    #[test]
    fn persistence_status_observation_invalidates_only_when_it_changes() {
        use shepr_protocol::SessionSaveStatus;
        let mut state = AppState::test_new();
        let initial = state.shell_projection_revision();
        state.record_session_save_status(SessionSaveStatus::Ready);
        assert_eq!(state.shell_projection_revision(), initial);
        assert!(state.shell_projection_is_current(initial));
        state.record_session_save_status(SessionSaveStatus::BlockedOnBackup);
        let blocked = state.shell_projection_revision();
        assert_ne!(blocked, initial);
        assert_eq!(
            state.session_save_status(),
            SessionSaveStatus::BlockedOnBackup
        );
        state.record_session_save_status(SessionSaveStatus::BlockedOnBackup);
        assert_eq!(state.shell_projection_revision(), blocked);
        state.record_session_save_status(SessionSaveStatus::Stopped);
        assert_ne!(state.shell_projection_revision(), blocked);
        assert_eq!(state.session_save_status(), SessionSaveStatus::Stopped);
        assert!(!state.session_dirty());
    }

    #[test]
    fn shell_projection_revision_is_explicit_and_monotonic() {
        let mut state = AppState::test_new();
        assert_eq!(
            state.shell_projection_revision,
            shepr_protocol::ProjectionRevision::ZERO
        );
        state.mark_shell_projection_dirty();
        state.mark_shell_projection_dirty();
        assert_eq!(
            state.shell_projection_revision,
            shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(2)
        );

        let max = shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(u64::MAX);
        state.shell_projection_revision = shepr_test_fixtures::counter_at(u64::MAX - 1);
        state.mark_shell_projection_dirty();
        assert_eq!(state.shell_projection_revision, max);
    }

    #[test]
    fn an_exhausted_shell_projection_revision_forces_a_cache_miss() {
        let mut state = AppState::test_new();
        let max = shepr_test_fixtures::counter_at(u64::MAX);
        state.shell_projection_revision = max;
        state.mark_shell_projection_dirty();
        assert_eq!(state.shell_projection_revision(), max);
        assert!(!state.shell_projection_is_current(max));
    }
}
