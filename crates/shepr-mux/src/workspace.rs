use std::path::{Path, PathBuf};

use shepr_core::absolute_path::AbsolutePath;

use crate::git::{AheadBehind, GitBranch, GitStatus, GitStatusKey};
use crate::pane::{PaneRuntime, PaneRuntimeRegistry};
use shepr_core::layout::{NavDirection, PaneId, RatioDelta, SplitPath, SplitRatio};
use shepr_protocol::WorkspaceId;

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
) -> Option<AbsolutePath> {
    let stored = terminal.map(|terminal| terminal.cwd().clone());
    let observed = match purpose {
        CwdPurpose::Identity => runtime.and_then(crate::pane::PaneRuntime::cwd),
        CwdPurpose::FollowForNewPane => runtime.and_then(crate::pane::PaneRuntime::follow_cwd),
        CwdPurpose::Save => runtime.and_then(crate::pane::PaneRuntime::remembered_cwd),
    };
    // The runtime classifies `/proc` cwd links at the observation boundary.
    // OSC 7 and saved paths are user paths and may legitimately end with the
    // same text the kernel appends to an unlinked link target.
    observed
        .and_then(|path| AbsolutePath::new(path).ok())
        .or(stored)
}

mod aggregate;
mod geometry;
mod pane_tree;
mod set;
mod shape;

pub use self::geometry::{SpawnGeometry, WorkspaceChrome};
pub use self::pane_tree::{
    PaneRecord, PaneTree, PreparedSplit, RemoveRefusal, SavedTreeState, SplitPreparationRefused,
    SplitRefused, TreePlan, TreeRejection,
};
pub use self::set::{
    InsertRefusal, InsertRefusalReason, PaneRef, PaneRemoval, PaneRemovalScope, PreparedWorkspace,
    WorkspaceIdAllocator, WorkspaceSet,
};
pub use self::shape::Shape;

/// Only admitted identities can supply a refresh cache hint. An undiscovered
/// identity has no cwd or Git key and cannot accidentally match an empty path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GitIdentity {
    Undiscovered,
    Admitted(GitStatus),
}

impl GitIdentity {
    fn branch(&self) -> Option<&GitBranch> {
        match self {
            Self::Admitted(status) => Some(&status.branch),
            Self::Undiscovered => None,
        }
    }

    fn ahead_behind(&self) -> Option<AheadBehind> {
        match self {
            Self::Admitted(status) => status.ahead_behind,
            Self::Undiscovered => None,
        }
    }
}

/// A named workspace: one pane layout and the panes in it.
pub struct Workspace {
    /// Stable public workspace identity, independent of display order.
    id: WorkspaceId,
    /// The name, set at creation and changed only by a rename. A `Label`, so it
    /// is never blank or padded, and a save always writes a name a restore
    /// accepts.
    name: crate::Label,
    /// Fallback workspace identity source for a missing runtime, fixed at
    /// construction.
    identity_cwd: AbsolutePath,
    git: GitIdentity,
    /// The layout and the pane records, kept in agreement by the tree.
    tree: pane_tree::PaneTree,
    /// The geometry the server last applied to this workspace's PTYs, or
    /// spawned its first pane at. A pane has one PTY size whichever client set
    /// it, so this is session data, not any client's view. Spawn sizing and API
    /// geometry (directional focus, resize steps, layout snapshots) read it, so
    /// they agree with the sizes the panes actually have.
    spawn_geometry: Option<SpawnGeometry>,
}

impl Workspace {
    /// A workspace around a pane tree, named `name` or, without one, after
    /// `identity_cwd` (`Label::for_directory`). The Git status is left
    /// undiscovered: finding it walks the filesystem up to `/` and can spawn
    /// `git`, which must not run on the server's main loop. The background Git
    /// refresh discovers it, because an undiscovered identity never matches the
    /// workspace's resolved cwd.
    pub(crate) fn from_tree(
        id: WorkspaceId,
        name: Option<crate::Label>,
        identity_cwd: AbsolutePath,
        tree: pane_tree::PaneTree,
    ) -> Self {
        let name = name.unwrap_or_else(|| crate::Label::for_directory(identity_cwd.as_path()));
        Self {
            id,
            name,
            identity_cwd,
            git: GitIdentity::Undiscovered,
            tree,
            spawn_geometry: None,
        }
    }

