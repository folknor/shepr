//! A session's workspaces: their order, the allocator their IDs come from and
//! the bookmark.

use std::collections::HashSet;
use std::path::Path;

use super::pane_tree::{PaneRecord, PaneTree};
use super::{SpawnGeometry, Workspace};
use crate::terminal::TerminalState;
use shepr_core::layout::PaneId;
use shepr_protocol::{PanePublicNumber, PublicPaneId, WorkspaceId};

/// Hands out the workspace IDs of one session. The session's `WorkspaceSet`
/// owns the one allocator its workspaces come from, and session restore takes
/// it by `&mut` and moves it past every saved ID before it allocates any, so a
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
            next: WorkspaceId::FIRST.number(),
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

/// The session's bookmark: the workspace saved with the session and where a
/// new client starts, and the index it had when it was last seen. Repaired by
/// the set inside every removal and move.
#[derive(Debug, Clone, Copy)]
struct Bookmark {
    id: WorkspaceId,
    position: usize,
}

/// The session's workspaces in display order, the allocator their IDs come
/// from, and the bookmark. IDs are unique, no pane ID is in two workspaces,
/// and the bookmark's remembered position is its index: `insert` refuses what
/// would break the first two, and every removal and move repairs the third.
pub struct WorkspaceSet {
    workspaces: Vec<Workspace>,
    ids: WorkspaceIdAllocator,
    bookmark: Option<Bookmark>,
}

impl Default for WorkspaceSet {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceSet {
    pub fn new() -> Self {
        Self {
            workspaces: Vec::new(),
            ids: WorkspaceIdAllocator::new(),
            bookmark: None,
        }
    }

    /// Restore's output: duplicates (which restore already prevents) are
    /// dropped with an error log; the allocator is moved past every ID; the
    /// bookmark is seeded from `bookmark`.
    pub fn restored(
        mut ids: WorkspaceIdAllocator,
        workspaces: Vec<Workspace>,
        bookmark: Option<usize>,
    ) -> Self {
        ids.reserve(workspaces.iter().map(|workspace| &workspace.id));
        let mut set = Self {
            workspaces: Vec::with_capacity(workspaces.len()),
            ids,
            bookmark: None,
        };
        for workspace in workspaces {
            if let Err(refused) = set.insert(workspace) {
                tracing::error!(
                    workspace = %refused.id,
                    "dropped a restored workspace that repeats an id or a pane"
                );
            }
        }
        set.seed_bookmark_index(bookmark);
        set
    }

    /// Appends; refuses a present ID or a shared pane ID. Moves the allocator
    /// past the ID.
    pub fn insert(&mut self, workspace: Workspace) -> Result<WorkspaceId, Box<Workspace>> {
        if self
            .workspaces
            .iter()
            .any(|present| present.id == workspace.id)
        {
            return Err(Box::new(workspace));
        }
        let present_panes: HashSet<PaneId> = self
            .workspaces
            .iter()
            .flat_map(|present| present.tree().panes().map(|(pane, _)| pane))
            .collect();
        if workspace
            .tree()
            .panes()
            .any(|(pane, _)| present_panes.contains(&pane))
        {
            return Err(Box::new(workspace));
        }
        let id = workspace.id;
        self.ids.reserve([&id]);
        self.workspaces.push(workspace);
        Ok(id)
    }

    /// A one-pane workspace for a shell about to launch in `cwd`: its ID from
    /// this set's allocator, its pane and terminal state, and its public ID.
    /// Nothing joins the set until `commit_workspace`, so a launch that fails
    /// leaves the set as it was (the ID is simply not used).
    pub fn prepare_workspace(&mut self, cwd: &Path) -> PreparedWorkspace {
        let id = self.ids.allocate();
        let terminal = TerminalState::new(cwd.to_path_buf());
        let tree = PaneTree::single(PaneId::alloc(), terminal);
        PreparedWorkspace {
            workspace: Workspace::from_tree(id, None, cwd.to_path_buf(), tree),
        }
    }

    /// Appends a prepared workspace with the geometry its first pane was
    /// spawned at. Refuses (handing the workspace back) what `insert` refuses.
    pub fn commit_workspace(
        &mut self,
        prepared: PreparedWorkspace,
        geometry: SpawnGeometry,
    ) -> Result<WorkspaceId, Box<Workspace>> {
        let mut workspace = prepared.workspace;
        workspace.record_spawn_geometry(geometry);
        self.insert(workspace)
    }

