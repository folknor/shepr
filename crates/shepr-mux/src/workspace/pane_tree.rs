//! A workspace's pane tree: the layout, a record per pane, the root pane, the
//! zoom and the public numbering, kept in agreement by construction.

use std::collections::{HashMap, HashSet};

use super::shape::Shape;
use super::{Workspace, WorkspaceChrome};
use crate::terminal::TerminalState;
use shepr_core::absolute_path::AbsolutePath;
use shepr_core::layout::{
    Direction, InvalidSavedLayout, LayoutEpoch, NavDirection, Node, PaneId, RatioDelta, SplitPath,
    SplitRatio, TileLayout,
};
use shepr_protocol::{PanePublicNumber, PublicPaneId, WorkspaceId};

/// One pane of a workspace: its public number, its terminal and its input
/// flag. Built only by `PaneTree`, so a record exists only inside a tree.
pub struct PaneRecord {
    number: PanePublicNumber,
    terminal: TerminalState,
    /// Whether unmodified right-click gestures are forwarded to the pane
    /// application.
    right_click_passthrough: bool,
}

impl PaneRecord {
    fn new(number: PanePublicNumber, terminal: TerminalState) -> Self {
        Self {
            number,
            terminal,
            right_click_passthrough: false,
        }
    }

    /// The pane's stable public number within its workspace.
    pub fn number(&self) -> PanePublicNumber {
        self.number
    }

    pub fn terminal(&self) -> &TerminalState {
        &self.terminal
    }

    pub fn terminal_mut(&mut self) -> &mut TerminalState {
        &mut self.terminal
    }

    pub fn right_click_passthrough(&self) -> bool {
        self.right_click_passthrough
    }

    /// True when the flag changed.
    pub fn set_right_click_passthrough(&mut self, on: bool) -> bool {
        let changed = self.right_click_passthrough != on;
        self.right_click_passthrough = on;
        changed
    }

    /// Replaces the pane's terminal state in place; its number and input flag
    /// stay.
    pub(crate) fn replace_terminal(&mut self, terminal: TerminalState) {
        self.terminal = terminal;
    }
}

/// A workspace's panes: the layout, a record per leaf, the root pane, the
/// zoom and the next public number, kept in agreement by construction.
///
/// The leaves of `layout` are exactly the keys of `panes`; `root` is one of
/// them; every number is distinct and below `next_number`; `zoomed` implies a
/// second pane. Every constructor and mutator below keeps all four, so
/// nothing re-checks them.
pub struct PaneTree {
    layout: TileLayout,
    panes: HashMap<PaneId, PaneRecord>,
    root: PaneId,
    zoomed: bool,
    next_number: PanePublicNumber,
    /// Advanced by every mutator that adds, removes or swaps a leaf, so a
    /// `SplitPath` read from `layout` is valid exactly while its epoch is.
    layout_epoch: LayoutEpoch,
}

/// What a tree keeps besides its shape, as a saved file states it.
#[derive(Debug, Clone, Copy)]
pub struct SavedTreeState {
    pub focus: PanePublicNumber,
    pub root: PanePublicNumber,
    pub zoomed: bool,
    pub next_number: PanePublicNumber,
}

/// Why a saved tree was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeRejection {
    RepeatedNumber(PanePublicNumber),
    NumberNotBelowNext(PanePublicNumber),
    MissingFocus(PanePublicNumber),
    MissingRoot(PanePublicNumber),
    LonePaneZoom,
    /// Only from `build`; fresh IDs and a resolved focus rule it out.
    Layout(InvalidSavedLayout),
}

/// Why a pane could not be removed from its tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveRefusal {
    /// The pane is not in the tree.
    NotHere,
    /// The pane is the tree's only one; its workspace goes instead.
    LastPane,
}

/// A shape whose numbers, focus, root and zoom were admitted. Building it
/// allocates the pane IDs and cannot be refused by the saved data.
pub struct TreePlan<T> {
    shape: Shape<(PanePublicNumber, T)>,
    focus: PanePublicNumber,
    root: PanePublicNumber,
    zoomed: bool,
    next_number: PanePublicNumber,
}

impl<T> TreePlan<T> {
    /// The leaf of the root pane.
    pub fn root_leaf(&self) -> &T {
        let leaves = self.shape.leaves();
        let entry = leaves
            .iter()
            .find(|(number, _)| *number == self.root)
            .copied()
            .unwrap_or_else(|| self.shape.first_leaf());
        let (_, leaf) = entry;
        leaf
    }

