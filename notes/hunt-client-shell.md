# Hunt: client-shell (`crates/shepr-client/src/shell/`)

Design reconnaissance of the TUI shell: about 18,400 lines of production code
in 38 files under `shell/`, all read in full (tests skipped except to see how
they are laid out). Wiring into `shell.rs`, `shell_runtime.rs`, `lib.rs`,
`endpoint/commands.rs`, `shepr-protocol` (`ids.rs`, `identity.rs`,
`projection.rs`, `command.rs`, `status.rs`, `frame.rs`), `shepr-termio`
(`ScrollMetrics`), `shepr-agent` (`AgentState`, `parse_agent_label`) and the
server's `LayoutSetSplitRatio` handler was followed where a question led there.

No code was changed. Function and type names are given instead of line
numbers.

## Headline

1. **`ClientShellState` is a god object.** About 58 fields, every one
   `pub(super)`, mutated by `impl ClientShellState` blocks in some 25 files
   that all start with `use super::*`. The `shell/` subdirectories are
   cosmetic: every file is hung off `shell.rs` with `#[path = ...]` as a flat
   sibling module (`shell::mouse`, `shell::copy_mode`, ...), and the actual
   module tree contradicts the folders (`overlays/overlays.rs` and
   `sidebar/sidebar.rs` are children of `presentation/render.rs`). The repo's
   own "No god objects" principle is stated for `AppState`; the client shell
   is where it is broken hardest. Most findings below are symptoms of this:
   state with no owner gets its invariants re-derived at each site.
2. **Modes are stored and also derived.** `ClientShellMode::Copy` coexists
   with `copy_mode: Option<ClientCopyModeState>`, and `Navigate` coexists
   with `navigate_workspace_id`. Six sites decide independently whether copy
   mode is the live mode, and leaving Navigate through the pane-scrollbar
   click leaves a stale preview that redirects later workspace actions
   (a real bug, see Bugs).
3. **Selection state is seven loose fields** (`selection`,
   `selection_focus_pending`, `last_pane_click`, `selection_autoscroll`,
   `selection_autoscroll_deadline`, `selection_highlight_clear_deadline`,
   `word_selection_gesture`), plus a mirrored copy in
   `ClientCopyModeState::selection`. Roughly a dozen sites clear a subset of
   them, and the subsets already disagree.
4. **The patch fast path second-guesses compose.** `fast_path_blocker`
   (`surface_patch.rs`) re-derives from shell state what `compose` draws over
   pane cells, as a list of `&'static str` reasons that production code then
   throws away. The two agree only because someone keeps them in step.
5. **Request identity is a string end to end**, minted by format in three
   places (`client-shell:{n}`, `client-shell-view:{serial}:off`, the focus
   lane), flattened to `String` in `ClientShellEndpointRequest.id`, rewrapped
   by `endpoint/commands.rs`, flattened again for cancellation lists, then
   handed back to `drop_request(&str)`. Every staleness check in the shell
   (copy pipeline, scroll lanes, word selection, workspace highlight, label
   lookup) keeps its own `RequestId` copy and compares it, so "is this answer
   still the one I want?" is answered in five places beside the ledger that
   claims to be "the sole owner of issued request identities".
6. **Split identity is reverse engineered from rectangles.** The client
   derives which panes sit on each side of a split from pane rects
   (`split_child_panes`), separately hashes the same membership into a
   topology signature (`pane_surface_topology_signature`), and the server
   maps the pane lists back to a split path (`split_path_for_children`). Three
   answers to "which split is this", for a value the server already has.

## 1. Axes that should be types

### 1.1 Endpoint-qualified addresses, spelled six ways

The same "a workspace or pane on some endpoint" appears as:

- `ClientEndpointFocusTarget::{Workspace, Pane}` (no endpoint; carried beside
  one)
- `ClientNavigatorTarget::{Machine, Workspace, Pane}` (endpoint inside each
  variant)
- `WorkspaceNavigationTarget { endpoint_id, workspace_id, boot_id, generation }`
- `AggregateAgentTarget { endpoint_id, pane_id }`
- `pending_agent_reveal: Option<(ClientEndpointId, PublicPaneId)>`
- `ShellHitMap::endpoint_agents: Vec<(Rect, ClientEndpointId, PublicPaneId)>`
  next to `ShellHitMap::agents: Vec<(Rect, PublicPaneId)>`
- `WorkspaceHit { endpoint_id, workspace_id }`,
  `ClientWorkspacePress { endpoint_id, workspace_id, .. }`

Proposal: one `Location { endpoint: ClientEndpointId, target: Target }` with
`Target::{Machine, Workspace(WorkspaceId), Pane(PublicPaneId)}`, and a
`PinnedLocation` that adds the `(BootId, ConnectionGeneration)` snapshot
identity `WorkspaceNavigationTarget` carries. Hits, navigator rows, agent
rows, reveal requests and `ClientShellAction::ActivateEndpoint` then all speak
one type, and the single-endpoint `hits.agents` list disappears (see 2.9).

### 1.2 Connection generation is a bare `u64`, and `Option<u64>` means "test"

