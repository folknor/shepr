use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{Notify, mpsc};

use crate::events::AppEvent;
use crate::git::{AheadBehind, GitSpaceMetadata, fallback_label_from_cwd};
use crate::limits::FIRST_WORKSPACE_NUMBER;
use crate::pane::{PaneLaunchEnv, PaneRuntime, PaneRuntimeRegistry, PaneState};
use crate::render_signal::RenderSignal;
use crate::terminal::TerminalState;
use shepr_core::layout::{Direction, PaneId, TileLayout};
use shepr_protocol::{PublicPaneId, TerminalId, WorkspaceId};

mod aggregate;
mod geometry;
mod pane_tree;

pub use self::geometry::apply_pane_chrome;
pub use self::geometry::{
    PaneChromeInfo, PaneGeometry, layout_rect, pane_inner_rect, spawn_geometry,
    terminal_content_rect,
};
pub use self::pane_tree::{NewPane, WorkspacePane};

/// The channels a pane runtime reports through once it is spawned, plus the
/// resolved server socket paths its child needs. `App` owns them and lends a
/// copy to each call that spawns a pane, so the workspace tree itself holds no
/// channels or async handles and stays plain data.
#[derive(Clone)]
pub struct PaneSpawnHandles {
    pub events: mpsc::Sender<AppEvent>,
    pub render_notify: Arc<Notify>,
    pub render_dirty: Arc<RenderSignal>,
    /// Counts this app's pane session teardowns, so its exit waits on them
    /// and on no other app's.
    pub pane_teardowns: Arc<crate::pane::PaneTeardownTracker>,
    /// Resolved API socket passed into every pane launched by this app.
    pub api_socket_path: PathBuf,
    /// Resolved client socket passed into every pane launched by this app.
    pub client_socket_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneRemovalScope {
    Pane,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRemovalPlan {
    workspace_id: WorkspaceId,
    pub pane_id: PaneId,
    pub scope: PaneRemovalScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRemoval {
    pub workspace_id: WorkspaceId,
    pub pane_id: PaneId,
    pub scope: PaneRemovalScope,
    pub pane_ids: Vec<PaneId>,
    pub terminal_ids: Vec<TerminalId>,
}

/// The public number the next allocated workspace ID spells.
///
/// This stays a process global rather than an allocator owned by the
/// server's app state, as the pane id counter in `shepr-core` does. One
/// process serves one session, so unique per process is unique per session;
/// an owned allocator would have to be threaded into every workspace
/// constructor and restore for no change in behaviour. Restore moves the
/// counter past every saved ID with `reserve_workspace_ids` before it allocates
/// any, and the counter never wraps, so a live ID is never handed out twice.
static NEXT_WORKSPACE_NUMBER: AtomicUsize = AtomicUsize::new(FIRST_WORKSPACE_NUMBER);

pub(crate) fn generate_workspace_id() -> WorkspaceId {
    match allocate_workspace_id(&NEXT_WORKSPACE_NUMBER) {
        Some(id) => id,
        // Continuing would have to reuse a live ID; there is no safe value.
        None => panic!("workspace id space exhausted"),
    }
}

/// Hands out the counter's number and advances it. `None` once the counter
/// is exhausted: advancing refuses to pass `usize::MAX` rather than wrap, so
/// that last number is never handed out and marks the space as used up.
fn allocate_workspace_id(counter: &AtomicUsize) -> Option<WorkspaceId> {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            next.checked_add(1)
        })
        .ok()
        .and_then(WorkspaceId::from_number)
}

/// Moves `counter` past every ID in `ids`. An ID at the top of the number
/// space leaves nothing to allocate, so the counter is exhausted rather than
/// left where it could reach that ID again.
fn reserve_workspace_numbers<'a>(
    counter: &AtomicUsize,
    ids: impl IntoIterator<Item = &'a WorkspaceId>,
) {
    let Some(max) = ids.into_iter().map(WorkspaceId::number).max() else {
        return;
    };
    counter.fetch_max(max.saturating_add(1), Ordering::Relaxed);
}

pub(crate) fn reserve_workspace_ids<'a>(ids: impl IntoIterator<Item = &'a WorkspaceId>) {
    reserve_workspace_numbers(&NEXT_WORKSPACE_NUMBER, ids);
}

