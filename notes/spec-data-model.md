# Spec: the pane tree owns layout and records, and panes own their terminals

A plan, so it lives in `notes/`. It is written against
`reference/technical-implementation-spec.md`, the contract every implementation
spec follows. It was spawned from these hunt entries, re-verified against the
code on the day it was written:

- `notes/hunt-structure.md`: STR-024 (the workspace pane tree should own the
  layout and the pane records together) and STR-031 (`AppState` invariants held
  only by `assert_invariants_for_test`; terminals in a global map beside the
  panes that attach them, plus the derived `pane_terminal_ids` index).
- `notes/hunt-bugs.md`: BUG-061 (consistency re-check per drag event).
- `notes/hunt-types.md`: TYP-003 (`WorkspacePane` derefs to `PaneState` with
  public fields) and TYP-004 (workspace positions as `usize` in server
  handlers).
- `notes/hunt-consolidations.md`: CON-061 (`find_pane` walks workspaces).

The pane record is designed once, here, for both halves: the tree that owns it
(STR-024) and the terminal state it owns (STR-031).

The owner's framing, which this spec builds to: the tree owns layout and
records together, which removes `has_consistent_panes` from every mutation and
the two-phase split re-diff; the split token carries no cloned layout; capture
and restore go through snapshot conversions that use read methods only;
invariants hold by construction, not by test assertion. shepr has never been
run and no saved state exists, so the on-disk format changes freely, with no
version bump and no migration.

## 1. What closes, and where

| Entry | What closes it | Landing |
|---|---|---|
| STR-024 | `PaneTree` owns `TileLayout`, the records, the root, the zoom and the public numbering; `Workspace` fields are private; the split token carries no layout; capture and restore go through `Shape` conversions over read methods; `SplitPath` owns path-addressed ratio access; `TileLayout::resize_pane` stops swapping focus; mux `PaneGeometry` is renamed `WorkspaceChrome`; `display_name()` returns `&str` | L1, L3 |
| STR-031 | `WorkspaceSet` owns the workspaces, the ID allocator, the bookmark and (through each workspace) the spawn geometry; each `PaneRecord` owns its `TerminalState`; `AppState` fields go private; both `assert_invariants_for_test` functions, `ensure_test_terminals`, `remove_unattached_terminal_ids`, `pane_terminal_ids` and `retain_live_workspace_geometry` are deleted | L2, L3, L4 |
| BUG-061 | The consistency re-check half: `has_consistent_panes` is deleted, so neither the drag path (`set_split_ratio_at`) nor the keyboard path (`resize_pane`) re-proves agreement. The per-event `split_path_for_children` half is not closed here; it stays with CON-109 (section 8) | L3 |
| TYP-003 | `WorkspacePane` and `PaneState` are replaced by `PaneRecord` with private fields and no `Deref`; the next public number is private inside `PaneTree` | L3 |
| TYP-004 | `AppState` and `App` lookups, mutators and outcome structs are keyed by `WorkspaceId` and `PaneId`; positions survive only where order is the subject (move, bookmark, sidebar order) | L4 |
| CON-061 | Attachment is recorded once (the record owns the terminal); the index is deleted; `find_pane` is deleted in favour of `WorkspaceSet::pane` | L2, L3 |

Lateral closures, listed in section 10: BUG-060's `display_name()` clone
clause, the snapshot half of TYP-002, and a duplicate identity nobody filed
(`TerminalId`, retired in L5).

## 2. Contracts

Inventory, as rule 5 asks: `reference/` holds only
`reference/technical-implementation-spec.md`; there is no `docs/` folder.
AGENTS.md states the rest of the contract. The parts of it this work touches:

- Principles, "State is separated from runtime": names `TerminalState` "(in
  `AppState::terminals`)" and says "`PaneState` is only the pane's link to its
  terminal plus per-pane input flags". Both stop being true in L3. This spec
  changes that contract; the replacement text is a brick of L3 (section 5).
- Principles, "Render is pure": `compute_surface_for()` reads `AppState` by
  shared reference. Unchanged; its signature keeps `&AppState` (L4 changes how
  it finds the workspace, not what it reads).
- Principles, "No god objects": `AppState` stays in
  `crates/shepr-server/src/app/state.rs`, which AGENTS.md cites by path. The
  file stays.
- Principles, "Presentation is per client": `workspace_geometry_source` "records
  each workspace's applied area in `AppState`". Still true: from L2 the area is
  recorded on the workspace inside `AppState`'s `WorkspaceSet`.
- Principles, "Hot paths multiply": governs the lookups this spec changes
  (section 4.5, risk R3).
- Code conventions: "New `AppState` or `Workspace` behaviour should be testable
  with `AppState::test_new()` / `Workspace::test_new()`." Kept; both constructors
  survive.
- The opening statement that shepr has never been run and legacy fields and
  migration code are removed freely: the licence for the on-disk format change
  in L3 without a version bump.

