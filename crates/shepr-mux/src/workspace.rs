use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::git::{AheadBehind, WorkspaceBranch, WorkspaceGitStatus, fallback_label_from_cwd};
use crate::limits::FIRST_WORKSPACE_NUMBER;
use crate::pane::{PaneRuntimeRegistry, PaneState};
use crate::terminal::TerminalState;
use shepr_core::layout::{PaneId, TileLayout};
use shepr_protocol::{PublicPaneId, TerminalId, WorkspaceId};

/// Whether a pane mutation changed the surface its clients render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceChange {
    Changed,
    Unchanged,
}

impl SurfaceChange {
    pub fn is_changed(self) -> bool {
        matches!(self, Self::Changed)
    }
}

/// Why a caller needs a terminal's cwd. A deferred agent resume reads the
/// terminal's stored path directly instead: it must reach the launch
/// unfiltered so the required chdir reports a path that is gone.
#[derive(Clone, Copy)]
pub enum CwdPurpose {
    Identity,
    FollowForNewPane,
    Save,
}

/// Resolve runtime observations and plain terminal state in one place. This
/// performs no directory stat; save probes validate live paths off the loop.
/// These are observations, not UsableCwd values: a path can disappear after
/// observation, and checking it here could block the event loop on a mount.
pub fn terminal_cwd(
    runtime: Option<&crate::pane::PaneRuntime>,
    terminal: Option<&crate::terminal::TerminalState>,
    purpose: CwdPurpose,
) -> Option<PathBuf> {
    let stored = terminal.map(|terminal| terminal.cwd().to_path_buf());
    let observed = match purpose {
        CwdPurpose::Identity => runtime.and_then(crate::pane::PaneRuntime::cwd),
        CwdPurpose::FollowForNewPane => runtime.and_then(crate::pane::PaneRuntime::follow_cwd),
        CwdPurpose::Save => runtime.and_then(crate::pane::PaneRuntime::remembered_cwd),
    };
    // Each source is filtered on its own, so an unusable observation falls
    // back to the stored report instead of hiding it.
    let usable = |path: &PathBuf| path.is_absolute() && !process_cwd_is_deleted(path);
    observed.filter(usable).or_else(|| stored.filter(usable))
}

pub(crate) fn process_cwd_is_deleted(path: &Path) -> bool {
    // The kernel adds this marker to a /proc cwd link after its directory is
    // removed. Reject it without statting the path on the event loop.
    path.as_os_str().as_encoded_bytes().ends_with(b" (deleted)")
}

mod aggregate;
mod geometry;
mod pane_tree;

pub use self::geometry::apply_pane_chrome;
pub use self::geometry::{
    PaneChromeInfo, PaneGeometry, layout_rect, pane_inner_rect, spawn_geometry,
    terminal_content_rect,
};
pub use self::pane_tree::{PreparedSplit, WorkspacePane};

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

/// Hands out the workspace IDs of one session. The server's app state owns
/// the one allocator its workspaces come from, and session restore takes it
/// by `&mut` and moves it past every saved ID before it allocates any, so a
/// fresh ID is never one a restored workspace owns. The counter never wraps,
/// so a live ID is never handed out twice by the allocator that issued it. It
/// is not `Clone`: a copy would issue the same IDs again.
#[derive(Debug, PartialEq, Eq)]
pub struct WorkspaceIdAllocator {
    /// The public number the next allocated ID spells.
    next: usize,
}

impl Default for WorkspaceIdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceIdAllocator {
    /// An allocator whose first ID is the first public number.
    pub const fn new() -> Self {
        Self {
            next: FIRST_WORKSPACE_NUMBER,
        }
    }

    /// The next ID. Panics once the number space is exhausted: continuing
    /// would have to reuse a live ID, and there is no safe value.
    pub fn allocate(&mut self) -> WorkspaceId {
        match self.try_allocate() {
            Some(id) => id,
            None => panic!("workspace id space exhausted"),
        }
    }