    /// Builds a workspace around a caller supplied pane without launching a
    /// PTY or discovering a Git identity. The server crate's unit tests need a
    /// real workspace with no spawned process, and this crate's `cfg(test)`
    /// does not reach a dependent crate's tests, so this constructor seam is
    /// public. The test fixture crate cannot own it: mux's own tests depend on
    /// that crate, so it cannot depend on mux. The caller supplies the ID, so
    /// the fixture keeps its IDs unique the way it chooses. The pane is the
    /// workspace's first: public number `FIRST`. The caller hands over a
    /// terminal, not a `PaneRecord`, so there is no caller-chosen number to
    /// keep or overwrite: the one-pane tree numbers its pane as a new
    /// workspace's first pane is numbered. A `name` that is blank once trimmed
    /// names the workspace after `identity_cwd`, as a blank rename does.
    pub fn test_from_pane(
        id: WorkspaceId,
        name: Option<String>,
        identity_cwd: &AbsolutePath,
        pane: PaneId,
        terminal: crate::terminal::TerminalState,
    ) -> Self {
        Self::from_tree(
            id,
            name.and_then(crate::Label::new),
            identity_cwd.clone(),
            pane_tree::PaneTree::single(pane, terminal),
        )
    }

    /// Stable public workspace identity.
    pub fn id(&self) -> WorkspaceId {
        self.id
    }

    /// The workspace name every consumer (API workspace info, sidebar, window
    /// title) reads.
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// The name as the label a save writes.
    pub fn name_label(&self) -> &crate::Label {
        &self.name
    }

    /// Renames the workspace. True when the name changed.
    pub fn set_name(&mut self, name: crate::Label) -> bool {
        let changed = self.name != name;
        self.name = name;
        changed
    }

    pub fn identity_cwd(&self) -> &AbsolutePath {
        &self.identity_cwd
    }

    /// The geometry recorded for this workspace, if the server has applied
    /// one to it.
    pub fn spawn_geometry(&self) -> Option<SpawnGeometry> {
        self.spawn_geometry
    }

    /// Records the geometry the server just applied this workspace's PTYs in,
    /// or spawned its first pane at.
    pub fn record_spawn_geometry(&mut self, geometry: SpawnGeometry) {
        self.spawn_geometry = Some(geometry);
    }

    pub fn git_status_key_for_cwd(&self, cwd: &Path) -> Option<&GitStatusKey> {
        match &self.git {
            GitIdentity::Admitted(status) if status.cwd == cwd => Some(&status.key),
            GitIdentity::Undiscovered | GitIdentity::Admitted(_) => None,
        }
    }

    /// Applies a Git status the refresh answered for this workspace, only
    /// while the cwd it was read for is still `current_cwd`. The caller has
    /// already matched the answer to this workspace. The return value
    /// describes visible change rather than changes to discovery bookkeeping.
    pub fn apply_git_status(
        &mut self,
        status: GitStatus,
        current_cwd: Option<&Path>,
    ) -> SurfaceChange {
        if current_cwd != Some(status.cwd.as_path()) {
            return SurfaceChange::Unchanged;
        }
        let next = GitIdentity::Admitted(status);
        let changed = self.git.branch().and_then(GitBranch::as_deref)
            != next.branch().and_then(GitBranch::as_deref)
            || self.git.ahead_behind() != next.ahead_behind();
        self.git = next;
        if changed {
            SurfaceChange::Changed
        } else {
            SurfaceChange::Unchanged
        }
    }

    pub fn branch(&self) -> Option<&str> {
        self.branch_state().and_then(GitBranch::as_deref)
    }

    pub fn branch_state(&self) -> Option<&GitBranch> {
        self.git.branch()
    }