/// A named workspace: one pane layout and the panes in it.
pub struct Workspace {
    /// Stable public workspace identity, independent of display order.
    pub id: WorkspaceId,
    /// User-provided override. If set, auto-derived identity stops updating.
    pub custom_name: Option<String>,
    /// Fallback workspace identity source for tests or missing runtimes.
    pub identity_cwd: PathBuf,
    /// CWD from which the cached automatic label and Git metadata were derived.
    pub cached_identity_cwd: PathBuf,
    /// Automatic workspace label cached outside the render path.
    pub cached_auto_label: String,
    /// Cache key for periodic Git status associated with `cached_identity_cwd`.
    pub cached_git_status_key: PathBuf,
    /// Cached current git branch for the workspace repo.
    pub cached_git_branch: Option<String>,
    /// Cached ahead/behind counts for the workspace repo's current branch upstream.
    pub cached_git_ahead_behind: Option<AheadBehind>,
    /// Cached derived Git repo metadata for status display.
    pub cached_git_space: Option<GitSpaceMetadata>,
    pub next_public_pane_number: usize,
    // Persistence reads the pane tree for snapshots and fills it during
    // restore; other crates use the accessors and workspace mutators.
    /// Identity source for the pane tree.
    pub(crate) root_pane: PaneId,
    pub(crate) layout: TileLayout,
    /// Runtime-independent pane records, keyed by internal ID.
    pub(crate) panes: HashMap<PaneId, WorkspacePane>,
    /// Shows only the focused pane. Implies more than one pane.
    pub(crate) zoomed: bool,
}

impl Workspace {
    /// A workspace around a pane tree. The Git identity (repo label, branch,
    /// space) is left undiscovered: finding it walks the filesystem up to `/`
    /// and can spawn `git`, which must not run on the server's main loop. The
    /// background Git refresh discovers it, because an undiscovered identity
    /// never matches the workspace's resolved cwd.
    fn assemble(
        id: WorkspaceId,
        custom_name: Option<String>,
        identity_cwd: PathBuf,
        root_pane: PaneId,
        layout: TileLayout,
        panes: HashMap<PaneId, WorkspacePane>,
        zoomed: bool,
        next_public_pane_number: usize,
    ) -> Self {
        let mut workspace = Self {
            id,
            custom_name,
            identity_cwd,
            cached_identity_cwd: PathBuf::new(),
            cached_auto_label: String::new(),
            cached_git_status_key: PathBuf::new(),
            cached_git_branch: None,
            cached_git_ahead_behind: None,
            cached_git_space: None,
            next_public_pane_number,
            root_pane,
            layout,
            panes,
            zoomed,
        };
        workspace.mark_identity_undiscovered();
        workspace
    }

    /// Check a pane tree when it enters a workspace. These checks run on
    /// restore, not on the view or render paths.
    fn valid_panes(&self) -> bool {
        if !self.has_consistent_panes() {
            return false;
        }
        if !Self::valid_public_numbers(
            self.panes.values().map(|pane| pane.public_number),
            self.next_public_pane_number,
        ) {
            return false;
        }
        let mut terminal_ids = HashSet::new();
        self.panes
            .values()
            .all(|pane| terminal_ids.insert(pane.attached_terminal_id.clone()))
    }

    pub(crate) fn valid_public_numbers(
        numbers: impl IntoIterator<Item = usize>,
        next: usize,
    ) -> bool {
        let mut used = HashSet::new();
        numbers
            .into_iter()
            .all(|number| number != 0 && number < next && used.insert(number))
    }

    /// A workspace rebuilt from a saved pane tree. `None` when the tree is
    /// inconsistent or its public numbers collide. A zoom saved on a workspace
    /// left with one pane is dropped: a zoom needs a second pane to hide.
    pub(crate) fn from_restored(
        id: WorkspaceId,
        custom_name: Option<String>,
        identity_cwd: PathBuf,
        root_pane: PaneId,
        layout: TileLayout,
        panes: HashMap<PaneId, WorkspacePane>,
        zoomed: bool,
        next_public_pane_number: usize,
    ) -> Option<Self> {
        let zoomed = zoomed && panes.len() > 1;
        let workspace = Self::assemble(
            id,
            custom_name,
            identity_cwd,
            root_pane,
            layout,
            panes,
            zoomed,
            next_public_pane_number,
        );
        workspace.valid_panes().then_some(workspace)
    }

    /// Builds a workspace around a caller supplied pane without launching a
    /// PTY or discovering a Git identity. The server crate's unit tests need a
    /// real workspace with no spawned process, and this crate's `cfg(test)`
    /// does not reach a dependent crate's tests, so this constructor seam is
    /// public. The test fixture crate cannot own it: mux's own tests depend on
    /// that crate, so it cannot depend on mux.
    pub fn test_from_pane(
        label: Option<String>,
        identity_cwd: &Path,
        pane_id: PaneId,
        mut pane: WorkspacePane,
    ) -> Self {
        pane.public_number = 1;
        Self::assemble(
            generate_workspace_id(),
            label,
            identity_cwd.to_path_buf(),
            pane_id,
            TileLayout::from_live_pane(pane_id),
            HashMap::from([(pane_id, pane)]),
            false,
            2,
        )
    }