    /// Hands out the counter's number and advances it. `None` once the
    /// counter is exhausted: advancing refuses to pass `usize::MAX` rather
    /// than wrap, so that last number is never handed out and marks the space
    /// as used up.
    fn try_allocate(&mut self) -> Option<WorkspaceId> {
        let number = self.next;
        self.next = number.checked_add(1)?;
        WorkspaceId::from_number(number)
    }

    /// Moves the counter past every ID in `ids`. An ID at the top of the
    /// number space leaves nothing to allocate, so the counter is exhausted
    /// rather than left where it could reach that ID again. It never moves
    /// back.
    pub fn reserve<'a>(&mut self, ids: impl IntoIterator<Item = &'a WorkspaceId>) {
        let Some(max) = ids.into_iter().map(WorkspaceId::number).max() else {
            return;
        };
        self.next = self.next.max(max.saturating_add(1));
    }
}

/// Only admitted identities can supply a refresh cache hint. The fallback
/// label has no cwd or Git key and cannot accidentally match an empty path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GitIdentity {
    Undiscovered { fallback_label: String },
    Admitted(WorkspaceGitStatus),
}

impl GitIdentity {
    fn label(&self) -> &str {
        match self {
            Self::Undiscovered { fallback_label } => fallback_label,
            Self::Admitted(status) => &status.auto_label,
        }
    }

    fn branch(&self) -> Option<&WorkspaceBranch> {
        match self {
            Self::Admitted(status) => Some(&status.branch),
            Self::Undiscovered { .. } => None,
        }
    }

    fn ahead_behind(&self) -> Option<AheadBehind> {
        match self {
            Self::Admitted(status) => status.ahead_behind,
            Self::Undiscovered { .. } => None,
        }
    }
}

