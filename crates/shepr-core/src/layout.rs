//! BSP tree layout for tiling panes within a workspace.

use std::cmp::Reverse;
use std::collections::HashSet;

use crate::geometry::Rect;
use crate::limits::{
    FIRST_PANE_ID, MIN_SPLIT_CHILD_CELLS, MIN_SPLIT_EXTENT_CELLS, MIN_WORKSPACE_PANES,
    SPLIT_EDGE_MATCH_TOLERANCE_CELLS,
};

pub use crate::limits::{EVEN_SPLIT, MAX_SPLIT_RATIO, MIN_SPLIT_RATIO};

/// First-child share of a BSP split.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(transparent)]
pub struct SplitRatio(f32);

impl SplitRatio {
    pub const EVEN: Self = Self(EVEN_SPLIT);

    /// Accept a finite ratio within the layout bounds.
    pub fn new(value: f32) -> Option<Self> {
        (value.is_finite() && (MIN_SPLIT_RATIO..=MAX_SPLIT_RATIO).contains(&value))
            .then_some(Self(value))
    }

    pub fn clamped(value: f32) -> Self {
        Self(if value.is_finite() {
            value.clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO)
        } else {
            EVEN_SPLIT
        })
    }

    pub fn get(self) -> f32 {
        self.0
    }

    /// Move a split by a signed fraction of its parent extent.
    pub fn nudged(self, delta: RatioDelta) -> Self {
        Self::clamped(self.0 + delta.0)
    }
}

// SplitRatio's constructor and deserializer exclude NaN, infinities, and both
// signed zero values, so its f32 equality is reflexive for every valid value.
impl Eq for SplitRatio {}

impl<'de> serde::Deserialize<'de> for SplitRatio {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <f32 as serde::Deserialize>::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "split ratio must be finite and between {MIN_SPLIT_RATIO} and {MAX_SPLIT_RATIO}"
            ))
        })
    }
}

/// Signed movement of a split as a fraction of its parent extent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RatioDelta(f32);

impl RatioDelta {
    pub const fn new(value: f32) -> Self {
        Self(value)
    }

    const fn negated(self) -> Self {
        Self(-self.0)
    }
}

/// Process-wide pane identity. Runtime IDs come from [`PaneId::alloc`];
/// [`PaneId::from_raw`] is the public seam for deterministic test fixtures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(u32);

/// Logging and thread-name form: the bare number. In tracing fields write
/// `pane = %pane_id`; the operator-facing `PublicPaneId` goes under
/// `public_pane_id`, so no field name carries both.
impl std::fmt::Display for PaneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Global atomic counter for unique PaneId generation across all workspaces.
///
/// Pane ids flow through server-wide events and render-source sets without a
/// workspace id, so allocations in separate workspaces must not collide. An
/// owned allocator would sit above this crate and need to be passed through
/// each layout creation and split path without changing that invariant. Tests
/// that want fixed ids build them with `from_raw`, and the exhaustion rule
/// is tested through `alloc_from`.
static NEXT_PANE_ID: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(FIRST_PANE_ID);

impl PaneId {
    /// Allocate a globally unique PaneId.
    ///
    /// Never hands out an ID twice. The counter refuses to advance past `u32::MAX` instead of wrapping, so
    /// exhausting it (four billion panes in one process) is a loud failure
    /// rather than a silent reuse of live ids.
    pub fn alloc() -> Self {
        match Self::alloc_from(&NEXT_PANE_ID) {
            Some(id) => id,
            // Continuing would have to reuse a live id; there is no safe value.
            None => panic!("pane id space exhausted: more than u32::MAX - 1 panes allocated"),
        }
    }

    fn alloc_from(counter: &std::sync::atomic::AtomicU32) -> Option<Self> {
        use std::sync::atomic::Ordering;
        counter
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .ok()
            .map(Self)
    }

    pub fn raw(self) -> u32 {
        self.0
    }

    /// Construct a fixed ID for test fixtures without advancing the allocator.
    /// It is public because tests in other crates (`shepr-test-fixtures`,
    /// `shepr-pty`) build fixed IDs, and no production crate has a test
    /// feature to hide it behind. A live pane's ID always comes from `alloc`.
    /// Textlint rejects calls outside test code and the fixture crate.
    pub fn from_raw(id: u32) -> Self {
        Self(id)
    }
}

/// A pane's position and focus state in the BSP tree. UI chrome is added
/// after layout, in `chrome` and the server's view code.
#[derive(Clone)]
pub struct PaneInfo {
    pub id: PaneId,
    pub rect: Rect,
    pub is_focused: bool,
}

/// Which child of a split a path step descends into.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum SplitBranch {
    First,
    Second,
}

/// The address of one split node: the branches taken from the root. Read
/// from a layout's `splits`, and valid for that layout until its tree changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SplitPath(Vec<SplitBranch>);

impl SplitPath {
    pub fn branches(&self) -> &[SplitBranch] {
        &self.0
    }

    fn child(&self, branch: SplitBranch) -> Self {
        let mut branches = Vec::with_capacity(self.0.len() + 1);
        branches.extend_from_slice(&self.0);
        branches.push(branch);
        Self(branches)
    }
}

impl From<Vec<SplitBranch>> for SplitPath {
    fn from(branches: Vec<SplitBranch>) -> Self {
        Self(branches)
    }
}

/// Which shape of a workspace's split tree a `SplitPath` was read from. The
/// owner of the tree advances it on every change that can move or replace a
/// split (a pane added, removed or swapped), so a path is valid exactly while
/// the epoch it was published with is current. Ratio edits do not advance it.
///
/// An epoch means something only within one server boot: every tree starts at
/// the default, a restored one included, so two boots reuse the same values.
/// That is safe because a client only ever holds epochs from the projection of
/// the boot it is connected to, and drops them with that boot's snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LayoutEpoch(u64);