`snapshot_generation: Option<u64>` on `ClientShellEndpoint` ("absent only in
tests"), `active_snapshot_generation: Option<u64>`,
`type SurfaceGeneration = Option<u64>` in `surfaces.rs`, `generation: u64`
parameters on `receive_pane_surface_from`, `apply_pane_surface_patch_from`,
`endpoint_snapshot_matches`, `endpoint_snapshot_identity`. The `None`
generation exists only so tests can skip a connection, yet it is threaded
through production types, and `receive_tagged_pane_surface` decides staleness
with `generation < newest` on `Option<u64>`, which leans on `None < Some(_)`
ordering. Make `ConnectionGeneration(u64)` a newtype (minted by the endpoint
registry only) and have tests mint real ones; the `#[cfg(test)]
receive_pane_surface` shims in `shell.rs` then go too.

### 1.3 Two coordinate spaces in one `Rect`/`u16`

Surface-local coordinates (what the server sends in `PaneSurfacePane`,
`PaneSurfaceSplit`, patch rows, the cursor) and screen coordinates (hits,
frame cells) are both `Rect` / `(u16, u16)`. The translation
`layout.pane_surface.x.saturating_add(...)` is written in `compose` (pane
hits, split hits), in `apply_tagged_pane_surface_patch` (rows, cursor,
scrollbar rect) and in `compose_pane_surface` (cursor). A `SurfaceRect` vs
`ScreenRect` split, with one `SurfaceOrigin::to_screen`, would make the
missing-offset bug unrepresentable and give 2.4 a home.

### 1.4 Scroll offsets: `u64` on the wire, `usize` in termio, top-based in lists

- `usize::try_from(scroll.offset_from_bottom).unwrap_or(usize::MAX)` (and the
  same for `max_offset_from_bottom`, `viewport_rows`) appears in
  `scroll_target_shown`, `presented_surface_changed` (twice),
  `answer_pane_scroll`, the patch fast path and `compose`; the reverse
  `offset as u64` is in `dispatch_pane_scroll`.
- Sidebar lists, the navigator and Help reuse `shepr_termio::ScrollMetrics`,
  a terminal-history type, with a dummy `history_origin: AbsRow(0)`, and each
  site converts "start row from top" to "offset from bottom" by hand:
  `render_expanded`, `render_agent_list`, the navigator renderer, the Help
  renderer, and four scrollbar drag and click arms in `mouse.rs`
  (`max_offset_from_bottom.saturating_sub(offset)`).
- `ClientCopyModeState` keeps its own `history_origin`, `offset_from_bottom`,
  `max_offset_from_bottom`, `geometry` and reimplements
  `ScrollMetrics::viewport_top_row` as `viewport_top`.

Proposal: one `HistoryScroll` (terminal) and one `ListScroll { start,
max_start, viewport_rows }` (chrome lists), each with its own scrollbar
mapping; a `From<PaneSurfaceScrollMetrics>` conversion owned once; copy mode
holds a `HistoryScroll` instead of four loose fields.

### 1.5 Notice identity is a free-form string

`ClientEndpointNoticeKey { boot_id, kind, code: String }`. Codes seen:
`"selection_empty"`, `"paste_rejected"` (twice, deliberately equal),
`"navigate_endpoint_inactive"`, `"server"`, `"cancelled"`, the command's
dotted name, `format!("{method}:{message}")` (keyed on server prose),
`format!("session_restore_incomplete:{storage_key}")`,
`format!("machine-diagnostic:{label}")`, and in `receive_endpoint_unavailable`
the human message itself. Classification by string then follows:
`render_notice` decides whether a card's body is capped with
`code.starts_with(MACHINE_DIAGNOSTIC_NOTICE_PREFIX)`, and
`reset_endpoint_projection` / `push_endpoint_notice_at_boot` decide "is this a
restore notice" by membership in `restore_notice_seen`. Proposal: a closed
`NoticeCode` enum (`SelectionEmpty`, `PasteRejected`, `Timeout(CommandKind)`,
`Rejected(CommandKind)`, `Interrupted`, `RestoreIncomplete(ClientEndpointId)`,
`MachineDiagnostic(ClientEndpointId)`, `NotReady(ClientEndpointId)`, ...)
whose methods answer "is the body capped", "is it queued", "how is it
deduplicated" (see 2.6).

### 1.6 Command identity in the ledger is a `String`

`ledger::Entry.method: String` is `command.name().to_owned()`, a
`&'static str` from `EndpointCommandTraits`. It is then used as a notice code
and for the timeout success-clear. Store the command kind (a fieldless
discriminant enum generated beside `EndpointCommand`), and let the notice
render its dotted name.

### 1.7 Agent kind crosses the wire as a display string

`ClientShellAgent.agent: Option<String>`. The client runs
`shepr_agent::detect::parse_agent_label` (the process-name identification
heuristic) on it to recover `Agent`, then calls `Agent::label()` to get a
string again to index `AgentsSidebarConfig::rows_by_agent` (a string-keyed
map), and the navigator falls back to the literal `"terminal"`. Since
`shepr-agent` sits below `shepr-protocol` in the documented layering, the
wire can carry `Option<Agent>` (verify against `brokkr.toml`), and
`rows_by_agent` can be keyed by `Agent` at config validation.

### 1.8 Agent status: three enums and a mapping in the client

`shepr_protocol::AgentStatus`, `shepr_agent::detect::PresentedAgentState` and
the client's `ClientNavigatorFilter::{Blocked, Working, Idle}` are the same
closed set. `status_priority` (`presentation/status.rs`) maps `AgentStatus`
into `shepr_agent::AgentState` only to call `attention_rank()`;
`status_text` and the navigator's filter label match the same three
variants again. Collapse `ClientNavigatorFilter` into
`Option<AgentStatus>`, and give `AgentStatus` its rank and label in one place
(protocol, or make the wire type `PresentedAgentState`).

### 1.9 Split ratios: `f32` with three clamps

`LayoutSetSplitRatioParams.ratio: f32`,
`ClientChromeDrag::PaneSplit.last_sent_ratio: Option<f32>` compared with
`f32::EPSILON`, `pane_split_ratio` clamping to `MIN_SPLIT_RATIO..MAX_SPLIT_RATIO`
by hand, `SectionSplit::from_drag` clamping to the same bounds, and the server
re-clamping through `shepr_core::layout::SplitRatio::clamped`. `SplitRatio`
already exists; use it on the wire and for `SectionSplit` (whose
`new`/`from_drag`/serde validation duplicate it).

### 1.10 Tuples that name things

- `ClientCopyModeState.geometry: (u16, u16)` compared against
  `(hit.inner_rect.width, hit.inner_rect.height)` in three places.
- `last_composed_size: Option<(u16, u16)>`, read with `unwrap_or_default()`
  as a `(0, 0)` sentinel in the patch path.
- `ClientChromeDrag::Workspace.target: Option<(Option<WorkspaceId>, u16)>`
  (insert-before target and indicator row).
- `ClientWordSelection`: `anchor: (AbsRow, u16)`, `anchor_bounds:
  Option<(u16, u16)>`, `cached_row: Option<(AbsRow, String)>`,
  `drag_word_selection(cursor: (AbsRow, u16))` while `shepr_vt::Point` and
  `PaneTextPoint` exist.
- `endpoint_notice_deadline: Option<(ClientEndpointNoticeKey, String, Instant)>`.
- `endpoint_status_presentation` returns `(&str, &str, Color)`.
- `ClientShellWorkspace.git_ahead_behind: Option<(usize, usize)>` reaches
  `SpaceTokenContext` as a tuple although `GitStatus { ahead, behind }` is a
  named shape one step later.
- `ShellHitMap::{global_menu_rows, context_menu_rows}: Vec<(Rect, usize)>` and
  `navigator_rows: Vec<(Rect, ClientNavigatorTarget)>`.

### 1.11 Outcomes that reach a caller as prose or as `bool`/`Option<bool>`

- Every `Work` completion treats a reply of the wrong variant with
  `set_endpoint_error("endpoint returned an unexpected ... result")` (five
  sites: selection copy, pane scroll, word selection, copy motion, copy
  search). The real fix is typed replies: give each command its reply type
  (an associated type, or a reply enum per command family) so the ledger's
  `Work` holds a continuation that cannot receive the wrong variant.
- `EndpointError::Rejected(String)`: the shell keys notice identity on that
  prose (`format!("{method}:{message}")`).
- `persist_chrome_preferences` reports `preferences::store`'s `String` error
  as the endpoint error banner; `store` returns `Result<(), String>` while
  `probe_writable` returns `io::Error` with formatted text.
- `TextEditor::handle_key -> Option<bool>` (unhandled / handled-unchanged /
  changed); `Work::dropped`, `answer_pane_scroll`, `drop_*`, `complete_*` all
  return a bare `bool` meaning "repaint". A three-way `EditOutcome` and a
  `Repaint` newtype (or always writing into the `ClientShellInput` outcome)
  would stop the `outcome.repaint |= ...` plumbing from silently dropping one.
- `fast_path_blocker -> Option<&'static str>` (see 2.4): a string-typed enum
  whose value production ignores.

### 1.12 Chrome values paired with `_manual` flags

`(sidebar_width, sidebar_width_manual)`, `(sidebar_collapsed,
sidebar_collapsed_manual)`, `(sidebar_section_split,
sidebar_section_split_manual)`, `(config.agent_panel_sort,
agent_panel_sort_manual)` plus `ConfiguredChrome`'s three booleans. A
`Chrome<T> { value, origin: Default | Configured | Remembered | Manual }`
would make `persist_chrome_preferences` and `without_configured` one rule
instead of four `then_some`s. `sidebar_width` is a raw `u16` although
`SidebarBounds::clamp_width` is applied at two separate entry points; a
`SidebarWidth` minted only by the clamp follows the `b81a7d4` precedent.
`agent_panel_sort` is runtime state kept in `ClientShellConfig` and mutated
there (`self.config.agent_panel_sort = sort`).

## 2. Decisions made in more than one place

### 2.1 "Is copy mode the live mode?" (six sites, slightly different answers)

- `copy_or_terminal_mode`: copy pane == focused pane.
- `route_key_press`, Prefix arm: the same expression written inline again.
- `mouse.rs`, pane-scrollbar press: copy pane == hit pane AND focused == hit
  pane.
- `apply_active_snapshot`: promotes Terminal to Copy when the copy pane is
  focused, demotes Copy to Terminal otherwise.
- `copy_mode_owns_input`: `mode == Copy` AND no overlay AND copy pane
  focused.
- `insert_copy_search_text`: `mode == Copy` AND no overlay (no focus check).

Owner: the mode should not store `Copy` at all. Either the copy session lives
inside the mode (`Mode::Copy(CopySession)`, parked sessions held apart), or
`Copy` is derived from `copy_mode` and focus in one function and never
assigned. The same holds for `Navigate` and `navigate_workspace_id`
(`Mode::Navigate { preview: Option<PinnedLocation> }`); see the bug below for
what the split already costs.

### 2.2 "Which state goes with a selection?" (sites disagree)

Sites that end a selection and the fields each clears:

- `route_key_press` (two branches), `prepare_committed_text`:
  selection, autoscroll, highlight deadline.
- `apply_active_snapshot` focus-loss branch: all seven fields.
- `apply_active_snapshot` copy-pane-removed and copy-pane-unfocused branches:
  selection, autoscroll, highlight deadline.
- `presented_surface_changed`: word gesture, selection, autoscroll, highlight
  deadline.
- `cancel_word_selection`: word gesture, selection, autoscroll (not the
  highlight deadline).
- `tick_selection_highlight`: selection and deadline only.
- `enter_copy_mode`, `exit_copy_mode`, mouse left press: their own subsets.

They disagree today (for example `selection_focus_pending` survives a
key-cleared selection; `cancel_word_selection` leaves a highlight deadline
armed). Harmless only because each reader re-checks `selection.is_some()`.
Owner: a `MouseSelection` struct (selection, gesture, focus-pending,
autoscroll with its deadline, highlight expiry, last click) with `clear()`,
and copy mode driving it instead of mirroring it
(`ClientCopyModeState::selection` + `sync_copy_selection` is a second copy
held in step).

### 2.3 "Is this endpoint usable?"

`endpoint_projection_available` and `endpoint_is_online` are the same
predicate written twice (Online AND snapshot present).
`navigation_target_valid` adds generation and boot; `CachedEndpointSnapshot::stale`,
`handle_endpoint_navigation`, `move_navigate_workspace`, the navigator rows
and both sidebars test `status == Online` / `!= Online` directly;
`handle_endpoint_machine_click`, `activate_endpoint` and `focus_or_activate`
add "or it is Local". The root cause is that `ClientShellEndpoint` keeps
`status`, `snapshot` and `snapshot_generation` as independent fields, so
"Online with no snapshot" is representable and every reader re-combines them.
Owner: an endpoint state enum (`Connecting`, `Online { snapshot, generation }`,
`Stale { last: snapshot }`, `Attention { last }`) on `ClientShellEndpoint`,
with `usable()` / `stale()` methods.

### 2.4 "What does compose draw over pane cells?" (fast path vs compose)

`fast_path_blocker` independently predicts compose's layers: mode bar
(`mode != Terminal`), overlay, endpoint error, notice card, selection,
copy-mode owner, unknown pane hits, surface overflow. `render_mode_bar`
decides on its own that the bar is drawn when `mode != Terminal ||
endpoint_error.is_some()`; compose additionally draws a lifecycle banner and
hides the cursor when the active endpoint is not Online, which the blocker
does not check (it is covered only indirectly because compose clears
`hits.panes`; a cursor-only patch still takes the fast path). Owner: compose
should record what it occluded (opaque rects, bar rect, whether the pane
cursor is suppressed, which panes carry highlights) in a `LastComposition`,
and the fast path should consult that record. The patch path also rebuilds
`PaneHit` fields by hand (scrollbar rect offset, scroll metric conversion,
mouse and pixel flags), duplicating the `PaneHit` construction in `compose`;
a single `PaneHit::from_wire(pane, origin, clip)` owns it.

### 2.5 "Which side of a split is a pane on?"

`split_child_panes` (`rect.x < split.pos` means first) and
`pane_surface_topology_signature` (`rect.x >= split.pos` means second; outside
the area means neither) answer it separately, and both are a reconstruction
of the server's layout tree, which the server then reverse maps again in
`split_path_for_children`. `pane_split_topology_matches_hit` and
`pane_split_target_is_current` are two further checks layered on the hash.
Owner: the server. `PaneSurfaceSplit` already carries `path`; adding a
server-minted layout epoch (or the child pane lists themselves) to the
surface lets the client send `(workspace, path, epoch, ratio)` and drop the
FNV hash, the rect classification and the topology comparisons.

### 2.6 Notice deduplication and lifetime

- The Timeout notice key is built in `answer_request`'s error branch with
  `code: entry.method` and rebuilt in its success branch to clear the
  suppression; two constructions of one key.
- `push_endpoint_notice_at_boot` dedupes Rejected/Unavailable by "equal to the
  visible card" and Timeout by `endpoint_notice_seen`; `receive_restore_notice`
  has its own `restore_notice_seen`; `handle_machine_badge_event` bypasses
  both and writes `visible_endpoint_notice` directly, resetting the deadline
  by hand.
- The visible notice's lifetime is "deadline tuple whose key and body equal
  the visible card's", compared in `endpoint_notice_drawn`,
  `tick_transient_banners` and `next_timer_deadline`.

Owner: a `Notices` component (visible card, restore queue, seen sets,
deadline) with `push`, `dismiss`, `drawn(now)`, `tick(now)`, `deadline()`.

### 2.7 Timer inventory

`next_timer_deadline` enumerates six deadlines; `lib.rs` separately calls
`tick_selection_autoscroll`, `tick_selection_highlight`,
`tick_workspace_highlight`, `tick_endpoint_error`, `tick_transient_banners`.
The two lists are held in step by hand; `tick_transient_banners` also does
work (popping the restore queue) that has no deadline at all (see Bugs). Only
`word_selection` uses `limits::Deadline`; every other deadline is a raw
`Option<Instant>`. Owner: one `ShellTimers` registry (or each component
exposing `deadline()` + `tick(now)` behind one trait the shell iterates).

### 2.8 Initial chrome, held in step by a pairwise test

`ClientShellState::new_at` and `ClientShellConfig::initial_surface_size` each
compute the starting `sidebar_collapsed` and `sidebar_width` from
preferences and config. `initial_surface_size_uses_persisted_endpoint_chrome`
is a pairwise-agreement test pinning them together. Owner: one
`InitialChrome::resolve(config, preferences)` used by both.

### 2.9 Agent order and the agent row filter

- Priority order is coded in `sort_agent_refs` (`agent_sidebar.rs`), again in
  `AgentRowIndex::new`'s sort, and a third, cross-endpoint version in
  `sort_aggregate_rows` (stale first, then rank, then client-side recency
  instead of `state_change_seq`).
- The agent panel computes `agent_sidebar::agent_rows` per endpoint (sorted),
  puts them in a `HashMap` keyed by `(ClientEndpointId, pane_id.to_string())`,
  then reorders by `aggregate_agent_rows`, so `AgentRowIndex`'s sort is dead
  work.
- "An agent row needs its workspace" is decided in `aggregate_agent_rows`
  (comment: "These are the same records the sidebar can render") and again in
  `AgentRowIndex::agent_row` (`self.workspace(...)?`).
- Single endpoint vs federated selects between `hits.agents` (PaneFocus
  directly) and `hits.endpoint_agents` (`focus_or_activate`).

Owner: one `AgentPanelModel` built once per snapshot change (rows with
endpoint, order, tokens, focus, stale), used by both sidebars, keyboard agent
navigation (`online_agent_targets`, "despite the historical name") and
`indexed_navigation_target_exists`.

### 2.10 Workspace order and "workspace N"

`sidebar::workspace_entries` returns `(0..len).collect()`: an identity map,
the vestige of a removed filter, yet callers index
`snapshot.workspaces[entry]` through it in `endpoint_command_for_action`,
`indexed_navigation_target_exists`, `workspace_drop_target_at`,
`handle_endpoint_navigation`, `move_navigate_workspace` and `render_expanded`,
while `render_collapsed` and `workspace_move_command` iterate
`snapshot.workspaces` directly. The displayed number is
`ClientShellWorkspace.number` (the server sets `index + 1` in
`app/creation.rs`), while `SwitchWorkspace(index)` uses position. Two answers
to "what is workspace 3", agreeing because the server keeps them aligned.
The federated workspace cycle list is built twice
(`handle_endpoint_navigation`, `move_navigate_workspace`), both filtering
Online endpoints.

### 2.11 Cycling with wraparound

Previous/next with wrap and a not-found case is written in
`agent_target_index`, `handle_endpoint_navigation`, `move_navigate_workspace`,
`endpoint_command_for_action` (workspaces and `CyclePane*`). They disagree on
the not-found case: the federated paths pick the first or last entry, the
single-endpoint workspace path starts from index 0 (so Next lands on the
second workspace), and `CyclePane*` does the same.

### 2.12 Reveal-into-view

Single-endpoint agent navigation sets `agent_scroll = index` (jump so the row
is at the top) in `endpoint_command_for_action`; federated agent navigation
calls `reveal_endpoint_agent` (minimal scroll via
`list_scroll_start_to_reveal`). `reveal_workspace` jumps
(`workspace_scroll = target`) for a single endpoint, while `render_expanded`
and `render_collapsed` reveal minimally (and with two different algorithms).
Same question, different answers.

### 2.13 Commands that change focus

`ledger::submit` decides with its own `matches!` that `WorkspaceFocus`,
`PaneFocus`, `PaneFocusDirection`, `WorkspaceCreate` and `PaneSplit` change
focus (to drop the pending workspace highlight). `EndpointCommandTraits` is
the "one exhaustive table so the client's notices, the server's logs and the
server loop's routing agree"; `changes_focus` belongs there (and
`WorkspaceClose`, `PaneClose`, `PaneSwap` deserve an explicit answer).