/// A named workspace: one pane layout and the panes in it.
pub struct Workspace {
    /// Stable public workspace identity, independent of display order.
    pub id: WorkspaceId,
    /// User-provided label override; Git identity still refreshes.
    pub custom_name: Option<String>,
    /// Fallback workspace identity source for tests or missing runtimes.
    pub identity_cwd: PathBuf,
    git_identity: GitIdentity,
    pub next_public_pane_number: shepr_protocol::PanePublicNumber,
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
    /// A workspace around a pane tree. The Git identity (workspace label and
    /// status) is left undiscovered: finding it walks the filesystem up to `/`
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
        next_public_pane_number: shepr_protocol::PanePublicNumber,
    ) -> Self {
        let git_identity = GitIdentity::Undiscovered {
            fallback_label: fallback_label_from_cwd(&identity_cwd),
        };
        Self {
            id,
            custom_name,
            identity_cwd,
            git_identity,
            next_public_pane_number,
            root_pane,
            layout,
            panes,
            zoomed: false,
        }
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
        numbers: impl IntoIterator<Item = shepr_protocol::PanePublicNumber>,
        next: shepr_protocol::PanePublicNumber,
    ) -> bool {
        let mut used = HashSet::new();
        numbers
            .into_iter()
            .all(|number| number < next && used.insert(number))
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
        next_public_pane_number: shepr_protocol::PanePublicNumber,
    ) -> Option<Self> {
        let mut workspace = Self::assemble(
            id,
            custom_name,
            identity_cwd,
            root_pane,
            layout,
            panes,
            next_public_pane_number,
        );
        workspace.set_zoomed(zoomed);
        workspace.valid_panes().then_some(workspace)
    }

    /// Builds a workspace around a caller supplied pane without launching a
    /// PTY or discovering a Git identity. The server crate's unit tests need a
    /// real workspace with no spawned process, and this crate's `cfg(test)`
    /// does not reach a dependent crate's tests, so this constructor seam is
    /// public. The test fixture crate cannot own it: mux's own tests depend on
    /// that crate, so it cannot depend on mux. The caller supplies the ID, so
    /// the fixture keeps its IDs unique the way it chooses.
    pub fn test_from_pane(
        id: WorkspaceId,
        label: Option<String>,
        identity_cwd: &Path,
        pane_id: PaneId,
        mut pane: WorkspacePane,
    ) -> Self {
        pane.public_number = shepr_protocol::PanePublicNumber::FIRST;
        Self::assemble(
            id,
            label,
            identity_cwd.to_path_buf(),
            pane_id,
            TileLayout::from_live_pane(pane_id),
            HashMap::from([(pane_id, pane)]),
            shepr_protocol::PanePublicNumber::SECOND,
        )
    }

    /// Forget discovery without doing IO on the server loop. Only a subsequent
    /// admitted status can supply a Git cache hint.
    pub fn mark_identity_undiscovered(&mut self) {
        self.git_identity = GitIdentity::Undiscovered {
            fallback_label: fallback_label_from_cwd(&self.identity_cwd),
        };
    }

    pub fn matches_identity_cwd(&self, cwd: &Path) -> bool {
        matches!(&self.git_identity, GitIdentity::Admitted(status)
            if status.resolved_identity_cwd == cwd)
    }

    pub fn git_status_key_for_cwd(&self, cwd: &Path) -> Option<&crate::git::GitStatusKey> {
        match &self.git_identity {
            GitIdentity::Admitted(status) if self.matches_identity_cwd(cwd) => {
                Some(&status.status_cache_key)
            }
            GitIdentity::Undiscovered { .. } | GitIdentity::Admitted(_) => None,
        }
    }

    /// Admit a worker result only while its workspace and cwd are current.
    /// The return value describes visible change, including the custom label
    /// override, rather than changes to discovery bookkeeping.
    pub fn admit_git_status(
        &mut self,
        status: WorkspaceGitStatus,
        current_cwd: Option<&Path>,
    ) -> SurfaceChange {
        if self.id != status.workspace_id
            || current_cwd != Some(status.resolved_identity_cwd.as_path())
        {
            return SurfaceChange::Unchanged;
        }
        let next = GitIdentity::Admitted(status);
        let changed = self.display_label(&self.git_identity) != self.display_label(&next)
            || self
                .git_identity
                .branch()
                .and_then(WorkspaceBranch::as_deref)
                != next.branch().and_then(WorkspaceBranch::as_deref)
            || self.git_identity.ahead_behind() != next.ahead_behind();
        self.git_identity = next;
        if changed {
            SurfaceChange::Changed
        } else {
            SurfaceChange::Unchanged
        }
    }

    /// Prepare one pane, its terminal state and public id without starting a
    /// child. The workspace ID comes from `ids`, the allocator of the state the
    /// workspace joins.
    pub fn prepare(
        ids: &mut WorkspaceIdAllocator,
        initial_cwd: &Path,
    ) -> (Self, TerminalState, PublicPaneId) {
        let id = ids.allocate();
        let (layout, root_pane) = TileLayout::new();
        let terminal_id = crate::terminal::allocate_terminal_id();
        let terminal = TerminalState::new(terminal_id.clone(), initial_cwd.to_path_buf());
        let pane = WorkspacePane::new(
            PaneState::new(terminal_id),
            shepr_protocol::PanePublicNumber::FIRST,
        );
        let root_public_id = PublicPaneId::new(&id, pane.public_number);
        let workspace = Self::assemble(
            id,
            None,
            initial_cwd.to_path_buf(),
            root_pane,
            layout,
            HashMap::from([(root_pane, pane)]),
            shepr_protocol::PanePublicNumber::SECOND,
        );
        (workspace, terminal, root_public_id)
    }

    /// Commits the split plan whose public ID the launched child was given:
    /// the pane takes that number and the counter moves past it. A plan for
    /// another workspace, or whose number the counter has already passed, is
    /// refused, as is one whose number has no successor.
    pub fn commit_new_pane(
        &mut self,
        prepared: PreparedSplit,
        focus: bool,
    ) -> Option<TerminalState> {
        if prepared.public_id.workspace_id() != &self.id {
            return None;
        }
        let public_number = prepared.public_id.number();
        let next = public_number.checked_next()?;
        self.commit_prepared_split(
            prepared.pane_id,
            prepared.prepared_layout,
            prepared.terminal.id.clone(),
            public_number,
        )
        .then_some(())?;
        if focus && !self.focus_pane(prepared.pane_id) {
            tracing::error!(workspace = %self.id, pane = ?prepared.pane_id,
                "refused to focus a pane after admitting its split");
        }
        // `commit_prepared_split` refused a number below the counter, so
        // `next` is never behind it.
        self.next_public_pane_number = next;
        Some(prepared.terminal)
    }

    pub fn next_public_pane_number(&self) -> shepr_protocol::PanePublicNumber {
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
        let root_cwd = self.terminal_id(self.root_pane).and_then(|id| {
            terminal_cwd(
                terminal_runtimes.get(id),
                terminals.get(id),
                CwdPurpose::Identity,
            )
        });
        Some(self.resolved_identity_cwd_from_root_pane(root_cwd))
    }

    /// Resolves the workspace identity from a root pane cwd already observed
    /// by the App. This stays as data-only path selection so state reducers can
    /// compare cwd snapshots without probing a pane runtime.
    pub fn resolved_identity_cwd_from_root_pane(&self, root_pane_cwd: Option<PathBuf>) -> PathBuf {
        root_pane_cwd
            .filter(|cwd| cwd.is_absolute() && !process_cwd_is_deleted(cwd))
            .unwrap_or_else(|| self.identity_cwd.clone())
    }

    /// The workspace label: the custom name, else the automatic label cached
    /// from the last admitted Git identity. Every consumer (API workspace
    /// info, sidebar, window title) reads this one value, so they cannot
    /// disagree. The cache follows the workspace's resolved cwd
    /// (`resolved_identity_cwd_from`) through the background Git refresh,
    /// which re-derives it whenever that cwd moves; reading it does no IO.
    pub fn display_name(&self) -> String {
        self.display_label(&self.git_identity).to_owned()
    }

    fn display_label<'a>(&'a self, identity: &'a GitIdentity) -> &'a str {
        self.custom_name
            .as_deref()
            .unwrap_or_else(|| identity.label())
    }

    pub fn branch(&self) -> Option<String> {
        self.branch_state()
            .and_then(WorkspaceBranch::as_deref)
            .map(str::to_owned)
    }

    pub fn branch_state(&self) -> Option<&WorkspaceBranch> {
        self.git_identity.branch()
    }

    pub fn git_ahead_behind(&self) -> Option<AheadBehind> {
        self.git_identity.ahead_behind()
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
            workspace_id: self.id,
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
            workspace_id: self.id,
            pane_id: plan.pane_id,
            scope: plan.scope,
            pane_ids,
            terminal_ids,
        })
    }

    #[cfg(test)]
    fn advance_next_public_pane_number(&mut self, number: shepr_protocol::PanePublicNumber) {
        self.next_public_pane_number = self.next_public_pane_number.max(
            number
                .checked_next()
                .expect("committed number has a successor"),
        );
    }
}