    /// Whether the plan keeps a zoom.
    pub fn zoomed(&self) -> bool {
        self.zoomed
    }

    /// Allocates one globally unique `PaneId` per leaf in tree order and asks
    /// `terminal_for` for each leaf's terminal. Pane IDs cross workspace-less
    /// event and render boundaries, so materializing a tree needs the shared
    /// process-wide identity source.
    pub fn build(
        self,
        mut terminal_for: impl FnMut(PaneId, T) -> TerminalState,
    ) -> Result<PaneTree, TreeRejection> {
        let mut panes = HashMap::new();
        let mut ids: HashMap<PanePublicNumber, PaneId> = HashMap::new();
        let node = build_node(self.shape, &mut |number, leaf| {
            let pane = PaneId::alloc();
            let terminal = terminal_for(pane, leaf);
            ids.insert(number, pane);
            panes.insert(pane, PaneRecord::new(number, terminal));
            pane
        });
        // Plan admission checked these identities. Keep a typed refusal if
        // construction ever loses either one; never synthesize an unrelated
        // pane ID to make TileLayout::from_saved reject the plan.
        let focus = ids
            .get(&self.focus)
            .copied()
            .ok_or(TreeRejection::MissingFocus(self.focus))?;
        let layout = TileLayout::from_saved(node, focus).map_err(TreeRejection::Layout)?;
        let root = ids
            .get(&self.root)
            .copied()
            .ok_or(TreeRejection::MissingRoot(self.root))?;
        Ok(PaneTree {
            layout,
            panes,
            root,
            zoomed: self.zoomed,
            next_number: self.next_number,
            layout_epoch: LayoutEpoch::default(),
        })
    }
}

fn build_node<T>(
    shape: Shape<(PanePublicNumber, T)>,
    leaf: &mut impl FnMut(PanePublicNumber, T) -> PaneId,
) -> Node {
    match shape {
        Shape::Pane((number, value)) => Node::Pane(leaf(number, value)),
        Shape::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let first = build_node(*first, leaf);
            let second = build_node(*second, leaf);
            Node::Split {
                direction,
                ratio,
                first: Box::new(first),
                second: Box::new(second),
            }
        }
    }
}

/// The one public-number rule: every number is below `next`, and no two are
/// equal. A number can then always be followed by `next`.
fn admit_numbers(
    numbers: &[PanePublicNumber],
    next: PanePublicNumber,
) -> Result<(), TreeRejection> {
    let mut used = HashSet::new();
    for &number in numbers {
        if number >= next {
            return Err(TreeRejection::NumberNotBelowNext(number));
        }
        if !used.insert(number) {
            return Err(TreeRejection::RepeatedNumber(number));
        }
    }
    Ok(())
}

impl PaneTree {
    /// A one-pane tree: number `FIRST`, next `SECOND`.
    pub(crate) fn single(pane: PaneId, terminal: TerminalState) -> Self {
        Self {
            layout: TileLayout::from_live_pane(pane),
            panes: HashMap::from([(pane, PaneRecord::new(PanePublicNumber::FIRST, terminal))]),
            root: pane,
            zoomed: false,
            next_number: PanePublicNumber::SECOND,
            layout_epoch: LayoutEpoch::default(),
        }
    }

    /// Validates a saved or fixture shape before anything is built from it.
    ///
    /// Numbers must be distinct and below the saved next number; focus and
    /// root must name leaves, and zoom requires a second pane. Refuse damaged
    /// saved values so restore reports the dropped workspace and preserves the
    /// source file, rather than silently rewriting repaired values.
    pub fn plan<T>(
        shape: Shape<T>,
        number_of: impl Fn(&T) -> PanePublicNumber,
        saved: SavedTreeState,
    ) -> Result<TreePlan<T>, TreeRejection> {
        let numbers: Vec<PanePublicNumber> = shape.leaves().into_iter().map(&number_of).collect();
        admit_numbers(&numbers, saved.next_number)?;
        if !numbers.contains(&saved.focus) {
            return Err(TreeRejection::MissingFocus(saved.focus));
        }
        if !numbers.contains(&saved.root) {
            return Err(TreeRejection::MissingRoot(saved.root));
        }
        if saved.zoomed && numbers.len() == 1 {
            return Err(TreeRejection::LonePaneZoom);
        }
        Ok(TreePlan {
            shape: shape.map(&mut |leaf| (number_of(&leaf), leaf)),
            focus: saved.focus,
            root: saved.root,
            zoomed: saved.zoomed,
            next_number: saved.next_number,
        })
    }