### 2.14 Keybinding help text vs routing

Copy-mode keys are routed by char literals in `route_copy_mode_key` and
described by literals in `render_mode_bar` (`"h/j/k/l w/b/e { }"`,
`"y/enter"`, ...). Resize keys: `route_resize_key` vs the RESIZE bar text.
Navigator and Help footers state their keys as literals with a comment
pointing at `route_overlay_key`. Owner: a small command table per fixed-key
surface (enum of commands, their keys, their labels) that both routing and
help read; `copy_mode_command_char -> Option<char>` from termio is a
char-typed enum that table would replace.

### 2.15 Smaller duplicated answers

- Panel contrast colour: `status::panel_contrast_fg`, `overlays::contrast`,
  and inline copies in `render_mode_bar` and the copy cursor in `compose`.
- Workspace selection background: `sidebar::workspace_selection_background`
  and an inline copy in `render_collapsed`.
- Scrollbar drawing: `scroll::render_list_scrollbar` (right one-eighth
  block glyph), `termio::scroll::render_scrollbar_buffer` (right half block
  glyph) and an inline loop in `render_help_overlay` (right half block
  again).
- Sidebar section threshold `< 6` in both `sidebar_section_heights` and
  `sidebar_section_divider_rect`.
- Endpoint status label for Attention: `"attention"` in
  `endpoint_status_presentation`, `"! error"` in `render_endpoint_row`.