#[cfg(test)]
use shepr_core::layout::Direction;
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

/// The allocator this crate's fixture workspaces share, so every fixture in a
/// test binary has its own ID, as workspaces of one session do.
#[cfg(test)]
pub(crate) fn test_workspace_id() -> WorkspaceId {
    static TEST_WORKSPACE_IDS: std::sync::Mutex<WorkspaceIdAllocator> =
        std::sync::Mutex::new(WorkspaceIdAllocator::new());
    TEST_WORKSPACE_IDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .allocate()
}

#[cfg(test)]
impl Workspace {
    pub fn test_new(name: &str) -> Self {
        let identity_cwd = TEST_WORKSPACE_CWD.with(|cwd| cwd.to_path_buf());
        let (layout, root_id) = TileLayout::new();
        let terminal_id = crate::terminal::allocate_terminal_id();
        let pane = WorkspacePane::new(
            PaneState::new(terminal_id),
            shepr_protocol::PanePublicNumber::FIRST,
        );
        Self::assemble(
            test_workspace_id(),
            Some(name.to_string()),
            identity_cwd,
            root_id,
            layout,
            HashMap::from([(root_id, pane)]),
            shepr_protocol::PanePublicNumber::SECOND,
        )
    }

    pub fn test_split(&mut self, direction: Direction) -> PaneId {
        let new_id = self.layout.split_focused(direction);
        self.panes.insert(
            new_id,
            WorkspacePane::new(
                PaneState::new(crate::terminal::allocate_terminal_id()),
                shepr_protocol::PanePublicNumber::FIRST,
            ),
        );
        self.set_zoomed(false);
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
                .expect("test pane has a public pane number")
                .get(),
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
            !self.zoomed || Workspace::resolved_zoomed(self.zoomed, self.pane_count(), true),
            "workspace {} is zoomed with {} pane(s); a zoom needs a second pane to hide",
            self.id,
            self.pane_count()
        );