    /// Resets the cached Git identity to "not discovered yet": the label is
    /// the basename of `identity_cwd` (pure string work, no filesystem) and
    /// there is no branch or space. The cached identity cwd is left empty, so
    /// it differs from every resolved cwd and the next background Git refresh
    /// rediscovers the real identity off the main loop.
    pub fn mark_identity_undiscovered(&mut self) {
        self.cached_identity_cwd = PathBuf::new();
        self.cached_auto_label = fallback_label_from_cwd(&self.identity_cwd);
        self.cached_git_status_key = self.identity_cwd.clone();
        self.cached_git_branch = None;
        self.cached_git_ahead_behind = None;
        self.cached_git_space = None;
    }

    /// A new workspace with one shell pane whose PTY is spawned at `geometry`:
    /// the grid it will have and the pixel size of one cell, so the shell's
    /// first `TIOCSWINSZ` already carries pixel dimensions.
    pub fn spawn(
        initial_cwd: &Path,
        geometry: shepr_core::geometry::PaneGeometry,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        spawn: &PaneSpawnHandles,
    ) -> std::io::Result<(Self, TerminalState, PaneRuntime)> {
        let id = generate_workspace_id();
        let launch_env = PaneLaunchEnv::from_extra_with_socket_paths(
            Vec::new(),
            spawn.api_socket_path.clone(),
            spawn.client_socket_path.clone(),
        )
        .with_pane_id(PublicPaneId::new(&id, 1));
        let (layout, root_pane) = TileLayout::new();
        let runtime = PaneRuntime::spawn(
            root_pane,
            geometry,
            initial_cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            &launch_env,
            &spawn.events,
            &spawn.render_notify,
            &spawn.render_dirty,
            &spawn.pane_teardowns,
        )?;
        let terminal_id = TerminalId::alloc();
        let terminal = TerminalState::new(terminal_id.clone(), initial_cwd.to_path_buf());
        let mut pane = WorkspacePane::new(PaneState::new(terminal_id));
        pane.public_number = 1;
        let workspace = Self::assemble(
            id,
            None,
            initial_cwd.to_path_buf(),
            root_pane,
            layout,
            HashMap::from([(root_pane, pane)]),
            false,
            2,
        );
        Ok((workspace, terminal, runtime))
    }