- "What is a word": client double-click `is_word_separator`
  (`word_bounds.rs`), mux copy-mode `COPY_MODE_WORD_SEPARATORS`
  (`shepr-mux/src/limits.rs`), and `TextEditor::word_boundary`. The first two
  apply to the same pane text; they differ on purpose or by accident, and
  nothing says which.
- Mouse capture off disables hits by clearing them in `render_shell` and in
  `compose`, plus an explicit check in the right-click arm.

## 3. Structure

### 3.1 Break up `ClientShellState`

Proposed components, each owning its fields and invariants, with the shell
as a thin coordinator that routes events and assembles a frame:

- `Endpoints` (endpoint list, active id, collapsed set, the active snapshot;
  today `self.snapshot` is a deep clone of `endpoints[active].snapshot`,
  re-cloned on every snapshot by `apply_cached_endpoint_snapshot`, and
  `active_snapshot_generation` mirrors `snapshot_generation`; both copies
  carry their own "older revision" check in `apply_active_snapshot` and
  `cache_endpoint_snapshot_at_generation`).
- `Presentation` (`PaneSurfaces`, hits, `LastComposition`, terminal size;
  today the shell's `last_composed_size` and `lib.rs`'s `reported_geometry`
  both hold the terminal size).
- `Mode` (the enum carrying Navigate preview and Copy session, 2.1).
- `MouseSelection` (2.2), `CopySession` (copy state, `CopyPipeline`, search
  as one `Option<CopySearch>` instead of seven `search_*` fields whose "clear
  search" is written out in `route_copy_mode_key` and
  `presented_surface_changed`).