        for (pane_id, pane) in &self.panes {
            assert!(
                pane_numbers.insert(pane.public_number),
                "workspace {} duplicate public pane number {} for pane {:?}",
                self.id,
                pane.public_number,
                pane_id
            );
            max_pane_number = max_pane_number.max(pane.public_number.get());
            assert!(
                terminal_ids.insert(pane.attached_terminal_id.clone()),
                "workspace {} terminal {} is attached to multiple panes",
                self.id,
                pane.attached_terminal_id
            );
        }

        assert!(
            self.next_public_pane_number.get() > max_pane_number,
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
    fn preparing_a_split_is_pure_and_commits_its_reserved_identity() {
        let cwd = Path::new("/__shepr_split_missing_directory__");
        let (mut workspace, _, root_public_id) =
            Workspace::prepare(&mut WorkspaceIdAllocator::new(), cwd);
        assert_eq!(
            root_public_id,
            PublicPaneId::new(
                &workspace.id,
                shepr_protocol::PanePublicNumber::new(1).expect("nonzero literal")
            )
        );
        let root = workspace.root_pane();
        let geometry = PaneGeometry {
            area: ratatui::layout::Rect::new(0, 0, 80, 24),
            pane_borders: shepr_config::PaneBordersConfig::Off,
            pane_gaps: false,
            pane_outer_borders: false,
            pane_scrollbars: false,
        };
        let split = workspace
            .prepare_split(
                root,
                Direction::Horizontal,
                &geometry,
                None,
                cwd.to_path_buf(),
                true,
            )
            .expect("split plan");
        assert_eq!(workspace.pane_count(), 1);
        assert_eq!(workspace.focused_pane_id(), root);
        assert_eq!(workspace.next_public_pane_number().get(), 2);
        assert_eq!(split.terminal.cwd(), cwd);
        assert_eq!(split.geometry, spawn_geometry(24, 40, None));
        assert_eq!(
            split.public_id,
            PublicPaneId::new(
                &workspace.id,
                shepr_protocol::PanePublicNumber::new(2).expect("nonzero literal")
            )
        );
        assert_eq!(split.prepared_layout.focused(), split.pane_id);
        assert!(workspace.commit_new_pane(split, true).is_some());
        assert_eq!(workspace.pane_count(), 2);
        assert_eq!(workspace.next_public_pane_number().get(), 3);

        // No child should be launched when there is no successor to commit.
        workspace.next_public_pane_number =
            shepr_protocol::PanePublicNumber::new(usize::MAX).expect("max number");
        assert!(
            workspace
                .prepare_split(
                    root,
                    Direction::Horizontal,
                    &geometry,
                    None,
                    cwd.to_path_buf(),
                    true,
                )
                .is_none()
        );
        assert_eq!(workspace.pane_count(), 2);
    }

    #[test]
    fn public_pane_ids_use_the_canonical_format() {
        let workspace_id: WorkspaceId = "wA".parse().expect("canonical workspace id");
        let pane_id = PublicPaneId::new(
            &workspace_id,
            shepr_protocol::PanePublicNumber::new(33).expect("nonzero literal"),
        );

        assert_eq!(pane_id.to_string(), "wA:p11");
        assert_eq!("wA:p11".parse::<PublicPaneId>(), Ok(pane_id));
        assert!("wA:p".parse::<PublicPaneId>().is_err());
        assert!("wA:t0".parse::<PublicPaneId>().is_err());
        assert!("wA:1".parse::<PublicPaneId>().is_err());
    }

    #[test]
    fn allocated_workspace_ids_are_short_base32_handles() {
        let mut ids = WorkspaceIdAllocator::new();
        let first = ids.try_allocate().expect("first number");
        let second = ids.try_allocate().expect("second number");
        assert_eq!(first.to_string(), "w1");
        assert_eq!(second.to_string(), "w2");

        let mut ids = WorkspaceIdAllocator { next: 32 * 32 };
        let thousandth = ids.try_allocate().expect("1024th number");
        assert_eq!(thousandth.to_string(), "wZ0");
    }

    #[test]
    fn separate_allocators_issue_independent_ids() {
        let mut first = WorkspaceIdAllocator::new();
        let mut second = WorkspaceIdAllocator::new();
        assert_eq!(first.allocate(), second.allocate());
        assert_ne!(first.allocate(), first.allocate());
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
        let mut ids = WorkspaceIdAllocator::new();

        ids.reserve([&restored]);

        let generated = ids.allocate();
        assert_ne!(generated.to_string(), "wZ");
        assert!(generated.number() > 31);
    }

    #[test]
    fn workspace_id_allocation_refuses_to_wrap() {
        let mut ids = WorkspaceIdAllocator {
            next: usize::MAX - 1,
        };
        let last = ids.try_allocate().expect("one number left");
        assert_eq!(last.number(), usize::MAX - 1);
        assert_eq!(ids.try_allocate(), None);
        assert_eq!(ids.try_allocate(), None);
    }

    #[test]
    fn reserving_an_id_at_the_top_of_the_space_exhausts_allocation() {
        let restored = WorkspaceId::from_number(usize::MAX).expect("nonzero number");
        let mut ids = WorkspaceIdAllocator::new();

        ids.reserve([&restored]);

        assert_eq!(ids.try_allocate(), None);
    }

    #[test]
    fn reserving_never_moves_the_counter_back() {
        let restored = WorkspaceId::from_number(3).expect("nonzero number");
        let mut ids = WorkspaceIdAllocator { next: 10 };

        ids.reserve([&restored]);

        let next = ids.try_allocate().expect("numbers left");
        assert_eq!(next.number(), 10);
    }

    #[test]
    fn pane_public_numbers_are_stable_and_not_reused_after_close() {
        let mut ws = Workspace::test_new("test");
        let root = ws.root_pane;
        let second = ws.test_split(Direction::Horizontal);
        let third = ws.test_split(Direction::Vertical);

        assert_eq!(
            ws.public_pane_number(root)
                .map(shepr_protocol::PanePublicNumber::get),
            Some(1)
        );
        assert_eq!(
            ws.public_pane_number(second)
                .map(shepr_protocol::PanePublicNumber::get),
            Some(2)
        );
        assert_eq!(
            ws.public_pane_number(third)
                .map(shepr_protocol::PanePublicNumber::get),
            Some(3)
        );

        assert_eq!(
            ws.close_pane(second).map(|removal| removal.scope),
            Some(PaneRemovalScope::Pane)
        );

        assert_eq!(
            ws.public_pane_number(root)
                .map(shepr_protocol::PanePublicNumber::get),
            Some(1)
        );
        assert_eq!(
            ws.public_pane_number(second)
                .map(shepr_protocol::PanePublicNumber::get),
            None
        );
        assert_eq!(
            ws.public_pane_number(third)
                .map(shepr_protocol::PanePublicNumber::get),
            Some(3)
        );

        let fourth = ws.test_split(Direction::Horizontal);
        assert_eq!(
            ws.public_pane_number(fourth)
                .map(shepr_protocol::PanePublicNumber::get),
            Some(4)
        );
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

        let restored = Workspace::from_restored(
            id,
            None,
            identity_cwd,
            root_pane,
            layout,
            panes,
            true,
            shepr_protocol::PanePublicNumber::new(2).expect("number"),
        )
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
                (pane_id.raw() as usize != pane.public_number.get()).then_some(*pane_id)
            })
            .expect("adversarial state should contain raw/public pane divergence");
        assert_ne!(
            divergent_pane.raw() as usize,
            ws.public_pane_number(divergent_pane)
                .expect("test precondition")
                .get()
        );

