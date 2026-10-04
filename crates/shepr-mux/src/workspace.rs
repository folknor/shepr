use std::path::{Path, PathBuf};

use shepr_core::absolute_path::AbsolutePath;

use crate::git::{AheadBehind, GitBranch, GitStatus, GitStatusKey};
use crate::pane::{PaneRuntime, PaneRuntimeRegistry};
use shepr_core::layout::{NavDirection, PaneId, RatioDelta, SplitPath, SplitRatio};
use shepr_git::fallback_label_from_cwd;
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
    // Each source is filtered on its own, so an unusable observation falls
    // back to the stored report instead of hiding it.
    let usable = |path: &AbsolutePath| !process_cwd_is_deleted(path);
    observed
        .and_then(|path| AbsolutePath::new(path).ok())
        .filter(usable)
        .or_else(|| stored.filter(usable))
}

pub(crate) fn process_cwd_is_deleted(path: &Path) -> bool {
    // The kernel adds this marker to a /proc cwd link after its directory is
    // removed. Reject it without statting the path on the event loop.
    path.as_os_str().as_encoded_bytes().ends_with(b" (deleted)")
}

mod aggregate;
mod geometry;
mod pane_tree;
mod set;
mod shape;

pub use self::geometry::{SpawnGeometry, WorkspaceChrome, spawn_geometry};
pub use self::pane_tree::{
    PaneRecord, PaneTree, PreparedSplit, RemoveRefusal, SavedTreeState, SplitRefused, TreePlan,
    TreeRejection,
};
pub use self::set::{
    PaneRef, PaneRemoval, PaneRemovalScope, PreparedWorkspace, WorkspaceIdAllocator, WorkspaceSet,
};
pub use self::shape::Shape;

/// Only admitted identities can supply a refresh cache hint. The fallback
/// label has no cwd or Git key and cannot accidentally match an empty path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GitIdentity {
    Undiscovered { fallback_label: String },
    Admitted(GitStatus),
}

impl GitIdentity {
    fn label(&self) -> &str {
        match self {
            Self::Undiscovered { fallback_label } => fallback_label,
            Self::Admitted(status) => &status.label,
        }
    }