    /// The layout, for geometry reads. Edits go through the tree's mutators,
    /// which keep the records in step.
    pub fn layout(&self) -> &TileLayout {
        &self.layout
    }

    /// The epoch the layout's split paths are valid for.
    pub fn layout_epoch(&self) -> LayoutEpoch {
        self.layout_epoch
    }

    pub fn root(&self) -> PaneId {
        self.root
    }

    pub fn focused(&self) -> PaneId {
        self.layout.focused()
    }

    pub fn zoomed(&self) -> bool {
        self.zoomed
    }

    /// Whether this tree has only its one required pane.
    pub fn is_lone(&self) -> bool {
        self.panes.len() == 1
    }

    /// The sole pane a zoomed tree shows.
    pub fn zoomed_pane(&self) -> Option<PaneId> {
        WorkspaceChrome::zoomed_pane(&self.layout, self.zoomed)
    }

    /// The public number the next new pane takes.
    pub fn next_number(&self) -> PanePublicNumber {
        self.next_number
    }

    pub fn len(&self) -> usize {
        self.panes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        self.panes.contains_key(&pane)
    }

    pub fn pane(&self, pane: PaneId) -> Option<&PaneRecord> {
        self.panes.get(&pane)
    }

    /// Every pane with its record, in no particular order.
    pub fn panes(&self) -> impl Iterator<Item = (PaneId, &PaneRecord)> {
        self.panes.iter().map(|(pane, record)| (*pane, record))
    }