    /// One-phase pane removal: a non-last pane leaves its workspace; a last
    /// pane takes its workspace with it (and the bookmark repairs as for any
    /// removed workspace). Runtimes are left to the caller. `None` when no
    /// workspace holds `pane`.
    pub fn remove_pane(&mut self, pane: PaneId) -> Option<PaneRemoval> {
        let index = self
            .workspaces
            .iter()
            .position(|workspace| workspace.tree().contains(pane))?;
        let workspace = self.workspaces.get_mut(index)?;
        let workspace_id = workspace.id;
        if workspace.tree().len() > 1 {
            let focused = workspace.tree().focused();
            let record = workspace.remove_pane(pane).ok()?;
            return Some(PaneRemoval {
                workspace_id,
                pane,
                scope: PaneRemovalScope::Pane,
                focus_changed: workspace.tree().focused() != focused,
                removed: vec![(pane, record)],
            });
        }
        let workspace = self.remove(&workspace_id)?;
        Some(PaneRemoval {
            workspace_id,
            pane,
            scope: PaneRemovalScope::Workspace,
            focus_changed: false,
            removed: workspace.tree.into_records(),
        })
    }

    /// Removes a workspace. A bookmark on it moves to the workspace now at its
    /// index, clamped to the last, or to none when the set is empty.
    pub fn remove(&mut self, id: &WorkspaceId) -> Option<Workspace> {
        let index = self.position(id)?;
        let workspace = self.workspaces.remove(index);
        self.repair_bookmark();
        Some(workspace)
    }

    /// Moves `id` to sit before `before`, or to the end when `before` is
    /// `None`. False, with nothing changed, when either is not in the set or
    /// the workspace would stay where it is. The bookmark follows its
    /// workspace.
    pub fn move_before(&mut self, id: &WorkspaceId, before: Option<&WorkspaceId>) -> bool {
        let Some(source) = self.position(id) else {
            return false;
        };
        let insert = match before {
            Some(anchor) => match self.position(anchor) {
                Some(index) => index,
                None => return false,
            },
            None => self.workspaces.len(),
        };
        let target = if source < insert { insert - 1 } else { insert };
        if source == target {
            return false;
        }
        let workspace = self.workspaces.remove(source);
        self.workspaces.insert(target, workspace);
        self.repair_bookmark();
        true
    }

    /// The workspace with `id`. The set holds a handful of workspaces, so this
    /// is a linear scan: a derived index would be one more structure to keep
    /// in step with the order the set already owns.
    pub fn get(&self, id: &WorkspaceId) -> Option<&Workspace> {
        self.workspaces.iter().find(|workspace| &workspace.id == id)
    }

    pub fn get_mut(&mut self, id: &WorkspaceId) -> Option<&mut Workspace> {
        self.workspaces
            .iter_mut()
            .find(|workspace| &workspace.id == id)
    }

    /// Position of the workspace with `id` in the display order. This index is
    /// local to synchronous state access, not an identity to retain across
    /// events.
    pub fn position(&self, id: &WorkspaceId) -> Option<usize> {
        self.workspaces
            .iter()
            .position(|workspace| &workspace.id == id)
    }

    pub fn as_slice(&self) -> &[Workspace] {
        &self.workspaces
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Workspace> {
        self.workspaces.iter()
    }

    pub fn len(&self) -> usize {
        self.workspaces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }

    /// The pane `pane` and the workspace that owns it. One `HashMap` probe per
    /// workspace, for the same reason `get` is a scan.
    pub fn pane(&self, pane: PaneId) -> Option<PaneRef<'_>> {
        self.workspaces.iter().find_map(|workspace| {
            workspace.tree().pane(pane).map(|record| PaneRef {
                workspace,
                id: pane,
                record,
            })
        })
    }

    /// The record of `pane`, in whichever workspace holds it.
    pub fn pane_mut(&mut self, pane: PaneId) -> Option<&mut PaneRecord> {
        self.workspaces
            .iter_mut()
            .find_map(|workspace| workspace.pane_mut(pane))
    }

