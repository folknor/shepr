//! BSP tree layout for tiling panes within a workspace.

use std::cmp::Reverse;
use std::collections::HashSet;

use ratatui::layout::{Direction, Rect};

use crate::geometry::SplitBranch;
use crate::limits::{
    FIRST_PANE_ID, MIN_SPLIT_CHILD_CELLS, MIN_SPLIT_EXTENT_CELLS, MIN_WORKSPACE_PANES,
    PLACEHOLDER_PANE_ID, SPLIT_EDGE_MATCH_TOLERANCE_CELLS,
};

pub use crate::limits::{EVEN_SPLIT, MAX_SPLIT_RATIO, MIN_SPLIT_RATIO};

/// First-child share of a BSP split.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SplitRatio(f32);

impl SplitRatio {
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PaneId(u32);

/// Global atomic counter for unique PaneId generation across all workspaces.
static NEXT_PANE_ID: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(FIRST_PANE_ID);

impl PaneId {
    /// Allocate a globally unique PaneId.
    ///
    /// Never returns the placeholder ID and never hands out an ID twice.
    /// The counter refuses to advance past `u32::MAX` instead of wrapping, so
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

    /// Reconstruct a raw id without advancing the allocator. Live restore must
    /// remap saved pane IDs through `alloc` before installing the layout.
    pub fn from_raw(id: u32) -> Self {
        Self(id)
    }
}

/// A pane's position and focus state in the BSP tree. UI chrome is added
/// after layout, in `workspace::geometry` and `ui`.
#[derive(Clone)]
pub struct PaneInfo {
    pub id: PaneId,
    pub rect: Rect,
    pub is_focused: bool,
}

/// Info about a split boundary, used for mouse drag resize.
#[derive(Clone)]
pub struct SplitBorder {
    /// Position of the divider line (x for horizontal split, y for vertical).
    pub pos: u16,
    /// Direction of the split that created this border.
    pub direction: Direction,
    /// Ratio assigned to the first child of this split.
    pub ratio: f32,
    /// Total area of the split node.
    pub area: Rect,
    /// Path from root to this split node.
    pub path: Vec<SplitBranch>,
}

/// Cardinal direction for pane navigation.
#[derive(Debug, Clone, Copy)]
pub enum NavDirection {
    Left,
    Right,
    Up,
    Down,
}

/// A node in the BSP tree. Pane leaves connect layout order to `Tab.panes`;
/// pane state and public numbers live in those tab records.
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

/// BSP tiling layout. Tracks a tree of splits and a focused pane.
#[derive(Clone)]
pub struct TileLayout {
    root: Node,
    focus: PaneId,
    /// Pane focused before `focus`, used by `close_focused`. Only a real focus
    /// move writes it; tree edits go through the target-taking primitives
    /// (`split_pane`, `close_pane`, unfocused `insert_pane_near`) so internal
    /// focus excursions never corrupt it.
    prev_focus: Option<PaneId>,
}

/// A saved layout defect rejected before it becomes a live [`TileLayout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidSavedLayout {
    /// A leaf uses the reserved placeholder ID while a layout edit is in progress.
    PlaceholderPaneId,
    /// Two leaves use the same pane identity.
    DuplicatePaneId(PaneId),
    /// The focused pane is not present among the leaves.
    FocusNotFound(PaneId),
    /// A saved split ratio is non-finite or outside the permitted bounds.
    InvalidSplitRatio,
}

impl TileLayout {
    /// Create a new layout with a single pane (globally unique ID).
    /// Returns (layout, root_pane_id) so the caller can create the pane.
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