    /// Pane IDs in layout order.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.layout.pane_ids()
    }

    pub fn pane_by_number(&self, number: PanePublicNumber) -> Option<PaneId> {
        self.panes
            .iter()
            .find_map(|(pane, record)| (record.number == number).then_some(*pane))
    }

    /// Pane IDs a workspace surface presents, in layout order.
    pub fn visible_pane_ids(&self) -> Vec<PaneId> {
        self.zoomed_pane()
            .map_or_else(|| self.layout.pane_ids(), |pane| vec![pane])
    }

    /// Whether `pane` is on screen: in the tree, and the focused pane when the
    /// tree is zoomed.
    pub fn shows(&self, pane: PaneId) -> bool {
        self.panes.contains_key(&pane) && self.zoomed_pane().is_none_or(|shown| shown == pane)
    }

    /// The layout with each leaf mapped through its record. `None` only if the
    /// layout and the records disagreed, which the constructors and mutators
    /// rule out; callers log it as an internal error.
    pub fn map_shape<T>(&self, mut leaf: impl FnMut(PaneId, &PaneRecord) -> T) -> Option<Shape<T>> {
        fn walk<T>(
            node: &Node,
            panes: &HashMap<PaneId, PaneRecord>,
            leaf: &mut impl FnMut(PaneId, &PaneRecord) -> T,
        ) -> Option<Shape<T>> {
            match node {
                Node::Pane(pane) => {
                    let record = panes.get(pane)?;
                    Some(Shape::Pane(leaf(*pane, record)))
                }
                Node::Split {
                    direction,
                    ratio,
                    first,
                    second,
                } => {
                    let first = walk(first, panes, leaf)?;
                    let second = walk(second, panes, leaf)?;
                    Some(Shape::Split {
                        direction: *direction,
                        ratio: *ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    })
                }
            }
        }
        walk(self.layout.root(), &self.panes, &mut leaf)
    }

    pub(super) fn pane_mut(&mut self, pane: PaneId) -> Option<&mut PaneRecord> {
        self.panes.get_mut(&pane)
    }

    /// Every pane with its mutable record, in no particular order.
    pub(super) fn panes_mut(&mut self) -> impl Iterator<Item = (PaneId, &mut PaneRecord)> {
        self.panes.iter_mut().map(|(pane, record)| (*pane, record))
    }

    /// The tree's records in layout order, for a workspace that is leaving.
    pub(super) fn into_records(mut self) -> Vec<(PaneId, PaneRecord)> {
        self.layout
            .pane_ids()
            .into_iter()
            .filter_map(|pane| self.panes.remove(&pane).map(|record| (pane, record)))
            .collect()
    }

    /// False when `pane` is not in the tree.
    pub(super) fn focus(&mut self, pane: PaneId) -> bool {
        if !self.panes.contains_key(&pane) {
            return false;
        }
        self.layout.focus_pane(pane);
        true
    }

    pub(super) fn swap(&mut self, first: PaneId, second: PaneId) -> bool {
        let swapped = self.layout.swap_panes(first, second);
        if swapped {
            self.layout_epoch = self.layout_epoch.next();
        }
        swapped
    }

    pub(super) fn resize(
        &mut self,
        pane: PaneId,
        nav: NavDirection,
        delta: RatioDelta,
        area: shepr_core::geometry::Rect,
    ) -> bool {
        self.layout.resize_pane(pane, nav, delta, area)
    }

    pub(super) fn set_split_ratio(&mut self, path: &SplitPath, ratio: SplitRatio) -> bool {
        self.layout.set_ratio_at(path, ratio)
    }

    /// Zooming a one-pane tree is refused; unzooming always succeeds.
    pub(super) fn set_zoomed(&mut self, zoomed: bool) -> bool {
        if zoomed && self.is_lone() {
            return false;
        }
        self.zoomed = zoomed;
        true
    }

    /// Takes `pane` out of the layout and the records, promoting the root to
    /// the first other leaf in layout order when the root goes, and unzooms.
    /// The last pane is refused: its workspace goes instead.
    pub(super) fn remove(&mut self, pane: PaneId) -> Result<PaneRecord, RemoveRefusal> {
        if !self.panes.contains_key(&pane) {
            return Err(RemoveRefusal::NotHere);
        }
        if self.is_lone() {
            return Err(RemoveRefusal::LastPane);
        }
        let promoted = (self.root == pane)
            .then(|| {
                self.layout
                    .pane_ids()
                    .into_iter()
                    .find(|other| *other != pane)
            })
            .flatten();
        // The layout cannot refuse a leaf the records hold; if it ever did,
        // nothing has been touched yet.
        if !self.layout.close_pane(pane) {
            return Err(RemoveRefusal::NotHere);
        }
        self.layout_epoch = self.layout_epoch.next();
        let record = self.panes.remove(&pane).ok_or(RemoveRefusal::NotHere)?;
        if let Some(root) = promoted {
            self.root = root;
        }
        self.zoomed = false;
        Ok(record)
    }

    /// Installs a prepared split: the new leaf and its record together, the
    /// new pane focused, the prepared zoom state and the number counter moved
    /// past the pane's number.
    fn commit_split(&mut self, split: PreparedSplit) -> Result<PaneId, SplitRefused> {
        if split.number != self.next_number {
            return Err(SplitRefused::NumberTaken);
        }
        // The token's ID was fresh at prepare and nothing between prepare and
        // commit can insert it, so the layout accepts it; should it ever
        // refuse, no record has been added.
        if self.panes.contains_key(&split.pane)
            || !self
                .layout
                .split_pane(split.target, split.direction, split.ratio, split.pane)
        {
            return Err(SplitRefused::TargetGone);
        }
        self.panes
            .insert(split.pane, PaneRecord::new(split.number, split.terminal));
        self.layout_epoch = self.layout_epoch.next();
        self.layout.focus_pane(split.pane);
        self.zoomed = split.zoomed;
        self.next_number = split.next_number;
        Ok(split.pane)
    }
}

/// A split planned without changing the workspace: the new pane's identity,
/// number, spawn size, terminal, split ratio and resulting zoom. The caller
/// launches from it and commits it in the same synchronous handler.
///
/// Only `Workspace::prepare_split` builds one and its fields are private, so
/// the split a commit installs is the one a launch read: nothing can pair a
/// launched child with another geometry, cwd or number. Prepare and commit
/// share one synchronous handler on the app thread, so nothing can edit the
/// layout or take a number between them; the token refuses another workspace,
/// a number that is no longer next and a target that is gone. If the two
/// phases ever span an await, they must collapse or the token must also carry
/// a tree generation.
pub struct PreparedSplit {
    workspace: WorkspaceId,
    target: PaneId,
    direction: Direction,
    pane: PaneId,
    /// The new pane's public number: the tree's next number at prepare.
    number: PanePublicNumber,
    /// The number after `number`, so a committed split cannot overflow.
    next_number: PanePublicNumber,
    /// The ratio used to size the new pane and install its split.
    ratio: SplitRatio,
    /// The zoom state used to size the new pane and install the tree.
    zoomed: bool,
    /// The new pane's PTY size in the prepared layout.
    geometry: shepr_core::geometry::PaneGeometry,
    terminal: TerminalState,
}