Code comments that state the old model are contracts of the same kind and are
rewritten in the landing that falsifies them (listed per landing): the `Node`
doc ("Pane leaves connect layout order to `Workspace.panes`"), the
`TileLayout::pane_ids` doc ("`Workspace.panes` owns the live records"), the
`TileLayout::split_pane` and `split_focused` docs ("Launch paths prepare on a
cloned layout"), the `TileLayout::from_saved` doc ("Callers must remap restored
IDs through `PaneId::alloc` first"), the `AppState` doc ("pure data"), the
`PaneRuntimeRegistry` doc ("keyed by durable terminal id"), the
`PreparedSplit` doc ("A split planned on a cloned layout"), and
`resolve_for_save`'s comment on restore remapping pane IDs.

## 3. Survey of the ground

### 3.1 Core layout (`crates/shepr-core/src/layout.rs`, `geometry.rs`)

- `PaneId(u32)` with a process-global allocator `PaneId::alloc` that never
  repeats; `PaneId::from_raw` is the public test seam.
- `Node` is a public enum (`Pane(PaneId)`, `Split { direction, ratio, first,
  second }`) with `pane_ids()` and `prune(&HashSet<PaneId>)`; only restore
  calls `prune`.
- `TileLayout { root, focus, prev_focus }` hides its root behind `root()` and
  validates saved trees in `from_saved(root, focus)` (`InvalidSavedLayout::
  {DuplicatePaneId, FocusNotFound}`).
- `split_pane(target, direction, ratio) -> Option<PaneId>` allocates the new ID
  itself. `split_focused` and `split_focused_with_ratio` split the focus; the
  only non-core caller of `split_focused` is mux's `#[cfg(test)]
  Workspace::test_split`.
- `resize_pane(pane, nav, delta, area)` saves `focus`, writes `pane` into it,
  calls `resize_focused`, restores it, and compares `split_ratios(&root)`
  before and after (a `Vec<(Vec<SplitBranch>, SplitRatio)>` built twice) to
  report change.
- `SplitBranch` is defined in `geometry.rs` beside `Rect`, though it is a path
  step. Paths are `Vec<SplitBranch>`: `SplitBorder.path`,
  `split_path_for_children` (which builds a `pane_ids` `Vec` per node and
  checks membership quadratically), `set_ratio_at(&[SplitBranch])` and the
  private `get_ratio_at`. The protocol's `PaneSurfaceSplit.path`
  (`crates/shepr-protocol/src/surface.rs`) and the client
  (`shell/presentation/topology.rs`, `shell/input/mouse.rs`, `shell/state.rs`)
  import it from `shepr_core::geometry`.

### 3.2 Mux workspace (`crates/shepr-mux/src/workspace.rs`, `workspace/pane_tree.rs`, `workspace/aggregate.rs`, `workspace/geometry.rs`, `pane/state.rs`)

- `Workspace` fields: `pub id`, `pub custom_name`, `pub identity_cwd`, private
  `git_identity`, `pub next_public_pane_number`, `pub(crate) root_pane`,
  `pub(crate) layout: TileLayout`, `pub(crate) panes: HashMap<PaneId,
  WorkspacePane>`, `pub(crate) zoomed`.
- `WorkspacePane { pub pane_state: PaneState, pub public_number }` with
  `Deref`/`DerefMut` to `PaneState { pub attached_terminal_id: TerminalId, pub
  right_click_passthrough: bool }`.
- `has_consistent_panes` (a `Vec` and a `HashSet` per call) runs in
  `valid_panes` (restore), `focus_pane`, `swap_panes`, `resize_pane`,
  `set_split_ratio_at`, `commit_prepared_split` and `detach_pane`.
- The split: `prepare_split(target, direction, &PaneGeometry, cell, cwd,
  focus_new_pane)` clones the layout, splits the clone, computes the spawn
  geometry against it, optionally focuses the new pane in the clone, and
  returns `PreparedSplit { pane_id, terminal, geometry, public_id,
  prepared_layout }`. `commit_new_pane(prepared, focus)` checks the workspace
  and the number, then `commit_prepared_split` re-diffs the prepared layout
  against the live one ("this layout plus exactly one pane") and installs it.
  Production (`handle_pane_split` in `crates/shepr-server/src/app/api/panes.rs`
  through `AppState::commit_pane_split`) always passes `true` for both knobs.
  The server fixture `WorkspaceFixture::test_split`
  (`crates/shepr-server/src/test_support.rs`) passes `true` then `false`, and
  the new pane is focused anyway because the prepared layout carries the
  focus.
- Removal is two-phase: `prepare_pane_removal(pane) -> PaneRemovalPlan` and
  `remove_pane(&plan) -> PaneRemoval { workspace_id, pane_id, scope, pane_ids,
  terminal_ids }`, which re-derives the plan and then `detach_pane`s. The
  server wraps both in its own `PaneRemovalPlan { workspace_index,
  workspace_plan }` and `PaneRemovalCommit::{Removed, Stale}`.
- Restore admission: `from_restored(id, custom_name, identity_cwd, root_pane,
  layout, panes, zoomed, next)` runs `valid_panes` (consistency, public numbers
  by `valid_public_numbers`, terminal uniqueness). Restore calls
  `valid_public_numbers` itself before building, so the rule is applied twice.
- `prepare(&mut WorkspaceIdAllocator, cwd) -> (Workspace, TerminalState,
  PublicPaneId)` builds a new one-pane workspace.
- Reads the server uses: `root_pane()`, `layout()`, `panes()`, `zoomed()`,
  `contains_pane`, `visible_pane_ids`, `shows_pane`, `pane_state(_mut)`,
  `terminal_id`, `public_pane_number`, `pane_id_for_public_number`,
  `pane_count`, `focused_pane_id`, `display_name() -> String` (a clone per
  read), `branch`, `branch_state`, `git_ahead_behind`,
  `resolved_identity_cwd_from(&terminals, &runtimes) -> Option<PathBuf>` (always
  `Some`), `cwd_for_pane(pane, &terminals, &runtimes)`,
  `foreground_cwd_for_pane`, `aggregate_state(&terminals)`.
- `set_custom_name(String)` only sets; `AppState::rename_workspace` writes the
  `pub custom_name` field directly instead.
- `mark_identity_undiscovered` has no production caller (two tests).
- `Workspace::test_from_pane(id, label, identity_cwd, pane_id, WorkspacePane)`
  is the public seam the server's fixtures build on; it overwrites the record's
  number with `FIRST`. `test_new`, `test_split`, `close_pane`,
  `test_adversarial_identity_state` and `assert_invariants_for_test` are
  `#[cfg(test)]` here and duplicated for the server in `WorkspaceFixture`.
- `workspace/geometry.rs` defines `PaneGeometry { area, pane_borders,
  pane_gaps, pane_outer_borders, pane_scrollbars }` (chrome inputs), which
  shares its name with core's `PaneGeometry` (PTY size). Its methods take
  `(&TileLayout, zoomed)`.
- `WorkspaceIdAllocator` lives in `workspace.rs`; `AppState.workspace_ids`
  owns the session's instance, and restore moves it past saved IDs.

### 3.3 Persistence (`crates/shepr-mux/src/persist/`)

- Schema (`snapshot.rs`): `WorkspaceSnapshot { id, custom_name,
  next_public_pane_number, layout: LayoutSnapshot, panes: HashMap<u32,
  PaneSnapshot>, zoomed, focused: u32, root_pane: u32 }`, `LayoutSnapshot::
  {Pane(u32), Split{..}}`, `PaneSnapshot { cwd, public_number, label,
  agent_session }`. Keys are raw, process-local `PaneId` values.
- History (`snapshot.rs`, `io.rs`): `WorkspaceHistorySnapshot { panes:
  HashMap<u32, PaneHistorySnapshot> }` with a custom sorted serializer
  (`serialize_history_panes`), `SessionHistory.workspaces: Vec<Vec<(u32,
  HistoryText)>>`, `HistoryStamp`, `PendingHistory`, and `HistoryCarry` keyed
  by `TerminalId`. `io.rs`'s hand-written writer prints each key with
  `Display`.
- Capture reads fields directly: `capture_workspace` iterates `ws.panes`,
  reads `ws.layout.root()` through `capture_node`, and reads `custom_name`,
  `next_public_pane_number`, `zoomed`, `root_pane`. Entry points:
  `capture_job` (`capture.rs`), `capture_deferred`, `capture`,
  `capture_pending_history`, `capture_pending_history_for_snapshot`,
  `capture_pending_cwds_for_snapshot`. They take `&[Workspace]`,
  `&HashMap<TerminalId, TerminalState>` and the runtime registry, and return
  `HashMap<SavedPaneRef, TerminalId>` where `SavedPaneRef = (usize, u32)`.
- Restore (`restore.rs`): `plan_workspace` clones the snapshot, drops panes with
  relative cwds, remaps leaves through `restore_node_remapped` (`id_map`,
  `reverse_id_map`), drops leaves without a record, prunes with `Node::prune`,
  resolves focus and root through `resolve_restored_pane`, calls
  `TileLayout::from_saved`, resolves the zoom with `Workspace::resolved_zoomed`,
  and checks numbers with `Workspace::valid_public_numbers`.
  `restore_workspace` then reserves agent sessions and carries history per pane
  and calls `Workspace::from_restored`, whose refusal would leave both side
  effects behind. `SessionRestorePlan::launch` replaces a pane's terminal in
  the `terminals` map on a launch failure. `RestoredSession` and
  `open.rs`'s `OpenedRestore` carry `workspaces`, `terminals`,
  `terminal_runtimes: HashMap<TerminalId, PaneRuntime>` and `active`.
- `writer.rs`'s `layout_fingerprint` encodes the layout and sorted raw pane
  IDs per workspace.

### 3.4 Server state (`crates/shepr-server/src/app/`)

- `AppState` (`state.rs`): `pub terminals: HashMap<TerminalId, TerminalState>`,
  `pub workspaces: Vec<Workspace>`, `pub(crate) workspace_ids`,
  `pub(super) pane_terminal_ids: HashMap<PaneId, TerminalId>`, `pub bookmark`,
  `pub(super) bookmark_position`, `pub(crate) workspace_geometry:
  HashMap<WorkspaceId, SpawnGeometry>`, `pub(crate) settings`, `pub
  next_agent_state_change_seq`, `pub(super) lifecycle_authority_dirty:
  HashSet<TerminalId>`, `pub host_terminal_appearance`, `pub
  host_terminal_appearance_explicit`, `pub host_terminal_theme`, `pub
  session_dirty`, `pub(crate) shell_projection_revision`, `pub(crate)
  clock_now`.
- `pane_terminal_ids` is written by `App::with_paths` (`app/mod.rs`),
  `index_workspace_terminals` (creation), `commit_pane_split`
  (`actions/focus.rs`), `commit_pane_removal` and `close_workspace_at`
  (`actions/workspace.rs`), and the fixtures (`test_set_workspaces`,
  `test_push_workspace`, `test_split_workspace`, `test_reindex_panes`,
  `ensure_test_terminals`).
- `remove_unattached_terminal_ids` rescans every pane of every workspace per
  removal.
- `bookmark`/`bookmark_position` are kept by `set_bookmark`,
  `set_bookmark_index` and `reconcile_bookmark`, called by convention after
  `move_workspace_outcome` and `close_workspace_at`; tests assign `bookmark`.
- `workspace_geometry` is pruned by `retain_live_workspace_geometry`, called
  from `server/headless/client_views.rs`.
- `assert_invariants_for_test` (here) and `Workspace::assert_invariants_for_test`
  (mux and the server fixture) are the only statement of the cross-structure
  invariants.
- Positions: `workspace_index`, `workspace_layout_area(ws_idx)`,
  `workspace_spawn_geometry(ws_idx)`, `workspace_area(ws_idx)`,
  `runtime_for_pane_in_workspace(runtimes, ws_idx, pane)`, `set_pane_input`,
  `edit_workspace_geometry`, `swap_workspace_panes`, `toggle_pane_zoom`,
  `focus_pane_in_workspace`, `rename_workspace`, `move_workspace(_outcome)`,
  `terminal_ids_for_workspace`, `pane_ids_for_workspace`,
  `prepare_pane_removal`, `close_workspace_at`, `commit_pane_split`; outcome
  structs in `actions.rs` (`PaneRemovalPlan.workspace_index`,
  `PaneRemovalOutcome`, `WorkspaceCreationOutcome`, `PaneCreationOutcome`);
  App helpers in `ids.rs` (`find_pane -> (usize, &PaneState)`,
  `public_workspace_id(ws_idx)`, `public_pane_id(ws_idx, pane)`,
  `resolve_workspace_id -> usize`, `resolve_pane_id -> (usize, PaneId)`),
  `api/endpoint.rs` (`endpoint_workspace -> usize`, `endpoint_pane -> (usize,
  PaneId)`), `creation.rs` (`seed_cwd_from_workspace`,
  `launch_cwd_for_pane_in_workspace`, `focused_pane_cwd_in_workspace`,
  `resolved_new_workspace_cwd`, `create_workspace -> usize`, `pane_info`,
  `lookup_runtime`, `workspace_info`), `window_title.rs`
  (`window_title_for(usize)`), `events.rs` (`send_pane_focus_event(ws_idx,
  ..)`), `agents.rs` (`agent_info(ws_idx, ..)`), `api/session.rs`
  (`snapshot_pane(ws_idx, ..)`), `api/panes.rs`
  (`directional_pane_target(ws_idx, ..)`), and outside the app module
  `ui/surface.rs`, `ui/panes.rs` (`compute_pane_infos_for_workspace`,
  `resize_pane_infos`, `pane_cursor`, `resize_surface`),
  `server/pane_surface.rs`, `server/headless.rs`,
  `server/headless/render.rs`, `server/headless/client_views.rs`
  (`ViewedWorkspace { index, workspace }`, `shell_client_views_pane`) and
  `server/headless/retained_surface.rs` (`workspace_index` in its pane and
  layout records).
- Terminal access by `TerminalId` through `state.terminals`: `terminal_titles.rs`,
  `events.rs` (`apply_lifecycle_authority_changes`,
  `install_terminal_runtime`, `abandon_terminal_agent_resume`, which walks
  every pane of every workspace to map a terminal back to its pane),
  `pane_launch.rs`, `agent_resume.rs`, `agents.rs`, `window_title.rs`,
  `creation.rs`, `api/detect.rs`, `api/session.rs`, `actions/pane.rs`
  (`rename_terminal`), `actions/events.rs`, `session.rs` (capture), `ui/panes.rs`,
  `ui/surface.rs`.
- `session.rs`'s `capture_preserved_layout` refuses a layout whose
  `terminal_ids` map has fewer entries than the snapshot has panes, a case only
  a pane without a terminal ID could produce.
- `events.rs`'s `apply_internal_event` prepares and commits a pane removal in
  one call and logs a `Stale` commit it cannot reach.

### 3.5 `TerminalId`

`TerminalId` (`crates/shepr-protocol/src/ids.rs`) is a process-stamped string
minted by `crates/shepr-mux/src/terminal/id.rs`. It never crosses the wire (no
message, API schema or client type names it), is never saved (snapshots key
panes by raw `PaneId`), is never exported to a child, and is never reassigned:
nothing writes `attached_terminal_id` after construction. A pane and its
terminal are created together and removed together, so the two identities are
one-to-one for life. `TerminalId` keys `PaneRuntimeRegistry`, `HistoryCarry`,
`lifecycle_authority_dirty`, the capture map and the exit checkpoint's
`PreservedLayout.terminal_ids` (`app/session/exit_checkpoint.rs`). Events carry
`PaneId`, so every event pays a pane-to-terminal hop, and the reverse hop
(`abandon_terminal_agent_resume`, `App::insert_test_runtime`,
`App::test_runtime`) walks every pane.

## 4. Target

### 4.1 Invariants and where each holds

| Invariant | Today | After |
|---|---|---|
| The layout's leaves and the records name the same panes, no leaf twice | `has_consistent_panes` per mutation, `valid_panes` on restore, both test asserts | `PaneTree`: built from one `Shape` walk (`TreePlan::build`, `single`); the only mutators that change the pane set (`commit_split`, `remove`) edit layout and records in the same call |
| The root pane is a record | test asserts | `PaneTree`: set at construction from a leaf; `remove` promotes before it drops |
| The focused pane is a record | `has_consistent_panes` | `TileLayout` keeps focus on a leaf; the leaves are the records |
| Public numbers are distinct and below the next number | `valid_public_numbers` (twice on restore), test asserts | `TreePlan` (one rule, `admit_numbers`); `commit_split` takes exactly the next number and advances it |
| A zoom has a second pane to hide | `set_zoomed`, `resolved_zoomed`, test asserts | `PaneTree::set_zoomed`, `remove`, `plan` |
| Every pane's terminal exists, none is shared | test assert, `remove_unattached_terminal_ids` scan | the record owns its `TerminalState` |
| The pane-to-terminal index equals the live panes | manual upkeep at five sites, test assert | no index |
| Workspace IDs are unique | test assert | `WorkspaceSet::insert` refuses a present ID; the set owns the allocator |
| Pane IDs are unique across workspaces | test assert | `PaneId::alloc` never repeats, and `WorkspaceSet::insert` refuses a workspace sharing a pane ID |
| The bookmark's remembered position is its index | doc comment, test assert | `WorkspaceSet` repairs it inside every removal and move |
| Recorded geometry belongs to live workspaces only | `retain_live_workspace_geometry` | the geometry is a field of the workspace |

There is no runtime re-check and no `assert_invariants_for_test` left. The
tests that remain prove the constructors and mutators of each type, at the
type.

### 4.2 Core (`crates/shepr-core/src/layout.rs`)

```rust
/// Which child of a split a path step descends into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SplitBranch { First, Second }

/// The address of one split node: the branches taken from the root. Read
/// from a layout's `splits`, and valid for that layout until its tree changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SplitPath(Vec<SplitBranch>);

impl SplitPath {
    pub fn branches(&self) -> &[SplitBranch];
    fn child(&self, branch: SplitBranch) -> Self;
}

impl From<Vec<SplitBranch>> for SplitPath { .. }
```

`SplitBranch` moves here from `geometry.rs` with its derives unchanged, so its
wire encoding is unchanged. `SplitBorder.path` becomes `SplitPath`. The
protocol's `PaneSurfaceSplit.path` stays `Vec<SplitBranch>` (CON-109 owns that
wire shape); `crates/shepr-server/src/server/pane_surface.rs` converts with
`split.path.branches().to_vec()`.