    pub fn git_ahead_behind(&self) -> Option<AheadBehind> {
        self.git.ahead_behind()
    }

    /// The layout and the pane records, for reads.
    pub fn tree(&self) -> &pane_tree::PaneTree {
        &self.tree
    }

    pub fn pane_mut(&mut self, pane: PaneId) -> Option<&mut pane_tree::PaneRecord> {
        self.tree.pane_mut(pane)
    }

    /// Focuses `pane`. False when it is not in this workspace.
    pub fn focus_pane(&mut self, pane: PaneId) -> bool {
        self.tree.focus(pane)
    }

    pub fn swap_panes(&mut self, first: PaneId, second: PaneId) -> bool {
        self.tree.swap(first, second)
    }

    pub fn resize_pane(
        &mut self,
        pane: PaneId,
        nav: NavDirection,
        delta: RatioDelta,
        area: shepr_core::geometry::Rect,
    ) -> bool {
        self.tree.resize(pane, nav, delta, area)
    }

    pub fn set_split_ratio(&mut self, path: &SplitPath, ratio: SplitRatio) -> bool {
        self.tree.set_split_ratio(path, ratio)
    }

    /// Zooms or unzooms the workspace. A zoom needs a second pane to hide:
    /// `false`, with the workspace unchanged, when asked to zoom a workspace
    /// of one pane. Unzooming always succeeds.
    pub fn set_zoomed(&mut self, zoomed: bool) -> bool {
        self.tree.set_zoomed(zoomed)
    }

    /// Removes a pane that is not the workspace's last;
    /// `WorkspaceSet::remove_pane` removes the workspace instead when it is.
    /// The runtime is left to the caller.
    pub fn remove_pane(
        &mut self,
        pane: PaneId,
    ) -> Result<pane_tree::PaneRecord, pane_tree::RemoveRefusal> {
        self.tree.remove(pane)
    }

    /// App-side convenience for resolving the live workspace identity. This
    /// may read the root pane's process cwd through its runtime; state reducers
    /// should instead receive the observed cwd and call
    /// `resolved_identity_cwd_from_root_pane`.
    pub fn resolved_identity_cwd(&self, runtimes: &PaneRuntimeRegistry) -> AbsolutePath {
        let root_cwd = self.cwd_for_pane(self.tree.root(), runtimes);
        self.resolved_identity_cwd_from_root_pane(root_cwd)
    }

    /// Resolves the workspace identity from a root pane cwd already observed
    /// by the App. This stays as data-only path selection so state reducers can
    /// compare cwd snapshots without probing a pane runtime. It always has an
    /// answer, the construction cwd being the fallback.
    pub fn resolved_identity_cwd_from_root_pane(
        &self,
        root_pane_cwd: Option<AbsolutePath>,
    ) -> AbsolutePath {
        root_pane_cwd.unwrap_or_else(|| self.identity_cwd.clone())
    }

    /// The cwd of `pane`: its runtime's observation, else its stored report.
    /// `None` when the pane is not in this workspace.
    pub fn cwd_for_pane(
        &self,
        pane: PaneId,
        runtimes: &PaneRuntimeRegistry,
    ) -> Option<AbsolutePath> {
        let terminal = self.tree.pane(pane)?.terminal();
        terminal_cwd(runtimes.get(&pane), Some(terminal), CwdPurpose::Identity)
    }

    pub fn foreground_cwd_for_pane(
        &self,
        pane: PaneId,
        runtimes: &PaneRuntimeRegistry,
    ) -> Option<PathBuf> {
        self.tree.pane(pane)?;
        runtimes.get(&pane).and_then(PaneRuntime::foreground_cwd)
    }
}

#[cfg(test)]
use shepr_core::layout::Direction;

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
        .try_allocate()
        .expect("test workspace ID space available")
}