        let new_pane = ws.test_split(Direction::Vertical);
        assert!(ws.public_pane_number(new_pane).is_some());
        ws.assert_invariants_for_test();
    }

    #[test]
    fn linked_worktree_auto_label_uses_checkout_root() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let (_, _, checkout) =
            crate::git::test_support::create_repo_with_linked_worktree("linked-auto-label");

        let (snapshot, _) = crate::git::git_status_snapshot_for_cwd(&checkout, None);
        let status = snapshot.into_workspace_status(
            shepr_protocol::WorkspaceId::from_number(1).expect("id"),
            checkout.clone(),
            crate::git::GitStatusKey::Outside(PathBuf::new()),
        );

        assert_eq!(
            status.auto_label,
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
        let status = crate::git::WorkspaceGitStatusSnapshot {
            repo_root: Some(PathBuf::from("/cached-repo")),
            branch: WorkspaceBranch::Detached,
            ahead_behind: None,
        }
        .into_workspace_status(
            ws.id,
            cwd.clone(),
            crate::git::GitStatusKey::Checkout(cwd.clone()),
        );
        ws.admit_git_status(status, Some(&cwd));

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
        let cwd = PathBuf::from("/new/repo");
        let status = crate::git::WorkspaceGitStatusSnapshot {
            repo_root: Some(cwd.clone()),
            branch: WorkspaceBranch::Detached,
            ahead_behind: None,
        }
        .into_workspace_status(
            ws.id,
            cwd.clone(),
            crate::git::GitStatusKey::Checkout(cwd.clone()),
        );
        ws.admit_git_status(status, Some(&cwd));
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
    fn cwd_purposes_preserve_missing_absolute_state_without_filesystem_checks() {
        let terminal = TerminalState::new(
            crate::terminal::allocate_terminal_id(),
            PathBuf::from("/shepr-test-missing-cwd/agent"),
        );
        for purpose in [
            super::CwdPurpose::Identity,
            super::CwdPurpose::FollowForNewPane,
            super::CwdPurpose::Save,
        ] {
            assert_eq!(
                super::terminal_cwd(None, Some(&terminal), purpose),
                Some(terminal.cwd().to_path_buf()),
            );
        }
    }

    #[test]
    fn cwd_query_rejects_relative_and_deleted_state() {
        for path in ["relative", "/gone (deleted)"] {
            let terminal =
                TerminalState::new(crate::terminal::allocate_terminal_id(), PathBuf::from(path));
            assert_eq!(
                super::terminal_cwd(None, Some(&terminal), super::CwdPurpose::Save),
                None,
            );
        }
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
    fn hidden_label_and_cache_changes_are_admitted_without_surface_change() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let mut ws = Workspace::test_new("custom");
        let cwd = ws.identity_cwd.clone();
        let status = WorkspaceGitStatus {
            workspace_id: ws.id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: crate::git::GitStatusKey::Checkout(PathBuf::from("/checkout")),
            auto_label: "automatic".into(),
            branch: WorkspaceBranch::Detached,
            ahead_behind: None,
        };

        assert_eq!(
            ws.admit_git_status(status, Some(&cwd)),
            SurfaceChange::Unchanged
        );
        assert_eq!(ws.display_name(), "custom");
        assert_eq!(
            ws.git_status_key_for_cwd(&cwd),
            Some(&crate::git::GitStatusKey::Checkout(PathBuf::from(
                "/checkout"
            )))
        );
        assert_eq!(ws.branch_state(), Some(&WorkspaceBranch::Detached));
        ws.custom_name = None;
        assert_eq!(ws.display_name(), "automatic");
    }

    #[test]
    fn failed_branch_read_updates_identity_without_changing_branch_presentation() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let mut ws = Workspace::test_new("custom");
        let cwd = ws.identity_cwd.clone();
        let mut status = WorkspaceGitStatus {
            workspace_id: ws.id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: crate::git::GitStatusKey::Outside(cwd.clone()),
            auto_label: "automatic".into(),
            branch: WorkspaceBranch::Detached,
            ahead_behind: None,
        };
        ws.admit_git_status(status.clone(), Some(&cwd));
        status.branch = WorkspaceBranch::ReadFailed;

        assert_eq!(
            ws.admit_git_status(status, Some(&cwd)),
            SurfaceChange::Unchanged
        );
        assert_eq!(ws.branch_state(), Some(&WorkspaceBranch::ReadFailed));
        assert_eq!(ws.branch(), None);
    }

    #[test]
    fn undiscovered_identity_labels_by_basename_and_never_matches_a_cwd() {
        let mut ws = Workspace::test_new("ignored");
        ws.custom_name = None;
        ws.identity_cwd = PathBuf::from("/shepr-test/repo/sub");
        let cwd = ws.identity_cwd.clone();
        let status = WorkspaceGitStatus {
            workspace_id: ws.id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: crate::git::GitStatusKey::Outside(cwd.clone()),
            auto_label: "repo".into(),
            branch: WorkspaceBranch::Named("main".into()),
            ahead_behind: None,
        };
        ws.admit_git_status(status, Some(&cwd));

        ws.mark_identity_undiscovered();

        assert_eq!(ws.display_name(), "sub");
        assert_eq!(ws.branch(), None);
        assert!(!ws.matches_identity_cwd(&ws.identity_cwd));
        assert!(!ws.matches_identity_cwd(Path::new("")));
        assert_eq!(ws.git_status_key_for_cwd(&ws.identity_cwd), None);
    }

    #[test]
    fn workspace_built_from_an_existing_pane_does_not_discover_git_identity() {
        let pane = PaneId::alloc();
        // A path that cannot exist: discovery would have to stat it.
        let cwd = PathBuf::from("/shepr-test-nonexistent/repo/sub");

        let ws = Workspace::test_from_pane(
            test_workspace_id(),
            None,
            &cwd,
            pane,
            WorkspacePane::new(
                PaneState::new(crate::terminal::allocate_terminal_id()),
                shepr_protocol::PanePublicNumber::FIRST,
            ),
        );

        assert_eq!(ws.display_name(), "sub");
        assert!(!ws.matches_identity_cwd(&ws.identity_cwd));
        assert_eq!(ws.pane_count(), 1);
        assert_eq!(
            ws.public_pane_number(pane)
                .map(shepr_protocol::PanePublicNumber::get),
            Some(1)
        );
        ws.assert_invariants_for_test();
    }
}