- `ChromeLayout` (sidebar width, collapse, split, agent sort with origins,
  persistence; 1.12, 2.8).
- `Notices` (2.6), `Timers` (2.7), `Requests` (ledger plus the typed
  continuations, 3.4).
- `Overlay` (3.3).

Make the folders the real module tree (`shell/input/mod.rs`, ...), drop
every `#[path]` and `use super::*`, and give each component a narrow
`pub(super)` API. Tests currently poke private fields directly
(`state.copy_mode = Some(ClientCopyModeState { ...19 fields... })` appears
in at least four test modules), which is the clearest sign construction has
no owner.

### 3.2 Render mutates state

`render::ShellRenderState` hands render `&mut workspace_scroll`,
`&mut agent_scroll`, `&mut reveal_focused_workspace`,
`&mut reveal_navigation_workspace`; the sidebars clamp and reveal inside
drawing. Help's scroll is clamped after compose from a hit-map value; the
navigator's effective scroll is computed in its renderer and never stored,
while `scroll_navigator_to` computes another. The server keeps "render is
pure"; the client should too: a layout/scroll resolution pass produces a view
model (rows, scroll starts, hit rects), then drawing is `&self`.
`endpoint_command_for_action` is a translator that also scrolls the agent
panel and calls `reveal_workspace`; that side effect belongs to the caller.