    /// Starts a shell in a new pane split off `pane_id`, sized from `geometry`
    /// (the workspace's area and chrome) and `cell` (the pixel size of one
    /// cell, `None` when unknown). The layout change is prepared on a clone and
    /// installed by `commit_new_pane`.
    #[expect(
        clippy::too_many_arguments,
        reason = "pane creation needs launch settings and spawn context"
    )]
    pub fn split_pane(
        &self,
        pane_id: PaneId,
        direction: Direction,
        geometry: &PaneGeometry,
        cell: Option<shepr_core::geometry::CellPx>,
        cwd: Option<PathBuf>,
        default_cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        focus_new_pane: bool,
        spawn: &PaneSpawnHandles,
    ) -> Option<std::io::Result<NewPane>> {
        if !self.contains_pane(pane_id) {
            return None;
        }
        let pane_number = self.next_public_pane_number;
        let launch_env = self.launch_env_for_new_pane(pane_number, spawn);
        Some(self.split_pane_shell(
            pane_id,
            focus_new_pane,
            direction,
            geometry,
            cell,
            cwd,
            default_cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            &launch_env,
            spawn,
        ))
    }

    pub fn commit_new_pane(
        &mut self,
        pane_id: PaneId,
        prepared_layout: TileLayout,
        terminal_id: TerminalId,
        focus: bool,
    ) -> Option<()> {
        let number = self.next_public_pane_number;
        self.commit_prepared_split(pane_id, prepared_layout, terminal_id, number)
            .then_some(())?;
        if focus && !self.focus_pane(pane_id) {
            tracing::error!(workspace = %self.id, ?pane_id, "refused to focus a pane after admitting its split");
        }
        self.advance_next_public_pane_number(number);
        Some(())
    }

    pub(crate) fn launch_env_for_new_pane(
        &self,
        pane_number: usize,
        spawn: &PaneSpawnHandles,
    ) -> PaneLaunchEnv {
        PaneLaunchEnv::from_extra_with_socket_paths(
            Vec::new(),
            spawn.api_socket_path.clone(),
            spawn.client_socket_path.clone(),
        )
        .with_pane_id(PublicPaneId::new(&self.id, pane_number))
    }

    pub fn next_public_pane_number(&self) -> usize {
        self.next_public_pane_number
    }

    pub fn set_custom_name(&mut self, name: String) {
        self.custom_name = Some(name);
    }

    /// App-side convenience for resolving the live workspace identity. This
    /// may read the root pane's process cwd through its runtime; state reducers
    /// should instead receive the observed cwd and call
    /// `resolved_identity_cwd_from_root_pane`.
    pub fn resolved_identity_cwd_from(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &PaneRuntimeRegistry,
    ) -> Option<PathBuf> {
        Some(self.resolved_identity_cwd_from_root_pane(self.cwd_for_pane(
            self.root_pane,
            terminals,
            terminal_runtimes,
        )))
    }

    /// Resolves the workspace identity from a root pane cwd already observed
    /// by the App. This stays as data-only path selection so state reducers can
    /// compare cwd snapshots without probing a pane runtime.
    pub fn resolved_identity_cwd_from_root_pane(&self, root_pane_cwd: Option<PathBuf>) -> PathBuf {
        root_pane_cwd.unwrap_or_else(|| self.identity_cwd.clone())
    }

    /// The workspace label: the custom name, else the automatic label cached
    /// from the last admitted Git identity. Every consumer (API workspace
    /// info, sidebar, window title) reads this one value, so they cannot
    /// disagree. The cache follows the workspace's resolved cwd
    /// (`resolved_identity_cwd_from`) through the background Git refresh,
    /// which re-derives it whenever that cwd moves; reading it does no IO.
    pub fn display_name(&self) -> String {
        self.custom_name
            .clone()
            .unwrap_or_else(|| self.cached_auto_label.clone())
    }

    pub fn branch(&self) -> Option<String> {
        self.cached_git_branch.clone()
    }

    pub fn git_ahead_behind(&self) -> Option<AheadBehind> {
        self.cached_git_ahead_behind
    }

    /// Scope is `Pane` when the workspace has more than one pane, else
    /// `Workspace` (the caller removes the workspace).
    pub fn prepare_pane_removal(&self, pane_id: PaneId) -> Option<PaneRemovalPlan> {
        if !self.contains_pane(pane_id) {
            return None;
        }
        let scope = if self.panes.len() > 1 {
            PaneRemovalScope::Pane
        } else {
            PaneRemovalScope::Workspace
        };
        Some(PaneRemovalPlan {
            workspace_id: self.id.clone(),
            pane_id,
            scope,
        })
    }

    /// Commits a pane removal prepared from this workspace. The typed scope
    /// tells the app whether the workspace itself was removed as a consequence.
    /// A workspace-scoped result leaves this value intact; `AppState` owns the
    /// workspace collection and removes it as part of the same command.
    pub fn remove_pane(&mut self, plan: &PaneRemovalPlan) -> Option<PaneRemoval> {
        if plan.workspace_id != self.id
            || self.prepare_pane_removal(plan.pane_id)?.scope != plan.scope
        {
            return None;
        }

        let (pane_ids, terminal_ids) = match plan.scope {
            PaneRemovalScope::Pane => (
                vec![plan.pane_id],
                vec![self.terminal_id(plan.pane_id)?.clone()],
            ),
            PaneRemovalScope::Workspace => (
                self.layout.pane_ids(),
                self.panes
                    .values()
                    .map(|pane| pane.attached_terminal_id.clone())
                    .collect(),
            ),
        };

        if plan.scope == PaneRemovalScope::Pane {
            self.detach_pane(plan.pane_id)?;
        }

        Some(PaneRemoval {
            workspace_id: self.id.clone(),
            pane_id: plan.pane_id,
            scope: plan.scope,
            pane_ids,
            terminal_ids,
        })
    }

    fn advance_next_public_pane_number(&mut self, number: usize) {
        self.next_public_pane_number = self.next_public_pane_number.max(number.saturating_add(1));
    }
}

#[cfg(test)]
impl Workspace {
    pub fn resolved_identity_cwd(&self) -> Option<PathBuf> {
        Some(self.identity_cwd.clone())
    }

    pub fn close_pane(&mut self, pane_id: PaneId) -> Option<PaneRemoval> {
        let plan = self.prepare_pane_removal(pane_id)?;
        self.remove_pane(&plan)
    }