#[cfg(test)]
impl Workspace {
    pub fn test_new(name: &str) -> Self {
        let identity_cwd = TEST_WORKSPACE_CWD
            .with(|cwd| AbsolutePath::new(cwd.to_path_buf()))
            .expect("the test workspace cwd is absolute");
        let terminal = crate::terminal::TerminalState::new(identity_cwd.clone());
        Self::from_tree(
            test_workspace_id(),
            crate::Label::new(name),
            identity_cwd,
            pane_tree::PaneTree::single(PaneId::alloc(), terminal),
        )
    }

    /// Splits the focused pane and focuses the new one, as the server does.
    pub fn test_split(&mut self, direction: Direction) -> PaneId {
        let chrome = WorkspaceChrome {
            area: shepr_core::geometry::Rect::new(0, 0, 80, 24),
            pane_gaps: false,
            pane_scrollbars: false,
        };
        let target = self.tree.focused();
        let prepared = self
            .prepare_split(target, direction, &chrome, None, self.identity_cwd.clone())
            .expect("the focused pane can be split");
        self.commit_split(prepared)
            .expect("a split prepared just now commits")
    }

    pub fn test_adversarial_identity_state() -> Self {
        let mut ws = Self::test_new("adversarial-identity");
        let removed_pane = ws.test_split(Direction::Horizontal);
        ws.test_split(Direction::Vertical);
        assert!(ws.close_pane(removed_pane).is_some());
        let _unused_raw_id = PaneId::alloc();
        let later_pane = ws.test_split(Direction::Horizontal);

        assert_ne!(
            later_pane.raw() as usize,
            ws.tree()
                .pane(later_pane)
                .expect("test pane has a record")
                .number()
                .get(),
            "adversarial pane must distinguish raw pane id from public pane number"
        );
        ws
    }