### 3.3 Overlays as one module each

Each overlay is spread across `state.rs` (types), `overlay_input.rs` (keys,
an `if matches!` chain per overlay), `mouse.rs` (an `if matches!` arm per
overlay), `overlays.rs` (render), `context_menu.rs`/`global_menu.rs` (items
and actions) and `ShellHitMap` (flat fields such as `help_*`,
`navigator_*`, `overlay_primary/clear/cancel`, `*_menu_rows`).
`OverlayRender` is a product type of every overlay's hit rects, copied field
by field into `ShellHitMap` in `compose`. Proposal: per-overlay modules each
owning state, `render -> (painted, Hits)`, `on_key`, `on_mouse`, with the
overlay enum holding its own hits so stale or foreign hit fields cannot
exist. `ClientRenameOverlay.title` should derive from its target.

### 3.4 Requests: one typed continuation per command

The ledger's `Work` enum plus `Work::answered`/`dropped` dispatch is good
(precedent `3ea7fa4`), but the feature-local `RequestId` copies
(`CopyPipeline::awaiting`, `ScrollFlight::request`,
`ClientWordSelection::pending`, `PendingWorkspaceHighlight::request_id`,
`ClientRenameTarget::NewWorkspace::label_lookup`) each re-answer staleness.
If `Work` carried a per-feature token (a session generation the feature
bumps on reset), the feature checks its token, not an id it copied, and the
`opened_since` orphan guard in `dropped_entry` (which exists because "the
types cannot rule that out") can become a type rule: `dropped` takes a
context without `submit`.

### 3.5 Crate placement

- `wire_cells.rs` and `compose_pane_surface.rs` are frame composition over
  `shepr_protocol::FrameData`; together with `FrameData`'s
  "Length must equal width * height" invariant (enforced only as a doc
  comment and re-checked by `compose_pane_surface`, `overwrite` and
  `patch_rect`) they belong in one place that owns a `FrameData` whose
  constructor guarantees the shape.
- `word_bounds.rs` is pure text logic over pane text; it should live with the
  other pane-text word logic (mux) or in `shepr-termio`, and the two word
  definitions should be reconciled there (2.15).
- `TextEditor` is a generic line editor (no shell state) and fits
  `shepr-termio` beside `TerminalKey`.
- `preferences.rs` is persistence (atomic write, probe) living under
  `overlays/`; it belongs with chrome state, and its FNV path hashing
  duplicates the FNV in `topology.rs`.
- `shell` depends on `shepr-agent` only for `status_priority` and
  `parse_agent_label` (1.7, 1.8); fixing those removes the edge.

### 3.6 Test layout

`shell/tests/` groups by feature (`copy.rs` 3040 lines, `endpoints.rs` 2549
lines) while unit tests also sit in the production files, and some copy-mode
tests live in `input/input.rs`. Once the components in 3.1 exist, tests
should sit with the component they exercise and drive it through its API
rather than writing `pub(super)` fields.

## 4. Types that resolve to primitives

- **`RequestId`** (`shepr-protocol/src/identity.rs`): `From<String>`,
  `From<&str>`, `Deref<Target = str>`, `Borrow<str>`, `PartialEq<str/&str/String>`.
  In the shell it is built by `format!` (`Ledger::id_for`), turned back into a
  `String` for `ClientShellEndpointRequest.id`, compared against `&str`
  (`release_highlight(&str)`, `keep_workspace_highlight_until_snapshot(&str)`
  converts back with `to_owned().into()`), and accepted as `&str` by
  `answer_request` and `drop_request`. Offer instead: minting only through a
  ledger (`RequestId::shell(serial)`, `RequestId::view(serial)`, ...), typed
  everywhere in between, and no string comparisons.
- **`BootId`**: `Deref<Target = str>` and `PartialEq<str>`; `answer_request`
  takes `boot_id: &str`; `active_boot_key: String` is
  `format!("{}:{}", storage_key, boot_id)` (bare boot id for Local), a string
  composite of two typed values used only for change detection; use
  `(ClientEndpointId, BootId)`. `ClientPresentationLogContext` stores
  `boot_id: Option<String>` and `endpoint: String`.
- **`WorkspaceId`, `PublicPaneId`**: `Deref<Target = str>`, `From<WorkspaceId>
  for String`, `PartialEq<str>`. Shell sites that drop to `&str`:
  `pane_split_target_is_current(.., workspace_id: &str)`,
  `pane_split_topology_matches_hit(.., &str)`,
  `apply_copy_search_result(pane_id: &str)`,
  `apply_copy_motion_target(pane_id: &str)`,
  `complete_word_selection_row(pane_id: &str)`,
  `reveal_endpoint_agent(pane_id: &str)`,
  `agent_target_index(focused_pane_id: Option<&str>)`,
  `AgentRowIndex::workspace(&str)`, plus many
  `x.as_deref() == Some(y.as_str())` comparisons (`apply_active_snapshot`,
  `copy_or_terminal_mode`, `reconcile_pending_workspace_highlight`,
  `render_expanded`'s `dragged` check compares two `WorkspaceId`s via
  `as_str()`), and `HashMap` keys built with `pane_id.to_string()`
  (`navigator_rows`, `endpoint_agents::agent_rows`). Removing the `Deref` and
  `PartialEq<str>` impls would surface all of them; a protocol helper such as
  `Option<&PublicPaneId>::is(&PublicPaneId)` is all callers need.
- **`TextEditor`**: `Deref<Target = str>`, `From<&str>`, `Display`, and a
  derived `Debug` that prints the typed text. `state.rs` and `ledger.rs`
  carry comments saying "never log with `{:?}`"; a redacting `Debug` on
  `TextEditor` and on the search query (a `TypedText` newtype used by
  `Work::CopySearch`, `ClientCopyOperation::Search`,
  `ClientCopyModeState::search_query`) turns the comment into a guarantee.
- **`ClientEndpointId::storage_key() -> String`** is used as an identity
  component in notice codes and the boot key, not just for storage.
- **`ClientShellSnapshot` redundancy**: `focused_workspace_id` and
  `ClientShellWorkspace.focused`, `focused_pane_id` and
  `ClientShellPane.focused` / `ClientShellAgent.focused` state the same fact
  twice; the shell reads one or the other per site (`render_collapsed` uses
  `workspace.focused`, `render_expanded`'s reveal uses
  `focused_workspace_id`). `ClientShellPane.workspace_id` and
  `ClientShellAgent.workspace_id` duplicate `PublicPaneId::workspace_id()`.
  `ClientShellWorkspace.number` duplicates position (2.10).
  `ClientShellWorkspace.custom_label` is not read anywhere in the shell's
  production code.
- **Sentinels**: `history_origin: AbsRow(0)` in every list `ScrollMetrics`;
  `last_composed_size.unwrap_or_default()` as `(0, 0)`;
  `hits.workspace_body`/`agent_body`/... `Rect::default()` meaning "absent"
  across `ShellHitMap`; `"unset"`, `"terminal"`, `"shell"`, `"workspace"`
  placeholder strings; `generation: None` meaning "test" (1.2).
- **String-typed closed sets**: notice codes (1.5);
  `copy_mode_command_char -> Option<char>` matched against `'q'`, `'y'`,
  `'v'`, ... (2.14); `fast_path_blocker`'s `&'static str` reasons;
  `agent` labels (1.7).

## Bugs and lateral findings

- **Stale Navigate preview redirects workspace actions (likely bug).** In
  `handle_mouse_with_accounting`, a left press on a pane scrollbar sets
  `self.mode` to Copy or Terminal without clearing `navigate_workspace_id`
  (the only clearing at the top of the handler is for a *blocked* preview).
  `workspace_action_id` and `workspace_preview_action_blocked` read
  `navigate_workspace_id` regardless of mode, so after previewing workspace
  B in Navigate mode and clicking a pane scrollbar, a later close, rename or
  new-workspace binding in Terminal mode acts on B (the preview), not the
  focused workspace; with a preview on another endpoint, Rename and Close are
  refused with "Select an available workspace ...". Fixed structurally by
  2.1 (`Mode::Navigate { preview }`).
- **Queued restore notices can stall after a click-dismiss.** Clicking the
  toast sets `visible_endpoint_notice = None` without popping
  `restore_notice_queue`. The pop happens in `tick_transient_banners`, but
  `next_timer_deadline` reports no deadline for an empty card, so the next
  queued restore notice waits for an unrelated timer (health ticks, request
  expiry) to fire the tick. Same root as 2.6/2.7.
- **Copy-mode word definition vs double-click word definition differ** (2.15);
  worth an explicit decision.
- **`word_bounds_at_column` maps pane text to columns with
  `shepr_vt::unicode_codepoint_width` per char**, while the server's grid
  widths and the client's chrome use `shepr_termio::blit::text_width` per
  grapheme (the `wire_cells`/`render` tests pin VS16 emoji at width 2). For
  emoji presentation sequences the double-click column mapping may drift by
  one cell per sequence. Unverified; worth a test with `"\u{2764}\u{fe0f}"`
  before a word.
- **Per-key navigator cost.** Every navigator key, wheel step and render calls
  `navigator_rows` over every endpoint, workspace and pane (lowercasing each
  candidate string per call); `End` and `scroll_navigator_to` compute it
  again. Fine at today's sizes, but it is a rebuild per event that a
  per-snapshot model (2.9) removes.
- **Agent panel cost.** `aggregate_agent_rows` does a linear `find` in
  `snapshot.agents` per ordered pane id (quadratic per endpoint), and every
  compose builds `AgentRowIndex`, sorts it, resolves tokens, builds a
  `HashMap`, then sorts again; `render_expanded` resolves every workspace's
  tokens twice (heights, then drawing). All of it runs per compose, not per
  snapshot.
- **Deep snapshot clone per snapshot** for the active endpoint
  (`apply_cached_endpoint_snapshot`, `activate_endpoint_projection`); an `Rc`
  or reading through `endpoints[active]` avoids it.
- **`aggregate_agent_rows(_active_endpoint_id)`** has an unused parameter;
  `online_agent_targets` is misnamed (its comment admits it); `endpoint_label`
  takes `&self` it does not use; `hit_test::contains` duplicates
  `Rect::contains`.
- **Two clock channels**: `ClientShellState.now` (set per loop event) and a
  `now` parameter on many methods (`answer_request`, `tick_*`,
  `handle_host_input`, mouse helpers). They agree in production because
  `handle_event` sets the field first; one channel would remove the question.
- **Pane rename to empty sends `label: Some("")`** while workspace rename
  treats empty as "do nothing" and the context menu's Clear sends `None`;
  whether the server treats `Some("")` as a clear is decided elsewhere.
  Worth one rule for "empty label".

## Suggested order

1. Mode enum carrying Navigate preview and Copy session (fixes the stale
   preview bug, removes 2.1).
2. `MouseSelection` and `CopySession` components (2.2, 1.4 for copy mode).
3. Notices and timers components (2.6, 2.7, the stall bug).
4. Typed `RequestId` minting, `ConnectionGeneration`, `CommandKind`,
   `NoticeCode` (1.2, 1.5, 1.6, 4).
5. `LastComposition` and `PaneHit::from_wire`, then delete
   `fast_path_blocker`'s re-derivation (2.4, 1.3).
6. Per-snapshot `AgentPanelModel` and workspace order model; delete
   `workspace_entries` (2.9, 2.10, 2.11, 2.12).
7. Server-minted split identity on the surface; delete the topology hash and
   rect classification (2.5, 1.9).
8. Real module tree, per-overlay modules, pure render pass (3.1 to 3.3).