impl LayoutEpoch {
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

/// Info about a split boundary, used for mouse drag resize.
#[derive(Clone)]
pub struct SplitBorder {
    /// Position of the divider line (x for horizontal split, y for vertical).
    pub pos: u16,
    /// Direction of the split that created this border.
    pub direction: Direction,
    /// Ratio assigned to the first child of this split.
    pub ratio: SplitRatio,
    /// Total area of the split node.
    pub area: Rect,
    /// Path from root to this split node.
    pub path: SplitPath,
}

/// Axis a BSP split divides its area along: `Horizontal` puts the children
/// side by side, `Vertical` stacks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Direction {
    Horizontal,
    Vertical,
}

/// Cardinal direction for pane navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NavDirection {
    Left,
    Right,
    Up,
    Down,
}

/// A node in the BSP tree. A pane leaf names a pane by its ID; the pane's
/// state and public number live in the record the owner of the layout keeps
/// under that ID.
#[derive(Clone)]
#[expect(
    variant_size_differences,
    reason = "a split is 23 bytes; boxing it would allocate per split to save that much per leaf"
)]
pub enum Node {
    Pane(PaneId),
    Split {
        direction: Direction,
        ratio: SplitRatio,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    /// Pane IDs in tree order.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        collect_ids(self, &mut ids);
        ids
    }
}

/// BSP tiling layout. Tracks a tree of splits and a focused pane.
#[derive(Clone)]
pub struct TileLayout {
    root: Node,
    focus: PaneId,
    /// Pane focused before `focus`, used by `close_focused`. Only a real focus
    /// move writes it; tree edits go through the target-taking primitives
    /// (`split_pane`, `close_pane`) so internal
    /// focus excursions never corrupt it.
    prev_focus: Option<PaneId>,
}

/// A saved layout defect rejected before it becomes a live [`TileLayout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidSavedLayout {
    /// Two leaves use the same pane identity.
    DuplicatePaneId(PaneId),
    /// The focused pane is not present among the leaves.
    FocusNotFound(PaneId),
}

impl TileLayout {
    /// Create a new layout with a single pane (globally unique ID).
    /// Returns the root ID separately so a workspace can retain its root pane
    /// identity after focus moves elsewhere. It equals `focused()` only at
    /// creation; returning it keeps callers from deriving the root from focus.
    pub fn new() -> (Self, PaneId) {
        let root_id = PaneId::alloc();
        (
            Self {
                root: Node::Pane(root_id),
                focus: root_id,
                prev_focus: None,
            },
            root_id,
        )
    }

    /// A one-pane layout around a pane id the caller already owns.
    /// The caller has already established that this ID is nonzero and unique.
    pub fn from_live_pane(pane_id: PaneId) -> Self {
        Self {
            root: Node::Pane(pane_id),
            focus: pane_id,
            prev_focus: None,
        }
    }

    /// Move focus, recording the pane being left. No-op when focus is unchanged.
    fn set_focus(&mut self, id: PaneId) {
        if id != self.focus {
            self.prev_focus = Some(self.focus);
            self.focus = id;
        }
    }

    pub fn focused(&self) -> PaneId {
        self.focus
    }

    pub fn pane_count(&self) -> usize {
        count_panes(&self.root)
    }

    /// Compute rects for all panes given the available area.
    pub fn panes(&self, area: Rect) -> Vec<PaneInfo> {
        let mut result = Vec::new();
        collect_panes(&self.root, area, self.focus, &mut result);
        result
    }

    /// Collect all split boundaries for mouse drag resize.
    pub fn splits(&self, area: Rect) -> Vec<SplitBorder> {
        let mut result = Vec::new();
        collect_splits(&self.root, area, &SplitPath::default(), &mut result);
        result
    }

    /// Splits `target`, naming the new pane `new_pane`. Focus is untouched.
    /// False, with the layout unchanged, when `target` is not a leaf or
    /// `new_pane` already is one.
    pub fn split_pane(
        &mut self,
        target: PaneId,
        direction: Direction,
        ratio: SplitRatio,
        new_pane: PaneId,
    ) -> bool {
        let ids = self.pane_ids();
        if !ids.contains(&target) || ids.contains(&new_pane) {
            return false;
        }
        split_at(&mut self.root, target, direction, new_pane, ratio)
    }

    /// Close the focused pane, returning focus to the pane it came from when
    /// that pane is still open. Returns false if it's the last pane.
    pub fn close_focused(&mut self) -> bool {
        if self.pane_count() <= MIN_WORKSPACE_PANES {
            return false;
        }
        let ids = self.pane_ids();
        let target = self.focus;
        // A focus outside the tree breaks the layout invariant; closing some
        // other pane in its place would desync callers that close the focused
        // pane's runtime, so refuse instead.
        let Some(pos) = ids.iter().position(|id| *id == target) else {
            return false;
        };
        let ordered = if pos + 1 < ids.len() {
            ids[pos + 1]
        } else {
            ids[pos - 1]
        };
        let new_focus = match self.prev_focus {
            Some(prev) if prev != target && ids.contains(&prev) => prev,
            _ => ordered,
        };
        if !remove_pane(&mut self.root, target) {
            return false;
        }
        self.focus = new_focus;
        self.prev_focus = None;
        true
    }

    /// Close any pane. Focus and its history are left alone unless the closed
    /// pane is the focused one.
    pub fn close_pane(&mut self, id: PaneId) -> bool {
        if self.focus == id {
            return self.close_focused();
        }
        if self.pane_count() <= MIN_WORKSPACE_PANES || !self.pane_ids().contains(&id) {
            return false;
        }
        if !remove_pane(&mut self.root, id) {
            return false;
        }
        if self.prev_focus == Some(id) {
            self.prev_focus = None;
        }
        true
    }

    pub fn focus_pane(&mut self, id: PaneId) {
        if self.pane_ids().contains(&id) {
            self.set_focus(id);
        }
    }