    fn register_new_pane(&mut self, pane_id: PaneId) {
        let number = self.next_public_pane_number;
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            tracing::error!(?pane_id, "cannot number a pane missing from its workspace");
            return;
        };
        pane.public_number = number;
        self.advance_next_public_pane_number(number);
    }
}

#[cfg(test)]
std::thread_local! {
    static TEST_WORKSPACE_CWD: crate::test_support::ScratchDir =
        crate::test_support::ScratchDir::new("workspace-test-cwd");
}

#[cfg(test)]
impl Workspace {
    pub fn test_new(name: &str) -> Self {
        let identity_cwd = TEST_WORKSPACE_CWD.with(|cwd| cwd.to_path_buf());
        let (layout, root_id) = TileLayout::new();
        let terminal_id = TerminalId::alloc();
        let mut pane = WorkspacePane::new(PaneState::new(terminal_id));
        pane.public_number = 1;
        Self::assemble(
            generate_workspace_id(),
            Some(name.to_string()),
            identity_cwd,
            root_id,
            layout,
            HashMap::from([(root_id, pane)]),
            false,
            2,
        )
    }

    pub fn test_split(&mut self, direction: Direction) -> PaneId {
        let new_id = self.layout.split_focused(direction);
        self.panes.insert(
            new_id,
            WorkspacePane::new(PaneState::new(TerminalId::alloc())),
        );
        self.zoomed = false;
        self.register_new_pane(new_id);
        new_id
    }

    pub fn test_adversarial_identity_state() -> Self {
        let mut ws = Self::test_new("adversarial-identity");
        let removed_pane = ws.test_split(Direction::Horizontal);
        ws.test_split(Direction::Vertical);
        assert_eq!(
            ws.close_pane(removed_pane).map(|removal| removal.scope),
            Some(PaneRemovalScope::Pane)
        );
        let _unused_raw_id = PaneId::alloc();
        let later_pane = ws.test_split(Direction::Horizontal);

        assert_ne!(
            later_pane.raw() as usize,
            ws.public_pane_number(later_pane)
                .expect("test pane has a public pane number"),
            "adversarial pane must distinguish raw pane id from public pane number"
        );
        ws
    }