impl PreparedSplit {
    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace
    }

    pub fn pane_id(&self) -> PaneId {
        self.pane
    }

    /// The id to export to the child as `SHEPR_PANE_ID`.
    pub fn public_id(&self) -> PublicPaneId {
        PublicPaneId::new(&self.workspace, self.number)
    }

    /// The new pane's PTY size in the prepared layout.
    pub fn geometry(&self) -> shepr_core::geometry::PaneGeometry {
        self.geometry
    }

    /// Where the new pane's child starts.
    pub fn cwd(&self) -> &AbsolutePath {
        self.terminal.cwd()
    }
}

/// Why a prepared split was not committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitRefused {
    /// The token was prepared against another workspace.
    OtherWorkspace,
    /// The token's number is no longer the tree's next number.
    NumberTaken,
    /// The split target is no longer a pane of the tree.
    TargetGone,
}

/// Why a split could not be planned before launching its child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitPreparationRefused {
    /// The requested pane is no longer in the workspace.
    TargetGone,
    /// The public pane number has no successor, so it cannot be committed.
    NumberExhausted,
    /// The layout refused a split despite the target being present.
    LayoutRefused,
}

impl Workspace {
    /// Plans a split without launching a child or changing this workspace.
    /// Refuses a missing target, an exhausted public number space, or a layout
    /// that cannot accept the split, so an exhausted workspace never launches
    /// a child its commit would refuse.
    ///
    /// The split is made on a local copy of the layout to size the new pane's
    /// PTY, then dropped. The token keeps the ratio and zoom decisions used to
    /// calculate that size, and commit installs those same values.
    pub fn prepare_split(
        &self,
        target: PaneId,
        direction: Direction,
        chrome: &WorkspaceChrome,
        cell: Option<shepr_core::geometry::CellPx>,
        cwd: AbsolutePath,
    ) -> Result<PreparedSplit, SplitPreparationRefused> {
        if !self.tree.contains(target) {
            return Err(SplitPreparationRefused::TargetGone);
        }
        let number = self.tree.next_number;
        let next_number = number
            .checked_next()
            .ok_or(SplitPreparationRefused::NumberExhausted)?;
        let pane = PaneId::alloc();
        let ratio = SplitRatio::EVEN;
        let zoomed = false;
        let mut planned = self.tree.layout.clone();
        if !planned.split_pane(target, direction, ratio, pane) {
            return Err(SplitPreparationRefused::LayoutRefused);
        }
        // A split unzooms the workspace, so launch against the tiled layout.
        let geometry = chrome
            .pane_spawn_geometry(&planned, zoomed, pane, cell)
            .unwrap_or_else(|| chrome.sole_pane_spawn_geometry(cell));
        let terminal = TerminalState::new(cwd);
        Ok(PreparedSplit {
            workspace: self.id,
            target,
            direction,
            pane,
            number,
            next_number,
            ratio,
            zoomed,
            geometry,
            terminal,
        })
    }