    /// Swap two pane ids in the layout tree while preserving split shape and
    /// ratios. Returns true only when both panes exist and are different.
    pub fn swap_panes(&mut self, first: PaneId, second: PaneId) -> bool {
        if first == second {
            return false;
        }
        let ids = self.pane_ids();
        if !ids.contains(&first) || !ids.contains(&second) {
            return false;
        }
        swap_pane_ids(&mut self.root, first, second);
        true
    }

    /// Set the ratio of a split node at the given path. Returns true only if
    /// an existing split's ratio changed.
    pub fn set_ratio_at(&mut self, path: &SplitPath, ratio: SplitRatio) -> bool {
        set_ratio_at(&mut self.root, path.branches(), ratio)
    }

    /// Moves the split nearest `pane`'s edge in `nav` by `delta`: positive
    /// grows the pane, negative shrinks it. Focus and its history are
    /// untouched. True only when a ratio changed.
    pub fn resize_pane(
        &mut self,
        pane: PaneId,
        nav: NavDirection,
        delta: RatioDelta,
        area: Rect,
    ) -> bool {
        let Some(rect) = self
            .panes(area)
            .into_iter()
            .find(|info| info.id == pane)
            .map(|info| info.rect)
        else {
            return false;
        };
        let splits = self.splits(area);

        let target_dir = match nav {
            NavDirection::Left | NavDirection::Right => Direction::Horizontal,
            NavDirection::Up | NavDirection::Down => Direction::Vertical,
        };
        let grows = matches!(nav, NavDirection::Right | NavDirection::Down);

        let best = nearest_resize_split(&splits, target_dir, rect, nav)
            .or_else(|| nearest_resize_split(&splits, target_dir, rect, opposite_direction(nav)));
        let Some(split) = best else {
            return false;
        };
        let current = split.ratio;
        let adj = if grows { delta } else { delta.negated() };
        let path = split.path.clone();
        self.set_ratio_at(&path, current.nudged(adj))
    }

    /// Pane IDs in layout order.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.root.pane_ids()
    }

    /// Access the tree root for serialization.
    pub fn root(&self) -> &Node {
        &self.root
    }

    /// Reconstruct a layout from a tree after checking pane identity invariants:
    /// no leaf repeats and `focus` is a leaf. The caller builds the tree from
    /// fresh IDs (`PaneTree`'s plan does), so they are unique across live
    /// layouts.
    pub fn from_saved(root: Node, focus: PaneId) -> Result<Self, InvalidSavedLayout> {
        let mut ids = HashSet::new();
        collect_validated_ids(&root, &mut ids)?;
        if !ids.contains(&focus) {
            return Err(InvalidSavedLayout::FocusNotFound(focus));
        }
        Ok(Self {
            root,
            focus,
            prev_focus: None,
        })
    }
}

fn collect_validated_ids(node: &Node, ids: &mut HashSet<PaneId>) -> Result<(), InvalidSavedLayout> {
    match node {
        Node::Pane(id) => {
            if !ids.insert(*id) {
                return Err(InvalidSavedLayout::DuplicatePaneId(*id));
            }
            Ok(())
        }
        Node::Split { first, second, .. } => {
            collect_validated_ids(first, ids)?;
            collect_validated_ids(second, ids)
        }
    }
}

// --- Directional pane navigation ---