    /// Rebuild a one-pane layout for a pane detached from a valid live layout.
    /// The source layout has already established that this ID is nonzero and unique.
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
        collect_splits(&self.root, area, vec![], &mut result);
        result
    }

    /// Split the focused pane. Returns the new pane's id. This helper is used
    /// by tests; production prepares a cloned layout before starting a runtime.
    pub fn split_focused(&mut self, direction: Direction) -> PaneId {
        self.split_focused_with_ratio(direction, EVEN_SPLIT)
    }

    /// Split the focused pane with a custom first-child ratio.
    pub(crate) fn split_focused_with_ratio(&mut self, direction: Direction, ratio: f32) -> PaneId {
        let ids = self.pane_ids();
        let target = if ids.contains(&self.focus) {
            self.focus
        } else {
            let first = first_pane_id(&self.root);
            self.focus = first;
            self.prev_focus = None;
            first
        };
        let new_id = PaneId::alloc();
        let placeholder = PaneId::from_raw(PLACEHOLDER_PANE_ID);
        let old = std::mem::replace(&mut self.root, Node::Pane(placeholder));
        self.root = split_at(old, target, direction, new_id, SplitRatio::clamped(ratio));
        self.set_focus(new_id);
        new_id
    }

    /// Split `target` without moving focus. Returns the new pane's id, or None
    /// when `target` is not in the layout. Launch paths prepare on a cloned
    /// layout and install that value only after the runtime starts.
    pub fn split_pane(
        &mut self,
        target: PaneId,
        direction: Direction,
        ratio: f32,
    ) -> Option<PaneId> {
        if !self.pane_ids().contains(&target) {
            return None;
        }
        let new_id = PaneId::alloc();
        let placeholder = PaneId::from_raw(PLACEHOLDER_PANE_ID);
        let old = std::mem::replace(&mut self.root, Node::Pane(placeholder));
        self.root = split_at(old, target, direction, new_id, SplitRatio::clamped(ratio));
        Some(new_id)
    }

    /// Insert an existing pane id next to a target pane without allocating a new
    /// pane or spawning a terminal runtime. When `focus` is false, focus and its
    /// history are left untouched.
    pub fn insert_pane_near(
        &mut self,
        target: PaneId,
        moved: PaneId,
        direction: Direction,
        ratio: f32,
        focus: bool,
    ) -> bool {
        if target == moved || moved.raw() == PLACEHOLDER_PANE_ID {
            return false;
        }
        let ids = self.pane_ids();
        if !ids.contains(&target) || ids.contains(&moved) {
            return false;
        }

        let placeholder = PaneId::from_raw(PLACEHOLDER_PANE_ID);
        let old = std::mem::replace(&mut self.root, Node::Pane(placeholder));
        self.root = split_at(old, target, direction, moved, SplitRatio::clamped(ratio));
        if focus {
            self.set_focus(moved);
        }
        true
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
        let placeholder = PaneId::from_raw(PLACEHOLDER_PANE_ID);
        let old = std::mem::replace(&mut self.root, Node::Pane(placeholder));
        if let Some(new_root) = remove_pane(old, target) {
            self.root = new_root;
            self.focus = new_focus;
            self.prev_focus = None;
            true
        } else {
            false
        }
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
        let placeholder = PaneId::from_raw(PLACEHOLDER_PANE_ID);
        let old = std::mem::replace(&mut self.root, Node::Pane(placeholder));
        let Some(new_root) = remove_pane(old, id) else {
            return false;
        };
        self.root = new_root;
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

    /// Set the ratio of a split node at the given path.
    pub fn set_ratio_at(&mut self, path: &[SplitBranch], ratio: f32) -> bool {
        set_ratio_at(&mut self.root, path, SplitRatio::clamped(ratio))
    }

    /// Adjust the nearest split in the given direction for the focused pane.
    /// `delta` is positive to grow, negative to shrink.
    pub fn resize_focused(&mut self, nav: NavDirection, delta: f32, area: Rect) {
        let panes = self.panes(area);
        let Some(focused) = panes.iter().find(|p| p.is_focused) else {
            return;
        };
        let focused_rect = focused.rect;
        let splits = self.splits(area);

        let target_dir = match nav {
            NavDirection::Left | NavDirection::Right => Direction::Horizontal,
            NavDirection::Up | NavDirection::Down => Direction::Vertical,
        };
        let grows = matches!(nav, NavDirection::Right | NavDirection::Down);

        let best = nearest_resize_split(&splits, target_dir, focused_rect, nav).or_else(|| {
            nearest_resize_split(&splits, target_dir, focused_rect, opposite_direction(nav))
        });

        if let Some(split) = best {
            let path = split.path.clone();
            let current_ratio = get_ratio_at(&self.root, &path).map_or(EVEN_SPLIT, SplitRatio::get);
            let adj = if grows { delta } else { -delta };
            self.set_ratio_at(&path, current_ratio + adj);
        }
    }

    pub fn resize_pane(
        &mut self,
        pane_id: PaneId,
        nav: NavDirection,
        delta: f32,
        area: Rect,
    ) -> bool {
        if !self.pane_ids().contains(&pane_id) {
            return false;
        }
        let before = split_ratios(&self.root);
        let previous_focus = self.focus;
        self.focus = pane_id;
        self.resize_focused(nav, delta, area);
        self.focus = previous_focus;
        split_ratios(&self.root) != before
    }

    /// Pane record keys in layout order. `Tab.panes` owns the live records.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        collect_ids(&self.root, &mut ids);
        ids
    }

    /// Access the tree root for serialization.
    pub fn root(&self) -> &Node {
        &self.root
    }

    /// Reconstruct a layout from a saved tree after checking pane identity invariants.
    /// Callers must remap restored IDs through [`PaneId::alloc`] first so they
    /// remain unique across live layouts.
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
            if id.raw() == PLACEHOLDER_PANE_ID {
                return Err(InvalidSavedLayout::PlaceholderPaneId);
            }
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
        .filter(|(_, p)| {
            let r = p.rect;
            match direction {
                NavDirection::Left => {
                    rect_end(r.x, r.width) <= u32::from(fr.x)
                        && ranges_overlap(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Right => {
                    u32::from(r.x) >= rect_end(fr.x, fr.width)
                        && ranges_overlap(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Up => {
                    rect_end(r.y, r.height) <= u32::from(fr.y)
                        && ranges_overlap(r.x, r.width, fr.x, fr.width)
                }
                NavDirection::Down => {
                    u32::from(r.y) >= rect_end(fr.y, fr.height)
                        && ranges_overlap(r.x, r.width, fr.x, fr.width)
                }
            }
        })
        .min_by_key(|(index, p)| {
            let r = p.rect;
            let edge_distance = match direction {
                NavDirection::Left => u32::from(fr.x) - rect_end(r.x, r.width),
                NavDirection::Right => u32::from(r.x) - rect_end(fr.x, fr.width),
                NavDirection::Up => u32::from(fr.y) - rect_end(r.y, r.height),
                NavDirection::Down => u32::from(r.y) - rect_end(fr.y, fr.height),
            };
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
            (edge_distance, Reverse(overlap), center_distance, *index)
        })
        .map(|(_, p)| p.id)
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
            let (a, b) = split_rect(area, *direction, ratio.get());
            collect_panes(first, a, focus, result);
            collect_panes(second, b, focus, result);
        }
    }
}

fn collect_splits(node: &Node, area: Rect, path: Vec<SplitBranch>, result: &mut Vec<SplitBorder>) {
    if let Node::Split {
        direction,
        ratio,
        first,
        second,
    } = node
    {
        let (a, b) = split_rect(area, *direction, ratio.get());
        let pos = match direction {
            Direction::Horizontal => a.x.saturating_add(a.width),
            Direction::Vertical => a.y.saturating_add(a.height),
        };
        result.push(SplitBorder {
            pos,
            direction: *direction,
            ratio: ratio.get(),
            area,
            path: path.clone(),
        });
        let mut lp = path.clone();
        lp.push(SplitBranch::First);
        collect_splits(first, a, lp, result);
        let mut rp = path;
        rp.push(SplitBranch::Second);
        collect_splits(second, b, rp, result);
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

fn first_pane_id(node: &Node) -> PaneId {
    match node {
        Node::Pane(id) => *id,
        Node::Split { first, .. } => first_pane_id(first),
    }
}

fn split_ratios(node: &Node) -> Vec<(Vec<SplitBranch>, f32)> {
    fn collect(node: &Node, path: &mut Vec<SplitBranch>, out: &mut Vec<(Vec<SplitBranch>, f32)>) {
        match node {
            Node::Pane(_) => {}
            Node::Split {
                ratio,
                first,
                second,
                ..
            } => {
                out.push((path.clone(), ratio.get()));
                path.push(SplitBranch::First);
                collect(first, path, out);
                path.pop();
                path.push(SplitBranch::Second);
                collect(second, path, out);
                path.pop();
            }
        }
    }

    let mut out = Vec::new();
    collect(node, &mut Vec::new(), &mut out);
    out
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

fn split_at(
    node: Node,
    target: PaneId,
    direction: Direction,
    new_id: PaneId,
    split_ratio: SplitRatio,
) -> Node {
    match node {
        Node::Pane(id) if id == target => Node::Split {
            direction,
            ratio: split_ratio,
            first: Box::new(Node::Pane(id)),
            second: Box::new(Node::Pane(new_id)),
        },
        Node::Pane(_) => node,
        Node::Split {
            direction: d,
            ratio,
            first,
            second,
        } => Node::Split {
            direction: d,
            ratio,
            first: Box::new(split_at(*first, target, direction, new_id, split_ratio)),
            second: Box::new(split_at(*second, target, direction, new_id, split_ratio)),
        },
    }
}

fn remove_pane(node: Node, target: PaneId) -> Option<Node> {
    match node {
        Node::Pane(id) if id == target => None,
        Node::Pane(_) => Some(node),
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => match (remove_pane(*first, target), remove_pane(*second, target)) {
            (None, Some(s)) => Some(s),
            (Some(f), None) => Some(f),
            (Some(f), Some(s)) => Some(Node::Split {
                direction,
                ratio,
                first: Box::new(f),
                second: Box::new(s),
            }),
            (None, None) => None,
        },
    }
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
            *ratio = new_ratio;
            true
        } else if path[0] == SplitBranch::Second {
            set_ratio_at(second, &path[1..], new_ratio)
        } else {
            set_ratio_at(first, &path[1..], new_ratio)
        }
    } else {
        false
    }
}

fn get_ratio_at(node: &Node, path: &[SplitBranch]) -> Option<SplitRatio> {
    if let Node::Split {
        ratio,
        first,
        second,
        ..
    } = node
    {
        if path.is_empty() {
            Some(*ratio)
        } else if path[0] == SplitBranch::Second {
            get_ratio_at(second, &path[1..])
        } else {
            get_ratio_at(first, &path[1..])
        }
    } else {
        None
    }
}

fn split_rect(area: Rect, direction: Direction, ratio: f32) -> (Rect, Rect) {
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

fn split_extent(total: u16, ratio: f32) -> (u16, u16) {
    // For axes at least `MIN_SPLIT_EXTENT_CELLS` wide, keep
    // `MIN_SPLIT_CHILD_CELLS` for each child even when the requested fraction
    // rounds to an endpoint. A smaller axis cannot show both children, so
    // retain the ratio-based allocation there.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "SplitRatio keeps the product finite and within [0, total] before rounding"
    )]
    let first = (f32::from(total) * ratio).round() as u16;
    let first = if total >= MIN_SPLIT_EXTENT_CELLS {
        first.clamp(MIN_SPLIT_CHILD_CELLS, total - MIN_SPLIT_CHILD_CELLS)
    } else {
        first
    };
    (first, total.saturating_sub(first))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn insert_existing_pane_near_target_preserves_existing_ids_and_focuses_moved_pane() {
        let (mut layout, root) = TileLayout::new();
        let moved = pane(99);

        assert!(layout.insert_pane_near(root, moved, Direction::Horizontal, 0.25, true));

        assert_eq!(layout.pane_count(), 2);
        assert_eq!(layout.pane_ids(), vec![root, moved]);
        assert_eq!(layout.focused(), moved);
        let splits = split_snapshot(&layout);
        assert_eq!(splits, vec![(Direction::Horizontal, 0.25)]);
        assert_eq!(pane_rect(&layout, root), Rect::new(0, 0, 25, 40));
        assert_eq!(pane_rect(&layout, moved), Rect::new(25, 0, 75, 40));
    }

    #[test]
    fn split_focused_with_ratio_sets_new_split_ratio() {
        let (mut layout, root) = TileLayout::new();
        layout.focus_pane(root);

        layout.split_focused_with_ratio(Direction::Horizontal, 0.333);

        let splits = split_snapshot(&layout);
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].0, Direction::Horizontal);
        assert!((splits[0].1 - 0.333).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_pane_preserves_focus_and_reports_change() {
        let mut layout = sample_layout();
        let original_focus = layout.focused();

        assert!(layout.resize_pane(pane(1), NavDirection::Right, 0.05, Rect::new(0, 0, 100, 40),));

        assert_eq!(layout.focused(), original_focus);
        let split = split_snapshot(&layout)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.35).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_second_child_toward_split_decreases_ratio() {
        let (mut layout, root) = TileLayout::new();
        let right = layout.split_focused(Direction::Horizontal);
        layout.focus_pane(root);

        assert!(layout.resize_pane(right, NavDirection::Left, 0.05, Rect::new(0, 0, 100, 40),));

        let split = split_snapshot(&layout)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.45).abs() < f32::EPSILON);
        assert_eq!(layout.focused(), root);
    }

    #[test]
    fn resize_outer_edges_shrink_focused_pane() {
        let (mut horizontal, left) = TileLayout::new();
        horizontal.split_focused(Direction::Horizontal);

        assert!(horizontal.resize_pane(left, NavDirection::Left, 0.05, Rect::new(0, 0, 100, 40),));
        let split = split_snapshot(&horizontal)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.45).abs() < f32::EPSILON);

        let (mut horizontal, _left) = TileLayout::new();
        let right = horizontal.split_focused(Direction::Horizontal);

        assert!(
            horizontal.resize_pane(right, NavDirection::Right, 0.05, Rect::new(0, 0, 100, 40),)
        );
        let split = split_snapshot(&horizontal)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.55).abs() < f32::EPSILON);

        let (mut vertical, top) = TileLayout::new();
        vertical.split_focused(Direction::Vertical);

        assert!(vertical.resize_pane(top, NavDirection::Up, 0.05, Rect::new(0, 0, 100, 40),));
        let split = split_snapshot(&vertical)[0];
        assert_eq!(split.0, Direction::Vertical);
        assert!((split.1 - 0.45).abs() < f32::EPSILON);

        let (mut vertical, _top) = TileLayout::new();
        let bottom = vertical.split_focused(Direction::Vertical);

        assert!(vertical.resize_pane(bottom, NavDirection::Down, 0.05, Rect::new(0, 0, 100, 40),));
        let split = split_snapshot(&vertical)[0];
        assert_eq!(split.0, Direction::Vertical);
        assert!((split.1 - 0.55).abs() < f32::EPSILON);
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

        assert!(layout.resize_pane(pane(1), NavDirection::Left, 0.05, Rect::new(0, 0, 100, 40),));

        let after = pane_rect(&layout, pane(1));
        assert_eq!(after.height, before.height);
        assert!(after.width < before.width);
        let splits = split_snapshot(&layout);
        assert_eq!(splits[0].0, Direction::Horizontal);
        assert!((splits[0].1 - 0.55).abs() < f32::EPSILON);
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

        assert!(layout.resize_pane(pane(1), NavDirection::Up, 0.05, Rect::new(0, 0, 100, 40),));

        let after = pane_rect(&layout, pane(1));
        assert_eq!(after.width, before.width);
        assert!(after.height < before.height);
        let splits = split_snapshot(&layout);
        assert_eq!(splits[0].0, Direction::Vertical);
        assert!((splits[0].1 - 0.55).abs() < f32::EPSILON);
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

        assert!(layout.resize_pane(pane(3), NavDirection::Right, 0.05, Rect::new(0, 0, 100, 40),));

        let splits = split_snapshot(&layout);
        assert_eq!(splits[0], (Direction::Vertical, 0.5));
        assert_eq!(splits[1], (Direction::Horizontal, 0.5));
        assert_eq!(splits[2].0, Direction::Horizontal);
        assert!((splits[2].1 - 0.55).abs() < f32::EPSILON);
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
        let second = layout.split_focused(Direction::Horizontal);
        let third = layout.split_focused(Direction::Vertical);
        assert_eq!(layout.pane_ids().len(), 3);

        layout.focus_pane(first);
        let opened = layout.split_focused(Direction::Horizontal);
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
        layout.resize_pane(pane(1), NavDirection::Right, 0.05, Rect::new(0, 0, 100, 40));

        assert!(layout.close_focused());

        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn split_pane_leaves_focus_and_history_untouched() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        let new_id = layout
            .split_pane(pane(1), Direction::Horizontal, 0.5)
            .expect("target exists");

        assert!(layout.pane_ids().contains(&new_id));
        assert_eq!(layout.focused(), pane(4));
        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn split_pane_missing_target_changes_nothing() {
        let mut layout = sample_layout();
        let ids = layout.pane_ids();

        assert_eq!(
            layout.split_pane(pane(99), Direction::Horizontal, 0.5),
            None
        );

        assert_eq!(layout.pane_ids(), ids);
    }

    #[test]
    fn insert_pane_near_unfocused_keeps_focus_and_history() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        assert!(layout.insert_pane_near(pane(1), pane(9), Direction::Horizontal, 0.5, false));

        assert_eq!(layout.focused(), pane(4));
        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn discarded_prepared_split_preserves_focus_history() {
        let (mut layout, root) = TileLayout::new();
        let _ = layout
            .split_pane(root, Direction::Horizontal, 0.5)
            .expect("target exists");
        let focused = layout
            .split_pane(root, Direction::Vertical, 0.5)
            .expect("target exists");
        layout.focus_pane(focused);
        let original_ids = layout.pane_ids();
        let mut prepared = layout.clone();
        let new_id = prepared
            .split_pane(focused, Direction::Horizontal, 0.5)
            .expect("target exists");
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
        assert_eq!(
            TileLayout::from_saved(pair(0, 1), pane(1)).err(),
            Some(InvalidSavedLayout::PlaceholderPaneId)
        );
    }

    #[test]
    fn split_leaves_a_cell_for_each_child() {
        for total in 2..=12 {
            for ratio in [0.1, 0.5, 0.9] {
                let (first, second) = split_extent(total, ratio);
                assert!(first >= 1 && second >= 1, "{total} x {ratio}");
                assert_eq!(first + second, total);
            }
        }
        assert_eq!(split_extent(1, 0.5).0 + split_extent(1, 0.5).1, 1);
        assert_eq!(split_extent(0, 0.5), (0, 0));
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