    /// Commits a split planned by `prepare_split`: the new pane takes the
    /// number its launched child was given, is focused, and the layout uses
    /// the ratio and zoom state prepared for its spawn geometry. A token of
    /// another workspace, one whose number is no longer next, or one whose
    /// target is gone is refused, with the workspace unchanged.
    pub fn commit_split(&mut self, split: PreparedSplit) -> Result<PaneId, SplitRefused> {
        if split.workspace != self.id {
            return Err(SplitRefused::OtherWorkspace);
        }
        self.tree.commit_split(split)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::test_workspace_id;
    use std::path::{Path, PathBuf};

    fn abs(path: impl Into<PathBuf>) -> AbsolutePath {
        AbsolutePath::new(path).expect("test cwd is absolute")
    }

    fn terminal() -> TerminalState {
        TerminalState::new(abs("/shepr-tree-test"))
    }

    fn number(value: usize) -> PanePublicNumber {
        PanePublicNumber::new(value).expect("nonzero literal")
    }

    fn chrome() -> WorkspaceChrome {
        WorkspaceChrome {
            area: shepr_core::geometry::Rect::new(0, 0, 80, 24),
            pane_gaps: false,
            pane_scrollbars: false,
        }
    }

    fn workspace_at(cwd: &Path) -> Workspace {
        Workspace::test_from_pane(
            test_workspace_id(),
            None,
            &abs(cwd),
            PaneId::alloc(),
            TerminalState::new(abs(cwd)),
        )
    }

    fn prepare(ws: &Workspace, target: PaneId) -> PreparedSplit {
        ws.prepare_split(
            target,
            Direction::Horizontal,
            &chrome(),
            None,
            abs("/shepr-tree-test"),
        )
        .expect("split plan")
    }

    fn split(first: Shape<usize>, second: Shape<usize>) -> Shape<usize> {
        Shape::Split {
            direction: Direction::Horizontal,
            ratio: SplitRatio::EVEN,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    fn saved(focus: usize, root: usize, zoomed: bool, next: usize) -> SavedTreeState {
        SavedTreeState {
            focus: number(focus),
            root: number(root),
            zoomed,
            next_number: number(next),
        }
    }

    fn plan(shape: Shape<usize>, saved: SavedTreeState) -> Result<TreePlan<usize>, TreeRejection> {
        PaneTree::plan(shape, |leaf| number(*leaf), saved)
    }

    /// Terminals named by the leaf's number, so a built tree can be read
    /// back by number.
    fn built(plan: TreePlan<usize>) -> PaneTree {
        plan.build(|_, leaf| TerminalState::new(abs(format!("/shepr-tree-test/{leaf}"))))
            .expect("a planned tree builds")
    }

    #[test]
    fn split_commit_installs_the_leaf_and_the_record_together() {
        let mut ws = workspace_at(Path::new("/shepr-tree-test"));
        let root = ws.tree().root();
        let prepared = prepare(&ws, root);
        let pane = prepared.pane_id();
        let public = prepared.public_id();
        let ratio = prepared.ratio;
        let zoomed = prepared.zoomed;
        let geometry = prepared.geometry();
        assert_eq!(public.number(), number(2));

        assert_eq!(ws.commit_split(prepared), Ok(pane));

        let tree = ws.tree();
        assert_eq!(tree.len(), 2);
        assert!(tree.layout().pane_ids().contains(&pane));
        assert_eq!(tree.layout().splits(chrome().area)[0].ratio, ratio);
        assert_eq!(tree.zoomed(), zoomed);
        assert_eq!(
            chrome().pane_spawn_geometry(tree.layout(), tree.zoomed(), pane, None),
            Some(geometry)
        );
        assert_eq!(tree.pane(pane).map(PaneRecord::number), Some(number(2)));
        assert_eq!(tree.pane_by_number(number(2)), Some(pane));
        assert_eq!(tree.next_number(), number(3));
        assert_eq!(tree.pane(root).map(PaneRecord::number), Some(number(1)));
    }

    #[test]
    fn a_split_token_of_another_workspace_is_refused() {
        let ws = workspace_at(Path::new("/shepr-tree-test"));
        let mut other = workspace_at(Path::new("/shepr-tree-test"));
        let prepared = prepare(&ws, ws.tree().root());

        assert_eq!(
            other.commit_split(prepared),
            Err(SplitRefused::OtherWorkspace)
        );
        assert_eq!(other.tree().len(), 1);
        assert_eq!(other.tree().next_number(), number(2));
    }

    #[test]
    fn a_split_token_is_refused_once_its_number_is_taken() {
        let mut ws = workspace_at(Path::new("/shepr-tree-test"));
        let root = ws.tree().root();
        let stale = prepare(&ws, root);
        let taker = prepare(&ws, root);
        ws.commit_split(taker).expect("the first token commits");

        assert_eq!(ws.commit_split(stale), Err(SplitRefused::NumberTaken));
        assert_eq!(ws.tree().len(), 2);
        assert_eq!(ws.tree().next_number(), number(3));
    }

    #[test]
    fn a_split_token_is_refused_once_its_target_is_gone() {
        let mut ws = workspace_at(Path::new("/shepr-tree-test"));
        let root = ws.tree().root();
        let target = ws.test_split(Direction::Horizontal);
        let prepared = prepare(&ws, target);
        ws.remove_pane(target).expect("a second pane goes");

        assert_eq!(ws.commit_split(prepared), Err(SplitRefused::TargetGone));
        assert_eq!(ws.tree().pane_ids(), vec![root]);
    }

    #[test]
    fn the_layout_epoch_advances_on_topology_changes_and_not_on_ratio_edits() {
        let mut ws = Workspace::test_new("epoch");
        let root = ws.tree().root();
        let start = ws.tree().layout_epoch();

        let second = ws.test_split(Direction::Horizontal);
        let split = ws.tree().layout_epoch();
        assert_ne!(split, start);

        assert!(ws.set_split_ratio(&SplitPath::default(), SplitRatio::clamped(0.3)));
        assert_eq!(ws.tree().layout_epoch(), split);

        assert!(ws.swap_panes(root, second));
        let swapped = ws.tree().layout_epoch();
        assert_ne!(swapped, split);

        ws.remove_pane(second).expect("a second pane goes");
        assert_ne!(ws.tree().layout_epoch(), swapped);
    }

    #[test]
    fn a_split_focuses_the_new_pane_and_unzooms() {
        let mut ws = Workspace::test_new("zoomed");
        let root = ws.tree().root();
        let second = ws.test_split(Direction::Horizontal);
        assert!(ws.focus_pane(root));
        assert!(ws.set_zoomed(true));
        assert!(ws.tree().zoomed());

        let prepared = prepare(&ws, root);
        let pane = ws.commit_split(prepared).expect("commits");

        assert_eq!(ws.tree().focused(), pane);
        assert!(!ws.tree().zoomed());
        assert!(ws.tree().contains(second));
    }

    #[test]
    fn preparing_a_split_changes_nothing_and_holds_no_layout() {
        let cwd = Path::new("/__shepr_split_missing_directory__");
        let mut ws = workspace_at(cwd);
        let root = ws.tree().root();
        let id = ws.id();

        let prepared = ws
            .prepare_split(root, Direction::Horizontal, &chrome(), None, abs(cwd))
            .expect("split plan");

        assert_eq!(ws.tree().len(), 1);
        assert_eq!(ws.tree().focused(), root);
        assert_eq!(ws.tree().next_number(), number(2));
        assert_eq!(prepared.cwd(), cwd);
        // The new right half is 40 by 24 cells less its four border sides.
        assert_eq!(prepared.geometry(), spawn_geometry_for(22, 38));
        assert_eq!(prepared.public_id(), PublicPaneId::new(&id, number(2)));
        assert_eq!(prepared.workspace_id(), id);
        // An unknown target plans nothing.
        assert!(matches!(
            ws.prepare_split(
                PaneId::alloc(),
                Direction::Horizontal,
                &chrome(),
                None,
                abs(cwd)
            ),
            Err(SplitPreparationRefused::TargetGone)
        ));

        assert!(ws.commit_split(prepared).is_ok());
        assert_eq!(ws.tree().len(), 2);
        assert_eq!(ws.tree().next_number(), number(3));
    }

    fn spawn_geometry_for(rows: u16, cols: u16) -> shepr_core::geometry::PaneGeometry {
        shepr_core::geometry::PaneGeometry::cells_only(cols, rows)
    }

    #[test]
    fn a_workspace_without_a_next_number_plans_no_split() {
        // No child should be launched when there is no successor to commit.
        let mut tree = PaneTree::single(PaneId::alloc(), terminal());
        tree.next_number = PanePublicNumber::new(usize::MAX).expect("max number");
        let root = tree.root();
        let mut ws = workspace_at(Path::new("/shepr-tree-test"));
        ws.tree = tree;

        assert!(matches!(
            ws.prepare_split(
                root,
                Direction::Horizontal,
                &chrome(),
                None,
                abs("/shepr-tree-test")
            ),
            Err(SplitPreparationRefused::NumberExhausted)
        ));
        assert_eq!(ws.tree().len(), 1);
    }

    #[test]
    fn removing_the_root_promotes_a_surviving_pane() {
        let mut ws = Workspace::test_new("promote");
        let root = ws.tree().root();
        let second = ws.test_split(Direction::Horizontal);

        let removed = ws.remove_pane(root).expect("the root goes");

        assert_eq!(removed.number(), number(1));
        assert_eq!(ws.tree().root(), second);
        assert!(ws.tree().contains(second));
        assert!(!ws.tree().contains(root));
        assert_eq!(ws.tree().len(), 1);
    }

    #[test]
    fn removing_the_last_pane_is_refused() {
        let mut ws = Workspace::test_new("last");
        let root = ws.tree().root();

        assert!(matches!(ws.remove_pane(root), Err(RemoveRefusal::LastPane)));
        assert!(matches!(
            ws.remove_pane(PaneId::alloc()),
            Err(RemoveRefusal::NotHere)
        ));
        assert_eq!(ws.tree().len(), 1);
        assert_eq!(ws.tree().root(), root);
    }

    #[test]
    fn plans_refuse_a_repeated_number_or_one_not_below_next() {
        let pair = || split(Shape::Pane(1), Shape::Pane(2));

        assert!(plan(pair(), saved(1, 1, false, 3)).is_ok());
        assert!(matches!(
            plan(split(Shape::Pane(2), Shape::Pane(2)), saved(2, 2, false, 3)),
            Err(TreeRejection::RepeatedNumber(repeated)) if repeated == number(2)
        ));
        assert!(matches!(
            plan(pair(), saved(1, 1, false, 2)),
            Err(TreeRejection::NumberNotBelowNext(over)) if over == number(2)
        ));
        assert!(matches!(
            plan(
                split(Shape::Pane(1), Shape::Pane(usize::MAX)),
                saved(1, 1, false, 8)
            ),
            Err(TreeRejection::NumberNotBelowNext(_))
        ));
    }

    #[test]
    fn plans_refuse_missing_focus_and_root() {
        let shape = || split(Shape::Pane(4), split(Shape::Pane(2), Shape::Pane(3)));
        assert!(matches!(
            plan(shape(), saved(9, 4, false, 5)),
            Err(TreeRejection::MissingFocus(_))
        ));
        assert!(matches!(
            plan(shape(), saved(4, 9, false, 5)),
            Err(TreeRejection::MissingRoot(_))
        ));
    }

    #[test]
    fn plans_keep_a_saved_focus_and_root() {
        let shape = split(Shape::Pane(4), split(Shape::Pane(2), Shape::Pane(3)));
        let plan = plan(shape, saved(3, 2, false, 5)).expect("admitted");
        assert_eq!(*plan.root_leaf(), 2);
        let tree = built(plan);

        let focused = tree.focused();
        assert_eq!(tree.pane(focused).map(PaneRecord::number), Some(number(3)));
        assert_eq!(
            tree.pane(tree.root()).map(PaneRecord::number),
            Some(number(2))
        );
    }

    #[test]
    fn plans_refuse_a_zoom_without_its_focus_or_a_second_pane() {
        let pair = || split(Shape::Pane(1), Shape::Pane(2));
        assert!(matches!(
            plan(pair(), saved(9, 1, true, 3)),
            Err(TreeRejection::MissingFocus(_))
        ));
        assert!(matches!(
            plan(Shape::Pane(1), saved(1, 1, true, 2)),
            Err(TreeRejection::LonePaneZoom)
        ));
        assert!(
            plan(pair(), saved(2, 1, true, 3))
                .expect("valid zoom")
                .zoomed()
        );
    }

    #[test]
    fn a_mapped_shape_plans_back_into_the_same_tree() {
        let shape = split(Shape::Pane(1), split(Shape::Pane(3), Shape::Pane(2)));
        let tree = built(plan(shape, saved(3, 1, true, 4)).expect("admitted"));

        let mapped = tree
            .map_shape(|_, record| record.number().get())
            .expect("a tree maps");
        assert_eq!(mapped.leaves(), vec![&1, &3, &2]);
        let again = built(
            plan(
                mapped,
                saved(
                    tree.pane(tree.focused())
                        .map(PaneRecord::number)
                        .map_or(1, PanePublicNumber::get),
                    tree.pane(tree.root())
                        .map(PaneRecord::number)
                        .map_or(1, PanePublicNumber::get),
                    tree.zoomed(),
                    tree.next_number().get(),
                ),
            )
            .expect("a captured tree plans"),
        );

        let numbers = |tree: &PaneTree| -> Vec<usize> {
            tree.pane_ids()
                .into_iter()
                .filter_map(|pane| tree.pane(pane).map(|record| record.number().get()))
                .collect()
        };
        assert_eq!(numbers(&again), numbers(&tree));
        assert_eq!(again.zoomed(), tree.zoomed());
        assert_eq!(again.next_number(), tree.next_number());
        assert_eq!(
            again.pane(again.focused()).map(PaneRecord::number),
            tree.pane(tree.focused()).map(PaneRecord::number)
        );
        assert_eq!(
            again.pane(again.root()).map(PaneRecord::number),
            tree.pane(tree.root()).map(PaneRecord::number)
        );
    }

    #[test]
    fn zooming_a_one_pane_tree_is_refused_and_unzooming_always_succeeds() {
        let mut tree = PaneTree::single(PaneId::alloc(), terminal());

        assert!(!tree.set_zoomed(true));
        assert!(!tree.zoomed());
        assert!(tree.set_zoomed(false));
    }

    #[test]
    fn the_input_flag_reports_whether_it_changed() {
        let tree = PaneTree::single(PaneId::alloc(), terminal());
        let mut tree = tree;
        let pane = tree.root();
        let record = tree.pane_mut(pane).expect("root record");

        assert!(record.set_right_click_passthrough(true));
        assert!(!record.set_right_click_passthrough(true));
        assert!(record.right_click_passthrough());
        assert!(record.set_right_click_passthrough(false));
    }
}