`TileLayout` changes:

```rust
/// Splits `target`, naming the new pane `new_pane`. Focus is untouched.
/// False, with the layout unchanged, when `target` is not a leaf or
/// `new_pane` already is one.
pub fn split_pane(&mut self, target: PaneId, direction: Direction,
                  ratio: SplitRatio, new_pane: PaneId) -> bool;

/// Moves the split nearest `pane`'s edge in `nav` by `delta`. Focus and its
/// history are untouched. True only when a ratio changed.
pub fn resize_pane(&mut self, pane: PaneId, nav: NavDirection,
                   delta: RatioDelta, area: Rect) -> bool;

pub fn set_ratio_at(&mut self, path: &SplitPath, ratio: SplitRatio) -> bool;
pub fn split_path_for_children(&self, first: &[PaneId], second: &[PaneId])
    -> Option<SplitPath>;
```

`resize_pane` finds `pane`'s rect in `self.panes(area)` (false when absent),
picks the split with the existing `nearest_resize_split` rule (falling back to
the opposite edge as `resize_focused` does today), reads the ratio with
`get_ratio_at`, and returns what `set_ratio_at` returns, which is already
"true only if an existing split's ratio changed". `focus` is never written.

Deleted: `split_focused`, `split_focused_with_ratio`, `resize_focused`,
`split_ratios`, and (in L3, once restore no longer calls it) `Node::prune`.
`split_pane` no longer allocates: the caller passes `PaneId::alloc()`, which
lets the split token name the pane before the layout holds it.

### 4.3 Mux

#### `Shape<T>` (new `crates/shepr-mux/src/workspace/shape.rs`)

The neutral form of a pane layout with one `T` per pane: what capture writes a
workspace into and what restore builds one from.

```rust
#[expect(variant_size_differences, reason = "as for core's Node")]
pub enum Shape<T> {
    Pane(T),
    Split { direction: Direction, ratio: SplitRatio,
            first: Box<Shape<T>>, second: Box<Shape<T>> },
}

impl<T> Shape<T> {
    /// Drops the leaves `keep` refuses, collapsing a split left with one
    /// child. `None` when no leaf is kept.
    pub fn prune(self, keep: &mut impl FnMut(&T) -> bool) -> Option<Self>;
    pub fn map<U>(self, f: &mut impl FnMut(T) -> U) -> Shape<U>;
    /// Leaves in tree order.
    pub fn leaves(&self) -> Vec<&T>;
}
```

#### `PaneRecord` (in `workspace/pane_tree.rs`; replaces `WorkspacePane` and `PaneState`)

```rust
/// One pane of a workspace: its public number, its terminal and its input
/// flag. Built only by `PaneTree`, so a record exists only inside a tree.
pub struct PaneRecord {
    number: PanePublicNumber,
    terminal: TerminalState,
    right_click_passthrough: bool,
}

impl PaneRecord {
    pub fn number(&self) -> PanePublicNumber;
    pub fn terminal(&self) -> &TerminalState;
    pub fn terminal_mut(&mut self) -> &mut TerminalState;
    pub fn right_click_passthrough(&self) -> bool;
    /// True when the flag changed.
    pub fn set_right_click_passthrough(&mut self, on: bool) -> bool;
    pub(crate) fn replace_terminal(&mut self, terminal: TerminalState);
}
```

The constructor is private to the module: records enter a tree only through
`PaneTree::single`, `TreePlan::build` and `commit_split`, which assign the
number. `crates/shepr-mux/src/pane/state.rs` is deleted with `PaneState`.

#### `PaneTree` (in `workspace/pane_tree.rs`)

```rust
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
}
```

Constructors:

```rust
impl PaneTree {
    /// A one-pane tree: number `FIRST`, next `SECOND`.
    pub(crate) fn single(pane: PaneId, terminal: TerminalState) -> Self;

    /// Validates a saved or fixture shape before anything is built from it.
    pub fn plan<T>(shape: Shape<T>, number_of: impl Fn(&T) -> PanePublicNumber,
                   saved: SavedTreeState) -> Result<TreePlan<T>, TreeRejection>;
}

/// What a tree keeps besides its shape, as a saved file states it.
#[derive(Debug, Clone, Copy)]
pub struct SavedTreeState {
    pub focus: PanePublicNumber,
    pub root: PanePublicNumber,
    pub zoomed: bool,
    pub next_number: PanePublicNumber,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeRejection {
    RepeatedNumber(PanePublicNumber),
    NumberNotBelowNext(PanePublicNumber),
    /// Only from `build`; fresh IDs and a resolved focus rule it out.
    Layout(InvalidSavedLayout),
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
    pub fn root_leaf(&self) -> &T;
    /// Allocates `PaneId::alloc()` per leaf in tree order and asks
    /// `terminal_for` for each leaf's terminal.
    pub fn build(self, terminal_for: impl FnMut(PaneId, T) -> TerminalState)
        -> Result<PaneTree, TreeRejection>;
}
```