/// Find the nearest pane in the given direction from `focused`.
pub fn find_in_direction(
    focused: &PaneInfo,
    direction: NavDirection,
    panes: &[PaneInfo],
) -> Option<PaneId> {
    let fr = focused.rect;

    panes
        .iter()
        .enumerate()
        .filter(|(_, p)| p.id != focused.id)
        .filter_map(|(index, pane)| {
            rect_distance_in_direction(fr, pane.rect, direction)
                .map(|distance| (index, pane, distance))
        })
        .min_by_key(|(index, p, edge_distance)| {
            let r = p.rect;
            let overlap = match direction {
                NavDirection::Left | NavDirection::Right => {
                    range_overlap_amount(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Up | NavDirection::Down => {
                    range_overlap_amount(r.x, r.width, fr.x, fr.width)
                }
            };
            let center_distance = match direction {
                NavDirection::Left | NavDirection::Right => {
                    range_center_distance(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Up | NavDirection::Down => {
                    range_center_distance(r.x, r.width, fr.x, fr.width)
                }
            };
            (*edge_distance, Reverse(overlap), center_distance, *index)
        })
        .map(|(_, p, _)| p.id)
}

/// Distance between two panes along `direction`, provided they are separated
/// on that axis and overlap on the other. Touching pane edges have distance 0.
pub fn rect_distance_in_direction(from: Rect, to: Rect, direction: NavDirection) -> Option<u32> {
    let (
        axis_from_start,
        from_len,
        axis_to_start,
        to_len,
        cross_from_start,
        cross_from_len,
        cross_to_start,
        cross_to_len,
    ) = match direction {
        NavDirection::Left | NavDirection::Right => (
            from.x,
            from.width,
            to.x,
            to.width,
            from.y,
            from.height,
            to.y,
            to.height,
        ),
        NavDirection::Up | NavDirection::Down => (
            from.y,
            from.height,
            to.y,
            to.height,
            from.x,
            from.width,
            to.x,
            to.width,
        ),
    };
    if !ranges_overlap(
        cross_from_start,
        cross_from_len,
        cross_to_start,
        cross_to_len,
    ) {
        return None;
    }

    let from_start = u32::from(axis_from_start);
    let to_start = u32::from(axis_to_start);
    let from_end = rect_end(axis_from_start, from_len);
    let to_end = rect_end(axis_to_start, to_len);
    match direction {
        NavDirection::Left | NavDirection::Up if to_end <= from_start => Some(from_start - to_end),
        NavDirection::Right | NavDirection::Down if to_start >= from_end => {
            Some(to_start - from_end)
        }
        _ => None,
    }
}

fn rect_end(start: u16, len: u16) -> u32 {
    u32::from(start) + u32::from(len)
}

fn ranges_overlap(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> bool {
    u32::from(a_start) < rect_end(b_start, b_len) && rect_end(a_start, a_len) > u32::from(b_start)
}

fn split_on_requested_edge(split: &SplitBorder, focused: Rect, nav: NavDirection) -> bool {
    split_edge_distance(split, focused, nav) <= SPLIT_EDGE_MATCH_TOLERANCE_CELLS
}

fn split_area_overlaps_focused_pane(split: &SplitBorder, focused: Rect, nav: NavDirection) -> bool {
    match nav {
        NavDirection::Left | NavDirection::Right => {
            ranges_overlap(split.area.y, split.area.height, focused.y, focused.height)
        }
        NavDirection::Up | NavDirection::Down => {
            ranges_overlap(split.area.x, split.area.width, focused.x, focused.width)
        }
    }
}

fn nearest_resize_split(
    splits: &[SplitBorder],
    target_dir: Direction,
    focused: Rect,
    nav: NavDirection,
) -> Option<&SplitBorder> {
    splits
        .iter()
        .filter(|s| s.direction == target_dir)
        .filter(|s| split_area_overlaps_focused_pane(s, focused, nav))
        .filter(|s| split_on_requested_edge(s, focused, nav))
        .min_by_key(|s| split_edge_distance(s, focused, nav))
}

fn opposite_direction(nav: NavDirection) -> NavDirection {
    match nav {
        NavDirection::Left => NavDirection::Right,
        NavDirection::Right => NavDirection::Left,
        NavDirection::Up => NavDirection::Down,
        NavDirection::Down => NavDirection::Up,
    }
}

fn split_edge_distance(split: &SplitBorder, focused: Rect, nav: NavDirection) -> u32 {
    match nav {
        NavDirection::Left => u32::from(split.pos).abs_diff(u32::from(focused.x)),
        NavDirection::Right => u32::from(split.pos).abs_diff(rect_end(focused.x, focused.width)),
        NavDirection::Up => u32::from(split.pos).abs_diff(u32::from(focused.y)),
        NavDirection::Down => u32::from(split.pos).abs_diff(rect_end(focused.y, focused.height)),
    }
}

fn range_overlap_amount(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u32 {
    rect_end(a_start, a_len)
        .min(rect_end(b_start, b_len))
        .saturating_sub(u32::from(a_start.max(b_start)))
}

fn range_center_distance(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u32 {
    let a_center = u32::from(a_start) * 2 + u32::from(a_len);
    let b_center = u32::from(b_start) * 2 + u32::from(b_len);
    a_center.abs_diff(b_center)
}

// --- Tree operations ---

fn count_panes(node: &Node) -> usize {
    match node {
        Node::Pane(_) => 1,
        Node::Split { first, second, .. } => count_panes(first) + count_panes(second),
    }
}

fn collect_panes(node: &Node, area: Rect, focus: PaneId, result: &mut Vec<PaneInfo>) {
    match node {
        Node::Pane(id) => {
            result.push(PaneInfo {
                id: *id,
                rect: area,
                is_focused: *id == focus,
            });
        }
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let (a, b) = split_rect(area, *direction, *ratio);
            collect_panes(first, a, focus, result);
            collect_panes(second, b, focus, result);
        }
    }
}

fn collect_splits(node: &Node, area: Rect, path: &SplitPath, result: &mut Vec<SplitBorder>) {
    if let Node::Split {
        direction,
        ratio,
        first,
        second,
    } = node
    {
        let (a, b) = split_rect(area, *direction, *ratio);
        let pos = match direction {
            Direction::Horizontal => a.x.saturating_add(a.width),
            Direction::Vertical => a.y.saturating_add(a.height),
        };
        result.push(SplitBorder {
            pos,
            direction: *direction,
            ratio: *ratio,
            area,
            path: path.clone(),
        });
        collect_splits(first, a, &path.child(SplitBranch::First), result);
        collect_splits(second, b, &path.child(SplitBranch::Second), result);
    }
}

fn collect_ids(node: &Node, ids: &mut Vec<PaneId>) {
    match node {
        Node::Pane(id) => ids.push(*id),
        Node::Split { first, second, .. } => {
            collect_ids(first, ids);
            collect_ids(second, ids);
        }
    }
}

fn swap_pane_ids(node: &mut Node, first: PaneId, second: PaneId) {
    match node {
        Node::Pane(id) if *id == first => *id = second,
        Node::Pane(id) if *id == second => *id = first,
        Node::Pane(_) => {}
        Node::Split {
            first: first_child,
            second: second_child,
            ..
        } => {
            swap_pane_ids(first_child, first, second);
            swap_pane_ids(second_child, first, second);
        }
    }
}

/// Replaces the `target` leaf, in place, with a split of it and `new_id`.
/// Returns whether the target was found; a tree without it is left untouched.
fn split_at(
    node: &mut Node,
    target: PaneId,
    direction: Direction,
    new_id: PaneId,
    split_ratio: SplitRatio,
) -> bool {
    match node {
        Node::Pane(id) if *id == target => {
            *node = Node::Split {
                direction,
                ratio: split_ratio,
                first: Box::new(Node::Pane(target)),
                second: Box::new(Node::Pane(new_id)),
            };
            true
        }
        Node::Pane(_) => false,
        Node::Split { first, second, .. } => {
            split_at(first, target, direction, new_id, split_ratio)
                || split_at(second, target, direction, new_id, split_ratio)
        }
    }
}

/// Removes the `target` leaf in place: its parent split is replaced by the
/// sibling subtree. Returns whether the target was removed; a tree without
/// it, or one that is only the target leaf, is left untouched.
fn remove_pane(node: &mut Node, target: PaneId) -> bool {
    let Node::Split { first, second, .. } = node else {
        return false;
    };
    let is_target = |child: &Node| matches!(child, Node::Pane(id) if *id == target);
    let sibling = if is_target(first) {
        second
    } else if is_target(second) {
        first
    } else {
        return remove_pane(first, target) || remove_pane(second, target);
    };
    // The removed leaf's own id stands in for the sibling for the one line
    // until the split holding both is overwritten; nothing reads it.
    let sibling = std::mem::replace(&mut **sibling, Node::Pane(target));
    *node = sibling;
    true
}

fn set_ratio_at(node: &mut Node, path: &[SplitBranch], new_ratio: SplitRatio) -> bool {
    if let Node::Split {
        ratio,
        first,
        second,
        ..
    } = node
    {
        if path.is_empty() {
            if *ratio == new_ratio {
                false
            } else {
                *ratio = new_ratio;
                true
            }
        } else if path[0] == SplitBranch::Second {
            set_ratio_at(second, &path[1..], new_ratio)
        } else {
            set_ratio_at(first, &path[1..], new_ratio)
        }
    } else {
        false
    }
}

fn split_rect(area: Rect, direction: Direction, ratio: SplitRatio) -> (Rect, Rect) {
    match direction {
        Direction::Horizontal => {
            let (first_w, second_w) = split_extent(area.width, ratio);
            (
                Rect::new(area.x, area.y, first_w, area.height),
                Rect::new(
                    area.x.saturating_add(first_w),
                    area.y,
                    second_w,
                    area.height,
                ),
            )
        }
        Direction::Vertical => {
            let (first_h, second_h) = split_extent(area.height, ratio);
            (
                Rect::new(area.x, area.y, area.width, first_h),
                Rect::new(area.x, area.y.saturating_add(first_h), area.width, second_h),
            )
        }
    }
}

fn split_extent(total: u16, ratio: SplitRatio) -> (u16, u16) {
    // For axes at least `MIN_SPLIT_EXTENT_CELLS` wide, keep
    // `MIN_SPLIT_CHILD_CELLS` for each child even when the requested fraction
    // rounds to an endpoint. A smaller axis cannot show both children, so
    // retain the ratio-based allocation there.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "SplitRatio keeps the product finite and within [0, total] before rounding"
    )]
    let first = (f32::from(total) * ratio.get()).round() as u16;
    let first = if total >= MIN_SPLIT_EXTENT_CELLS {
        first.clamp(MIN_SPLIT_CHILD_CELLS, total - MIN_SPLIT_CHILD_CELLS)
    } else {
        first
    };
    (first, total.saturating_sub(first))
}

#[cfg(test)]
mod tests {
    // Layout accepts an explicit delta; the server owns its keyboard step policy.
    const RESIZE_DELTA: super::RatioDelta = super::RatioDelta::new(0.05);
    use super::*;

    #[test]
    fn layout_epoch_advances() {
        let epoch = LayoutEpoch::default();
        assert_ne!(epoch, epoch.next());
    }

    #[test]
    fn pane_id_allocation_stops_at_the_end_of_the_id_space_instead_of_wrapping() {
        let counter = std::sync::atomic::AtomicU32::new(u32::MAX - 2);
        assert_eq!(PaneId::alloc_from(&counter), Some(PaneId(u32::MAX - 2)));
        assert_eq!(PaneId::alloc_from(&counter), Some(PaneId(u32::MAX - 1)));
        assert_eq!(PaneId::alloc_from(&counter), None);
        assert_eq!(PaneId::alloc_from(&counter), None, "exhaustion is sticky");
        assert_ne!(PaneId::alloc().raw(), 0);
    }

    #[test]
    fn split_ratio_rejects_values_outside_layout_bounds() {
        assert_eq!(
            SplitRatio::new(MIN_SPLIT_RATIO).map(SplitRatio::get),
            Some(MIN_SPLIT_RATIO)
        );
        assert_eq!(
            SplitRatio::new(MAX_SPLIT_RATIO).map(SplitRatio::get),
            Some(MAX_SPLIT_RATIO)
        );
        assert!(SplitRatio::new(MIN_SPLIT_RATIO - f32::EPSILON).is_none());
        assert!(SplitRatio::new(f32::NAN).is_none());
        assert_eq!(
            SplitRatio::clamped(MIN_SPLIT_RATIO - f32::EPSILON).get(),
            MIN_SPLIT_RATIO
        );
        assert_eq!(SplitRatio::clamped(f32::NAN).get(), EVEN_SPLIT);
    }

    fn pane(id: u32) -> PaneId {
        PaneId::from_raw(id)
    }

    fn saved_layout(root: Node, focus: PaneId) -> TileLayout {
        TileLayout::from_saved(root, focus).expect("test layout is valid")
    }

    fn sample_layout() -> TileLayout {
        saved_layout(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: crate::layout::SplitRatio::clamped(0.3),
                first: Box::new(Node::Pane(pane(1))),
                second: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: crate::layout::SplitRatio::clamped(0.6),
                    first: Box::new(Node::Pane(pane(2))),
                    second: Box::new(Node::Split {
                        direction: Direction::Horizontal,
                        ratio: crate::layout::SplitRatio::clamped(0.4),
                        first: Box::new(Node::Pane(pane(3))),
                        second: Box::new(Node::Pane(pane(4))),
                    }),
                }),
            },
            pane(2),
        )
    }

    fn pane_rects(layout: &TileLayout) -> Vec<(PaneId, Rect)> {
        layout
            .panes(Rect::new(0, 0, 100, 40))
            .into_iter()
            .map(|info| (info.id, info.rect))
            .collect()
    }

    fn pane_rect(layout: &TileLayout, pane_id: PaneId) -> Rect {
        pane_rects(layout)
            .into_iter()
            .find_map(|(id, rect)| (id == pane_id).then_some(rect))
            .expect("pane should exist")
    }

    fn split_snapshot(layout: &TileLayout) -> Vec<(Direction, f32)> {
        fn collect(node: &Node, out: &mut Vec<(Direction, f32)>) {
            match node {
                Node::Pane(_) => {}
                Node::Split {
                    direction,
                    ratio,
                    first,
                    second,
                } => {
                    out.push((*direction, ratio.get()));
                    collect(first, out);
                    collect(second, out);
                }
            }
        }

        let mut out = Vec::new();
        collect(layout.root(), &mut out);
        out
    }

    #[test]
    fn swap_panes_exchanges_leaf_ids_without_changing_cells() {
        let mut layout = sample_layout();
        let before_rects = pane_rects(&layout);
        let before_splits = split_snapshot(&layout);

        assert!(layout.swap_panes(pane(2), pane(4)));

        assert_eq!(layout.pane_count(), 4);
        assert_eq!(split_snapshot(&layout), before_splits);
        assert_eq!(layout.focused(), pane(2));

        let after_rects = pane_rects(&layout);
        assert_eq!(after_rects[0], before_rects[0]);
        assert_eq!(after_rects[1], (pane(4), before_rects[1].1));
        assert_eq!(after_rects[2], before_rects[2]);
        assert_eq!(after_rects[3], (pane(2), before_rects[3].1));
    }

    #[test]
    fn swap_panes_is_noop_for_same_or_missing_pane() {
        let mut layout = sample_layout();
        let before_rects = pane_rects(&layout);
        let before_splits = split_snapshot(&layout);
        let before_focus = layout.focused();

        assert!(!layout.swap_panes(pane(2), pane(2)));
        assert!(!layout.swap_panes(pane(2), pane(99)));
        assert!(!layout.swap_panes(pane(99), pane(2)));

        assert_eq!(pane_rects(&layout), before_rects);
        assert_eq!(split_snapshot(&layout), before_splits);
        assert_eq!(layout.focused(), before_focus);
    }

    #[test]
    fn split_pane_with_a_ratio_sets_the_new_split_ratio() {
        let (mut layout, root) = TileLayout::new();

        assert!(layout.split_pane(
            root,
            Direction::Horizontal,
            SplitRatio::clamped(0.333),
            PaneId::alloc()
        ));

        let splits = split_snapshot(&layout);
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].0, Direction::Horizontal);
        assert!((splits[0].1 - 0.333).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_pane_moves_a_split_without_touching_focus_or_its_history() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));
        let original_focus = layout.focused();

        assert!(layout.resize_pane(
            pane(1),
            NavDirection::Right,
            RESIZE_DELTA,
            Rect::new(0, 0, 100, 40),
        ));

        assert_eq!(layout.focused(), original_focus);
        let split = split_snapshot(&layout)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.35).abs() < f32::EPSILON);
        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(2), "history still names pane 2");
    }

    #[test]
    fn resize_pane_reports_no_change_when_the_ratio_is_already_clamped() {
        let mut layout = sample_layout();
        let area = Rect::new(0, 0, 100, 40);
        for _ in 0..40 {
            layout.resize_pane(pane(1), NavDirection::Right, RESIZE_DELTA, area);
        }
        assert_eq!(split_snapshot(&layout)[0].1, MAX_SPLIT_RATIO);

        assert!(!layout.resize_pane(pane(1), NavDirection::Right, RESIZE_DELTA, area));
        assert!(!layout.resize_pane(pane(99), NavDirection::Right, RESIZE_DELTA, area));
    }

    #[test]
    fn split_pane_installs_the_given_id_and_leaves_focus() {
        let (mut layout, root) = TileLayout::new();
        let new_pane = PaneId::alloc();

        assert!(layout.split_pane(root, Direction::Vertical, SplitRatio::EVEN, new_pane));

        assert_eq!(layout.pane_ids(), vec![root, new_pane]);
        assert_eq!(layout.focused(), root);
    }

    #[test]
    fn split_pane_refuses_a_missing_target_or_a_present_id() {
        let mut layout = sample_layout();
        let ids = layout.pane_ids();

        assert!(!layout.split_pane(pane(99), Direction::Horizontal, SplitRatio::EVEN, pane(50)));
        assert!(!layout.split_pane(pane(1), Direction::Horizontal, SplitRatio::EVEN, pane(2)));
        assert!(!layout.split_pane(pane(1), Direction::Horizontal, SplitRatio::EVEN, pane(1)));

        assert_eq!(layout.pane_ids(), ids);
    }

    #[test]
    fn split_paths_address_the_split_they_were_read_from() {
        let mut layout = sample_layout();
        let area = Rect::new(0, 0, 100, 40);
        let splits = layout.splits(area);
        assert_eq!(splits.len(), 3);
        assert_eq!(splits[0].path, SplitPath::default());
        assert_eq!(splits[1].path, SplitPath::from(vec![SplitBranch::Second]));
        assert_eq!(
            splits[2].path.branches(),
            [SplitBranch::Second, SplitBranch::Second]
        );

        let ratio = SplitRatio::clamped(0.7);
        assert!(layout.set_ratio_at(&splits[2].path, ratio));
        let snapshot = split_snapshot(&layout);
        assert_eq!(snapshot[0].1, 0.3);
        assert_eq!(snapshot[1].1, 0.6);
        assert_eq!(snapshot[2].1, ratio.get());
        assert!(!layout.set_ratio_at(&splits[2].path, ratio));
        assert!(!layout.set_ratio_at(&SplitPath::from(vec![SplitBranch::First]), ratio));
    }

    #[test]
    fn resize_second_child_toward_split_decreases_ratio() {
        let (mut layout, root) = TileLayout::new();
        let right = PaneId::alloc();
        assert!(layout.split_pane(root, Direction::Horizontal, SplitRatio::EVEN, right));

        assert!(layout.resize_pane(
            right,
            NavDirection::Left,
            RESIZE_DELTA,
            Rect::new(0, 0, 100, 40),
        ));

        let split = split_snapshot(&layout)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - (EVEN_SPLIT - RESIZE_DELTA.0)).abs() < f32::EPSILON);
        assert_eq!(layout.focused(), root);
    }

    #[test]
    fn resize_outer_edges_shrink_focused_pane() {
        let (mut horizontal, left) = TileLayout::new();
        assert!(horizontal.split_pane(
            left,
            Direction::Horizontal,
            SplitRatio::EVEN,
            PaneId::alloc()
        ));

        assert!(horizontal.resize_pane(
            left,
            NavDirection::Left,
            RESIZE_DELTA,
            Rect::new(0, 0, 100, 40),
        ));
        let split = split_snapshot(&horizontal)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - (EVEN_SPLIT - RESIZE_DELTA.0)).abs() < f32::EPSILON);

        let (mut horizontal, left) = TileLayout::new();
        let right = PaneId::alloc();
        assert!(horizontal.split_pane(left, Direction::Horizontal, SplitRatio::EVEN, right));

        assert!(horizontal.resize_pane(
            right,
            NavDirection::Right,
            RESIZE_DELTA,
            Rect::new(0, 0, 100, 40),
        ));
        let split = split_snapshot(&horizontal)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - (EVEN_SPLIT + RESIZE_DELTA.0)).abs() < f32::EPSILON);

        let (mut vertical, top) = TileLayout::new();
        assert!(vertical.split_pane(top, Direction::Vertical, SplitRatio::EVEN, PaneId::alloc()));

        assert!(vertical.resize_pane(
            top,
            NavDirection::Up,
            RESIZE_DELTA,
            Rect::new(0, 0, 100, 40),
        ));
        let split = split_snapshot(&vertical)[0];
        assert_eq!(split.0, Direction::Vertical);
        assert!((split.1 - (EVEN_SPLIT - RESIZE_DELTA.0)).abs() < f32::EPSILON);

        let (mut vertical, top) = TileLayout::new();
        let bottom = PaneId::alloc();
        assert!(vertical.split_pane(top, Direction::Vertical, SplitRatio::EVEN, bottom));

        assert!(vertical.resize_pane(
            bottom,
            NavDirection::Down,
            RESIZE_DELTA,
            Rect::new(0, 0, 100, 40),
        ));
        let split = split_snapshot(&vertical)[0];
        assert_eq!(split.0, Direction::Vertical);
        assert!((split.1 - (EVEN_SPLIT + RESIZE_DELTA.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_outer_edge_falls_back_to_horizontal_ancestor_split() {
        let mut layout = saved_layout(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: crate::layout::SplitRatio::clamped(0.6),
                first: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: crate::layout::SplitRatio::clamped(0.5),
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Pane(pane(3))),
            },
            pane(1),
        );
        let before = pane_rect(&layout, pane(1));

        assert!(layout.resize_pane(
            pane(1),
            NavDirection::Left,
            RESIZE_DELTA,
            Rect::new(0, 0, 100, 40),
        ));

        let after = pane_rect(&layout, pane(1));
        assert_eq!(after.height, before.height);
        assert!(after.width < before.width);
        let splits = split_snapshot(&layout);
        assert_eq!(splits[0].0, Direction::Horizontal);
        assert!((splits[0].1 - (0.6 - RESIZE_DELTA.0)).abs() < f32::EPSILON);
        assert_eq!(splits[1], (Direction::Vertical, 0.5));
    }

    #[test]
    fn resize_outer_edge_falls_back_to_vertical_ancestor_split() {
        let mut layout = saved_layout(
            Node::Split {
                direction: Direction::Vertical,
                ratio: crate::layout::SplitRatio::clamped(0.6),
                first: Box::new(Node::Split {
                    direction: Direction::Horizontal,
                    ratio: crate::layout::SplitRatio::clamped(0.5),
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Pane(pane(3))),
            },
            pane(1),
        );
        let before = pane_rect(&layout, pane(1));

        assert!(layout.resize_pane(
            pane(1),
            NavDirection::Up,
            RESIZE_DELTA,
            Rect::new(0, 0, 100, 40),
        ));

        let after = pane_rect(&layout, pane(1));
        assert_eq!(after.width, before.width);
        assert!(after.height < before.height);
        let splits = split_snapshot(&layout);
        assert_eq!(splits[0].0, Direction::Vertical);
        assert!((splits[0].1 - (0.6 - RESIZE_DELTA.0)).abs() < f32::EPSILON);
        assert_eq!(splits[1], (Direction::Horizontal, 0.5));
    }

    #[test]
    fn resize_uses_split_in_same_branch_when_borders_share_coordinate() {
        let mut layout = saved_layout(
            Node::Split {
                direction: Direction::Vertical,
                ratio: crate::layout::SplitRatio::clamped(0.5),
                first: Box::new(Node::Split {
                    direction: Direction::Horizontal,
                    ratio: crate::layout::SplitRatio::clamped(0.5),
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Split {
                    direction: Direction::Horizontal,
                    ratio: crate::layout::SplitRatio::clamped(0.5),
                    first: Box::new(Node::Pane(pane(3))),
                    second: Box::new(Node::Pane(pane(4))),
                }),
            },
            pane(3),
        );

        assert!(layout.resize_pane(
            pane(3),
            NavDirection::Right,
            RESIZE_DELTA,
            Rect::new(0, 0, 100, 40),
        ));

        let splits = split_snapshot(&layout);
        assert_eq!(splits[0], (Direction::Vertical, 0.5));
        assert_eq!(splits[1], (Direction::Horizontal, 0.5));
        assert_eq!(splits[2].0, Direction::Horizontal);
        assert!((splits[2].1 - (EVEN_SPLIT + RESIZE_DELTA.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn find_in_direction_tiebreaks_by_larger_overlap_before_layout_order() {
        let focused = PaneInfo {
            id: pane(1),
            rect: Rect::new(10, 10, 10, 10),
            is_focused: true,
        };
        let small_overlap_first = PaneInfo {
            id: pane(2),
            rect: Rect::new(0, 10, 10, 2),
            is_focused: false,
        };
        let larger_overlap_second = PaneInfo {
            id: pane(3),
            rect: Rect::new(0, 10, 10, 8),
            is_focused: false,
        };
        let panes = vec![focused.clone(), small_overlap_first, larger_overlap_second];

        assert_eq!(
            find_in_direction(&focused, NavDirection::Left, &panes),
            Some(pane(3))
        );
    }

    #[test]
    fn close_focused_returns_to_the_pane_focus_came_from() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        assert!(layout.close_focused());

        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn close_focused_returns_to_the_pane_that_opened_a_split() {
        // Allocated ids only: sample_layout() uses from_raw and shares the id
        // space with the allocator.
        let (mut layout, first) = TileLayout::new();
        let second = PaneId::alloc();
        assert!(layout.split_pane(first, Direction::Horizontal, SplitRatio::EVEN, second));
        layout.focus_pane(second);
        let third = PaneId::alloc();
        assert!(layout.split_pane(second, Direction::Vertical, SplitRatio::EVEN, third));
        layout.focus_pane(third);
        assert_eq!(layout.pane_ids().len(), 3);

        layout.focus_pane(first);
        let opened = PaneId::alloc();
        assert!(layout.split_pane(first, Direction::Horizontal, SplitRatio::EVEN, opened));
        layout.focus_pane(opened);
        assert_eq!(layout.focused(), opened);

        assert!(layout.close_focused());

        assert_eq!(layout.focused(), first);
        assert!(layout.pane_ids().contains(&second));
        assert!(layout.pane_ids().contains(&third));
    }

    #[test]
    fn closing_a_background_pane_keeps_the_focused_pane_history() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        assert!(layout.close_pane(pane(1)));
        assert_eq!(layout.focused(), pane(4));

        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn closing_the_remembered_pane_drops_the_focus_history() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        assert!(layout.close_pane(pane(2)));

        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(3));
    }

    #[test]
    fn close_focused_uses_tree_order_without_focus_history() {
        let mut layout = sample_layout();

        assert!(layout.close_focused());

        assert_eq!(layout.focused(), pane(3));
    }

    #[test]
    fn close_focused_does_not_reuse_history_after_it_is_consumed() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(2));

        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(3));
    }

    #[test]
    fn resize_does_not_disturb_the_close_focus_target() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));
        layout.resize_pane(
            pane(1),
            NavDirection::Right,
            RESIZE_DELTA,
            Rect::new(0, 0, 100, 40),
        );

        assert!(layout.close_focused());

        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn split_pane_leaves_focus_and_history_untouched() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        let new_id = pane(50);
        assert!(layout.split_pane(
            pane(1),
            Direction::Horizontal,
            SplitRatio::clamped(0.5),
            new_id
        ));

        assert!(layout.pane_ids().contains(&new_id));
        assert_eq!(layout.focused(), pane(4));
        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn discarded_prepared_split_preserves_focus_history() {
        let (mut layout, root) = TileLayout::new();
        assert!(layout.split_pane(
            root,
            Direction::Horizontal,
            SplitRatio::clamped(0.5),
            PaneId::alloc()
        ));
        let focused = PaneId::alloc();
        assert!(layout.split_pane(root, Direction::Vertical, SplitRatio::clamped(0.5), focused));
        layout.focus_pane(focused);
        let original_ids = layout.pane_ids();
        let mut prepared = layout.clone();
        let new_id = PaneId::alloc();
        assert!(prepared.split_pane(
            focused,
            Direction::Horizontal,
            SplitRatio::clamped(0.5),
            new_id
        ));
        assert!(prepared.pane_ids().contains(&new_id));

        assert_eq!(layout.pane_ids(), original_ids);
        assert_eq!(layout.focused(), focused);
        let mut probe = layout.clone();
        assert!(probe.close_focused());
        assert_eq!(probe.focused(), root);
    }

    #[test]
    fn from_saved_rejects_broken_pane_identity() {
        let pair = |a: u32, b: u32| Node::Split {
            direction: Direction::Horizontal,
            ratio: SplitRatio::clamped(0.5),
            first: Box::new(Node::Pane(pane(a))),
            second: Box::new(Node::Pane(pane(b))),
        };
        assert!(TileLayout::from_saved(pair(1, 2), pane(2)).is_ok());
        assert_eq!(
            TileLayout::from_saved(pair(1, 2), pane(3)).err(),
            Some(InvalidSavedLayout::FocusNotFound(pane(3)))
        );
        assert_eq!(
            TileLayout::from_saved(pair(1, 1), pane(1)).err(),
            Some(InvalidSavedLayout::DuplicatePaneId(pane(1)))
        );
    }

    #[test]
    fn split_leaves_a_cell_for_each_child() {
        for total in 2..=12 {
            for ratio in [0.1, 0.5, 0.9].map(SplitRatio::clamped) {
                let (first, second) = split_extent(total, ratio);
                assert!(first >= 1 && second >= 1, "{total} x {}", ratio.get());
                assert_eq!(first + second, total);
            }
        }
        let even = SplitRatio::clamped(0.5);
        assert_eq!(split_extent(1, even).0 + split_extent(1, even).1, 1);
        assert_eq!(split_extent(0, even), (0, 0));
    }

    #[test]
    fn direction_search_handles_rects_at_the_u16_edge() {
        let far = Rect::new(u16::MAX - 1, 0, 1, 10);
        let right_edge = Rect::new(u16::MAX, 0, u16::MAX, 10);
        assert!(ranges_overlap(
            far.y,
            far.height,
            right_edge.y,
            right_edge.height
        ));
        assert!(!ranges_overlap(
            far.x,
            far.width,
            right_edge.x,
            right_edge.width
        ));
        assert_eq!(rect_end(u16::MAX, u16::MAX), 2 * u32::from(u16::MAX));
    }
}