    pub fn close_pane(&mut self, pane: PaneId) -> Option<pane_tree::PaneRecord> {
        self.remove_pane(pane).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::TerminalState;
    use shepr_protocol::{PanePublicNumber, PublicPaneId};

    fn terminal_at(cwd: &str) -> TerminalState {
        TerminalState::new(AbsolutePath::new(cwd).expect("test cwd is absolute"))
    }

    /// A one-pane workspace created without a name (so named after
    /// `identity_cwd`) whose root pane's terminal reports `terminal_cwd`.
    fn workspace_at(identity_cwd: &Path, terminal_cwd: &str) -> Workspace {
        Workspace::test_from_pane(
            test_workspace_id(),
            None,
            &AbsolutePath::new(identity_cwd).expect("test cwd is absolute"),
            PaneId::alloc(),
            terminal_at(terminal_cwd),
        )
    }

    fn number_of(ws: &Workspace, pane: PaneId) -> Option<usize> {
        ws.tree()
            .pane(pane)
            .map(PaneRecord::number)
            .map(PanePublicNumber::get)
    }

    #[test]
    fn public_pane_ids_use_the_canonical_format() {
        let workspace_id: WorkspaceId = "wA".parse().expect("canonical workspace id");
        let pane_id = PublicPaneId::new(
            &workspace_id,
            PanePublicNumber::new(33).expect("nonzero literal"),
        );

        assert_eq!(pane_id.to_string(), "wA:p11");
        assert_eq!("wA:p11".parse::<PublicPaneId>(), Ok(pane_id));
        assert!("wA:p".parse::<PublicPaneId>().is_err());
        assert!("wA:t0".parse::<PublicPaneId>().is_err());
        assert!("wA:1".parse::<PublicPaneId>().is_err());
    }

    #[test]
    fn public_numbers_round_trip_readable_base32_handles() {
        let workspace = WorkspaceId::from_number(1).expect("nonzero number");
        let id = |value: usize| {
            PublicPaneId::new(&workspace, PanePublicNumber::new(value).expect("nonzero"))
        };
        for (value, text) in [
            (1, "w1:p1"),
            (9, "w1:p9"),
            (10, "w1:pA"),
            (31, "w1:pZ"),
            (32, "w1:p0"),
            (33, "w1:p11"),
        ] {
            assert_eq!(id(value).to_string(), text);
        }

        for value in [1, 9, 10, 31, 32, 33, 1024, 1025] {
            let pane = id(value);
            assert_eq!(pane.to_string().parse::<PublicPaneId>(), Ok(pane));
        }
    }

    #[test]
    fn every_public_number_round_trips() {
        let workspace = WorkspaceId::from_number(1).expect("nonzero number");
        for value in (1..=2048).chain([usize::MAX]) {
            let pane =
                PublicPaneId::new(&workspace, PanePublicNumber::new(value).expect("nonzero"));
            let text = pane.to_string();
            assert_eq!(
                text.parse::<PublicPaneId>(),
                Ok(pane),
                "{value} as {text:?}"
            );
        }
    }

    #[test]
    fn pane_public_numbers_are_stable_and_not_reused_after_close() {
        let mut ws = Workspace::test_new("test");
        let root = ws.tree().root();
        let second = ws.test_split(Direction::Horizontal);
        let third = ws.test_split(Direction::Vertical);

        assert_eq!(number_of(&ws, root), Some(1));
        assert_eq!(number_of(&ws, second), Some(2));
        assert_eq!(number_of(&ws, third), Some(3));

        assert!(ws.close_pane(second).is_some());

        assert_eq!(number_of(&ws, root), Some(1));
        assert_eq!(number_of(&ws, second), None);
        assert_eq!(number_of(&ws, third), Some(3));

        let fourth = ws.test_split(Direction::Horizontal);
        assert_eq!(number_of(&ws, fourth), Some(4));
    }

    #[test]
    fn shows_pane_follows_zoom_and_focus() {
        let mut ws = Workspace::test_new("test");
        let root = ws.tree().root();
        let second = ws.test_split(Direction::Horizontal);
        assert!(ws.tree().shows(root));
        assert!(ws.tree().shows(second));
        assert!(!ws.tree().shows(PaneId::alloc()));

        assert!(ws.focus_pane(second));
        assert!(ws.set_zoomed(true));
        assert!(ws.tree().shows(second));
        assert!(!ws.tree().shows(root));
        assert_eq!(ws.tree().visible_pane_ids(), vec![second]);
    }

    #[test]
    fn a_one_pane_workspace_refuses_to_zoom() {
        let mut ws = Workspace::test_new("test");

        assert!(!ws.set_zoomed(true));
        assert!(!ws.tree().zoomed());

        // Unzooming is always accepted.
        assert!(ws.set_zoomed(false));
        assert!(!ws.tree().zoomed());

        ws.test_split(Direction::Horizontal);
        assert!(ws.set_zoomed(true));
        assert!(ws.tree().zoomed());
    }

    #[test]
    fn closing_down_to_one_pane_clears_the_zoom() {
        let mut ws = Workspace::test_new("test");
        let second = ws.test_split(Direction::Horizontal);
        assert!(ws.set_zoomed(true));

        assert!(ws.close_pane(second).is_some());

        assert!(!ws.tree().zoomed());
    }

    #[test]
    fn adversarial_identity_state_keeps_raw_ids_and_public_numbers_apart_after_mutation() {
        let mut ws = Workspace::test_adversarial_identity_state();

        let divergent_pane = ws
            .tree()
            .panes()
            .find_map(|(pane, record)| {
                (pane.raw() as usize != record.number().get()).then_some(pane)
            })
            .expect("adversarial state should contain raw/public pane divergence");
        assert_ne!(
            divergent_pane.raw() as usize,
            number_of(&ws, divergent_pane).expect("test precondition")
        );

        let new_pane = ws.test_split(Direction::Vertical);
        assert!(number_of(&ws, new_pane).is_some());
    }

    #[test]
    fn an_unnamed_workspace_is_named_after_its_cwd_and_keeps_that_name() {
        let mut ws = workspace_at(Path::new("/home/someone"), "/elsewhere/entirely");
        assert_eq!(ws.name(), "someone");

        // A Git status for a moved cwd changes the branch, never the name.
        let cwd = PathBuf::from("/elsewhere/entirely");
        let status = GitStatus {
            cwd: cwd.clone(),
            key: GitStatusKey::Checkout(cwd.clone()),
            branch: GitBranch::Detached,
            ahead_behind: None,
        };
        ws.apply_git_status(status, Some(&cwd));
        assert_eq!(ws.name(), "someone");
    }

    #[test]
    fn renaming_reports_whether_the_name_changed() {
        let mut ws = Workspace::test_new("first");
        let label = |name| crate::Label::new(name).expect("test label");

        assert!(!ws.set_name(label("first")));
        assert!(ws.set_name(label("second")));
        assert_eq!(ws.name(), "second");
    }

    #[test]
    fn a_directory_named_only_with_spaces_names_its_workspace_by_path() {
        let ws = workspace_at(Path::new("/srv/  "), "/srv/  ");
        assert_eq!(ws.name(), "/srv/");
        assert_eq!(workspace_at(Path::new("/"), "/").name(), "/");
    }

    #[test]
    fn resolved_identity_cwd_keeps_a_stored_path_ending_with_deleted_text() {
        let registry = PaneRuntimeRegistry::default();
        let identity = Path::new("/shepr-test/construction");
        let stored = Path::new("/shepr-test/real (deleted)");

        let ws = workspace_at(identity, stored.to_str().expect("UTF-8 test path"));
        assert_eq!(ws.resolved_identity_cwd(&registry), stored.to_path_buf());
    }

    #[test]
    fn cwd_purposes_preserve_missing_absolute_state_without_filesystem_checks() {
        let terminal = terminal_at("/shepr-test-missing-cwd/agent");
        for purpose in [
            super::CwdPurpose::Identity,
            super::CwdPurpose::FollowForNewPane,
            super::CwdPurpose::Save,
        ] {
            assert_eq!(
                super::terminal_cwd(None, Some(&terminal), purpose),
                Some(terminal.cwd().clone()),
            );
        }
    }

    /// A childless runtime: no /proc cwd and no foreground group, so each
    /// purpose's observation is exactly its arbitration over the seeded state.
    fn runtime_with_cwd_state(
        reported: Option<(&str, Option<&str>)>,
        remembered: Option<&str>,
    ) -> PaneRuntime {
        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        runtime.test_seed_cwd_state(
            reported.map(|(path, shell)| (PathBuf::from(path), shell.map(PathBuf::from))),
            remembered.map(PathBuf::from),
        );
        runtime
    }

    fn cwd_for(runtime: &PaneRuntime, terminal: &TerminalState, purpose: CwdPurpose) -> PathBuf {
        terminal_cwd(Some(runtime), Some(terminal), purpose)
            .expect("a usable cwd")
            .to_path_buf()
    }

    #[test]
    fn each_cwd_purpose_reads_its_own_runtime_observation_over_the_stored_report() {
        // The OSC 7 report was taken with the shell in /shell, and the save
        // remembered /saved since, so the save arbitration keeps its own
        // observation while identity and follow take the report.
        let runtime = runtime_with_cwd_state(Some(("/osc", Some("/shell"))), Some("/saved"));
        let terminal = terminal_at("/stored");

        assert_eq!(
            cwd_for(&runtime, &terminal, CwdPurpose::Identity),
            PathBuf::from("/osc")
        );
        assert_eq!(
            cwd_for(&runtime, &terminal, CwdPurpose::FollowForNewPane),
            PathBuf::from("/osc")
        );
        assert_eq!(
            cwd_for(&runtime, &terminal, CwdPurpose::Save),
            PathBuf::from("/saved")
        );

        // With no terminal state at all, the observation alone answers.
        assert_eq!(
            terminal_cwd(Some(&runtime), None, CwdPurpose::Save).as_deref(),
            Some(Path::new("/saved"))
        );
    }

    #[test]
    fn save_purpose_takes_the_report_when_the_remembered_cwd_is_where_it_was_made() {
        // The save remembered the very /proc cwd the report was sampled at:
        // the shell has not moved since, so the logical OSC 7 path wins.
        let runtime = runtime_with_cwd_state(Some(("/osc", Some("/shell"))), Some("/shell"));
        let terminal = terminal_at("/stored");

        assert_eq!(
            cwd_for(&runtime, &terminal, CwdPurpose::Save),
            PathBuf::from("/osc")
        );
    }

    #[test]
    fn save_purpose_uses_the_remembered_cwd_without_any_report() {
        let runtime = runtime_with_cwd_state(None, Some("/saved"));
        let terminal = terminal_at("/stored");

        assert_eq!(
            cwd_for(&runtime, &terminal, CwdPurpose::Save),
            PathBuf::from("/saved")
        );
        // Identity and follow never read the save's memory.
        assert_eq!(
            cwd_for(&runtime, &terminal, CwdPurpose::Identity),
            PathBuf::from("/stored")
        );
        assert_eq!(
            cwd_for(&runtime, &terminal, CwdPurpose::FollowForNewPane),
            PathBuf::from("/stored")
        );
    }

    #[test]
    fn every_purpose_falls_back_to_the_stored_report_when_the_runtime_has_none() {
        let runtime = runtime_with_cwd_state(None, None);
        let terminal = terminal_at("/stored");
        for purpose in [
            CwdPurpose::Identity,
            CwdPurpose::FollowForNewPane,
            CwdPurpose::Save,
        ] {
            assert_eq!(
                cwd_for(&runtime, &terminal, purpose),
                PathBuf::from("/stored")
            );
        }
        assert_eq!(
            terminal_cwd(Some(&runtime), None, CwdPurpose::Identity),
            None
        );
    }

    #[test]
    fn every_purpose_keeps_paths_ending_with_deleted_text_as_stored_state() {
        let terminal = terminal_at("/stored");
        let path = "/gone (deleted)";
        let runtime = runtime_with_cwd_state(Some((path, None)), Some(path));
        for purpose in [
            CwdPurpose::Identity,
            CwdPurpose::FollowForNewPane,
            CwdPurpose::Save,
        ] {
            assert_eq!(cwd_for(&runtime, &terminal, purpose), PathBuf::from(path));
        }

        let relative = runtime_with_cwd_state(Some(("relative/cwd", None)), Some("relative/cwd"));
        for purpose in [
            CwdPurpose::Identity,
            CwdPurpose::FollowForNewPane,
            CwdPurpose::Save,
        ] {
            assert_eq!(
                cwd_for(&relative, &terminal, purpose),
                PathBuf::from("/stored")
            );
        }

        let stored = terminal_at(path);
        assert_eq!(
            terminal_cwd(None, Some(&stored), CwdPurpose::Save),
            Some(stored.cwd().clone())
        );
    }

    #[test]
    fn cwd_for_pane_reads_the_registered_runtime_over_the_stored_report() {
        let ws = workspace_at(Path::new("/shepr-test/identity"), "/stored");
        let root = ws.tree().root();
        let mut registry = PaneRuntimeRegistry::default();
        registry.insert(
            root,
            runtime_with_cwd_state(Some(("/osc", None)), Some("/saved")),
        );

        assert_eq!(
            ws.cwd_for_pane(root, &registry).as_deref(),
            Some(Path::new("/osc"))
        );
        assert_eq!(ws.resolved_identity_cwd(&registry), PathBuf::from("/osc"));
    }

    #[test]
    fn cwd_query_keeps_deleted_text_in_stored_state() {
        let terminal = terminal_at("/gone (deleted)");
        assert_eq!(
            super::terminal_cwd(None, Some(&terminal), super::CwdPurpose::Save),
            Some(terminal.cwd().clone()),
        );
    }

    #[test]
    fn workspace_identity_follows_root_pane_cwd() {
        let ws = workspace_at(Path::new("/shepr-test/identity"), "/shepr-test/pion");

        assert_eq!(
            ws.resolved_identity_cwd(&PaneRuntimeRegistry::default()),
            PathBuf::from("/shepr-test/pion")
        );
        assert_eq!(
            ws.cwd_for_pane(ws.tree().root(), &PaneRuntimeRegistry::default())
                .as_deref(),
            Some(Path::new("/shepr-test/pion"))
        );
        assert_eq!(
            ws.cwd_for_pane(PaneId::alloc(), &PaneRuntimeRegistry::default()),
            None
        );
    }

    #[test]
    fn resolved_identity_cwd_from_root_pane_uses_observation_or_identity_fallback() {
        let ws = workspace_at(Path::new("/saved/workspace"), "/shepr-test/unused");

        assert_eq!(
            ws.resolved_identity_cwd_from_root_pane(Some(
                AbsolutePath::new("/live/pane").expect("absolute")
            )),
            PathBuf::from("/live/pane")
        );
        assert_eq!(
            ws.resolved_identity_cwd_from_root_pane(None),
            PathBuf::from("/saved/workspace")
        );
        assert_eq!(
            ws.resolved_identity_cwd_from_root_pane(Some(
                AbsolutePath::new("/saved/workspace/real (deleted)").expect("absolute")
            )),
            PathBuf::from("/saved/workspace/real (deleted)")
        );
    }

    #[test]
    fn hidden_cache_changes_are_admitted_without_surface_change() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let mut ws = Workspace::test_new("custom");
        let cwd = ws.identity_cwd().to_path_buf();
        let status = GitStatus {
            cwd: cwd.clone(),
            key: GitStatusKey::Checkout(PathBuf::from("/checkout")),
            branch: GitBranch::Detached,
            ahead_behind: None,
        };

        assert_eq!(
            ws.apply_git_status(status, Some(&cwd)),
            SurfaceChange::Unchanged
        );
        assert_eq!(ws.name(), "custom");
        assert_eq!(
            ws.git_status_key_for_cwd(&cwd),
            Some(&GitStatusKey::Checkout(PathBuf::from("/checkout")))
        );
        assert_eq!(ws.branch_state(), Some(&GitBranch::Detached));
    }