    pub fn assert_invariants_for_test(&self) {
        let mut terminal_ids = std::collections::HashSet::new();
        let mut pane_numbers = std::collections::HashSet::new();
        let mut max_pane_number = 0usize;

        assert!(
            self.panes.contains_key(&self.root_pane),
            "workspace {} root pane {:?} is missing from its panes",
            self.id,
            self.root_pane
        );

        let layout_panes = self.layout.pane_ids();
        let layout_set: std::collections::HashSet<_> = layout_panes.iter().copied().collect();
        assert_eq!(
            layout_panes.len(),
            layout_set.len(),
            "workspace {} layout contains duplicate pane ids",
            self.id
        );
        assert!(
            layout_set.contains(&self.layout.focused()),
            "workspace {} focused pane {:?} is not in layout",
            self.id,
            self.layout.focused()
        );
        let pane_set: std::collections::HashSet<_> = self.panes.keys().copied().collect();
        assert_eq!(
            layout_set, pane_set,
            "workspace {} layout panes must exactly match pane records",
            self.id
        );
        assert!(
            !self.zoomed || self.pane_count() > 1,
            "workspace {} is zoomed with {} pane(s); a zoom needs a second pane to hide",
            self.id,
            self.pane_count()
        );

        for (pane_id, pane) in &self.panes {
            assert!(
                pane.public_number > 0,
                "workspace {} pane {:?} has invalid public pane number 0",
                self.id,
                pane_id
            );
            assert!(
                pane_numbers.insert(pane.public_number),
                "workspace {} duplicate public pane number {} for pane {:?}",
                self.id,
                pane.public_number,
                pane_id
            );
            max_pane_number = max_pane_number.max(pane.public_number);
            assert!(
                terminal_ids.insert(pane.attached_terminal_id.clone()),
                "workspace {} terminal {} is attached to multiple panes",
                self.id,
                pane.attached_terminal_id
            );
        }

        assert!(
            self.next_public_pane_number > 0,
            "workspace {} next_public_pane_number must be greater than 0",
            self.id
        );
        assert!(
            self.next_public_pane_number > max_pane_number,
            "workspace {} next_public_pane_number {} must be greater than max live public pane number {}",
            self.id,
            self.next_public_pane_number,
            max_pane_number
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_protocol::{decode_public_number, encode_public_number};

    #[test]
    fn public_pane_ids_use_the_canonical_format() {
        let workspace_id: WorkspaceId = "wA".parse().expect("canonical workspace id");
        let pane_id = PublicPaneId::new(&workspace_id, 33);

        assert_eq!(pane_id.to_string(), "wA:p11");
        assert_eq!("wA:p11".parse::<PublicPaneId>(), Ok(pane_id));
        assert!("wA:p".parse::<PublicPaneId>().is_err());
        assert!("wA:t0".parse::<PublicPaneId>().is_err());
        assert!("wA:1".parse::<PublicPaneId>().is_err());
    }

    /// A counter of its own, so the numbers do not depend on how many IDs
    /// other tests in this binary took from the process-wide one.
    #[test]
    fn allocated_workspace_ids_are_short_base32_handles() {
        let counter = AtomicUsize::new(FIRST_WORKSPACE_NUMBER);
        let first = allocate_workspace_id(&counter).expect("first number");
        let second = allocate_workspace_id(&counter).expect("second number");
        assert_eq!(first, "w1");
        assert_eq!(second, "w2");

        let counter = AtomicUsize::new(32 * 32);
        let thousandth = allocate_workspace_id(&counter).expect("1024th number");
        assert_eq!(thousandth, "wZ0");
    }

    #[test]
    fn public_numbers_round_trip_readable_base32_handles() {
        assert_eq!(encode_public_number(1), "1");
        assert_eq!(encode_public_number(9), "9");
        assert_eq!(encode_public_number(10), "A");
        assert_eq!(encode_public_number(31), "Z");
        assert_eq!(encode_public_number(32), "0");
        assert_eq!(encode_public_number(33), "11");

        for value in [1, 9, 10, 31, 32, 33, 1024, 1025] {
            let encoded = encode_public_number(value);
            assert_eq!(decode_public_number(&encoded), Some(value));
        }
    }

    #[test]
    fn every_public_number_round_trips_including_zero() {
        for value in (0..=2048).chain([usize::MAX]) {
            let encoded = encode_public_number(value);
            assert_eq!(
                decode_public_number(&encoded),
                Some(value),
                "{value} encoded as {encoded:?}"
            );
        }
    }

    #[test]
    fn reserving_restored_workspace_ids_prevents_reuse() {
        let restored: WorkspaceId = "wZ".parse().expect("canonical workspace id");

        reserve_workspace_ids([&restored]);

        let generated = generate_workspace_id();
        assert_ne!(generated, "wZ");
        assert!(generated.number() > 31);
    }

    #[test]
    fn workspace_id_allocation_refuses_to_wrap() {
        let counter = AtomicUsize::new(usize::MAX - 1);
        let last = allocate_workspace_id(&counter).expect("one number left");
        assert_eq!(last.number(), usize::MAX - 1);
        assert_eq!(allocate_workspace_id(&counter), None);
        assert_eq!(allocate_workspace_id(&counter), None);
    }

    #[test]
    fn reserving_an_id_at_the_top_of_the_space_exhausts_allocation() {
        let restored = WorkspaceId::from_number(usize::MAX).expect("nonzero number");
        let counter = AtomicUsize::new(FIRST_WORKSPACE_NUMBER);

        reserve_workspace_numbers(&counter, [&restored]);

        assert_eq!(allocate_workspace_id(&counter), None);
    }

    #[test]
    fn reserving_never_moves_the_counter_back() {
        let restored = WorkspaceId::from_number(3).expect("nonzero number");
        let counter = AtomicUsize::new(10);

        reserve_workspace_numbers(&counter, [&restored]);

        let next = allocate_workspace_id(&counter).expect("numbers left");
        assert_eq!(next.number(), 10);
    }

    #[test]
    fn pane_public_numbers_are_stable_and_not_reused_after_close() {
        let mut ws = Workspace::test_new("test");
        let root = ws.root_pane;
        let second = ws.test_split(Direction::Horizontal);
        let third = ws.test_split(Direction::Vertical);

        assert_eq!(ws.public_pane_number(root), Some(1));
        assert_eq!(ws.public_pane_number(second), Some(2));
        assert_eq!(ws.public_pane_number(third), Some(3));

        assert_eq!(
            ws.close_pane(second).map(|removal| removal.scope),
            Some(PaneRemovalScope::Pane)
        );

        assert_eq!(ws.public_pane_number(root), Some(1));
        assert_eq!(ws.public_pane_number(second), None);
        assert_eq!(ws.public_pane_number(third), Some(3));

        let fourth = ws.test_split(Direction::Horizontal);
        assert_eq!(ws.public_pane_number(fourth), Some(4));
    }

    #[test]
    fn closing_the_last_pane_is_workspace_scoped_and_leaves_the_workspace_intact() {
        let mut ws = Workspace::test_new("test");
        let root = ws.root_pane;
        let terminal_id = ws.terminal_id(root).expect("test precondition").clone();

        let removal = ws.close_pane(root).expect("removal");

        assert_eq!(removal.scope, PaneRemovalScope::Workspace);
        assert_eq!(removal.pane_ids, vec![root]);
        assert_eq!(removal.terminal_ids, vec![terminal_id]);
        assert_eq!(ws.pane_count(), 1);
        ws.assert_invariants_for_test();
    }

    #[test]
    fn shows_pane_follows_zoom_and_focus() {
        let mut ws = Workspace::test_new("test");
        let root = ws.root_pane;
        let second = ws.test_split(Direction::Horizontal);
        assert!(ws.shows_pane(root));
        assert!(ws.shows_pane(second));
        assert!(!ws.shows_pane(PaneId::alloc()));

        assert!(ws.focus_pane(second));
        assert!(ws.set_zoomed(true));
        assert!(ws.shows_pane(second));
        assert!(!ws.shows_pane(root));
        ws.assert_invariants_for_test();
    }

    #[test]
    fn a_one_pane_workspace_refuses_to_zoom() {
        let mut ws = Workspace::test_new("test");

        assert!(!ws.set_zoomed(true));
        assert!(!ws.zoomed());
        ws.assert_invariants_for_test();

        // Unzooming is always accepted.
        assert!(ws.set_zoomed(false));
        assert!(!ws.zoomed());

        ws.test_split(Direction::Horizontal);
        assert!(ws.set_zoomed(true));
        assert!(ws.zoomed());
        ws.assert_invariants_for_test();
    }

    #[test]
    fn closing_down_to_one_pane_clears_the_zoom() {
        let mut ws = Workspace::test_new("test");
        let second = ws.test_split(Direction::Horizontal);
        assert!(ws.set_zoomed(true));

        assert!(ws.close_pane(second).is_some());

        assert!(!ws.zoomed());
        ws.assert_invariants_for_test();
    }

    #[test]
    fn restore_clears_a_zoom_saved_on_a_one_pane_workspace() {
        let one = Workspace::test_new("one");
        let Workspace {
            id,
            root_pane,
            layout,
            panes,
            identity_cwd,
            ..
        } = one;

        let restored =
            Workspace::from_restored(id, None, identity_cwd, root_pane, layout, panes, true, 2)
                .expect("valid pane tree");

        assert!(!restored.zoomed());
        restored.assert_invariants_for_test();
    }

    #[test]
    fn restore_keeps_a_zoom_saved_on_a_split_workspace() {
        let mut two = Workspace::test_new("two");
        two.test_split(Direction::Horizontal);
        let next = two.next_public_pane_number;
        let Workspace {
            id,
            root_pane,
            layout,
            panes,
            identity_cwd,
            ..
        } = two;

        let restored =
            Workspace::from_restored(id, None, identity_cwd, root_pane, layout, panes, true, next)
                .expect("valid pane tree");

        assert!(restored.zoomed());
        restored.assert_invariants_for_test();
    }

    #[test]
    fn adversarial_identity_state_satisfies_workspace_invariants_after_mutation() {
        let mut ws = Workspace::test_adversarial_identity_state();
        ws.assert_invariants_for_test();

        let divergent_pane = ws
            .panes
            .iter()
            .find_map(|(pane_id, pane)| {
                (pane_id.raw() as usize != pane.public_number).then_some(*pane_id)
            })
            .expect("adversarial state should contain raw/public pane divergence");
        assert_ne!(
            divergent_pane.raw() as usize,
            ws.public_pane_number(divergent_pane)
                .expect("test precondition")
        );

        let new_pane = ws.test_split(Direction::Vertical);
        assert!(ws.public_pane_number(new_pane).is_some());
        ws.assert_invariants_for_test();
    }

    #[test]
    fn linked_worktree_auto_label_uses_checkout_name_not_repo_name() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let (_, repo, checkout) =
            crate::git::test_support::create_repo_with_linked_worktree("linked-auto-label");

        let (snapshot, _) = crate::git::git_status_snapshot_for_cwd(&checkout, None);
        let space = snapshot.space;
        let auto_label = snapshot.auto_label;

        assert_eq!(
            space.expect("test precondition").repo_name,
            repo.file_name()
                .expect("test precondition")
                .to_str()
                .expect("test precondition")
        );
        assert_eq!(
            auto_label,
            checkout
                .file_name()
                .expect("test precondition")
                .to_str()
                .expect("test precondition")
        );
    }

    #[test]
    fn display_name_reads_cached_identity_without_rechecking_filesystem() {
        let root = crate::test_support::ScratchDir::new("label-cache");
        let cwd = root.join("deep/nested");
        std::fs::create_dir_all(&cwd).expect("create nested cwd");

        let mut ws = Workspace::test_new("ignored");
        ws.custom_name = None;
        ws.identity_cwd = cwd.clone();
        ws.cached_identity_cwd = cwd;
        ws.cached_auto_label = "cached-repo".into();

        std::fs::remove_dir_all(root).expect("remove cwd after cache admission");

        assert_eq!(ws.display_name(), "cached-repo");
    }

    #[test]
    fn label_is_the_admitted_identity_even_when_the_live_cwd_has_moved() {
        // A subdirectory `cd` without OSC 7 used to make the API show the
        // subdirectory basename while the window title showed the repo name.
        // Both now read the one cached label until the background refresh
        // admits the new cwd.
        let mut ws = Workspace::test_new("ignored");
        let root_pane = ws.root_pane;
        let terminal_id = ws
            .terminal_id(root_pane)
            .expect("test precondition")
            .clone();
        ws.custom_name = None;
        ws.identity_cwd = PathBuf::from("/old/workspace");
        ws.cached_identity_cwd = PathBuf::from("/new/repo");
        ws.cached_auto_label = "repo".into();
        let terminals = HashMap::from([(
            terminal_id.clone(),
            TerminalState::new(terminal_id, PathBuf::from("/new/repo/deep")),
        )]);

        assert_eq!(
            ws.resolved_identity_cwd_from(&terminals, &PaneRuntimeRegistry::new()),
            Some(PathBuf::from("/new/repo/deep"))
        );
        assert_eq!(ws.display_name(), "repo");
    }

    #[test]
    fn workspace_identity_follows_root_pane_cwd() {
        let mut ws = Workspace::test_new("ignored");
        ws.custom_name = None;
        let root_pane = ws.root_pane;
        let terminal_id = ws
            .terminal_id(root_pane)
            .expect("test precondition")
            .clone();
        let mut terminals = HashMap::new();
        terminals.insert(
            terminal_id.clone(),
            TerminalState::new(terminal_id, PathBuf::from("/shepr-test/pion")),
        );
        let terminal_runtimes = PaneRuntimeRegistry::new();

        assert_eq!(
            ws.resolved_identity_cwd_from(&terminals, &terminal_runtimes),
            Some(PathBuf::from("/shepr-test/pion"))
        );
    }

    #[test]
    fn resolved_identity_cwd_from_root_pane_uses_observation_or_identity_fallback() {
        let mut ws = Workspace::test_new("ignored");
        ws.identity_cwd = PathBuf::from("/saved/workspace");

        assert_eq!(
            ws.resolved_identity_cwd_from_root_pane(Some(PathBuf::from("/live/pane"))),
            PathBuf::from("/live/pane")
        );
        assert_eq!(
            ws.resolved_identity_cwd_from_root_pane(None),
            PathBuf::from("/saved/workspace")
        );
    }

    #[test]
    fn undiscovered_identity_labels_by_basename_and_never_matches_a_cwd() {
        let mut ws = Workspace::test_new("ignored");
        ws.custom_name = None;
        ws.identity_cwd = PathBuf::from("/shepr-test/repo/sub");
        ws.cached_git_branch = Some("main".into());

        ws.mark_identity_undiscovered();

        assert_eq!(ws.display_name(), "sub");
        assert_eq!(ws.branch(), None);
        assert_eq!(ws.cached_git_space, None);
        assert_ne!(ws.cached_identity_cwd, ws.identity_cwd);
        assert!(ws.cached_identity_cwd.as_os_str().is_empty());
    }

    #[test]
    fn workspace_built_from_an_existing_pane_does_not_discover_git_identity() {
        let pane = PaneId::alloc();
        // A path that cannot exist: discovery would have to stat it.
        let cwd = PathBuf::from("/shepr-test-nonexistent/repo/sub");

        let ws = Workspace::test_from_pane(
            None,
            &cwd,
            pane,
            WorkspacePane::new(PaneState::new(TerminalId::alloc())),
        );

        assert_eq!(ws.display_name(), "sub");
        assert!(ws.cached_identity_cwd.as_os_str().is_empty());
        assert_eq!(ws.pane_count(), 1);
        assert_eq!(ws.public_pane_number(pane), Some(1));
        ws.assert_invariants_for_test();
    }
}