    fn branch(&self) -> Option<&GitBranch> {
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
    id: WorkspaceId,
    /// User-provided label override; Git identity still refreshes.
    custom_name: Option<String>,
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
    /// A workspace around a pane tree. The Git identity (workspace label and
    /// status) is left undiscovered: finding it walks the filesystem up to `/`
    /// and can spawn `git`, which must not run on the server's main loop. The
    /// background Git refresh discovers it, because an undiscovered identity
    /// never matches the workspace's resolved cwd.
    pub(crate) fn from_tree(
        id: WorkspaceId,
        custom_name: Option<String>,
        identity_cwd: AbsolutePath,
        tree: pane_tree::PaneTree,
    ) -> Self {
        let git = GitIdentity::Undiscovered {
            fallback_label: fallback_label_from_cwd(&identity_cwd),
        };
        Self {
            id,
            custom_name,
            identity_cwd,
            git,
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
    /// workspace's first: public number `FIRST`.
    pub fn test_from_pane(
        id: WorkspaceId,
        label: Option<String>,
        identity_cwd: &AbsolutePath,
        pane: PaneId,
        terminal: crate::terminal::TerminalState,
    ) -> Self {
        Self::from_tree(
            id,
            label,
            identity_cwd.clone(),
            pane_tree::PaneTree::single(pane, terminal),
        )
    }

    /// Stable public workspace identity.
    pub fn id(&self) -> WorkspaceId {
        self.id
    }

    pub fn custom_name(&self) -> Option<&str> {
        self.custom_name.as_deref()
    }

    /// Sets or clears the label override. True when the name changed.
    pub fn set_custom_name(&mut self, name: Option<String>) -> bool {
        let changed = self.custom_name != name;
        self.custom_name = name;
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

    pub fn matches_identity_cwd(&self, cwd: &Path) -> bool {
        matches!(&self.git, GitIdentity::Admitted(status) if status.cwd == cwd)
    }

    pub fn git_status_key_for_cwd(&self, cwd: &Path) -> Option<&GitStatusKey> {
        match &self.git {
            GitIdentity::Admitted(status) if self.matches_identity_cwd(cwd) => Some(&status.key),
            GitIdentity::Undiscovered { .. } | GitIdentity::Admitted(_) => None,
        }
    }

    /// Applies a Git status the refresh answered for this workspace, only
    /// while the cwd it was read for is still `current_cwd`. The caller has
    /// already matched the answer to this workspace. The return value
    /// describes visible change, including the custom label override, rather
    /// than changes to discovery bookkeeping.
    pub fn apply_git_status(
        &mut self,
        status: GitStatus,
        current_cwd: Option<&Path>,
    ) -> SurfaceChange {
        if current_cwd != Some(status.cwd.as_path()) {
            return SurfaceChange::Unchanged;
        }
        let next = GitIdentity::Admitted(status);
        let changed = self.display_label(&self.git) != self.display_label(&next)
            || self.git.branch().and_then(GitBranch::as_deref)
                != next.branch().and_then(GitBranch::as_deref)
            || self.git.ahead_behind() != next.ahead_behind();
        self.git = next;
        if changed {
            SurfaceChange::Changed
        } else {
            SurfaceChange::Unchanged
        }
    }

    /// The workspace label: the custom name, else the automatic label cached
    /// from the last admitted Git identity. Every consumer (API workspace
    /// info, sidebar, window title) reads this one value, so they cannot
    /// disagree. The cache follows the workspace's resolved cwd
    /// (`resolved_identity_cwd`) through the background Git refresh, which
    /// re-derives it whenever that cwd moves; reading it does no IO.
    pub fn display_name(&self) -> &str {
        self.display_label(&self.git)
    }

    fn display_label<'a>(&'a self, identity: &'a GitIdentity) -> &'a str {
        self.custom_name
            .as_deref()
            .unwrap_or_else(|| identity.label())
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
    /// compare cwd snapshots without probing a pane runtime.
    pub fn resolved_identity_cwd_from_root_pane(
        &self,
        root_pane_cwd: Option<AbsolutePath>,
    ) -> AbsolutePath {
        root_pane_cwd
            .filter(|cwd| !process_cwd_is_deleted(cwd))
            .unwrap_or_else(|| self.identity_cwd.clone())
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
        .allocate()
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
            Some(name.to_string()),
            identity_cwd,
            pane_tree::PaneTree::single(PaneId::alloc(), terminal),
        )
    }

    /// Splits the focused pane and focuses the new one, as the server does.
    pub fn test_split(&mut self, direction: Direction) -> PaneId {
        let chrome = WorkspaceChrome {
            area: shepr_core::geometry::Rect::new(0, 0, 80, 24),
            pane_borders: shepr_config::PaneBordersConfig::Off,
            pane_gaps: false,
            pane_outer_borders: false,
            pane_scrollbars: false,
        };
        let target = self.tree.focused();
        let prepared = self
            .prepare_split(target, direction, &chrome, None, self.identity_cwd.clone())
            .expect("the focused pane is in the tree");
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

    /// A one-pane workspace with no custom name whose identity cwd is `cwd`
    /// and whose root pane's terminal reports `terminal_cwd`.
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
    fn display_name_borrows_the_cached_label() {
        let mut ws = Workspace::test_new("custom");
        let name = ws.custom_name().expect("a named workspace");
        assert!(std::ptr::eq(name.as_ptr(), ws.display_name().as_ptr()));

        ws.set_custom_name(None);
        let cwd = ws.identity_cwd().to_path_buf();
        let status = GitStatus {
            cwd: cwd.clone(),
            key: GitStatusKey::Checkout(cwd.clone()),
            label: "cached-label".into(),
            branch: GitBranch::Detached,
            ahead_behind: None,
        };
        ws.apply_git_status(status, Some(&cwd));

        let label = ws.display_name();
        assert_eq!(label, "cached-label");
        let GitIdentity::Admitted(admitted) = &ws.git else {
            panic!("the status was admitted");
        };
        assert!(std::ptr::eq(label.as_ptr(), admitted.label.as_ptr()));
    }

    #[test]
    fn renaming_reports_whether_the_name_changed() {
        let mut ws = Workspace::test_new("first");

        assert!(!ws.set_custom_name(Some("first".into())));
        assert!(ws.set_custom_name(Some("second".into())));
        assert_eq!(ws.custom_name(), Some("second"));
        assert!(ws.set_custom_name(None));
        assert!(!ws.set_custom_name(None));
        assert_eq!(ws.custom_name(), None);
    }

    #[test]
    fn resolved_identity_cwd_falls_back_to_the_construction_cwd() {
        let registry = PaneRuntimeRegistry::new();
        let identity = Path::new("/shepr-test/construction");

        // A root pane that reports no usable directory. A relative stored
        // cwd cannot occur: the terminal state holds an `AbsolutePath`.
        let ws = workspace_at(identity, "/gone (deleted)");
        assert_eq!(ws.resolved_identity_cwd(&registry), identity);

        // A usable stored report wins.
        let ws = workspace_at(identity, "/shepr-test/pion");
        assert_eq!(
            ws.resolved_identity_cwd(&registry),
            PathBuf::from("/shepr-test/pion")
        );
    }

    #[test]
    fn display_name_reads_cached_identity_without_rechecking_filesystem() {
        let root = crate::test_support::ScratchDir::new("label-cache");
        let cwd = root.join("deep/nested");
        std::fs::create_dir_all(&cwd).expect("create nested cwd");

        let mut ws = workspace_at(&cwd, "/shepr-test/unused");
        let status = shepr_git::GitStatusSnapshot {
            repo_root: Some(PathBuf::from("/cached-repo")),
            branch: GitBranch::Detached,
            ahead_behind: None,
        }
        .into_status(cwd.clone(), GitStatusKey::Checkout(cwd.clone()));
        ws.apply_git_status(status, Some(&cwd));

        std::fs::remove_dir_all(root).expect("remove cwd after cache admission");

        assert_eq!(ws.display_name(), "cached-repo");
    }

    #[test]
    fn label_is_the_admitted_identity_even_when_the_live_cwd_has_moved() {
        // A subdirectory `cd` without OSC 7 used to make the API show the
        // subdirectory basename while the window title showed the repo name.
        // Both now read the one cached label until the background refresh
        // admits the new cwd.
        let mut ws = workspace_at(Path::new("/old/workspace"), "/new/repo/deep");
        let cwd = PathBuf::from("/new/repo");
        let status = shepr_git::GitStatusSnapshot {
            repo_root: Some(cwd.clone()),
            branch: GitBranch::Detached,
            ahead_behind: None,
        }
        .into_status(cwd.clone(), GitStatusKey::Checkout(cwd.clone()));
        ws.apply_git_status(status, Some(&cwd));

        assert_eq!(
            ws.resolved_identity_cwd(&PaneRuntimeRegistry::new()),
            PathBuf::from("/new/repo/deep")
        );
        assert_eq!(ws.display_name(), "repo");
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

    #[test]
    fn cwd_query_rejects_deleted_state() {
        let terminal = terminal_at("/gone (deleted)");
        assert_eq!(
            super::terminal_cwd(None, Some(&terminal), super::CwdPurpose::Save),
            None,
        );
    }

    #[test]
    fn workspace_identity_follows_root_pane_cwd() {
        let ws = workspace_at(Path::new("/shepr-test/identity"), "/shepr-test/pion");

        assert_eq!(
            ws.resolved_identity_cwd(&PaneRuntimeRegistry::new()),
            PathBuf::from("/shepr-test/pion")
        );
        assert_eq!(
            ws.cwd_for_pane(ws.tree().root(), &PaneRuntimeRegistry::new())
                .as_deref(),
            Some(Path::new("/shepr-test/pion"))
        );
        assert_eq!(
            ws.cwd_for_pane(PaneId::alloc(), &PaneRuntimeRegistry::new()),
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
    }

    #[test]
    fn hidden_label_and_cache_changes_are_admitted_without_surface_change() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let mut ws = Workspace::test_new("custom");
        let cwd = ws.identity_cwd().to_path_buf();
        let status = GitStatus {
            cwd: cwd.clone(),
            key: GitStatusKey::Checkout(PathBuf::from("/checkout")),
            label: "automatic".into(),
            branch: GitBranch::Detached,
            ahead_behind: None,
        };

        assert_eq!(
            ws.apply_git_status(status, Some(&cwd)),
            SurfaceChange::Unchanged
        );
        assert_eq!(ws.display_name(), "custom");
        assert_eq!(
            ws.git_status_key_for_cwd(&cwd),
            Some(&GitStatusKey::Checkout(PathBuf::from("/checkout")))
        );
        assert_eq!(ws.branch_state(), Some(&GitBranch::Detached));
        ws.set_custom_name(None);
        assert_eq!(ws.display_name(), "automatic");
    }

    #[test]
    fn failed_branch_read_updates_identity_without_changing_branch_presentation() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let mut ws = Workspace::test_new("custom");
        let cwd = ws.identity_cwd().to_path_buf();
        let mut status = GitStatus {
            cwd: cwd.clone(),
            key: GitStatusKey::Outside(cwd.clone()),
            label: "automatic".into(),
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
    fn undiscovered_identity_labels_by_basename_and_never_matches_a_cwd() {
        let ws = workspace_at(Path::new("/shepr-test/repo/sub"), "/shepr-test/repo/sub");

        assert_eq!(ws.display_name(), "sub");
        assert_eq!(ws.branch(), None);
        assert!(!ws.matches_identity_cwd(ws.identity_cwd()));
        assert!(!ws.matches_identity_cwd(Path::new("")));
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

        assert_eq!(ws.display_name(), "sub");
        assert!(!ws.matches_identity_cwd(ws.identity_cwd()));
        assert_eq!(ws.tree().len(), 1);
        assert_eq!(number_of(&ws, pane), Some(1));
        assert_eq!(ws.tree().root(), pane);
        assert_eq!(ws.tree().next_number(), PanePublicNumber::SECOND);
    }
}