    #[test]
    fn failed_branch_read_updates_identity_without_changing_branch_presentation() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let mut ws = Workspace::test_new("custom");
        let cwd = ws.identity_cwd().to_path_buf();
        let mut status = GitStatus {
            cwd: cwd.clone(),
            key: GitStatusKey::Outside(cwd.clone()),
            branch: GitBranch::Detached,
            ahead_behind: None,
        };
        ws.apply_git_status(status.clone(), Some(&cwd));
        status.branch = GitBranch::ReadFailed;

        assert_eq!(
            ws.apply_git_status(status, Some(&cwd)),
            SurfaceChange::Unchanged
        );
        assert_eq!(ws.branch_state(), Some(&GitBranch::ReadFailed));
        assert_eq!(ws.branch(), None);
    }

    #[test]
    fn undiscovered_identity_has_no_git_status_key_for_its_cwd() {
        let ws = workspace_at(Path::new("/shepr-test/repo/sub"), "/shepr-test/repo/sub");

        assert_eq!(ws.name(), "sub");
        assert_eq!(ws.branch(), None);
        assert_eq!(ws.git_status_key_for_cwd(ws.identity_cwd()), None);
    }

    #[test]
    fn workspace_built_from_an_existing_pane_does_not_discover_git_identity() {
        let pane = PaneId::alloc();
        // A path that cannot exist: discovery would have to stat it.
        let cwd = AbsolutePath::new("/shepr-test-nonexistent/repo/sub").expect("absolute");

        let ws = Workspace::test_from_pane(
            test_workspace_id(),
            None,
            &cwd,
            pane,
            terminal_at("/shepr-test-nonexistent/repo/sub"),
        );

        assert_eq!(ws.name(), "sub");
        assert_eq!(ws.tree().len(), 1);
        assert_eq!(number_of(&ws, pane), Some(1));
        assert_eq!(ws.tree().root(), pane);
        assert_eq!(ws.tree().next_number(), PanePublicNumber::SECOND);
    }
}