    /// The pane a public ID names, `None` when its workspace or number is not
    /// in the set. Only the exact stable ID names a workspace.
    pub fn resolve(&self, id: &PublicPaneId) -> Option<PaneRef<'_>> {
        let workspace = self.get(id.workspace_id())?;
        let pane = workspace.tree().pane_by_number(id.number())?;
        workspace.tree().pane(pane).map(|record| PaneRef {
            workspace,
            id: pane,
            record,
        })
    }

    /// Every pane of every workspace with its record, in no particular order
    /// within a workspace.
    pub fn records(&self) -> impl Iterator<Item = (PaneId, &PaneRecord)> {
        self.workspaces
            .iter()
            .flat_map(|workspace| workspace.tree().panes())
    }

    pub fn records_mut(&mut self) -> impl Iterator<Item = (PaneId, &mut PaneRecord)> {
        self.workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.tree.panes_mut())
    }

    /// The bookmarked workspace.
    pub fn bookmark(&self) -> Option<WorkspaceId> {
        self.bookmark.map(|bookmark| bookmark.id)
    }

    /// The current position of the bookmarked workspace.
    pub fn bookmark_index(&self) -> Option<usize> {
        self.bookmark.map(|bookmark| bookmark.position)
    }

    /// Bookmarks workspace `id`, the navigation of an active client. True when
    /// the bookmark moved; false when the workspace is already bookmarked or is
    /// not in the set.
    pub fn set_bookmark(&mut self, id: &WorkspaceId) -> bool {
        let Some(position) = self.position(id) else {
            return false;
        };
        let moved = self.bookmark().as_ref() != Some(id);
        self.bookmark = Some(Bookmark { id: *id, position });
        moved
    }

    /// Bookmarks the workspace at `index` (or nothing): startup and tests seed
    /// it this way.
    pub fn seed_bookmark_index(&mut self, index: Option<usize>) {
        self.bookmark = index.and_then(|position| {
            self.workspaces.get(position).map(|workspace| Bookmark {
                id: workspace.id,
                position,
            })
        });
    }

    /// Brings the bookmark in line with the order after a removal or a move. A
    /// bookmarked workspace still there only has its remembered index
    /// refreshed. One that vanished is replaced by the workspace now at that
    /// index, clamped to the last one, or by nothing when none is left.
    fn repair_bookmark(&mut self) {
        let Some(bookmark) = self.bookmark else {
            return;
        };
        if let Some(position) = self.position(&bookmark.id) {
            self.bookmark = Some(Bookmark {
                position,
                ..bookmark
            });
            return;
        }
        let landed = self
            .workspaces
            .len()
            .checked_sub(1)
            .map(|last| bookmark.position.min(last));
        self.seed_bookmark_index(landed);
    }
}

/// A one-pane workspace planned for a shell about to launch: plain data, no
/// child. The caller launches the root pane from it and commits it with
/// `WorkspaceSet::commit_workspace`.
pub struct PreparedWorkspace {
    workspace: Workspace,
}

impl PreparedWorkspace {
    pub fn id(&self) -> WorkspaceId {
        self.workspace.id
    }

    pub fn root_pane(&self) -> PaneId {
        self.workspace.tree().root()
    }

    /// The id to export to the root pane's child as `SHEPR_PANE_ID`.
    pub fn root_public_id(&self) -> PublicPaneId {
        PublicPaneId::new(&self.workspace.id, PanePublicNumber::FIRST)
    }

    /// Where the root pane's child starts.
    pub fn cwd(&self) -> &Path {
        self.workspace.identity_cwd()
    }
}

/// A pane found through the set, with the workspace that owns it.
#[derive(Clone, Copy)]
pub struct PaneRef<'a> {
    workspace: &'a Workspace,
    id: PaneId,
    record: &'a PaneRecord,
}

impl<'a> PaneRef<'a> {
    pub fn workspace(&self) -> &'a Workspace {
        self.workspace
    }

    pub fn id(&self) -> PaneId {
        self.id
    }

    pub fn record(&self) -> &'a PaneRecord {
        self.record
    }

    pub fn terminal(&self) -> &'a TerminalState {
        self.record.terminal()
    }

    /// The pane's public ID. Every pane in a workspace has a public number, so
    /// this is the ID `SHEPR_PANE_ID` carries.
    pub fn public_id(&self) -> PublicPaneId {
        PublicPaneId::new(&self.workspace.id, self.record.number())
    }
}