`plan` is the one public-number rule (a private `admit_numbers`: every number
distinct and below `next_number`, which also refuses a next number with no
successor below it). It resolves `focus` and `root` to a leaf's number,
falling back to the first leaf in tree order when the saved one names no
leaf, and resolves `zoomed` to `saved.zoomed && saved focus named a leaf &&
leaves > 1` (today's `resolved_zoomed`). Validation precedes every side effect
of the caller: restore reserves agent sessions and carries history only inside
`build`'s closure, after `plan` accepted the workspace.

Reads (public):

```rust
pub fn layout(&self) -> &TileLayout;          // geometry reads only
pub fn root(&self) -> PaneId;
pub fn focused(&self) -> PaneId;
pub fn zoomed(&self) -> bool;
pub fn zoomed_pane(&self) -> Option<PaneId>;
pub fn next_number(&self) -> PanePublicNumber;
pub fn len(&self) -> usize;
pub fn contains(&self, pane: PaneId) -> bool;
pub fn pane(&self, pane: PaneId) -> Option<&PaneRecord>;
pub fn panes(&self) -> impl Iterator<Item = (PaneId, &PaneRecord)>;  // unordered
pub fn pane_ids(&self) -> Vec<PaneId>;                               // layout order
pub fn pane_by_number(&self, number: PanePublicNumber) -> Option<PaneId>;
pub fn visible_pane_ids(&self) -> Vec<PaneId>;
pub fn shows(&self, pane: PaneId) -> bool;
/// The layout with each leaf mapped through its record. `None` only if the
/// layout and the records disagreed, which the constructors and mutators
/// rule out; callers log it as an internal error.
pub fn map_shape<T>(&self, leaf: impl FnMut(PaneId, &PaneRecord) -> T)
    -> Option<Shape<T>>;
```

Mutators (`pub(super)`; `Workspace` exposes them):

```rust
fn pane_mut(&mut self, pane: PaneId) -> Option<&mut PaneRecord>;
fn focus(&mut self, pane: PaneId) -> bool;            // false when absent
fn swap(&mut self, first: PaneId, second: PaneId) -> bool;
fn resize(&mut self, pane: PaneId, nav: NavDirection, delta: RatioDelta,
          area: shepr_core::geometry::Rect) -> bool;
fn set_split_ratio(&mut self, path: &SplitPath, ratio: SplitRatio) -> bool;
/// Zooming a one-pane tree is refused; unzooming always succeeds.
fn set_zoomed(&mut self, zoomed: bool) -> bool;
fn remove(&mut self, pane: PaneId) -> Result<PaneRecord, RemoveRefusal>;
```

`focus`, `swap`, `resize` and `set_split_ratio` call straight into
`TileLayout`; none of them re-checks agreement (BUG-061's first half). `remove`
refuses `NotHere` and `LastPane`, promotes the root to the first other leaf in
layout order when the root goes, calls `TileLayout::close_pane`, removes the
record and unzooms; the `TileLayout` call cannot refuse a leaf the records hold,
and if it ever did `remove` returns `NotHere` before touching the records.

#### The split token

```rust
/// A split planned without changing the workspace: the new pane's identity,
/// number, spawn size and terminal. The caller launches from it and commits it
/// in the same synchronous handler.
pub struct PreparedSplit {
    workspace: WorkspaceId,
    target: PaneId,
    direction: Direction,
    pane: PaneId,
    number: PanePublicNumber,
    next_number: PanePublicNumber,
    geometry: shepr_core::geometry::PaneGeometry,
    terminal: TerminalState,
}

impl PreparedSplit {
    pub fn workspace_id(&self) -> WorkspaceId;
    pub fn pane_id(&self) -> PaneId;
    pub fn public_id(&self) -> PublicPaneId;
    pub fn geometry(&self) -> shepr_core::geometry::PaneGeometry;
    pub fn cwd(&self) -> &Path;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitRefused { OtherWorkspace, NumberTaken, TargetGone }
```

`Workspace::prepare_split(&self, target, direction, chrome: &WorkspaceChrome,
cell: Option<CellPx>, cwd: PathBuf) -> Option<PreparedSplit>` refuses when
`target` is not in the tree or `next_number.checked_next()` is `None`; it
allocates `PaneId::alloc()`, clones the layout into a local, splits the local
with that ID, computes the spawn geometry from the local (tiled, since a split
unzooms), and drops the local. The token never holds a layout.

`Workspace::commit_split(&mut self, split) -> Result<PaneId, SplitRefused>`
refuses a token of another workspace, a number that is no longer the tree's
next number, and a target that is gone; otherwise it calls
`TileLayout::split_pane(target, direction, SplitRatio::EVEN, pane)`, inserts the
record, focuses the new pane, unzooms, and sets `next_number` to the token's
precomputed `next_number`. There is one focus behaviour (the new pane is
focused), which is what production and the fixtures both get today. The token's
ID was fresh at prepare and nothing between prepare and commit can insert it,
so `split_pane` accepts it; should it ever refuse, `commit_split` returns
`TargetGone` before inserting the record.

#### `Workspace` (`crates/shepr-mux/src/workspace.rs`)

```rust
pub struct Workspace {
    id: WorkspaceId,
    custom_name: Option<String>,
    identity_cwd: PathBuf,          // fixed at construction
    git: GitIdentity,
    tree: PaneTree,
    spawn_geometry: Option<SpawnGeometry>,
}
```

API:

```rust
// Construction
pub(crate) fn from_tree(id: WorkspaceId, custom_name: Option<String>,
                        identity_cwd: PathBuf, tree: PaneTree) -> Self;
/// Test seam for crates above mux (no test feature exists).
pub fn test_from_pane(id: WorkspaceId, label: Option<String>, identity_cwd: &Path,
                      pane: PaneId, terminal: TerminalState) -> Self;

// Identity and label
pub fn id(&self) -> WorkspaceId;
pub fn custom_name(&self) -> Option<&str>;
/// True when the name changed.
pub fn set_custom_name(&mut self, name: Option<String>) -> bool;
pub fn display_name(&self) -> &str;
pub fn identity_cwd(&self) -> &Path;
pub fn branch(&self) -> Option<&str>;
pub fn branch_state(&self) -> Option<&GitBranch>;
pub fn git_ahead_behind(&self) -> Option<AheadBehind>;
pub fn matches_identity_cwd(&self, cwd: &Path) -> bool;
pub fn git_status_key_for_cwd(&self, cwd: &Path) -> Option<&GitStatusKey>;
pub fn apply_git_status(&mut self, status: GitStatus, current_cwd: Option<&Path>)
    -> SurfaceChange;

// The tree
pub fn tree(&self) -> &PaneTree;
pub fn pane_mut(&mut self, pane: PaneId) -> Option<&mut PaneRecord>;
pub fn focus_pane(&mut self, pane: PaneId) -> bool;
pub fn swap_panes(&mut self, first: PaneId, second: PaneId) -> bool;
pub fn resize_pane(&mut self, pane: PaneId, nav: NavDirection, delta: RatioDelta,
                   area: shepr_core::geometry::Rect) -> bool;
pub fn set_split_ratio(&mut self, path: &SplitPath, ratio: SplitRatio) -> bool;
pub fn set_zoomed(&mut self, zoomed: bool) -> bool;
pub fn prepare_split(..) -> Option<PreparedSplit>;
pub fn commit_split(&mut self, split: PreparedSplit) -> Result<PaneId, SplitRefused>;
/// Removes a pane that is not the workspace's last; `WorkspaceSet::remove_pane`
/// removes the workspace instead when it is.
pub fn remove_pane(&mut self, pane: PaneId) -> Result<PaneRecord, RemoveRefusal>;

// Cwd and state reads, over the records it owns
pub fn resolved_identity_cwd(&self, runtimes: &PaneRuntimeRegistry) -> PathBuf;
pub fn resolved_identity_cwd_from_root_pane(&self, root_cwd: Option<PathBuf>) -> PathBuf;
pub fn cwd_for_pane(&self, pane: PaneId, runtimes: &PaneRuntimeRegistry) -> Option<PathBuf>;
pub fn foreground_cwd_for_pane(&self, pane: PaneId, runtimes: &PaneRuntimeRegistry)
    -> Option<PathBuf>;
pub fn aggregate_state(&self) -> PresentedAgentState;

// Applied geometry (from L2)
pub fn spawn_geometry(&self) -> Option<SpawnGeometry>;
pub fn record_spawn_geometry(&mut self, geometry: SpawnGeometry);
```

The runtime lookups inside the cwd reads use the registry's key for the pane
(the record's `terminal().id` until L5, the `PaneId` after). Deleted:
`assemble`, `from_restored`, `valid_panes`, `valid_public_numbers`,
`resolved_zoomed` (moved into `plan`), `prepare` (moved to `WorkspaceSet`),
`commit_new_pane`, `next_public_pane_number`, `prepare_pane_removal`,
`PaneRemovalPlan`, the old `remove_pane`, `mark_identity_undiscovered`,
`root_pane`, `layout`, `panes`, `zoomed`, `contains_pane`, `visible_pane_ids`,
`shows_pane`, `pane_state(_mut)`, `terminal_id`, `public_pane_number`,
`pane_id_for_public_number`, `pane_count`, `focused_pane_id` (all read through
`tree()` now), `resolved_identity_cwd_from` (replaced by
`resolved_identity_cwd`, which returns `PathBuf` because it always had a
value), and `cwd_for_pane`'s `terminals` parameter. The `#[cfg(test)]` helpers
`test_new`, `test_split` (through `prepare_split`/`commit_split`),
`test_adversarial_identity_state` and `close_pane` stay; `register_new_pane`,
`advance_next_public_pane_number`, `resolved_identity_cwd` (the test twin) and
`assert_invariants_for_test` go.

#### `WorkspaceChrome` and `SpawnGeometry` (`workspace/geometry.rs`)

Mux's `PaneGeometry` is renamed `WorkspaceChrome`; its fields and methods are
unchanged. `SpawnGeometry` (area and host cell size) moves here from
`crates/shepr-server/src/app/state.rs` with `for_grid` and `cell_px`, because
it is now a field of `Workspace`. STR-026 (chrome math out of mux) is not done
here.

#### `WorkspaceSet` (new `crates/shepr-mux/src/workspace/set.rs`)

```rust
/// The session's workspaces in display order, the allocator their IDs come
/// from, and the bookmark. IDs are unique, no pane ID is in two workspaces,
/// and the bookmark's remembered position is its index: `insert` refuses what
/// would break the first two, and every removal and move repairs the third.
pub struct WorkspaceSet {
    workspaces: Vec<Workspace>,
    ids: WorkspaceIdAllocator,
    bookmark: Option<Bookmark>,
}

struct Bookmark { id: WorkspaceId, position: usize }
```

`WorkspaceIdAllocator` moves into `set.rs`.

```rust
impl WorkspaceSet {
    pub fn new() -> Self;
    /// Restore's output: duplicates (which restore already prevents) are
    /// dropped with an error log; the allocator is moved past every ID; the
    /// bookmark is seeded from `bookmark`.
    pub fn restored(ids: WorkspaceIdAllocator, workspaces: Vec<Workspace>,
                    bookmark: Option<usize>) -> Self;
    /// Appends; refuses a present ID or a shared pane ID. Moves the allocator
    /// past the ID.
    pub fn insert(&mut self, workspace: Workspace) -> Result<WorkspaceId, Workspace>;
    pub fn prepare_workspace(&mut self, cwd: &Path) -> PreparedWorkspace;  // L3
    pub fn commit_workspace(&mut self, prepared: PreparedWorkspace,
                            geometry: SpawnGeometry) -> Result<WorkspaceId, Workspace>;
    pub fn remove(&mut self, id: &WorkspaceId) -> Option<Workspace>;
    /// One-phase pane removal: a non-last pane leaves its workspace; a last
    /// pane takes its workspace with it.
    pub fn remove_pane(&mut self, pane: PaneId) -> Option<PaneRemoval>;     // L3
    pub fn move_before(&mut self, id: &WorkspaceId, before: Option<&WorkspaceId>) -> bool;

    pub fn get(&self, id: &WorkspaceId) -> Option<&Workspace>;
    pub fn get_mut(&mut self, id: &WorkspaceId) -> Option<&mut Workspace>;
    pub fn position(&self, id: &WorkspaceId) -> Option<usize>;
    pub fn as_slice(&self) -> &[Workspace];
    pub fn iter(&self) -> std::slice::Iter<'_, Workspace>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;

    pub fn pane(&self, pane: PaneId) -> Option<PaneRef<'_>>;
    pub fn pane_mut(&mut self, pane: PaneId) -> Option<&mut PaneRecord>;   // L3
    pub fn resolve(&self, id: &PublicPaneId) -> Option<PaneRef<'_>>;
    pub fn records(&self) -> impl Iterator<Item = (PaneId, &PaneRecord)>;   // L3
    pub fn records_mut(&mut self) -> impl Iterator<Item = (PaneId, &mut PaneRecord)>;

    pub fn bookmark(&self) -> Option<WorkspaceId>;
    pub fn bookmark_index(&self) -> Option<usize>;
    /// True when the bookmark moved.
    pub fn set_bookmark(&mut self, id: &WorkspaceId) -> bool;
    pub fn seed_bookmark_index(&mut self, index: Option<usize>);
}

pub struct PreparedWorkspace { workspace: Workspace }
impl PreparedWorkspace {
    pub fn id(&self) -> WorkspaceId;
    pub fn root_pane(&self) -> PaneId;
    pub fn root_public_id(&self) -> PublicPaneId;
    pub fn cwd(&self) -> &Path;
}

#[derive(Clone, Copy)]
pub struct PaneRef<'a> { workspace: &'a Workspace, id: PaneId, record: &'a PaneRecord }
impl<'a> PaneRef<'a> {
    pub fn workspace(&self) -> &'a Workspace;
    pub fn id(&self) -> PaneId;
    pub fn record(&self) -> &'a PaneRecord;
    pub fn terminal(&self) -> &'a TerminalState;
    pub fn public_id(&self) -> PublicPaneId;
}

pub struct PaneRemoval {
    pub workspace_id: WorkspaceId,
    pub pane: PaneId,
    pub scope: PaneRemovalScope,
    /// Pane scope only; a removed workspace reports false.
    pub focus_changed: bool,
    /// Every pane that left, with its record (one, or the whole workspace).
    pub removed: Vec<(PaneId, PaneRecord)>,
}
```

`get`, `position` and `pane` are linear in the number of workspaces (one
`HashMap` probe per workspace for `pane`). That is deliberate: a set holds a
handful of workspaces, and a derived index would be one more structure to keep
in step. `WorkspaceSet::get` carries a comment saying so, which also settles
BUG-060's clause about `workspace_index` scans. Bookmark repair on removal is
today's `reconcile_bookmark` rule (the workspace now at the bookmark's index,
clamped to the last, none when empty); a move refreshes the position. The set
does not mark the session dirty; `AppState` does, as it does today for every
removal and move.

### 4.4 Persistence

Schema (`snapshot.rs`), records in the leaves, keyed by public number:

```rust
pub struct WorkspaceSnapshot {
    pub id: WorkspaceId,
    #[serde(deserialize_with = "required_nullable")]
    pub custom_name: Option<String>,
    pub next_public_pane_number: PanePublicNumber,
    pub layout: LayoutSnapshot,
    pub zoomed: bool,
    pub focused: PanePublicNumber,
    pub root_pane: PanePublicNumber,
}

pub enum LayoutSnapshot {
    Pane(PaneSnapshot),
    Split { direction: DirectionSnapshot, ratio: SplitRatio,
            first: Box<LayoutSnapshot>, second: Box<LayoutSnapshot> },
}

pub struct PaneSnapshot {           // field order and serde attributes as today
    pub public_number: PanePublicNumber,
    pub cwd: PathBuf,
    pub label: Option<Label>,
    pub agent_session: Option<PaneAgentSessionSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SavedPaneRef { pub workspace: usize, pub pane: PanePublicNumber }

pub struct WorkspaceHistorySnapshot {
    pub panes: BTreeMap<PanePublicNumber, PaneHistorySnapshot>,
}
```

A layout leaf without a record, a record without a leaf and a repeated raw ID
can no longer be written. `SNAPSHOT_VERSION` stays 1 (nothing was ever saved).
`BTreeMap` replaces the custom `serialize_history_panes`. `SessionHistory`,
`HistoryStamp` and `PendingHistory` key panes by `PanePublicNumber`; `io.rs`'s
writer prints keys with `Display`, which for `PanePublicNumber` is the number,
so its output shape is unchanged. `layout_fingerprint` encodes each leaf's
public number in place of the separate sorted raw-ID list.

Capture (`capture_workspace` and the pending-history and pending-cwd captures)
reads only `Workspace` and `PaneTree` read methods: `id()`, `custom_name()`,
`tree().next_number()`, `tree().zoomed()`, the numbers of `tree().focused()`
and `tree().root()` through `tree().pane(..)`, and `tree().map_shape(..)` for
the layout with a `PaneSnapshot` per leaf, converted to `LayoutSnapshot` by a
small recursive function. Entry points take `&WorkspaceSet` and read the
bookmark from it:

```rust
pub fn capture_job(workspaces: &WorkspaceSet, runtimes: &PaneRuntimeRegistry,
                   fallback_cwd: &Path, host_theme: TerminalTheme,
                   persist_pane_history: bool)
    -> (PersistJob, HashMap<SavedPaneRef, TerminalId>);   // PaneId from L5
```

Restore per workspace:

1. Convert `LayoutSnapshot` to `Shape<&PaneSnapshot>` and `prune` leaves with a
   relative cwd (warn per pane, set the pruned flag). `None` drops the
   workspace.
2. `PaneTree::plan(shape, |pane| pane.public_number, SavedTreeState { focus,
   root, zoomed, next })`. A rejection drops the workspace with a warning
   naming it, as today's collision and exhaustion checks do.
3. The identity cwd is `plan.root_leaf().cwd`.
4. `plan.build(|pane_id, saved| ..)`: per leaf, decide the start
   (`pane_restore_startup`, which reserves the agent session), carry history
   (`HistoryCarry::carry_restored`), push a `RestoredLaunch` without geometry
   for a running pane, and return the `TerminalState` from `restored_terminal`.
   A `build` error drops the workspace with an error log (unreachable).
5. Fill each launch's geometry from the built tree (`restored_pane_size` over
   `tree.layout()` and `tree.zoomed()`), then `Workspace::from_tree`.

`RestoredLaunch` gains `workspace: usize` (its index in the plan's
`workspaces`). `SessionRestorePlan` and `RestoredSession` lose `terminals`; a
failed launch replaces the pane's terminal in place through
`workspaces[launch.workspace].pane_mut(launch.pane_id)` and
`PaneRecord::replace_terminal`. `OpenedRestore` loses `terminals`. Deleted:
`restore_node_remapped`, `remap_inner`, `resolve_restored_pane`,
`WorkspaceRestorePlan`'s `reverse_id_map`, `pane_ids`, `numbers`, and the
"leaf with no saved pane" and "repeated pane" damage paths, which the format no
longer admits.

### 4.5 Server

`AppState` after L4 (`crates/shepr-server/src/app/state.rs`):

```rust
/// The session's data: its workspaces (with their panes and terminals) and
/// the presentation facts every client shares, plus the bookkeeping the App
/// drains (`lifecycle_authority_dirty`, `session_dirty`). Live runtimes are
/// the App's; reducers here take runtime observations as arguments.
pub struct AppState {
    pub(crate) clock_now: Instant,            // Spec B's to settle (STR-030)
    workspaces: WorkspaceSet,
    settings: AppSettings,
    next_agent_state_change_seq: u64,
    lifecycle_authority_dirty: HashSet<PaneId>,
    host_terminal_appearance: Option<HostAppearance>,
    host_terminal_appearance_explicit: bool,
    host_terminal_theme: TerminalTheme,
    session_dirty: bool,
    shell_projection_revision: ProjectionRevision,
}
```

Fields are private to `state.rs`'s parent (`pub(super)`), so the `app`
module's own files keep direct access and everything outside goes through
methods. The read surface the loop, the renderer and the API use:

```rust
pub(crate) fn new(settings: AppSettings, workspaces: WorkspaceSet,
                  host_theme: TerminalTheme, now: Instant) -> Self;
pub(crate) fn workspaces(&self) -> &WorkspaceSet;
pub(crate) fn workspace(&self, id: &WorkspaceId) -> Option<&Workspace>;
pub(crate) fn pane(&self, pane: PaneId) -> Option<PaneRef<'_>>;
pub(crate) fn resolve_pane(&self, id: &PublicPaneId) -> Option<PaneRef<'_>>;
pub(crate) fn terminal(&self, pane: PaneId) -> Option<&TerminalState>;
pub(crate) fn bookmark_index(&self) -> Option<usize>;
pub(crate) fn layout_area(&self, workspace: &Workspace) -> Rect;   // recorded or headless
pub(crate) fn chrome_in(&self, area: Rect) -> WorkspaceChrome;     // was pane_geometry_in
pub(crate) fn settings(&self) -> &AppSettings;
pub(crate) fn host_terminal_theme(&self) -> TerminalTheme;
pub(crate) fn host_terminal_appearance(&self) -> Option<HostAppearance>;
pub(crate) fn shell_projection_revision(&self) -> ProjectionRevision;
pub(crate) fn next_agent_state_change_seq(&self) -> u64;
```

Mutators, each marking the session dirty where today's does:

```rust
pub(crate) fn commit_workspace_creation(&mut self, prepared: PreparedWorkspace,
    geometry: SpawnGeometry) -> Option<WorkspaceCreationOutcome>;
/// Commits into the workspace the token names (`PreparedSplit::workspace_id`).
pub(crate) fn commit_pane_split(&mut self, split: PreparedSplit)
    -> Option<PaneCreationOutcome>;
pub(crate) fn remove_pane(&mut self, pane: PaneId) -> Option<PaneRemovalOutcome>;
pub(crate) fn close_workspace(&mut self, id: &WorkspaceId) -> Option<WorkspaceRemovalOutcome>;
pub(crate) fn rename_workspace(&mut self, id: &WorkspaceId, label: Option<String>)
    -> Option<ViewMutation>;
pub(crate) fn move_workspace(&mut self, id: &WorkspaceId, before: Option<&WorkspaceId>)
    -> ViewMutation;
pub(crate) fn focus_pane(&mut self, pane: PaneId) -> ViewMutation;
pub(crate) fn swap_panes(&mut self, source: PaneId, target: PaneId) -> ViewMutation;
pub(crate) fn toggle_pane_zoom(&mut self, pane: PaneId) -> Option<PaneZoomOutcome>;
pub(crate) fn edit_workspace_geometry(&mut self, id: &WorkspaceId,
    edit: impl FnOnce(&mut Workspace) -> bool) -> ViewMutation;
pub(crate) fn set_pane_input(&mut self, pane: PaneId, right_click_passthrough: bool)
    -> Option<ViewMutation>;
pub(crate) fn rename_pane(&mut self, pane: PaneId, label: Option<String>)
    -> Option<ViewMutation>;                       // was rename_terminal(&TerminalId)
pub(crate) fn record_workspace_geometry(&mut self, id: &WorkspaceId, geometry: SpawnGeometry);
pub(crate) fn set_bookmark(&mut self, id: &WorkspaceId) -> bool;
pub(crate) fn seed_bookmark_index(&mut self, index: Option<usize>);
pub(crate) fn drain_lifecycle_authority_dirty(&mut self) -> Vec<PaneId>;
// unchanged: mark_session_dirty, mark_shell_projection_dirty, handle_state_event,
// update_terminal_state(pane, ..), publish_pane_process_exit,
// adopt_checkpoint_candidates_for_shutdown, apply_workspace_git_statuses,
// session_dirty / take_session_dirty (new accessors for the session saver).
```

Outcome structs (`actions.rs`): `WorkspaceCreationOutcome { workspace_id,
root_pane }`, `PaneCreationOutcome { workspace_id, pane_id }`,
`PaneRemovalOutcome { workspace_id, pane_id, scope, focus_changed, removed }`
and `WorkspaceRemovalOutcome { workspace_id, removed }`, where `removed` lists
the runtime registry keys of the panes that left, for the App to shut down:
`Vec<TerminalId>` (read from each removed record before it is dropped) in L3
and L4, `Vec<PaneId>` from L5. `PaneRemovalPlan` and
`PaneRemovalCommit` are deleted; `prepare_pane_exit`'s "would this pane be
removed" probe becomes `state.pane(pane).is_some()`.

App helpers (`ids.rs`, `api/endpoint.rs`, `creation.rs` and the rest): `find_pane`,
`public_workspace_id`, `public_pane_id`, `resolve_workspace_id` and
`resolve_pane_id` are deleted; their callers use `state.pane`,
`state.resolve_pane`, `PaneRef::public_id` and `state.workspace`.
`endpoint_workspace(&WorkspaceId) -> Result<WorkspaceId, EndpointError>`
checks presence; `endpoint_pane(&PublicPaneId) -> Result<(WorkspaceId, PaneId),
EndpointError>` resolves through `resolve_pane`. Every helper listed in 3.4
takes `&WorkspaceId` or `PaneId` instead of a position: `pane_info(pane)`,
`lookup_runtime(pane)`, `workspace_info(&id)`, `seed_cwd_from_workspace(&id)`,
`launch_cwd_for_pane(pane)`, `focused_pane_cwd_in_workspace(&id)`,
`resolved_new_workspace_cwd(&id)`, `create_workspace(..) -> WorkspaceId`,
`window_title_for(&id)`, `send_pane_focus_event(pane, event)`,
`agent_info(PaneRef)`, `snapshot_pane(PaneRef)`,
`directional_pane_target(&id, pane, direction)`, `hold_resume_command(pane)`,
`abandon_agent_resume(pane, ..)` (no walk), `install_runtime(pane, runtime)`.

Outside the app module: `ui/surface.rs` and `ui/panes.rs` take `&Workspace`
(compute) or `&WorkspaceId` (resize) instead of a position, and look a pane's
terminal up through that workspace's tree (one probe, not a set walk);
`pane_cursor(app, runtime, pane, area)`; `ViewedWorkspace` drops `index`;
`shell_client_views_pane(client, &WorkspaceId, pane)`; `retained_surface.rs`
records carry `WorkspaceId`; `render.rs`'s `terminal_id_for_pane` becomes
`state.pane(pane).is_none()`.

### 4.6 `TerminalId` retired

`PaneRuntimeRegistry` is keyed by `PaneId`. `TerminalState::new(cwd)` takes no
ID and the `id` field goes. `HistoryCarry`, `PendingPaneHistory`, the capture
map, `RestoredSession.terminal_runtimes` and `PreservedLayout` (renamed field
`pane_ids: HashMap<SavedPaneRef, PaneId>`) key by `PaneId`.
`crates/shepr-mux/src/terminal/id.rs` (`allocate_terminal_id`) and the
protocol's `TerminalId`, `TerminalIdParseError` and `terminal_id_tests` are
deleted. `AppState::runtime_of` and `runtime_for_pane_in_workspace` are deleted:
a runtime is `terminal_runtimes.get(&pane)`, and a runtime exists only for a
live pane because removal shuts it down. A runtime replaced by an agent resume
is inserted under the same `PaneId`, which is today's semantics under the same
`TerminalId`.

## 5. Landings

Five landings, each one coherent change kept or reverted on its gate. Each ends
green: `brokkr check`. Within a landing the steps are an order of work, not
separate gates. Run `brokkr fmt` before each commit. Each landing's commit also
carries the notes edits that retire what it closed.

### L1: core layout primitives

Steps:

1. Move `SplitBranch` from `crates/shepr-core/src/geometry.rs` to `layout.rs`
   and add `SplitPath`. Re-point imports: `crates/shepr-protocol/src/surface.rs`,
   `crates/shepr-protocol/src/wire_tests.rs`,
   `crates/shepr-client/src/shell/presentation/topology.rs`,
   `crates/shepr-client/src/shell/input/mouse.rs`,
   `crates/shepr-client/src/shell/state.rs`,
   `crates/shepr-client/src/shell/tests/mouse_selection.rs`,
   `crates/shepr-server/src/ui/panes.rs` (tests).
2. `SplitBorder.path: SplitPath`; `collect_splits` builds paths with
   `SplitPath::child`; `set_ratio_at`, `get_ratio_at` and
   `split_path_for_children` take or return `SplitPath`. Callers:
   `crates/shepr-mux/src/workspace/pane_tree.rs` (`set_split_ratio_at` takes
   `&SplitPath`), `crates/shepr-server/src/app/api/layouts.rs`,
   `crates/shepr-server/src/server/pane_surface.rs`
   (`split.path.branches().to_vec()`), and tests building a `SplitBorder`
   (`SplitPath::from(vec![..])`).
3. `split_pane` takes the new ID. Callers: `Workspace::prepare_split` passes
   `PaneId::alloc()`; mux `workspace/geometry.rs` tests;
   `crates/shepr-server/src/app/state.rs`'s
   `split_spawn_size_is_the_new_panes_content_size_not_the_first_panes_outer_rect`;
   mux `#[cfg(test)] Workspace::test_split` (split the focus with
   `split_pane(focused, direction, EVEN, PaneId::alloc())`, then
   `focus_pane`).
4. Rewrite `resize_pane` as in 4.2.

Deletes: `split_focused`, `split_focused_with_ratio`, `resize_focused`,
`split_ratios`; the `split_focused` and `split_pane` doc sentences about
launch paths and cross-crate test support.

Tests (`crates/shepr-core/src/layout.rs`): new
`split_pane_installs_the_given_id_and_leaves_focus`,
`split_pane_refuses_a_missing_target_or_a_present_id`,
`resize_pane_moves_a_split_without_touching_focus_or_its_history`,
`resize_pane_reports_no_change_when_the_ratio_is_already_clamped`,
`split_paths_address_the_split_they_were_read_from`. Adapted:
`split_focused_with_ratio_sets_new_split_ratio` becomes
`split_pane_with_a_ratio_sets_the_new_split_ratio`;
`resize_pane_preserves_focus_and_reports_change`,
`split_pane_leaves_focus_and_history_untouched`,
`split_pane_missing_target_changes_nothing`,
`discarded_prepared_split_preserves_focus_history`,
`split_identity_survives_path_change_but_refuses_changed_children` and the
`resize_*` tests that called `resize_focused` move to `resize_pane` on the
focused pane.

Gate: `brokkr check`.

Notes: STR-024 loses its `SplitBranch`, `Vec<SplitBranch>` and
`resize_pane` focus-swap sentences.

### L2: the workspace collection

Mux:

1. Add `crates/shepr-mux/src/workspace/set.rs` with `WorkspaceSet`, `Bookmark`,
   `PaneRef` and the moved `WorkspaceIdAllocator`. In this landing `PaneRef`
   holds `&WorkspacePane` (`pane()`), which L3 replaces with `record()`;
   `remove_pane`, `pane_mut`, `records`, `records_mut`, `prepare_workspace`
   and `commit_workspace` arrive in L3. For creation in this landing the set
   offers `allocate_id(&mut self) -> WorkspaceId`, and `Workspace::prepare`
   takes that ID instead of the allocator (L3 replaces both with
   `prepare_workspace`).
2. Move `SpawnGeometry` into `workspace/geometry.rs`; add the private
   `spawn_geometry` field and its two methods to `Workspace`.
3. `persist::capture_job`, `capture_deferred` and `capture` take
   `&WorkspaceSet` (reading `as_slice()` and `bookmark_index()`) instead of
   `&[Workspace]` and `active`.

Server:

1. `AppState`: replace `workspaces: Vec<Workspace>`, `workspace_ids`,
   `pane_terminal_ids`, `bookmark`, `bookmark_position` and
   `workspace_geometry` with `workspaces: WorkspaceSet` (`pub(super)`). Add
   `workspaces()`, `pane()` and `AppState::new`. `terminal_of(pane)` reads
   `workspaces.pane(pane)?.pane().attached_terminal_id`.
   `record_workspace_geometry` writes the workspace;
   `workspace_spawn_geometry`, `workspace_area` and `workspace_layout_area`
   read it (still by position in this landing). `set_bookmark`,
   `set_bookmark_index` (renamed `seed_bookmark_index`) and `bookmark_index`
   delegate. `move_workspace(id, before)` takes IDs (its one production caller,
   `handle_workspace_move`, already holds them).
2. `App::with_paths` (`app/mod.rs`): build `WorkspaceSet::restored(ids,
   restored.workspaces, restored.active)` and `AppState::new(..)`; the
   `pane_terminal_ids` assembly goes.
3. Creation (`creation.rs`, `actions/focus.rs`): `set.allocate_id()` then
   `Workspace::prepare(id, cwd)`; commit through `set.insert`, then
   `record_spawn_geometry` on the inserted workspace.
4. `find_pane` (`ids.rs`) is deleted; its callers (`terminal_titles.rs`,
   `pane_launch.rs`, `agent_resume.rs`, `server/headless/render.rs`,
   `server/headless/client_views.rs`) use `state.pane(..)` and, where they
   still need a position until L4, `workspaces().position(pane.workspace().id)`.
5. Readers of `state.workspaces` outside the app module (`ui/`,
   `server/headless/bootstrap.rs`, `render.rs`, `client_views.rs`,
   `retained_surface.rs`) read `state.workspaces()`; `.get(i)` becomes
   `as_slice().get(i)` until L4.
6. `client_views.rs` drops its `retain_live_workspace_geometry` call.

Deletes: `pane_terminal_ids`, `terminal_of`'s index, `index_workspace_terminals`,
`retain_live_workspace_geometry`, `reconcile_bookmark`, `workspace_index` (the
set's `position` replaces it), `test_reindex_panes`, the index half of
`AppState::assert_invariants_for_test` and of `ensure_test_terminals`.

Rewrite rules for tests: `state.workspaces[i]` reads become
`state.workspaces().as_slice()[i]` (or a `ws(i)` helper on a test-only
`AppStateFixture` trait in `crates/shepr-server/src/test_support.rs`, with a
`ws_mut(i)` that takes the ID at position `i` and returns `get_mut` of it); `state.workspaces.remove(i)` becomes `state.close_workspace_at(i)`
(L4: `close_workspace(&id)`); `state.bookmark = ..` goes through
`set_bookmark`; `state.workspace_geometry.insert(..)` through
`record_workspace_geometry`; `state.workspaces.truncate(..)`/`clear()` through
closes.

Tests: new in `set.rs`: `inserting_a_workspace_with_a_present_id_is_refused`,
`inserting_a_workspace_that_shares_a_pane_id_is_refused`,
`inserting_moves_the_allocator_past_the_id`,
`the_bookmark_remembers_its_index_and_repairs_by_it` (moved from
`crates/shepr-server/src/app/state.rs`),
`pane_lookup_finds_the_owning_workspace`,
`a_removed_workspace_takes_its_spawn_geometry_with_it`,
`restored_sets_drop_a_duplicate_id_and_seed_the_bookmark`. Adapted in
`state.rs`:
`a_workspaces_layout_area_is_its_own_recorded_geometry_never_another_workspaces`
(its "closed workspaces drop their geometry" half moves to the set test),
`runtime_lookup_goes_through_the_registry_by_terminal_id`. Deleted:
`fixture_changes_index_live_panes_and_retire_replaced_panes`.

Gate: `brokkr check`.

Notes: STR-031 loses the bookmark, `workspace_geometry`,
`retain_live_workspace_geometry` and `pane_terminal_ids` sentences; CON-061
loses "and in the index".

### L3: the pane tree and the pane record

The core of the spec: `PaneTree`, `PaneRecord` owning `TerminalState`, the
split token, one-phase removal, the private `Workspace`, the format change,
and the server's terminal access by pane. It compiles only at its end.

Mux:

1. Add `workspace/shape.rs` (`Shape<T>`).
2. Rewrite `workspace/pane_tree.rs`: `PaneRecord`, `PaneTree`,
   `SavedTreeState`, `TreePlan`, `TreeRejection`, `PreparedSplit`,
   `SplitRefused`, `RemoveRefusal`. Delete `WorkspacePane` and its `Deref`
   impls, `has_consistent_panes`, `commit_prepared_split`, `detach_pane`,
   `promoted_root_if_needed` (into `PaneTree::remove`).
3. Delete `crates/shepr-mux/src/pane/state.rs` and the `PaneState` export in
   `crates/shepr-mux/src/pane.rs`.
4. Rewrite `Workspace` (4.3). `aggregate.rs`: `aggregate_state(&self)` over the
   records. Rename `PaneGeometry` to `WorkspaceChrome` in
   `workspace/geometry.rs` and its users (`workspace.rs`, `pane_tree.rs`,
   `persist/open.rs`, `persist/restore.rs`, server `app/state.rs`,
   `app/agent_resume.rs`, `test_support.rs`).
5. `WorkspaceSet` gains `prepare_workspace`, `commit_workspace`,
   `remove_pane`, `pane_mut`, `records`, `records_mut`; `allocate_id` goes;
   `PaneRef` holds `&PaneRecord`; `PaneRemoval`, `PaneRemovalScope` move to
   `set.rs`.
6. Persistence: the schema of 4.4; capture through read methods; restore
   through `Shape` and `TreePlan`; `SessionRestorePlan::launch` replaces the
   terminal in place; `RestoredSession` and `OpenedRestore` lose `terminals`;
   history keys; `layout_fingerprint`; `SavedPaneRef` struct. Delete
   `Node::prune` from core (restore was its last caller), the `Node` doc
   sentence about `Workspace.panes`, the `TileLayout::pane_ids` doc sentence,
   and the `from_saved` doc's remapping obligation (now inside
   `TreePlan::build`). Rewrite `resolve_for_save`'s comment on remapped IDs:
   saved keys are public numbers, stable across a restore.

Server:

1. `AppState` loses `terminals`. Terminal access by pane:
   `update_terminal_state(pane, ..)` reads `workspaces.pane_mut(pane)`;
   `record_agent_state_change_seq(pane, ..)`; `TerminalCwdReported`;
   `adopt_checkpoint_candidates_for_shutdown` over `records_mut()`;
   `lifecycle_authority_dirty: HashSet<PaneId>` (the App looks the runtime up
   under the record's `terminal().id` until L5); `rename_terminal(&TerminalId)`
   becomes `rename_pane(pane)`; `set_pane_input` through
   `PaneRecord::set_right_click_passthrough`.
2. Removal: `prepare_pane_removal`, `prepare_pane_removal_by_id`,
   `commit_pane_removal`, `close_workspace_at`'s terminal bookkeeping,
   `remove_unattached_terminal_ids`, `terminal_ids_for_workspace` and
   `pane_ids_for_workspace` collapse into `AppState::remove_pane(pane)` over
   `WorkspaceSet::remove_pane`, and `close_workspace_at` over
   `WorkspaceSet::remove`. `PaneRemovalPlan` and `PaneRemovalCommit` are
   deleted; `apply_internal_event` (`events.rs`) calls `remove_pane` once and
   loses its unreachable `Stale` branch; `prepare_pane_exit`'s probe is
   `state.pane(pane).is_some()`; `handle_pane_close` and
   `handle_workspace_close` shut down the runtimes the outcome's `removed`
   names (each removed record's `terminal().id`, collected by `AppState`
   before the records drop, until L5).
3. Split: `handle_pane_split` calls `prepare_split(.., cwd)` (no focus knob)
   and `AppState::commit_pane_split(split)`, which finds the workspace the
   token names and calls `Workspace::commit_split`; the `terminals.insert` and
   index insert go. (`commit_pane_split` drops its `workspace_index`
   parameter here rather than in L4, since the token names the workspace.)
4. Creation: `create_workspace_outcome` uses `prepare_workspace` and
   `commit_workspace_creation(prepared, geometry)`; the `TerminalState` no
   longer travels beside the workspace.
5. App code that held a `TerminalId` to reach state passes the `PaneId`:
   `pane_launch.rs` (`hold_resume_command`, `handle_pane_launch_settled`,
   `send_resume_command`), `agent_resume.rs` (`PendingAgentResumeCandidate`
   drops `terminal_id`; `has_pending_agent_resumes` walks `records()`),
   `events.rs` (`install_terminal_runtime` becomes `install_runtime(pane,
   runtime)`; `abandon_terminal_agent_resume` becomes
   `abandon_agent_resume(pane, ..)` with no walk;
   `apply_lifecycle_authority_changes` drains `PaneId`s),
   `terminal_titles.rs`, `window_title.rs`, `agents.rs`, `creation.rs`
   (`workspace_info` reads `aggregate_state()`), `git_refresh.rs`
   (`resolved_identity_cwd(&runtimes)`, no `?`), `api/detect.rs`,
   `api/session.rs`, `session.rs` (`capture_job(&state.workspaces, ..)`;
   `capture_preserved_layout` loses its pane-count check), `ui/panes.rs`,
   `ui/surface.rs`.
6. `rename_workspace` uses `set_custom_name(label)`; `handle_workspace_create`
   too. `display_name()` callers that need an owned `String` (the API's
   `WorkspaceInfo.label`, the window title) call `.to_owned()` at that
   boundary.
7. AGENTS.md, Principles, replace the "State is separated from runtime"
   bullet with:

   > - **State is separated from runtime.** `AppState` is the session's data,
   >   testable without PTYs or async. Each workspace's `PaneTree` owns its
   >   layout and its pane records together, and each `PaneRecord` owns its
   >   pane's `TerminalState`, which is plain data testable without a PTY;
   >   `PaneRuntime` (held by `App`, outside `AppState`) owns the PTY, its tasks
   >   and the state shared with them.

Deletes: `AppState::assert_invariants_for_test`,
`Workspace::assert_invariants_for_test` (mux and the server fixture),
`ensure_test_terminals`, the `terminals` field, `index_workspace_terminals`'s
last trace, `WorkspacePane`, `PaneState`, `has_consistent_panes`,
`PaneRemovalPlan` (mux and server), `PaneRemovalCommit`,
`remove_unattached_terminal_ids`, `mark_identity_undiscovered`, the restore
remapping functions, `serialize_history_panes`.

Rewrite rules for tests (mechanical; each pattern maps one way):

| Old | New |
|---|---|
| `state.ensure_test_terminals();` | delete the line |
| `ws.terminal_id(pane)` then `state.terminals[&tid]` / `.get_mut(&tid)` | `state.terminal(pane)` / `state.workspaces.pane_mut(pane).map(PaneRecord::terminal_mut)` (a `terminal_mut(pane)` method on the test fixture trait keeps call sites short) |
| `ws.panes()[&pane].attached_terminal_id` | the `PaneId` itself |
| `ws.identity_cwd = cwd` before the workspace enters state | `Workspace::test_at(label, &cwd)` on `WorkspaceFixture` (built on `test_from_pane`) |
| `ws.custom_name = None` | build with `test_at(None, ..)` or `set_custom_name(None)` |
| `ws.mark_identity_undiscovered()` | delete; a new workspace is undiscovered |
| `ws.root_pane()`, `ws.layout()`, `ws.zoomed()`, `ws.panes().len()`, `ws.focused_pane_id()`, `ws.contains_pane(p)`, `ws.public_pane_number(p)` | `ws.tree().root()`, `.layout()`, `.zoomed()`, `.len()`, `.focused()`, `.contains(p)`, `.pane(p).map(PaneRecord::number)` |
| `ws.id` | `ws.id()` |
| `ws.test_split(direction)` | unchanged name; `WorkspaceFixture::test_split` goes through `prepare_split`/`commit_split` |
| `WorkspacePane::new(PaneState::new(tid), n)` | pass a `TerminalState` to `test_from_pane` |
| `ws.assert_invariants_for_test()` / `state.assert_invariants_for_test()` | delete; where the test's subject is an invariant, assert the specific fact (pane count, focus, number) instead |

Tests, new:

- `crates/shepr-mux/src/workspace/pane_tree.rs`:
  `split_commit_installs_the_leaf_and_the_record_together`,
  `a_split_token_of_another_workspace_is_refused`,
  `a_split_token_is_refused_once_its_number_is_taken`,
  `a_split_focuses_the_new_pane_and_unzooms`,
  `preparing_a_split_changes_nothing_and_holds_no_layout` (adapts
  `preparing_a_split_is_pure_and_commits_its_reserved_identity`),
  `removing_the_root_promotes_a_surviving_pane`,
  `removing_the_last_pane_is_refused`,
  `plans_refuse_a_repeated_number_or_one_not_below_next`,
  `plans_fall_back_focus_and_root_to_the_first_leaf`,
  `plans_drop_a_zoom_without_its_focus_or_a_second_pane`,
  `a_mapped_shape_plans_back_into_the_same_tree`.
- `crates/shepr-mux/src/workspace.rs`:
  `display_name_borrows_the_cached_label`,
  `renaming_reports_whether_the_name_changed`,
  `resolved_identity_cwd_falls_back_to_the_construction_cwd`.
- `crates/shepr-mux/src/workspace/set.rs`:
  `removing_the_last_pane_removes_the_workspace_and_repairs_the_bookmark`,
  `removing_a_pane_reports_whether_focus_moved`,
  `pane_mut_reaches_the_record_in_any_workspace`.
- `crates/shepr-mux/src/persist/restore.rs`:
  `capture_then_restore_keeps_layout_records_focus_zoom_and_numbers`,
  `a_leaf_with_a_relative_cwd_is_pruned_and_its_split_collapses`,
  `a_rejected_workspace_reserves_no_agent_session` (two workspaces share an
  agent session; the first repeats a public number and is dropped; the second
  must resume it rather than start as a duplicate),
  `a_failed_launch_keeps_the_saved_state_on_the_pane`.
- `crates/shepr-mux/src/persist/snapshot.rs`:
  `history_keys_are_public_numbers_and_round_trip`,
  `a_saved_file_without_records_in_its_leaves_fails_to_parse`.

Tests, adapted to the format: in `restore.rs`,
`restore_drops_a_workspace_whose_panes_share_a_public_number`,
`restore_plans_reject_collisions_and_exhaustion_before_execution`,
`restore_plans_prune_relative_panes_and_derive_identity_cwd_from_a_surviving_root`,
`zoom_does_not_survive_pruning_to_one_pane_or_losing_the_zoomed_pane`,
`restored_panes_keep_saved_fields_and_runtimeless_history`,
`failed_cold_restore_preserves_panes_and_saved_directories`,
`restore_preserves_public_id_mapping_after_pane_id_remap` (renamed
`restore_keeps_each_panes_public_id`), `restored_panes_start_at_their_own_layout_size`,
`restore_seeds_saved_pane_history_into_runtime` and the remaining fixtures
built by `workspace_snapshot`/`runtimeless_pane`; in `snapshot.rs`,
`every_saved_key_is_required_and_no_other_is_accepted` and
`history_panes_serialize_in_numeric_id_order`; in `io.rs`,
`history_is_restored_only_for_the_digest_its_layout_names` and the history
writer tests (keys are numbers); in `writer.rs`, the two fixtures that rewrote
pane `0` as `1`. Deleted: `capture_and_restore_node_round_trip` (replaced by the
new round trip), `repeated_saved_pane_maps_only_its_first_leaf`,
`restore_drops_layout_leaves_without_saved_state`,
`node_prune_collapses_missing_branch`,
`resolve_restored_pane_prefers_surviving_saved_id_and_falls_back_to_first_remaining`
(the shapes they guarded cannot be written), and in mux `workspace.rs`
`restore_clears_a_zoom_saved_on_a_one_pane_workspace` and
`restore_keeps_a_zoom_saved_on_a_split_workspace` (replaced by the plan zoom
test). The server suites (`app/actions/tests.rs`, `app/api/panes/tests.rs`,
`app/snapshot_tests.rs`, `server/headless/tests/`, and the inline tests listed
in 3.4) are adapted by the table above; their assertions stay.

Gate: `brokkr check`.

Notes: remove STR-024 except its STR-026/TYP-034 cross-references, which those
entries already carry; remove TYP-003; remove CON-061; reduce STR-031 to the
`AppState` field-visibility sentences L4 closes; reword BUG-061 to the
per-event `split_path_for_children` cost only, or fold that cost into CON-109
and delete BUG-061; remove BUG-060's `display_name()` clause; trim TYP-002 to
its logging and `Display` half (section 10).

### L4: an id-keyed `AppState` and `App`

Steps: the API of 4.5. `AppState` fields go `pub(super)` and gain the
accessors listed; `WorkspaceSet` loses no method, but production code stops
reading `as_slice()` by position. Every helper and outcome struct in 3.4
switches from a position to `WorkspaceId` or `PaneId`, in this order so each
file compiles against the one before: `state.rs` and `actions/*.rs`;
`ids.rs` (deleted), `api/endpoint.rs`; `creation.rs`, `events.rs`, `agents.rs`,
`window_title.rs`, `agent_resume.rs`, `pane_launch.rs`, `api/*.rs`; `ui/`;
`server/pane_surface.rs`, `server/headless.rs` and
`server/headless/{render.rs,client_views.rs,retained_surface.rs,bootstrap.rs}`.
Positions remain in `move_workspace` (as IDs at the API, positions inside the
set), the bookmark, `SessionSnapshot.active` and the sidebar order, which
derives from iteration.

Deletes: `workspace_index`, `workspace_layout_area(ws_idx)`,
`workspace_spawn_geometry(ws_idx)`, `workspace_area(ws_idx)`,
`runtime_for_pane_in_workspace`, `pane_geometry_for_workspace` (test),
`public_workspace_id`, `public_pane_id`, `resolve_workspace_id`,
`resolve_pane_id`, `focus_pane_in_workspace`, `swap_workspace_panes` (renamed
`swap_panes`), the `workspace_index` fields of the outcome structs and of
`retained_surface.rs`'s records, `ViewedWorkspace::index`.

Tests: new in `crates/shepr-server/src/app/state.rs`:
`lookups_by_id_survive_a_reorder`,
`a_pane_resolves_to_its_workspace_after_an_earlier_workspace_closes`. Adapted:
`ids.rs`'s `public_ids_resolve`, `unknown_public_pane_id_does_not_resolve` and
`positional_and_raw_ids_are_rejected` move to `state.rs` against
`resolve_pane`; `public_workspace_id_returns_none_for_a_missing_workspace` and
`workspace_info_for_a_stale_index_is_none` become id tests (a closed
workspace's ID resolves to nothing). Every test calling a positional helper
passes the ID it already holds (fixtures return it from `test_push_workspace`).

Gate: `brokkr check`.

Notes: remove TYP-004 and STR-031; trim BUG-060's `workspace_index` clause,
which this spec adjudicates with the comment at `WorkspaceSet::get`.

### L5: retire `TerminalId`

Steps (4.6): `PaneRuntimeRegistry` keyed by `PaneId` (`pane/runtime_registry.rs`,
its doc rewritten); `TerminalState::new(cwd)` and the `id` field
(`terminal/state/mod.rs`, `terminal/state/init.rs`); delete
`terminal/id.rs` and its export in `terminal/mod.rs`; `HistoryCarry` and
`PendingPaneHistory` (`persist/snapshot.rs`); the capture map;
`RestoredSession` and `OpenedRestore`; `PreservedLayout.pane_ids`
(`app/session/exit_checkpoint.rs`, `app/session.rs`); every
`terminal_runtimes.get/insert/remove` in the server by `PaneId`;
`AppState::runtime_of` deleted; log fields `terminal = %terminal_id` dropped
(the pane field stays); `crates/shepr-protocol/src/ids.rs` loses `TerminalId`,
`TerminalIdParseError` and `terminal_id_tests`, and `lib.rs` its re-export;
`crates/shepr-server/src/test_support.rs`'s registry fixture drains
`(PaneId, PaneRuntime)`; `App::insert_test_runtime` and `App::test_runtime`
index the registry directly.

Tests: new `crates/shepr-server/src/app/events.rs`
`a_resume_runtime_replaces_the_panes_runtime_under_the_same_key`. Adapted:
`runtime_lookup_goes_through_the_registry_by_terminal_id` (renamed
`runtime_lookup_is_by_pane`), the history carry tests in `snapshot.rs`, and the
tests that built `TerminalState::new(allocate_terminal_id(), cwd)`. Deleted:
`allocated_terminal_ids_parse_back_and_differ` and the protocol's
`terminal_id_tests`.

Gate: `brokkr check`.

Notes: none of the hunt files filed this; section 10 records it.

## 6. Test strategy

Each type is proved at the type: `TileLayout` in core, `PaneTree` and
`TreePlan` in `pane_tree.rs`, `WorkspaceSet` in `set.rs`, the format in
`snapshot.rs` and `restore.rs`. The server suites keep their behavioural
assertions and lose their invariant sweeps; nothing replaces
`assert_invariants_for_test`, because no reachable state violates what it
asserted. The one ordering property the old code got wrong structurally
(validation after side effects in restore) is pinned by
`a_rejected_workspace_reserves_no_agent_session`. The history map-key change
is pinned by `history_keys_are_public_numbers_and_round_trip`, which writes
through the production writer and reads through serde.

No gate needs a separate run: every new test is unignored and `brokkr check`
runs it. The command per landing is `brokkr check`.

## 7. Risks

- R1. Map keys: `BTreeMap<PanePublicNumber, _>` relies on serde_json reading
  a `NonZeroUsize` newtype as a map key. If it does not, the history key
  becomes `usize` (`number.get()`) at the serde boundary with conversion at
  read; `history_keys_are_public_numbers_and_round_trip` decides which, and
  the writer in `io.rs` is unaffected either way.
- R2. Borrows: reducers that read a terminal and then write `AppState`
  (`record_agent_state_change_seq` after `update`) now hold a borrow into
  `workspaces`. Compute the sequence number first, then take the record
  mutably, as `update_terminal_state` already separates its two phases.
- R3. Hot paths: a pane lookup by `PaneId` walks the workspaces (one hash probe
  each) where it was one probe in the index, and event admission does it once
  per runtime event. Workspaces number in the single digits, so this is a few
  probes per event. The render path must not regress further: `ui/` and
  `retained_surface.rs` already hold the `&Workspace` they draw and must look
  each pane up through `workspace.tree().pane(..)`, not `state.pane(..)`. In L5
  event admission stops needing a pane lookup at all (the registry is keyed by
  the event's `PaneId`).
- R4. Test churn: hundreds of test sites change. The rewrite tables keep the
  change mechanical, and the assertions do not change, so a test that passes
  after the rewrite proves what it proved before.
- R5. Neighbour overlap: Spec B (`notes/spec-app-loop.md`) edits `app/mod.rs`,
  `events.rs`, `runtime.rs`, `session.rs` and `server/headless/`. Whichever
  lands second rebases call sites; section 9 fixes the API both sides use so
  the rebase is mechanical.
- R6. Restore's `build` closure has side effects (session reservation, history
  carry) and `build` can still return an error in principle. The error is
  unreachable (fresh IDs, a resolved focus), it is logged as an internal error,
  and the plan step has already refused every saved-data defect.
- R7. The split token's spawn geometry was computed at prepare against a
  cloned layout. Prepare and commit share one synchronous handler and the
  token refuses a changed number, a gone target and another workspace, so a
  token cannot be committed against a layout that moved; the doc on
  `PreparedSplit` keeps the "same synchronous handler" requirement, and if
  prepare and commit ever span an await the token must also carry a tree
  generation.

## 8. Stopping rule

Out of scope, each a separate entry or a neighbour's:

- CON-109 and the second half of BUG-061: the client's split addressing, the
  server-minted layout epoch and the per-event `split_path_for_children` cost.
  `SplitPath` is introduced here but the protocol keeps `Vec<SplitBranch>`.
- STR-026: chrome math out of mux. `WorkspaceChrome` is a rename only.
- TYP-034: `pane_size` tuples.
- STR-028: splitting `persist/` files; this spec changes the schema and the
  capture and restore functions in place.
- TYP-002's logging half: `Display`/`tracing::Value` for `PaneId`.
- TYP-086's host appearance pair: the fields go private in L4 unchanged.
- CON-056: chrome content finalization.
- BUG-060's pending-resume scan clause (Spec B's resume schedule).
- Everything Spec B owns: `App`'s fields and shape, the loop surface, render
  cadence, schedulers, `clock_now`'s duplication, where host theme state lives.
- Everything Spec E owns: core `PaneGeometry`'s pixel extent, `CellPx`,
  `HostCellSize`.

The teardown stops at `AppState`'s fields and the call sites its API change
forces. No client crate changes except L1's `SplitBranch` import path.

## 9. Neighbours

### Assumptions this spec makes

Of Spec B (`notes/spec-app-loop.md`):

- `App` keeps a `PaneRuntimeRegistry` for live runtimes, outside `AppState`.
  L5 changes its key type, not its owner.
- `App` construction (today `App::with_paths`) builds state through
  `AppState::new(settings, WorkspaceSet, host_theme, now)` from restore's
  `WorkspaceSet::restored(..)`; Spec B may move where that call sits.
- The loop and render code read state only through the accessors of 4.5 and
  do not take `&mut WorkspaceSet`.
- If Spec B moves the host theme fields or `lifecycle_authority_dirty` out of
  `AppState`, the accessors listed here move with them; this spec does not
  depend on where they live.
- `runtimes_replaced_panes`, `has_pending_agent_resumes`' scheduling and the
  resume schedule are Spec B's; L3 only changes what they iterate
  (`records()` instead of the terminal map).

Of Spec E (`notes/spec-pixel-geometry.md`):

- `shepr_core::geometry` stays the home of grid and pixel types. L1 removes
  `SplitBranch` from `geometry.rs`; Spec E's edits to that file should expect
  it gone.
- `SpawnGeometry` (area plus `HostCellSize`, `cell_px()`) moves to
  `shepr_mux::workspace` in L2. Spec E's changes to the cell-size types apply
  to it there.
- The split token carries whatever `WorkspaceChrome::pane_spawn_geometry`
  returns (today core `PaneGeometry`). Spec E may change that type; the token
  passes it through without inspecting it.
- L3 renames mux's `PaneGeometry` to `WorkspaceChrome`, so "PaneGeometry" means
  core's type only. Spec E should name `WorkspaceChrome` where it means mux's.

### API this spec offers

To Spec B (the loop, render, persistence driving):

- `AppState::{workspaces, workspace, pane, resolve_pane, terminal,
  bookmark_index, layout_area, chrome_in, settings, host_terminal_theme,
  host_terminal_appearance, shell_projection_revision,
  next_agent_state_change_seq, session_dirty, take_session_dirty,
  mark_session_dirty, mark_shell_projection_dirty, record_workspace_geometry,
  drain_lifecycle_authority_dirty}`.
- `WorkspaceSet::{iter, len, is_empty, get, position, pane, resolve, records,
  bookmark, bookmark_index}` and `Workspace::{id, tree, spawn_geometry,
  display_name}`, `PaneTree::{focused, visible_pane_ids, shows, layout,
  zoomed}`, `PaneRef::{workspace, id, record, terminal, public_id}`.
- Persistence: `capture_job(&WorkspaceSet, &PaneRuntimeRegistry, ..)`, the
  `SavedPaneRef` struct, and `PreservedLayout.pane_ids` keyed by it.
- Runtimes keyed by `PaneId` from L5.

To Spec E: `SpawnGeometry` in `shepr_mux::workspace`, `WorkspaceChrome` as
the mux chrome type, and `PreparedSplit::geometry()` as the one carrier of a
new pane's first PTY size.

## 10. Findings

Bugs and structural defects met on the way:

- F1. Restore validates after its side effects. `restore_workspace` reserves
  agent sessions (`pane_restore_startup`) and carries history per pane, then
  `Workspace::from_restored` may refuse the workspace, leaving the
  reservation behind: a later pane with the same session would start as a
  duplicate shell instead of resuming. The refusal is unreachable today
  because `plan_workspace` checks numbers first, but only by the order of two
  functions. L3's `TreePlan` makes the order structural.
- F2. The pane removal plan is ceremony. Both phases always run in one
  synchronous call (`apply_internal_event`, `handle_pane_close`), so
  `PaneRemovalCommit::Stale` and its warning are unreachable, and
  `Workspace::remove_pane` re-derives the plan it was given. L3 removes it.
- F3. The split's second focus knob is dead. `commit_new_pane(prepared,
  false)` still focuses the new pane when prepare focused it, which is what
  `WorkspaceFixture::test_split` does; production passes `true` twice.
- F4. `capture_preserved_layout`'s pane-count check guards a pane without a
  terminal ID, which cannot exist once records own terminals. Deleted in L3.
- F5. `TerminalId` duplicates `PaneId` one-to-one for a pane's whole life,
  never crosses the wire and is never saved, yet keys runtimes and history
  and forces reverse walks (`abandon_terminal_agent_resume` walks every pane
  of every workspace; so do `App::insert_test_runtime` and `App::test_runtime`).
  No hunt entry names it. Retired in L5.
- F6. `Workspace::mark_identity_undiscovered` has no production caller. A
  workspace is undiscovered from construction, so the method only served tests
  that mutated `identity_cwd` after construction.
- F7. `Workspace::resolved_identity_cwd_from` returns `Option<PathBuf>` but is
  always `Some`; `workspace_git_refresh_targets`' `?` on it never fires.
- F8. `Workspace::test_from_pane` silently overwrites the record's number
  with `FIRST`, so a fixture passing another number gets a different pane
  than it asked for.
- F9. `AppState::move_workspace` is `pub` while every sibling mutator is
  `pub(crate)`; L2 replaces it.

Stale or shifted hunt entries:

- TYP-002 describes `focused`/`root_pane` as `Option<u32>` and a `type PaneKey
  = (usize, u32)`; the code has plain `u32` fields and `SavedPaneRef`. Its
  snapshot half (raw IDs as saved keys, restore's `id_map` juggling,
  `from_saved`'s comment-enforced remapping) closes with L3; what remains is the
  logging and `Display` half.
- BUG-060 has three clauses: `display_name()`'s clone closes in L3; the
  `workspace_index` scan is adjudicated in L4 (kept deliberately, with the
  reason at `WorkspaceSet::get`); the pending-resume scan stays (Spec B).
- BUG-061 is half closed (L3); its remaining half is the same work as CON-109.
- STR-031's remark that the `AppState` doc no longer describes the struct is
  answered by the doc in 4.5, rewritten in L4.

Everything else STR-024, STR-031, BUG-061, TYP-003, TYP-004 and CON-061 state
was confirmed against the code.