/// What a pane removal took out of its workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneRemovalScope {
    /// The pane left its workspace, which keeps its other panes.
    Pane,
    /// The pane was the workspace's last, so the workspace left the set.
    Workspace,
}

/// The result of `WorkspaceSet::remove_pane`.
pub struct PaneRemoval {
    pub workspace_id: WorkspaceId,
    /// The pane the removal was asked for.
    pub pane: PaneId,
    pub scope: PaneRemovalScope,
    /// Pane scope only: whether the workspace's focus moved to another pane.
    /// A removed workspace reports false.
    pub focus_changed: bool,
    /// Every pane that left, with its record: the one pane, or the whole
    /// workspace in layout order.
    pub removed: Vec<(PaneId, PaneRecord)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_core::layout::Direction;

    fn set_of(names: &[&str]) -> WorkspaceSet {
        WorkspaceSet::restored(
            WorkspaceIdAllocator::new(),
            names.iter().map(|name| Workspace::test_new(name)).collect(),
            None,
        )
    }

    fn ids_of(set: &WorkspaceSet) -> Vec<WorkspaceId> {
        set.iter().map(|workspace| workspace.id).collect()
    }

    fn test_terminal() -> TerminalState {
        TerminalState::new(Path::new("/shepr-set-test").to_path_buf())
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
    fn inserting_a_workspace_with_a_present_id_is_refused() {
        let mut set = set_of(&["a"]);
        let present = set.as_slice()[0].id;
        let mut repeat = Workspace::test_new("repeat");
        repeat.id = present;

        let refused = set.insert(repeat).expect_err("the id is present");

        assert_eq!(refused.display_name(), "repeat");
        assert_eq!(set.len(), 1);
        assert_eq!(set.as_slice()[0].display_name(), "a");
    }

    #[test]
    fn inserting_a_workspace_that_shares_a_pane_id_is_refused() {
        let mut set = set_of(&["a"]);
        let shared = set.as_slice()[0].tree().root();
        let sharing = Workspace::test_from_pane(
            super::super::test_workspace_id(),
            Some("sharing".into()),
            Path::new("/shepr-set-test"),
            shared,
            test_terminal(),
        );

        assert!(set.insert(sharing).is_err());
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn inserting_moves_the_allocator_past_the_id() {
        let mut set = WorkspaceSet::new();
        let mut workspace = Workspace::test_new("high");
        workspace.id = WorkspaceId::from_number(40).expect("nonzero number");

        let inserted = set
            .insert(workspace)
            .unwrap_or_else(|_| panic!("a fresh id and panes"));
        assert_eq!(inserted.number(), 40);

        let next = set.prepare_workspace(Path::new("/shepr-set-test"));
        assert_eq!(next.id().number(), 41);
    }

    #[test]
    fn the_bookmark_remembers_its_index_and_repairs_by_it() {
        let mut set = set_of(&["a", "b", "c", "d"]);
        let ids = ids_of(&set);

        assert!(set.set_bookmark(&ids[2]));
        assert!(!set.set_bookmark(&ids[2]), "already bookmarked");
        assert!(!set.set_bookmark(&WorkspaceId::from_number(usize::MAX).expect("id")));

        // An order change refreshes the remembered index and moves nothing.
        assert!(set.move_before(&ids[0], None));
        assert_eq!(set.bookmark(), Some(ids[2]));
        assert_eq!(set.bookmark_index(), Some(1));

        // The bookmarked workspace vanishes: the one now at its index takes
        // over.
        set.remove(&ids[2]).expect("present");
        assert_eq!(set.bookmark(), Some(ids[3]));
        assert_eq!(set.bookmark_index(), Some(1));

        // Past the end it clamps, and with nothing left it is none.
        set.remove(&ids[3]).expect("present");
        assert_eq!(set.bookmark(), Some(ids[0]));
        assert_eq!(set.bookmark_index(), Some(1));
        set.remove(&ids[1]).expect("present");
        assert_eq!(set.bookmark_index(), Some(0));
        set.remove(&ids[0]).expect("present");
        assert_eq!(set.bookmark(), None);
        assert_eq!(set.bookmark_index(), None);
    }

    #[test]
    fn pane_lookup_finds_the_owning_workspace() {
        let mut set = set_of(&["a", "b"]);
        let second_id = set.as_slice()[1].id;
        let split = set
            .get_mut(&second_id)
            .expect("present")
            .test_split(Direction::Horizontal);

        let found = set.pane(split).expect("the pane is in the set");
        assert_eq!(found.workspace().id, second_id);
        assert_eq!(found.id(), split);
        assert_eq!(
            found.public_id(),
            PublicPaneId::new(
                &second_id,
                shepr_protocol::PanePublicNumber::new(2).expect("number")
            )
        );
        assert_eq!(
            set.resolve(&found.public_id()).map(|found| found.id()),
            Some(split)
        );
        assert!(set.pane(PaneId::alloc()).is_none());
        let retired = PublicPaneId::new(
            &WorkspaceId::from_number(usize::MAX).expect("id"),
            shepr_protocol::PanePublicNumber::FIRST,
        );
        assert!(set.resolve(&retired).is_none());
    }

    #[test]
    fn a_removed_workspace_takes_its_spawn_geometry_with_it() {
        let mut set = set_of(&["a", "b"]);
        let ids = ids_of(&set);
        let geometry = super::super::SpawnGeometry {
            area: shepr_core::geometry::Rect::new(0, 0, 61, 17),
            cell: None,
        };
        set.get_mut(&ids[1])
            .expect("present")
            .record_spawn_geometry(geometry);
        assert_eq!(set.as_slice()[1].spawn_geometry(), Some(geometry));
        assert_eq!(set.as_slice()[0].spawn_geometry(), None);

        let removed = set.remove(&ids[1]).expect("present");

        assert_eq!(removed.spawn_geometry(), Some(geometry));
        assert_eq!(set.len(), 1);
        assert_eq!(set.as_slice()[0].spawn_geometry(), None);
    }

    #[test]
    fn restored_sets_drop_a_duplicate_id_and_seed_the_bookmark() {
        let first = Workspace::test_new("first");
        let mut repeat = Workspace::test_new("repeat");
        repeat.id = first.id;
        let last = Workspace::test_new("last");

        let set = WorkspaceSet::restored(
            WorkspaceIdAllocator::new(),
            vec![first, repeat, last],
            Some(1),
        );

        assert_eq!(set.len(), 2);
        assert_eq!(set.as_slice()[0].display_name(), "first");
        assert_eq!(set.as_slice()[1].display_name(), "last");
        assert_eq!(set.bookmark_index(), Some(1));
        assert_eq!(set.bookmark(), Some(set.as_slice()[1].id));
    }

    #[test]
    fn moving_before_an_anchor_or_to_the_end_reorders() {
        let mut set = set_of(&["a", "b", "c"]);
        let ids = ids_of(&set);

        assert!(set.move_before(&ids[2], Some(&ids[0])));
        assert_eq!(ids_of(&set), vec![ids[2], ids[0], ids[1]]);
        assert!(!set.move_before(&ids[2], Some(&ids[0])), "already there");
        assert!(set.move_before(&ids[2], None));
        assert_eq!(ids_of(&set), vec![ids[0], ids[1], ids[2]]);
        assert!(!set.move_before(&ids[2], None), "already last");
        let missing = WorkspaceId::from_number(usize::MAX).expect("id");
        assert!(!set.move_before(&missing, None));
        assert!(!set.move_before(&ids[0], Some(&missing)));
    }

    #[test]
    fn removing_the_last_pane_removes_the_workspace_and_repairs_the_bookmark() {
        let mut set = set_of(&["a", "b", "c"]);
        let ids = ids_of(&set);
        assert!(set.set_bookmark(&ids[1]));
        let only = set.as_slice()[1].tree().root();
        let removal = set.remove_pane(only).expect("the pane is in the set");

        assert_eq!(removal.scope, PaneRemovalScope::Workspace);
        assert_eq!(removal.workspace_id, ids[1]);
        assert_eq!(removal.pane, only);
        assert!(!removal.focus_changed);
        assert_eq!(removal.removed.len(), 1);
        assert_eq!(removal.removed[0].0, only);
        assert_eq!(ids_of(&set), vec![ids[0], ids[2]]);
        // The workspace now at the bookmark's index takes it over.
        assert_eq!(set.bookmark(), Some(ids[2]));
        assert_eq!(set.bookmark_index(), Some(1));
        assert!(set.pane(only).is_none());
        assert!(set.remove_pane(only).is_none());
    }

    #[test]
    fn removing_every_pane_one_by_one_ends_with_the_workspace() {
        let mut set = set_of(&["a"]);
        let id = set.as_slice()[0].id;
        let workspace = set.get_mut(&id).expect("present");
        workspace.test_split(Direction::Horizontal);
        workspace.test_split(Direction::Vertical);
        let order = workspace.tree().pane_ids();
        // Take every pane but one out first, then the last one.
        let (last, rest) = order.split_last().expect("three panes");
        for pane in rest {
            let removal = set.remove_pane(*pane).expect("present");
            assert_eq!(removal.scope, PaneRemovalScope::Pane);
        }

        let removal = set.remove_pane(*last).expect("present");

        assert_eq!(removal.scope, PaneRemovalScope::Workspace);
        assert_eq!(
            removal
                .removed
                .iter()
                .map(|(pane, _)| *pane)
                .collect::<Vec<_>>(),
            vec![*last]
        );
        assert!(set.is_empty());
        assert_eq!(set.bookmark(), None);
    }

    #[test]
    fn removing_a_pane_reports_whether_focus_moved() {
        let mut set = set_of(&["a"]);
        let id = set.as_slice()[0].id;
        let workspace = set.get_mut(&id).expect("present");
        let root = workspace.tree().root();
        let second = workspace.test_split(Direction::Horizontal);
        let third = workspace.test_split(Direction::Horizontal);
        assert_eq!(workspace.tree().focused(), third);

        // A pane that is not focused leaves focus where it was.
        let removal = set.remove_pane(root).expect("present");
        assert_eq!(removal.scope, PaneRemovalScope::Pane);
        assert!(!removal.focus_changed);
        assert_eq!(set.as_slice()[0].tree().focused(), third);
        assert_eq!(set.as_slice()[0].tree().root(), second);

        // The focused pane's removal hands focus to another pane.
        let removal = set.remove_pane(third).expect("present");
        assert!(removal.focus_changed);
        assert_eq!(set.as_slice()[0].tree().focused(), second);
        assert_eq!(removal.removed.len(), 1);
        assert_eq!(removal.removed[0].0, third);
    }

    #[test]
    fn pane_mut_reaches_the_record_in_any_workspace() {
        let mut set = set_of(&["a", "b"]);
        let second = set.as_slice()[1].tree().root();

        let record = set.pane_mut(second).expect("the pane is in the set");
        assert!(record.set_right_click_passthrough(true));

        assert!(
            set.pane(second)
                .expect("present")
                .record()
                .right_click_passthrough()
        );
        assert!(set.pane_mut(PaneId::alloc()).is_none());
        assert_eq!(set.records().count(), 2);
        assert_eq!(
            set.records_mut()
                .filter(|(_, record)| record.right_click_passthrough())
                .count(),
            1
        );
    }

    #[test]
    fn a_prepared_workspace_commits_with_its_geometry() {
        let mut set = WorkspaceSet::new();
        let cwd = Path::new("/shepr-set-test");
        let prepared = set.prepare_workspace(cwd);
        let id = prepared.id();
        let root = prepared.root_pane();
        assert_eq!(
            prepared.root_public_id(),
            PublicPaneId::new(&id, PanePublicNumber::FIRST)
        );
        assert_eq!(prepared.cwd(), cwd);
        assert!(set.is_empty(), "nothing joins before the commit");
        let geometry = SpawnGeometry {
            area: shepr_core::geometry::Rect::new(0, 0, 61, 17),
            cell: None,
        };

        assert_eq!(set.commit_workspace(prepared, geometry).ok(), Some(id));

        let workspace = set.get(&id).expect("committed");
        assert_eq!(workspace.spawn_geometry(), Some(geometry));
        assert_eq!(workspace.tree().root(), root);
        assert_eq!(workspace.tree().len(), 1);
        assert_eq!(workspace.identity_cwd(), cwd);
        assert_eq!(
            workspace.tree().pane(root).map(PaneRecord::number),
            Some(PanePublicNumber::FIRST)
        );
    }
}
