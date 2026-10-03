# Spec: a pure shell render, the shell state split, overlays as modules

Written against `reference/technical-implementation-spec.md`. Spawned from four
entries of the design hunt, re-verified against the code on this spec's date:

- `notes/hunt-structure.md`, "The shell's render mutates state"
- `notes/hunt-structure.md`, "`ClientShellState` still owns the endpoint,
  presentation, mode and copy state" (its client half only; the
  `shepr-remote` `#[path]` half is excluded and stays in that entry)
- `notes/hunt-structure.md`, "Overlays should be one module each"
- `notes/hunt-cleanup.md`, "The client shell has no item-level visibility pass"

The entries are named by title here, not by their hunt IDs:
`notes/hunt-structure.md` rule 5 forbids writing those IDs into other
documents.

Sibling spec, written at the same time: `notes/spec-shell-requests.md` (called
D below). D owns request identity. The last section lists what this spec
assumes of D and what it offers D. The two specs land in one interleaved
order, given under "Combined order with D" in the migration section and
recorded identically in D.

All paths below are relative to `crates/shepr-client/src/` unless they start
with `crates/`, `notes/` or `reference/`.

## Contracts inventoried

`docs/` does not exist. `reference/` holds only
`reference/technical-implementation-spec.md`. AGENTS.md is the remaining
contract. It says nothing about the client shell's internals; its "Render is
pure" principle names only the server's `compute_surface_for`. This spec
extends that principle to the client shell, so landing 3 edits that AGENTS.md
bullet (the exact text is in landing 3). No other contract changes. The wire,
the config files and the CLI are untouched.

## Stopping rule

In scope: everything under `shell/`, plus the one-line call sites in
`state.rs`, `client_loop.rs` and `shell_runtime.rs` that name a moved field or
function.

Out of scope, and not touched beyond renames forced by moves:

- Request identity: `shell/ledger.rs` (`Ledger`, `Work`, `answered`,
  `dropped`), `CopyPipeline::awaiting`, `ScrollFlight::request` in
  `shell/input/scroll_lanes.rs`, `ClientWordSelection::pending`,
  `PendingWorkspaceHighlight::request_id`,
  `ClientRenameTarget::NewWorkspace::label_lookup`, and the endpoint layer's
  request ids. All of that is D's.
- The endpoint move protocol (`endpoint/`, `shell_runtime.rs`, `dispatch.rs`,
  `reconcile.rs`). They reach `ClientShellState::endpoints` and
  `Endpoints::choice` directly. That stays `pub(crate)`, and the visibility
  pass leaves it alone.
- `ClientState` in `state.rs` (blit encoder, dirty tracking, frame writes).
- The `shepr-remote` half of the state-split entry.
- The findings listed at the end as out of scope.

## Survey: the ground as it stands

### The shell struct

`ClientShellState` (`shell/state.rs`) has 43 fields. Grouped by what they
mean:

- Endpoints: `endpoints: Endpoints` (entries and `EndpointChoice`),
  `collapsed_endpoints`, `snapshot`, `active_snapshot_generation`,
  `active_boot_key`, `previous_pane_id`, plus `agent_panel_model` and
  `navigator_index`, the two models derived from them.
- Presentation: `surfaces: PaneSurfaces`, `hits: ShellHitMap`,
  `last_composition`, `last_composed_size`, `last_composed_at`.
- Sidebar: `chrome: ChromeLayout`, `agent_panel_sort_chrome`,
  `workspace_scroll`, `agent_scroll`, `reveal_focused_workspace`,
  `reveal_navigation_workspace`, `pending_agent_reveal`.
- Mode: `mode: ClientShellMode`, `navigate_workspace_id`.
- Copy: `copy_mode: Option<ClientCopyModeState>`, `copy_pipeline:
  CopyPipeline`.
- Pointer: `mouse_selection`, `chrome_drag`, `workspace_press`,
  `pane_mouse_gesture`, `last_sidebar_divider_click`, `host_mouse_pixels`.
- Overlay: `overlay: Option<ClientShellOverlay>`.
- Requests (D): `ledger`, `scroll_lanes`, `pending_workspace_highlight`.
- Rest: `now`, `config`, `machine_diagnostics`, `input_leases`,
  `host_reports_all_keys`, `notices`, `outer_focused`, `host_background`,
  `endpoint_error`.

`state.rs` also defines the hit types (`ShellHitMap`, `PaneHit`,
`PaneSplitHit`, `AgentHit`, `WorkspaceHit`), every overlay's state type, the
copy-mode types, `MouseSelection`, the pointer gesture types, the outcome
vocabulary (`Repaint`, `ClientShellInput`, `ClientShellAction`,
`ClientShellRequest`) and the three cross-domain transitions.

### `compose` mutates

`ClientShellState::compose(&mut self, cols, rows)`
(`shell/presentation/composition.rs`) is called by `ClientState::present_pending`
in `state.rs`. While it draws, it writes:

- `reveal_navigation_workspace = true` when the size differs from
  `last_composed_size` in Navigate mode;
- `last_composed_size`, even on the early `None` return taken while the
  presented surface is unpaired;
- `hits`, wholesale from `render::render_shell`, then `hits.panes`,
  `hits.pane_splits` and `hits.notification_toast`, then eleven overlay fields
  copied one by one out of `OverlayRender`;
- through `render::ShellRenderState`: `&mut workspace_scroll`,
  `&mut agent_scroll`, `&mut reveal_focused_workspace`,
  `&mut reveal_navigation_workspace`;
- `Help.scroll`, clamped to `hits.help_max_scroll` after the draw. When Help
  does not fit, that max is 0, so a too-small window resets Help's scroll;
- `notices.drawn(now)`, `last_composition`, and, in `record_composed_frame`,
  `last_composed_at` and `mouse_selection.repaint_deadline`.

### The sidebar renderers mutate

`sidebar/endpoint_sidebar.rs`:

- `render_collapsed` takes both reveal flags with `std::mem::take`
  unconditionally, then applies them only when the workspace area has height.
  A reveal requested while the area is empty is lost.
- `render_expanded` takes the flags only when the body is non-empty, but
  writes `*state.workspace_scroll = metrics.start()`. With an empty body,
  `list_scroll_metrics` returns start 0, so a frame with an empty body resets
  the workspace scroll to 0.

`sidebar/agent_sidebar.rs`:

- `render_agent_list` sets `*agent_scroll = 0` when the body is empty, and
  otherwise writes back the clamped start. Shrinking the terminal for one
  frame loses the aggregate agent scroll. It is generic over the row type and
  takes an `empty_message`, but its only caller,
  `endpoint_agents::render_expanded`, passes `None`.

### The navigator's scroll is computed while drawing

`overlays/mod.rs::render_navigator_overlay` computes the effective scroll as
`n.scroll.max(selected - (h - 1)).min(selected).min(max)` and never stores
it. `navigator.scroll` changes only through `scroll_navigator_to` (scrollbar)
and the Top command. The bug this causes: move the selection down past the
viewport with the keyboard. The displayed start follows the selection, but the
stored start stays 0. Then press Up once. The displayed start becomes
`selected - h + 1` again, so the viewport scrolls up with the selection pinned
to the bottom row, instead of the selection moving up inside a still
viewport. `scroll_navigator_to` (`overlays/overlay_input.rs`) clamps
differently: it takes `viewport_rows.max(1)`, clamps against
`rows.len() - viewport_rows`, and moves the selection into the viewport.

### Input reads what render produced

The readers of `hits`, all `impl ClientShellState`:

- `input/mouse.rs`: almost every branch. The scroll wheel clamps to
  `hits.workspace_max_scroll` and `hits.agent_max_scroll`; the scrollbar
  drags read `*_scroll_metrics` and `*_scrollbar`; the overlay blocks read
  `global_menu_rows`, `context_menu_rows`, `help_*`, `navigator_*` and
  `overlay_primary/clear/cancel`; the pane and split gestures read
  `panes` and `pane_splits`; `workspace_drop_target_at` reads
  `workspace_body`, `workspaces` and `new_workspace`.
- `overlays/overlay_input.rs`: `help_max_scroll`, three times.
- `overlays/global_menu.rs`: `toggle_global_menu` captures
  `hits.global_launcher` as the menu anchor.
- `overlays/machine_diagnostics.rs`: `notification_toast`, `machines`.
- `navigation/endpoint_navigation.rs`: `workspaces`, `machines`,
  `agent_hits`, `agent_body.height`.
- `navigation/actions.rs`: `request_selection_copy` reads `panes` for the
  linewise width. `endpoint_command_for_action` reads `agent_hits` and
  `agent_body.height`, then calls `reveal_endpoint_agent` and
  `reveal_workspace`. That is a translator with scroll side effects.
- `endpoints.rs`: `activate_endpoint_projection` reads `agent_body.height`.
- `state.rs`: `reveal_workspace` reads `workspaces` and
  `workspace_body.height`.
- `input/copy_mode.rs`: `copy_hit` reads `panes`;
  `mode_bar_covers_copy_pane` reads `last_composed_size`.
- `presentation/surface_patch.rs`: `fast_path_blocker` reads `panes` and
  `last_composition`, `apply_tagged_pane_surface_patch` reads
  `last_composed_size` and rewrites entries of `hits.panes` on the fast path.

Reading the last frame's geometry in input is correct in itself: a click aims
at what is on screen. The defects are that render produces that geometry as a
side output while it draws, and that the reveal computations use the last
frame's body heights. Both `reveal_workspace` and `reveal_endpoint_agent`
compute a scroll start from `hits.*_body.height` immediately. So a reveal in
the same input batch as a sidebar toggle uses the old layout, and
`reveal_endpoint_agent` returns early when that height is 0, which drops the
reveal.

### The save and restore around activation

`activate_endpoint_projection` (`shell/endpoints.rs`) saves `agent_scroll`,
calls `apply_active_snapshot`, and restores it when switching endpoints.
`apply_active_snapshot` sees a changed boot key and calls
`reset_endpoint_projection`, and that zeroes `agent_scroll`. The aggregate
agent list belongs to the client, so a switch must keep it, while a reboot of
the same endpoint resets it (pinned by
`same_machine_reboot_still_resets_agent_scroll`). The reset cannot tell a
switch from a reboot, so the caller patches it up afterwards.

### The three cross-domain transitions

`apply_active_snapshot` (`state.rs`) does all of the following, in this
order:

1. the older-revision guard;
2. the surfaces' generation change;
3. setting the generation and boot key;
4. a reboot reset that keeps the Navigate preview and the boot's surfaces;
5. `previous_pane_id`;
6. the focus reveal flag;
7. word-gesture and selection focus-loss rules;
8. copy session reconciliation, which can end, park or re-activate the
   session, change the mode and clear or project the selection;
9. the Navigate preview fill;
10. `scroll_lanes.retain_panes`;
11. storing the snapshot;
12. `reconcile_pending_workspace_highlight`;
13. `pair_surfaces`.

`reset_endpoint_projection` resets about twenty fields one statement at a
time, and clears `mouse_selection` twice. It calls `drop_all_requests(DropReason::Reset)` halfway through, after
the selection is cleared but before the copy session and overlay are.

`presented_surface_changed` invalidates the selection or word gesture when
its pane changed size or screen. It also marks shown scroll targets, and
refreshes the copy session's geometry, clamps its rows, prunes evicted search
matches, re-projects the selection and clears it. It needs `&mut self` while
reading the surface, so both callers (`pair_surfaces` in `state.rs`, the slow
path of `apply_tagged_pane_surface_patch`) move `self.surfaces` out with
`std::mem::take` and back.

The per-endpoint snapshot cache (`cache_endpoint_snapshot_at_generation` in
`endpoints.rs`) and `apply_active_snapshot` each spell the older-revision rule
their own way: `previous.revision > snapshot.revision` against
`snapshot.revision < current.revision`, with different guards around it.

### The overlays are spread out

Per overlay, the code lives in five places:

- the state type in `state.rs`;
- an `if matches!` block in `route_overlay_key`
  (`overlays/overlay_input.rs`);
- a block in `handle_mouse_with_accounting` (`input/mouse.rs`), with
  Rename and ConfirmClose sharing the `overlay_primary`/`overlay_clear`
  block;
- render in `overlays/mod.rs`;
- items and actions in `context_menu.rs` and `global_menu.rs`.

`render_client_overlay` returns `None` for the two menus. `compose` matches
them first and calls `render_context_menu` and `render_global_menu` itself.
Help's and the navigator's scrollbar drags are `ClientChromeDrag` variants
(`HelpScrollbar`, `NavigatorScrollbar`) handled by the generic chrome-drag
code. `ClientRenameOverlay.title` is a `&'static str` chosen by each of its
five construction sites. It always follows the target: "new workspace",
"rename workspace" or "rename pane".

`overlays/` also holds things that are not modal overlays: `notices.rs`,
`endpoint_notices.rs` (the notice card and the lifecycle banner),
`machine_diagnostics.rs`, `transient_error.rs` and `text_editor.rs`.

### Visibility

Almost every item under `shell/` is `pub(crate)` or `pub(in crate::shell)`.
The exceptions are `overlays/fixed_keys.rs` (eight narrower items) and
`presentation/selection_render.rs` (one). So the cleanup entry's "none
narrower" is slightly stale, but its point stands.

### Hunt claims checked

Every claim in the render-mutation and overlay entries holds as written. For
the state-split entry:

- "Production and tests build `ClientCopyModeState` field by field in
  several places": production builds it once, in `enter_copy_mode`. Tests
  build it four times: in `input/mod.rs`'s tests, `surface_patch.rs`'s tests,
  `composition.rs`'s tests and `shell/tests/copy.rs`.
- "`ClientState::reported_geometry` repeats the size": only partly true.
  `last_composed_size` means the size of the frame on screen, and it
  legitimately differs from the host's reported size between a resize and the
  next composition. The real defect is that the unpaired early return writes
  it when nothing was drawn. The target replaces it with the drawn view's
  size.

## Target

### Three-phase compose

`compose` keeps its signature, so that `ClientState::present_pending` and
about 200 test call sites do not change:

```rust
impl ClientShellState {
    pub(crate) fn compose(&mut self, cols: u16, rows: u16) -> Option<FrameData> {
        if !self.presentation.can_draw(self.endpoints.active.snapshot().is_some()) {
            return None; // presented surface held unpaired: the last frame stays
        }
        let resolved = crate::shell::view::resolve::resolve_frame(self, cols, rows);
        let drawn = crate::shell::view::draw::draw_frame(self, &resolved.view)?;
        Some(self.commit_frame(resolved, drawn))
    }
}
```

- `resolve_frame(&ClientShellState, cols, rows) -> ResolvedFrame` is a pure
  function. It lays out every element and resolves every scroll position
  (clamps and reveals). It returns the view model and the scroll positions to
  store.
- `draw_frame(&ClientShellState, &ShellView) -> Option<DrawnFrame>` is a pure
  function. It draws only what the view says, reads shared state for content
  (labels, palette, cells), and returns the frame and the composition effects.
  `None` only when `Canvas::from_buffer` refuses, which keeps the last frame,
  as today.
- `commit_frame(&mut self, ResolvedFrame, DrawnFrame) -> FrameData` is the one
  place a composition writes shell state.

`can_draw(has_snapshot)` is the current unpaired test, moved: `!(has_snapshot
&& surfaces.presented().is_some() && !surfaces.is_paired())`.

```rust
pub(in crate::shell) struct ResolvedFrame {
    pub(in crate::shell) view: ShellView,
    pub(in crate::shell) sidebar: SidebarScrollResolution,
    pub(in crate::shell) overlay_scroll: Option<OverlayScroll>,
}

pub(in crate::shell) enum OverlayScroll {
    Navigator(usize),
    Help(usize),
}

pub(in crate::shell) struct DrawnFrame {
    pub(in crate::shell) frame: FrameData,
    pub(in crate::shell) effects: LastComposition,
}

fn commit_frame(&mut self, resolved: ResolvedFrame, drawn: DrawnFrame) -> FrameData {
    self.sidebar_scroll.commit(resolved.sidebar);
    if let (Some(scroll), Some(overlay)) = (resolved.overlay_scroll, self.overlay.as_mut()) {
        overlay.commit_scroll(scroll);
    }
    self.notices.drawn(self.now);
    self.mouse_selection.frame_drawn(); // repaint_deadline = None
    self.presentation.commit(resolved.view, drawn.effects, self.now);
    drawn.frame
}
```

Consequences, each intended:

- Nothing changes when the canvas refuses: reveal requests survive, and no
  notice lifetime starts for a frame that was never produced.
- The unpaired early return records nothing. The view's size is the size of a
  drawn frame. The size-change reveal in Navigate mode is now computed by
  `resolve_frame`, which compares `(cols, rows)` with
  `presentation.view().map(|v| v.size)`, so it no longer needs a flag write.
- A Help overlay that does not fit keeps its scroll, because only a laid-out
  Help view yields `OverlayScroll::Help`.

### `ShellView`: the view model of the frame on screen

New module `shell/view/mod.rs`:

```rust
/// What the last composed frame shows and where. Produced by `resolve_frame`,
/// drawn by `draw_frame`, kept by `Presentation` until the next composition.
/// Input aims at it because it describes the screen.
pub(in crate::shell) struct ShellView {
    pub(in crate::shell) size: (u16, u16),
    /// The layout this frame was drawn with (`ClientShellState::layout` at `size`).
    pub(in crate::shell) layout: ClientShellLayout,
    /// The pane cells were drawn (a snapshot and a paired surface existed).
    pub(in crate::shell) has_surface: bool,
    pub(in crate::shell) placeholder: Option<Placeholder>,
    pub(in crate::shell) sidebar: SidebarView,
    pub(in crate::shell) panes: Vec<PaneHit>,
    pub(in crate::shell) splits: Vec<PaneSplitHit>,
    pub(in crate::shell) lifecycle: Option<LifecycleBanner>,
    pub(in crate::shell) notice: Option<NoticeCard>,
    pub(in crate::shell) mode_bar_area: Rect,
    /// The open overlay as laid out; `None` when no overlay is open or it does not fit.
    pub(in crate::shell) overlay: Option<OverlayView>,
    /// `ui.mouse_capture`: when false, the sidebar chrome hits below are inert.
    chrome_armed: bool,
}

pub(in crate::shell) struct Placeholder { pub(in crate::shell) area: Rect, pub(in crate::shell) message: String }
pub(in crate::shell) struct LifecycleBanner { pub(in crate::shell) rect: Rect, pub(in crate::shell) label: String, pub(in crate::shell) status: ClientEndpointStatus }
pub(in crate::shell) struct NoticeCard { pub(in crate::shell) rect: Rect, pub(in crate::shell) top_offset: u16 }
```

`PaneHit`, `PaneSplitHit`, `WorkspaceHit`, `AgentHit` move here from
`state.rs`, unchanged. `MachineHit` moves here from `endpoints.rs`, unchanged.

The hit accessors replace `ShellHitMap`'s fields one for one. The
mouse-capture gate that `render::render_shell` applies today moves into these
accessors, with exactly the same membership:

| `ShellHitMap` field | `ShellView` accessor | gated by `chrome_armed` |
|---|---|---|
| `machines` | `machines() -> impl Iterator<Item = &MachineHit>` | yes |
| `workspaces` | `workspaces() -> impl Iterator<Item = &WorkspaceHit>` | yes |
| `workspace_body`, `workspace_scroll_metrics`, `workspace_max_scroll` | `workspace_list() -> Option<&ListView<..>>` (body, scroll) | no |
| `workspace_scrollbar` | `workspace_scrollbar() -> Option<Rect>` | yes |
| `agent_hits` | `agents() -> impl Iterator<Item = &AgentHit>` | yes |
| `agent_body`, `agent_scroll_metrics`, `agent_max_scroll` | `agent_list() -> Option<&ListView<..>>` | no |
| `agent_scrollbar` | `agent_scrollbar() -> Option<Rect>` | yes |
| `agent_sort_toggle` | `agent_sort_toggle() -> Option<Rect>` | yes (already gated at render) |
| `sidebar_divider`, `sidebar_section_divider` | `sidebar_divider()`, `section_divider()` | yes |
| `sidebar_toggle` | `sidebar_toggle() -> Option<Rect>` | no |
| `new_workspace`, `global_launcher` | `new_workspace()`, `global_launcher()` | only drawn when armed |
| `notification_toast` | `notice.map(|n| n.rect)` | no |
| `panes` | `panes` (field) | no |
| `pane_splits` | `splits` (field; empty when unarmed or overflowing, as today) | built empty |
| `help_*`, `navigator_*`, `overlay_*`, `*_menu_rows` | `overlay: Option<OverlayView>` | no |

`ClientShellState` gets two read helpers used by every input site:
`fn view(&self) -> Option<&ShellView>` and `fn pane_hits(&self) ->
&[PaneHit]`, which returns an empty slice with no view. Input reading the
committed view is the design, not the defect. The view is a deliberate
product of the resolution pass, and it describes the screen the user aims at.

### `ListView` and the list scroll rule

`shell/navigation/scroll.rs` moves to `shell/view/list.rs`, and gains:

```rust
pub(in crate::shell) struct ListView<S> {
    pub(in crate::shell) body: Rect,
    pub(in crate::shell) scroll: shepr_term::scroll::ListScroll,
    pub(in crate::shell) scrollbar: Option<Rect>,
    /// The rows drawn, top to bottom.
    pub(in crate::shell) slots: Vec<S>,
}

/// Resolves a list's start. An empty body leaves the stored start alone (there is
/// nothing to clamp against) and leaves every reveal pending.
pub(in crate::shell) fn resolve_list(
    row_heights: &[u16],
    gaps_after: &[u16],
    body: Rect,
    stored_start: usize,
    reveal: Option<usize>,
) -> ListResolution;

pub(in crate::shell) struct ListResolution {
    pub(in crate::shell) scroll: ListScroll,
    /// `None` when the body is empty: keep the stored start.
    pub(in crate::shell) start: Option<usize>,
    /// The reveal was applied (or found no target) and is consumed.
    pub(in crate::shell) reveal_consumed: bool,
}
```

`list_scroll_metrics`, `list_scroll_start_to_reveal`, `render_list_scrollbar`,
`render_scrollbar_buffer` and `scroll_track` move with it, unchanged. The rule
"an empty body neither clamps nor consumes" applies to all four lists
(expanded workspaces, collapsed workspaces, agents, navigator). It fixes the
two resets to 0 and the collapsed reveal loss.

### Sidebar scroll and reveal requests

New `shell/sidebar/scroll.rs`. It replaces five fields: `workspace_scroll`,
`agent_scroll`, `reveal_focused_workspace`, `reveal_navigation_workspace` and
`pending_agent_reveal`.

```rust
pub(in crate::shell) struct SidebarScroll {
    workspaces: usize,
    agents: usize,
    workspace_reveal: WorkspaceReveal,
    agent_reveal: Option<Location>,
    /// An agent on another endpoint, revealed once that endpoint is activated.
    agent_reveal_after_activation: Option<Location>,
}

/// Pending workspace reveals. One frame with a non-empty workspace body consumes all
/// of them; the first with a target in the list wins, in field order.
#[derive(Clone, Default)]
pub(in crate::shell) struct WorkspaceReveal {
    /// The Navigate preview, or the pending focus highlight outside Navigate.
    selected: bool,
    /// A named workspace (keyboard workspace switching).
    explicit: Option<Location>,
    /// The presented endpoint's focused workspace.
    focused: bool,
}

impl SidebarScroll {
    pub(in crate::shell) fn new() -> Self; // focused reveal requested, as today
    pub(in crate::shell) fn workspace_start(&self) -> usize;
    pub(in crate::shell) fn agent_start(&self) -> usize;
    pub(in crate::shell) fn workspace_reveal(&self) -> &WorkspaceReveal;
    pub(in crate::shell) fn agent_reveal(&self) -> Option<&Location>;
    /// Scrollbar and wheel input; each returns whether the start changed.
    pub(in crate::shell) fn scroll_workspaces_to(&mut self, start: usize) -> bool;
    pub(in crate::shell) fn scroll_agents_to(&mut self, start: usize) -> bool;
    pub(in crate::shell) fn reveal_selected_workspace(&mut self);
    pub(in crate::shell) fn reveal_workspace(&mut self, location: Location);
    pub(in crate::shell) fn reveal_focused_workspace(&mut self);
    pub(in crate::shell) fn reveal_agent(&mut self, location: Location);
    pub(in crate::shell) fn reveal_agent_after_activation(&mut self, location: Location);
    pub(in crate::shell) fn cancel_agent_reveal_after_activation(&mut self);
    /// Promotes the deferred agent reveal when `endpoint` is the one it waited for.
    pub(in crate::shell) fn endpoint_activated(&mut self, endpoint: &ClientEndpointId);
    /// Endpoint switch or reboot: start 0, focused reveal requested, other workspace reveals dropped.
    pub(in crate::shell) fn reset_workspaces(&mut self);
    /// Same-endpoint reboot and the sort toggle: start 0.
    pub(in crate::shell) fn reset_agents(&mut self);
    pub(in crate::shell) fn commit(&mut self, resolution: SidebarScrollResolution);
}

pub(in crate::shell) struct SidebarScrollResolution {
    pub(in crate::shell) workspaces: Option<usize>,
    pub(in crate::shell) agents: Option<usize>,
    pub(in crate::shell) workspace_reveal_consumed: bool,
    pub(in crate::shell) agent_reveal_consumed: bool,
}
```

The order selected, then explicit, then focused keeps today's rule that a
pending navigation reveal wins over a focus reveal and both are consumed
together. An explicit reveal is new for multi-endpoint shells (see the
behaviour changes).

Call sites, current to target:

| current | target |
|---|---|
| `reveal_navigation_workspace = true` (`actions.rs` ToggleSidebar, WorkspacePicker; `overlay_input.rs` confirm-close Esc; `workspace_navigation.rs` `move_navigate_workspace`) | `self.sidebar_scroll.reveal_selected_workspace()` |
| `compose`'s size-change write in Navigate | implied in `resolve_frame` (no write) |
| `reveal_focused_workspace = true` in `apply_active_snapshot` and the reset | `reveal_focused_workspace()` / `reset_workspaces()` |
| `ClientShellState::reveal_workspace` (`state.rs`), both paths | `ClientShellState::request_workspace_reveal(id)`: removes the presented endpoint from `endpoints.collapsed`, then `reveal_workspace(Location::workspace(presented, id))` |
| `move_navigate_workspace`'s single-endpoint `reveal_workspace` call plus its conditional flag | uncollapse the target endpoint, then `reveal_selected_workspace()` unconditionally |
| `reveal_endpoint_agent(endpoint, pane, body_height)` (`endpoint_agents.rs`) | deleted; `reveal_agent(Location::pane(endpoint, pane))` |
| `pending_agent_reveal = Some(target)` (`endpoint_navigation.rs`) | `reveal_agent_after_activation(target)` |
| `pending_agent_reveal = None` in `activate_endpoint`, `focus_or_activate` | `cancel_agent_reveal_after_activation()` |
| `pending_agent_reveal.take_if(..)` in `activate_endpoint_projection` | `endpoint_activated(endpoint_id)` after the snapshot is applied |
| mouse wheel and scrollbar writes of `workspace_scroll`, `agent_scroll` | `scroll_workspaces_to` / `scroll_agents_to` with `view.workspace_list()` / `agent_list()` max |
| sort toggle `agent_scroll = 0` | `reset_agents()` |

`endpoint_command_for_action` (`navigation/actions.rs`) becomes `&self` and
returns the command with the reveal it implies. The caller requests the
reveal:

```rust
pub(in crate::shell) struct ActionCommand {
    pub(in crate::shell) command: EndpointCommand,
    pub(in crate::shell) reveal: Option<ActionReveal>,
}
pub(in crate::shell) enum ActionReveal {
    Workspace(shepr_protocol::WorkspaceId),
    Agent(Location),
}
pub(in crate::shell) fn endpoint_command_for_action(&self, action: KeybindAction) -> Option<ActionCommand>;
```

`record_binding` pushes the command and then applies the reveal, even when the
push finds the endpoint unusable. That is today's order, and it is harmless.
`cycle_pane` ignores the reveal, because cycling never carries one.

The visibility check that `endpoint_command_for_action` does today ("is the
target in `hits.agent_hits`?") goes away. A reveal of a visible row is a no-op
by construction of `list_scroll_start_to_reveal`.
`agent_navigation_keeps_scroll_when_target_is_visible` pins that.

### Sidebar layout and drawing

New `shell/sidebar/layout.rs`:

```rust
pub(in crate::shell) enum SidebarView {
    Hidden,
    Collapsed(CollapsedSidebarView),
    Expanded(ExpandedSidebarView),
}

pub(in crate::shell) struct ExpandedSidebarView {
    pub(in crate::shell) area: Rect,
    pub(in crate::shell) divider: Rect,
    pub(in crate::shell) section_divider: Rect,
    pub(in crate::shell) toggle: Rect,
    pub(in crate::shell) workspace_area: Rect,
    pub(in crate::shell) workspaces: ListView<ExpandedSlot>,
    pub(in crate::shell) drop_indicator: Option<u16>,
    /// `None` without mouse capture (not drawn).
    pub(in crate::shell) footer: Option<SidebarFooter>,
    pub(in crate::shell) agents: Option<AgentPanelView>,
}
pub(in crate::shell) enum ExpandedSlot {
    Machine { hit: MachineHit, endpoint: usize },
    Workspace { hit: WorkspaceHit, nested: Rect, endpoint: usize, entry: usize },
}
pub(in crate::shell) struct SidebarFooter { pub(in crate::shell) new_workspace: Rect, pub(in crate::shell) global_launcher: Rect, pub(in crate::shell) label: String }
pub(in crate::shell) struct AgentPanelView {
    pub(in crate::shell) area: Rect,
    /// `None` when the panel is too short for its header row (today's `false` from the header).
    pub(in crate::shell) sort_toggle: Option<Rect>,
    pub(in crate::shell) list: Option<ListView<AgentSlot>>,
}
pub(in crate::shell) struct AgentSlot { pub(in crate::shell) hit: AgentHit, pub(in crate::shell) row: usize } // index into AgentPanelModel::rows

pub(in crate::shell) struct CollapsedSidebarView {
    pub(in crate::shell) area: Rect,
    pub(in crate::shell) workspaces: ListView<CollapsedSlot>, // no scrollbar, as today
    pub(in crate::shell) divider_y: Option<u16>,
    pub(in crate::shell) agents: Vec<AgentSlot>,
    pub(in crate::shell) toggle: Rect,
}
pub(in crate::shell) enum CollapsedSlot {
    Machine { hit: MachineHit, endpoint: usize, machine_number: usize },
    Workspace { hit: WorkspaceHit, endpoint: usize, entry: usize },
}

/// Everything sidebar layout and drawing read, borrowed from the shell.
pub(in crate::shell) struct SidebarInputs<'a> {
    pub(in crate::shell) endpoints: &'a [ClientShellEndpoint],
    pub(in crate::shell) presented: &'a ClientEndpointId,
    pub(in crate::shell) collapsed: &'a HashSet<ClientEndpointId>,
    pub(in crate::shell) model: &'a AgentPanelModel,
    pub(in crate::shell) config: &'a ClientShellConfig,
    pub(in crate::shell) machine_diagnostics: &'a MachineDiagnostics,
    pub(in crate::shell) active_snapshot: Option<&'a ClientShellSnapshot>,
    pub(in crate::shell) selected: Option<&'a PinnedLocation>,
    pub(in crate::shell) section_split: SectionSplit,
    pub(in crate::shell) dragged_workspace: Option<&'a shepr_protocol::WorkspaceId>,
    pub(in crate::shell) drop_indicator_row: Option<u16>,
}

pub(in crate::shell) fn resolve_sidebar(
    area: Rect,
    form: SidebarForm, // Hidden, Collapsed or Expanded, decided by the caller as today
    inputs: &SidebarInputs<'_>,
    scroll: &SidebarScroll,
    implied_selected_reveal: bool,
) -> (SidebarView, SidebarScrollResolution);
```

The row lists, heights and gaps that `render_collapsed` and `render_expanded`
compute today move into `resolve_sidebar` unchanged, including the
`Row::{Endpoint, Workspace}` list, `workspace_rows(..).len().max(1)`, and the
`row_gap` within one machine. Workspace reveal targets resolve to row indices
in the flattened list:

- selected: `inputs.selected` matched by `matches_workspace`;
- explicit: endpoint and workspace id;
- focused: presented endpoint and `active_snapshot.focused_workspace_id`.

Agent reveal targets resolve to the index in `model.rows` whose endpoint and
pane match.

Drawing keeps its files: `endpoint_sidebar.rs` and `endpoint_agents.rs`
become `draw_collapsed`, `draw_expanded` and `draw_agent_panel`, each
`(buffer: &mut Buffer, view: &..View, inputs: &SidebarInputs<'_>)`.
`agent_sidebar.rs` keeps `render_agent_row`, `AgentRow`, `agent_rows` and the
header drawing. The generic `render_agent_list` and its `empty_message` are
deleted. `render::ShellRenderState` and `render::render_shell` are deleted.

### Overlay views

Each overlay's render splits into a layout half, which returns the overlay's
view or `None` when it does not fit, and an infallible draw half:

```rust
pub(in crate::shell) enum OverlayView {
    Rename(DialogView),
    ConfirmClose(DialogView),
    Help(HelpView),
    Navigator(NavigatorView),
    ContextMenu(MenuView),
    GlobalMenu(MenuView),
}

pub(in crate::shell) struct DialogView {
    pub(in crate::shell) popup: Rect,
    pub(in crate::shell) inner: Rect,
    pub(in crate::shell) input: Option<Rect>, // Rename only
    pub(in crate::shell) primary: Rect,
    pub(in crate::shell) clear: Option<Rect>, // Rename only
    pub(in crate::shell) cancel: Rect,
}
pub(in crate::shell) struct HelpView {
    pub(in crate::shell) popup: Rect,
    pub(in crate::shell) inner: Rect,
    pub(in crate::shell) close: Rect,
    pub(in crate::shell) text_area: Rect,
    pub(in crate::shell) scroll: ListScroll,
    pub(in crate::shell) scrollbar: Option<Rect>,
}
pub(in crate::shell) struct NavigatorView {
    pub(in crate::shell) popup: Rect,
    pub(in crate::shell) inner: Rect,
    pub(in crate::shell) search: Rect,
    pub(in crate::shell) body: Rect,
    pub(in crate::shell) rows: Vec<ClientNavigatorRow>, // computed once per frame, as today
    pub(in crate::shell) selected: usize,
    pub(in crate::shell) list: ListView<NavigatorSlot>,
}
pub(in crate::shell) struct NavigatorSlot { pub(in crate::shell) rect: Rect, pub(in crate::shell) row: usize }
pub(in crate::shell) struct MenuView {
    pub(in crate::shell) rect: Rect,
    pub(in crate::shell) inner: Rect,
    pub(in crate::shell) rows: Vec<(Rect, usize)>,
}

/// What drawing an overlay committed beyond its cells.
pub(in crate::shell) struct OverlayPaint {
    pub(in crate::shell) opaque: Vec<Rect>,
    pub(in crate::shell) backdrop: bool,
    pub(in crate::shell) cursor: Option<shepr_protocol::CursorState>,
}
```

- Navigator layout computes the effective start with today's formula, and
  returns `OverlayScroll::Navigator(start)`. `commit_frame` stores it in
  `navigator.scroll`. That fixes the snap. `scroll_navigator_to` loses its own
  clamp: the scrollbar input passes `view.list.scroll.max_start()` and
  `viewport_rows()` from the drawn `NavigatorView`, and
  `NavigatorOverlay::scroll_to(start, scroll: ListScroll, rows)` clamps the
  start to `scroll.max_start()`. It moves the selection into `[start, start
  + viewport_rows - 1]`, with no `.max(1)` and no second formula.
- Help layout returns `OverlayScroll::Help(state.scroll.min(max))`. Help's
  key and wheel input clamp to `HelpView::scroll.max_start()` when a Help view
  is on screen. Without one they leave the value unclamped, and the next
  resolution clamps it.
- `ClientGlobalMenuOverlay::launcher` is still captured at open time, from
  `view.global_launcher()`.

`OverlayRender`, `render_client_overlay`, `render_global_menu`,
`render_context_menu` and the overlay half of `ShellHitMap` are deleted. All
six overlays go through one `layout_overlay` and one `draw_overlay`, with no
special case for the menus.

Overlay views live in `ShellView`, not inside the overlay state. A view
describes the frame on screen, so a click between an overlay change and the
next frame aims at what the user sees. Foreign fields cannot exist, because
each `OverlayView` variant carries only its own overlay's geometry. Input
pairs `self.overlay` with `view.overlay` by kind (`Overlay::kind()` against
`OverlayView::kind()`). A mismatch, or no view, means the overlay is not on
screen, and a press is handled as "outside the popup". That matches today's
empty `Rect::default()` hits.

### State domains

`ClientShellState` after the split:

```rust
pub struct ClientShellState {
    pub(crate) now: std::time::Instant,
    config: ClientShellConfig,
    pub(crate) endpoints: Endpoints, // entries, choice, collapsed, active projection, derived models
    presentation: Presentation,
    chrome: ChromeLayout,
    agent_panel_sort_chrome: Chrome<shepr_config::AgentPanelSortConfig>,
    sidebar_scroll: SidebarScroll,
    mode: ModeState,
    copy: Option<CopySession>,
    mouse_selection: MouseSelection,
    pointer: Pointer,
    overlay: Option<Overlay>,
    ledger: Ledger,                 // D
    scroll_lanes: ScrollLanes,      // D
    pending_workspace_highlight: Option<PendingWorkspaceHighlight>, // D's request id inside
    input_leases: ClientInputLeases,
    host_reports_all_keys: bool,
    notices: Notices,
    endpoint_error: TransientError,
    machine_diagnostics: MachineDiagnostics,
    outer_focused: Option<bool>,
    host_background: Option<shepr_term::host::RgbColor>,
}
```

Every non-`pub(crate)` field above is `pub(in crate::shell)` until the
visibility pass narrows it.

`Endpoints` (`shell/endpoints.rs`) absorbs `collapsed_endpoints` as
`collapsed`, `agent_panel_model`, `navigator_index`, and a new
`active: ActiveProjection`:

```rust
pub(in crate::shell) struct ActiveProjection {
    snapshot: Option<Arc<ClientShellSnapshot>>,
    generation: Option<u64>,
    boot_key: Option<ClientEndpointBootKey>,
    previous_pane_id: Option<shepr_protocol::PublicPaneId>,
}

pub(in crate::shell) struct ProjectionChange {
    pub(in crate::shell) generation_changed: bool,
    pub(in crate::shell) focused_workspace_changed: bool,
    pub(in crate::shell) step: ProjectionStep,
}
pub(in crate::shell) enum ProjectionStep {
    /// The first snapshot this shell presents.
    First,
    /// Same endpoint and boot.
    Advanced,
    /// The presented endpoint changed, or its server rebooted (ids can be reused).
    Replaced(ProjectionReset),
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::shell) enum ProjectionReset {
    EndpointSwitched,
    Rebooted,
}

impl ActiveProjection {
    pub(in crate::shell) fn snapshot(&self) -> Option<&ClientShellSnapshot>;
    pub(in crate::shell) fn shared_snapshot(&self) -> Option<&Arc<ClientShellSnapshot>>;
    pub(in crate::shell) fn generation(&self) -> Option<u64>;
    pub(in crate::shell) fn previous_pane_id(&self) -> Option<&shepr_protocol::PublicPaneId>;
    /// Accepts `snapshot` as the presented projection, or returns `None` for an older
    /// revision of the same endpoint, boot and generation. Stores it on acceptance.
    pub(in crate::shell) fn accept(
        &mut self,
        presented: &ClientEndpointId,
        snapshot: Arc<ClientShellSnapshot>,
        generation: u64,
    ) -> Option<ProjectionChange>;
    /// For a reset: drops `previous_pane_id`.
    pub(in crate::shell) fn reset(&mut self);
}

/// The one older-revision rule, for the active projection and the per-endpoint cache.
pub(in crate::shell) fn revision_is_older(
    current: &ClientShellSnapshot,
    current_generation: u64,
    next: &ClientShellSnapshot,
    next_generation: u64,
) -> bool {
    current_generation == next_generation
        && current.boot_id == next.boot_id
        && next.revision < current.revision
}
```

`accept` reproduces today's guard exactly. The active rule adds "same boot
key", which `revision_is_older` covers, because the boot key differs whenever
the boot id does. `Replaced(EndpointSwitched)` means the previous boot key
named another endpoint. `Replaced(Rebooted)` means the same endpoint with
another boot id. `First` is today's `self.snapshot.is_none()`.
`ClientShellState::snapshot` becomes `self.endpoints.active.snapshot()` at
every read site, and `active_snapshot_generation` becomes
`self.endpoints.active.generation()`.

`Presentation` (`shell/presentation/mod.rs`, moved out of the loose fields):

```rust
pub(in crate::shell) struct Presentation {
    surfaces: PaneSurfaces,
    view: Option<ShellView>,
    composition: LastComposition,
    composed_at: Option<std::time::Instant>,
}
impl Presentation {
    pub(in crate::shell) fn surfaces(&self) -> &PaneSurfaces;
    pub(in crate::shell) fn surfaces_mut(&mut self) -> &mut PaneSurfaces;
    pub(in crate::shell) fn view(&self) -> Option<&ShellView>;
    /// The surface patch fast path keeps the drawn pane hits in step with the patch.
    pub(in crate::shell) fn patch_pane_hit(&mut self, hit: PaneHit);
    pub(in crate::shell) fn composition(&self) -> &LastComposition;
    pub(in crate::shell) fn composed_at(&self) -> Option<std::time::Instant>;
    pub(in crate::shell) fn can_draw(&self, has_snapshot: bool) -> bool;
    pub(in crate::shell) fn commit(&mut self, view: ShellView, composition: LastComposition, now: std::time::Instant);
    /// Drops the view and composition effects; surfaces are reset by their own rule.
    pub(in crate::shell) fn reset_view(&mut self);
}
```

`ModeState` (new `shell/mode.rs`) replaces `mode` and
`navigate_workspace_id`. The preview exists only in Navigate:

```rust
pub(in crate::shell) struct ModeState {
    kind: ClientShellMode,
    preview: Option<PinnedLocation>,
}
impl ModeState {
    pub(in crate::shell) fn kind(&self) -> ClientShellMode;
    pub(in crate::shell) fn is(&self, kind: ClientShellMode) -> bool;
    /// Any mode but Navigate; drops the preview.
    pub(in crate::shell) fn set(&mut self, kind: ClientShellMode);
    pub(in crate::shell) fn enter_navigate(&mut self, preview: Option<PinnedLocation>);
    pub(in crate::shell) fn preview(&self) -> Option<&PinnedLocation>;
    pub(in crate::shell) fn set_preview(&mut self, preview: Option<PinnedLocation>); // Navigate only
    pub(in crate::shell) fn take_preview(&mut self) -> Option<PinnedLocation>;
    /// Fills an empty Navigate preview; no-op outside Navigate or with a preview.
    pub(in crate::shell) fn fill_preview(&mut self, preview: impl FnOnce() -> Option<PinnedLocation>);
}
```

`set` with `ClientShellMode::Navigate` is a debug assertion plus
`enter_navigate(None)`. The current pair writes `self.mode = X;
self.navigate_workspace_id = None;` become `self.mode.set(X)`. Every site
listed by `grep navigate_workspace_id` collapses this way. One exception: the
reset keeps Navigate with no preview, and that is `set_preview(None)`. The
invariant "a preview outside Navigate" becomes unrepresentable.
`copy_or_terminal_mode` stays on `ClientShellState`, since it reads focus and
the copy session.

`CopySession` (new `shell/copy/mod.rs`) merges `copy_mode` and
`copy_pipeline`. A session owns its pipeline, so ending a session discards
everything queued against it. That is what every reset site does today in
two statements:

```rust
pub(in crate::shell) struct CopySession {
    pane_id: shepr_protocol::PublicPaneId,
    scroll: shepr_term::ScrollMetrics,
    geometry: (u16, u16),
    alternate_screen_active: bool,
    cursor: shepr_protocol::command::PaneTextPoint,
    entry_offset_from_bottom: usize,
    selection: Option<ClientCopySelection>,
    search: Option<ClientCopySearch>,
    /// Landing 5 moves `operation_generation: u64` here unchanged; D's landing 2,
    /// which lands after it, replaces it with this ticket.
    rows: Ticket,
    pipeline: CopyPipeline,
}

pub(in crate::shell) struct CopyEntry {
    pub(in crate::shell) pane_id: shepr_protocol::PublicPaneId,
    pub(in crate::shell) scroll: shepr_term::ScrollMetrics,
    pub(in crate::shell) geometry: (u16, u16),
    pub(in crate::shell) alternate_screen_active: bool,
    pub(in crate::shell) cursor: shepr_protocol::command::PaneTextPoint,
    /// Added by D's landing 2: `enter_copy_mode` mints it with `self.ledger.ticket()`.
    pub(in crate::shell) rows: Ticket,
}

impl CopySession {
    /// The only constructor; `entry_offset_from_bottom` is `scroll.offset_from_bottom`.
    pub(in crate::shell) fn start(entry: CopyEntry) -> Self;
    pub(in crate::shell) fn pane_id(&self) -> &shepr_protocol::PublicPaneId;
    pub(in crate::shell) fn pipeline(&self) -> &CopyPipeline;
    pub(in crate::shell) fn pipeline_mut(&mut self) -> &mut CopyPipeline;
    /// The VT selection this session's anchor and cursor project, if it selects.
    pub(in crate::shell) fn projected_selection(&self) -> Option<shepr_term::selection::Selection<shepr_protocol::PublicPaneId>>;
    // plus the existing ClientCopyModeState methods: pane_is_focused, viewport_top,
    // last_row, retained_row, offset_for_top, and accessors the copy code uses.
}
```

All field reads in `copy/keys.rs` stay as direct field access, because they
are in the same module tree (`pub(super)` fields). Tests build sessions
through `CopySession::start` and a `#[cfg(test)] fn with_search(mut self,
ClientCopySearch) -> Self` / `with_selection`. Those replace the four
field-by-field test constructions. `ClientCopyModeState` is deleted.

The `copy` field is `Option<CopySession>`. These free functions in
`shell/copy/mod.rs` take disjoint fields, so the shell can call them while it
borrows the surface:

```rust
/// Snapshot reconciliation: pane removed ends the session (and its selection);
/// focused re-activates Copy from Terminal and re-projects an absent selection;
/// unfocused parks (clears its selection, Copy becomes Terminal).
pub(in crate::shell) fn reconcile_snapshot(
    copy: &mut Option<CopySession>,
    mode: &mut ModeState,
    selection: &mut MouseSelection,
    snapshot: &ClientShellSnapshot,
);

/// A presented surface or slow-path patch: refreshes geometry, clamps rows, prunes
/// evicted matches, then re-projects or clears the selection. A resize or screen
/// switch re-issues the session's `rows` ticket from `ledger`.
pub(in crate::shell) fn surface_presented(
    copy: &mut Option<CopySession>,
    selection: &mut MouseSelection,
    lanes: &ScrollLanes,
    ledger: &mut Ledger,
    surface: &PaneSurfaceFrame,
);
```

The `ledger` parameter exists because D's landing 2 (which lands before
landing 6) turns today's `operation_generation` bump in
`presented_surface_changed` into `rows = self.ledger.ticket()`. Once that body
is a free function it has no `self`, so it takes the ledger as one more
disjoint field borrow. It only mints tickets; it never opens, answers or drops
a request.

`MouseSelection` and its satellite types (`ClientPaneClick`,
`ClientSelectionAutoscroll`, `ClientSelectionAutoscrollDirection`,
`PreviousPane`, `PaneFacts`) move from `state.rs` to new
`shell/input/selection.rs`, with three methods lifted out of
`ClientShellState`:

```rust
impl MouseSelection {
    /// The selected or word-gesture pane as `previous` showed it (was `pane_facts_before`).
    pub(in crate::shell) fn facts_in(&self, previous: Option<&PaneSurfaceFrame>) -> PreviousPane;
    /// Word-gesture and selection focus-loss rules from `apply_active_snapshot`.
    pub(in crate::shell) fn reconcile_snapshot(&mut self, snapshot: &ClientShellSnapshot);
    /// The size, screen and word-content invalidation from `presented_surface_changed`.
    pub(in crate::shell) fn surface_presented(&mut self, before: PreviousPane, surface: &PaneSurfaceFrame);
    pub(in crate::shell) fn frame_drawn(&mut self); // repaint_deadline = None
}
```

`Pointer` (new `shell/input/pointer.rs`) takes `chrome_drag`,
`workspace_press`, `pane_mouse_gesture`, `last_sidebar_divider_click` and
`host_mouse_pixels`, plus their types (`ClientChromeDrag`,
`ClientWorkspacePress`, `ClientPaneMouseGesture`, `Throttle` from
`mouse.rs`). It has public fields within the shell and one method,
`reset_for_projection(&mut self)`. That clears all fields but
`last_sidebar_divider_click`, which is what the reset does today.

### The transitions, reshaped

New `shell/transitions.rs` holds the cross-domain orchestrators. Each is a
fixed sequence of domain reactions, in the order given here. The order is
today's, with one deliberate move (requests first in the reset).

```rust
impl ClientShellState {
    pub(in crate::shell) fn apply_active_snapshot(&mut self, snapshot: Arc<ClientShellSnapshot>, generation: u64) {
        let presented = self.endpoints.presented().clone();
        let Some(change) = self.endpoints.active.accept(&presented, Arc::clone(&snapshot), generation) else {
            return;
        };
        if change.generation_changed {
            self.presentation.surfaces_mut().snapshot_generation_changed(generation);
        }
        if let ProjectionStep::Replaced(reset) = change.step {
            let preview = self.mode.is(ClientShellMode::Navigate).then(|| self.mode.take_preview()).flatten();
            let mut surfaces = std::mem::take(self.presentation.surfaces_mut());
            surfaces.reset_for_boot(&snapshot.boot_id, generation);
            self.reset_endpoint_projection(reset);
            *self.presentation.surfaces_mut() = surfaces;
            self.mode.set_preview(preview);
        }
        if change.focused_workspace_changed {
            self.sidebar_scroll.reveal_focused_workspace();
        }
        self.mouse_selection.reconcile_snapshot(&snapshot);
        crate::shell::copy::reconcile_snapshot(&mut self.copy, &mut self.mode, &mut self.mouse_selection, &snapshot);
        let focused = snapshot.focused_workspace_id;
        self.mode.fill_preview(|| focused.and_then(|id| self.navigation_target(&presented, &id)));
        self.scroll_lanes.retain_panes(|pane_id| snapshot.panes.iter().any(|pane| &pane.pane_id == pane_id));
        self.reconcile_pending_workspace_highlight();
        self.pair_surfaces();
    }
}
```

The `fill_preview` closure borrows `self` immutably while `self.mode` is
borrowed mutably. That does not compile as written, so compute the candidate
first: `let candidate = self.mode.is(Navigate) && self.mode.preview().is_none()`
and then `.then(|| ...)`, followed by `self.mode.set_preview(...)`. The
implementer writes it that way. The shape above shows only the order.

`previous_pane_id` is set inside `accept` for `Advanced` (the previous
focused pane, when focus moved), and cleared by `ActiveProjection::reset`.

`accept` stores the new snapshot before the reset runs. Today the snapshot is
stored only at the end, after the reset. That reorder is safe because nothing
the reset reaches reads the active snapshot. D's landing 3 lands before this
landing, so the rollbacks under `drop_all_requests(DropReason::Reset)` are
`Work::dropped(self, parts: Rollback<'_>)`: `Rollback` holds only `&mut` to
the copy session, the mouse selection, the scroll lanes, the overlay slot and
the highlight slot, none of which holds or reaches the active projection. So
the rule holds by type, not by audit. The interruption notice is not raised
for `Reset`. Because `ActiveProjection::reset` would
otherwise wipe what `accept` just stored, it clears only `previous_pane_id`,
never the snapshot, generation or boot key.

`reset_endpoint_projection(reset: ProjectionReset)`:

1. `self.drop_all_requests(DropReason::Reset)`, first, while every feature's
   state still exists. Today it runs after the selection is cleared and
   before the copy session and overlay are. Moving it first changes no end
   state: each rollback acts on state the steps below then reset anyway. The
   rule is "a reset drops requests before any feature is reset". With D's
   tickets, staleness no longer depends on it (a stale answer finds no held
   ticket either way, and every rollback is a no-op on absent state), so it
   is kept as the one reset order both specs describe, not as a correctness
   requirement of D's.
2. `self.presentation.reset_view()` and `*self.presentation.surfaces_mut() =
   PaneSurfaces::default()`.
3. `self.input_leases = ClientInputLeases::default()`.
4. `self.pointer.reset_for_projection()`.
5. `self.sidebar_scroll.reset_workspaces()`, and when `reset ==
   ProjectionReset::Rebooted` also `reset_agents()`.
6. `self.mouse_selection.clear()` (once).
7. `self.scroll_lanes.clear()`.
8. `self.notices.reset_endpoint()`, `self.endpoint_error.dismiss()`.
9. `self.mode.set_preview(None)`; `if self.mode.is(Copy) {
   self.mode.set(Terminal) }`.
10. `self.pending_workspace_highlight = None`, `self.overlay = None`,
    `self.endpoints.active.reset()`.
11. `self.copy = None`.

`activate_endpoint_projection` (`endpoints.rs`) loses `agent_body_height`, the
`agent_scroll` save and restore, and `pending_agent_reveal`. It keeps the
choice commit, and `if switching_endpoint {
self.presentation.surfaces_mut().clear() }`. It applies the snapshot, then
calls `self.sidebar_scroll.endpoint_activated(endpoint_id)`. The switch keeps
the agent start because `accept` reports `Replaced(EndpointSwitched)`.

`presented_surface_changed(before, surface)` becomes a free function in
`transitions.rs` over disjoint fields:

```rust
fn surface_presented(
    selection: &mut MouseSelection,
    copy: &mut Option<CopySession>,
    lanes: &mut ScrollLanes,
    ledger: &mut Ledger,
    before: PreviousPane,
    surface: &PaneSurfaceFrame,
) {
    selection.surface_presented(before, surface);
    for pane in &surface.panes {
        if let Some(scroll) = pane.scroll {
            lanes.shown(&pane.pane_id, scroll.offset_from_bottom, scroll.max_offset_from_bottom);
        }
    }
    crate::shell::copy::surface_presented(copy, selection, lanes, ledger, surface);
}
```

`pair_surfaces` and the patch slow path call it with
`self.presentation.surfaces().paired()` borrowed alongside
`&mut self.mouse_selection`, `&mut self.copy`, `&mut self.scroll_lanes` and
`&mut self.ledger`.
Both `std::mem::take(&mut self.surfaces)` dances are deleted.
`scroll_target_shown` stays for the fast path.

### Overlays as modules

New tree under `shell/overlays/`, one file per overlay. Each owns its state
type, layout, draw, key, mouse and paste handling. None calls a
`ClientShellState` method. Each returns an effect that `overlays/mod.rs`
applies:

```rust
// overlays/mod.rs
pub(in crate::shell) enum Overlay {
    Rename(rename::RenameOverlay),
    ConfirmClose(confirm_close::ConfirmCloseOverlay),
    Help(help::HelpOverlay),
    Navigator(navigator::NavigatorOverlay),
    ContextMenu(context_menu::ContextMenuOverlay),
    GlobalMenu(global_menu::GlobalMenuOverlay),
}

/// Read-only context overlays lay out, draw and handle input against.
pub(in crate::shell) struct OverlayContext<'a> {
    pub(in crate::shell) palette: &'a Palette,
    pub(in crate::shell) keybinds: &'a LiveKeybindConfig,
    pub(in crate::shell) navigator_index: &'a NavigatorIndex,
    pub(in crate::shell) presented: &'a ClientEndpointId,
    /// The drawn sidebar's menu launcher, for the global menu's toggle click.
    pub(in crate::shell) global_launcher: Option<Rect>,
}

pub(in crate::shell) enum OverlayEffect {
    /// Consumed, nothing changed.
    Unchanged,
    /// Consumed; the overlay changed and the frame repaints.
    Changed,
    /// Close the overlay and repaint.
    Close,
    /// Shell work; the overlay stays open unless the command closes it.
    Command(OverlayCommand),
}

pub(in crate::shell) enum OverlayCommand {
    /// Activate the navigator's target; the overlay closes only if activation succeeds.
    OpenTarget(Location),
    /// Rename or create; the overlay was taken by the caller.
    SaveRename { target: rename::RenameTarget, label: Option<String> },
    CloseWorkspace(shepr_protocol::WorkspaceId),
    /// Esc on the close dialog; back to Navigate when it came from there.
    CancelClose { return_to_navigate: bool },
    GlobalMenu(global_menu::GlobalMenuAction),
    ContextMenu { target: context_menu::ContextMenuTarget, action: context_menu::ContextMenuAction },
    ToggleGlobalMenu,
}

impl Overlay {
    pub(in crate::shell) fn kind(&self) -> OverlayKind;
    pub(in crate::shell) fn layout(&self, screen: Rect, ctx: &OverlayContext<'_>) -> Option<(OverlayView, Option<OverlayScroll>)>;
    pub(in crate::shell) fn draw(&self, view: &OverlayView, buffer: &mut Buffer, ctx: &OverlayContext<'_>) -> OverlayPaint;
    pub(in crate::shell) fn on_key(&mut self, key: &TerminalKey, view: Option<&OverlayView>, ctx: &OverlayContext<'_>) -> OverlayEffect;
    pub(in crate::shell) fn on_mouse(&mut self, mouse: MouseEvent, view: Option<&OverlayView>, ctx: &OverlayContext<'_>) -> OverlayEffect;
    /// Paste and modal-paste text; returns whether it was taken.
    pub(in crate::shell) fn on_text(&mut self, text: &str) -> bool;
    pub(in crate::shell) fn accepts_modal_paste(&self) -> bool;
    pub(in crate::shell) fn commit_scroll(&mut self, scroll: OverlayScroll);
}

impl ClientShellState {
    pub(in crate::shell) fn route_overlay_key(&mut self, key: &TerminalKey, outcome: &mut ClientShellInput);
    pub(in crate::shell) fn route_overlay_mouse(&mut self, mouse: MouseEvent, outcome: &mut ClientShellInput);
    fn apply_overlay_effect(&mut self, effect: OverlayEffect, outcome: &mut ClientShellInput);
}
```

`Overlay` dispatches by `match` to the variant's inherent methods of the same
names (`layout`, `draw`, `on_key`, `on_mouse`, `on_text`). The pairing of
state and view by kind happens once, in that match. There is no trait,
because no generic code needs one.

Per module:

- `rename.rs`: `RenameOverlay { input: TextEditor, target: RenameTarget }`,
  with no `title` field. `fn title(&self) -> &'static str` matches the
  target. Constructors are `new_workspace(cwd, suggested_name, label_lookup)`,
  `workspace(id, label)` and `pane(id, label)`; they replace the five literal
  constructions in `overlay_input.rs` and `context_menu.rs`. It also holds
  `RenameTarget` (was `ClientRenameTarget`, with D's `label_lookup` field
  kept as is), and `apply_checkout_root(&mut self, ..) -> Repaint`, the body
  of `complete_workspace_label_lookup` from "is the label lookup this
  request" onward. D decides how the request match is expressed.
  `ClientShellState::complete_workspace_label_lookup` becomes a three-line
  forwarder.
- `confirm_close.rs`: `ConfirmCloseOverlay { workspace_id, detail,
  return_to_navigate }`. The constant title `"Close workspace?"` moves into
  draw, and the `title: String` field goes.
- `help.rs`: `HelpOverlay { query, search_focused, scroll, drag:
  Option<u16> }`. The scrollbar grab offset moves in from
  `ClientChromeDrag::HelpScrollbar`, together with Help's key table
  (`HelpCommand`, `help_command_for_main`, `help_command_for_search`,
  `help_footer` from `fixed_keys.rs`) and `help_lines`.
- `navigator.rs`: `NavigatorOverlay { query, search_focused, selected,
  scroll, filter, drag: Option<u16> }`. The grab offset moves in from
  `ClientChromeDrag::NavigatorScrollbar`, together with the navigator's key
  table from `fixed_keys.rs` and the `move_navigator_selection`,
  `scroll_navigator_to` and `move_navigator_workspace` bodies (made inherent:
  each takes `rows: &[ClientNavigatorRow]`). `ClientNavigatorRow` and
  `ClientNavigatorFilter` move here from `state.rs`. `NavigatorIndex::rows`
  takes `&NavigatorOverlay`.
- `context_menu.rs` and `global_menu.rs`: state, items and layout. The action
  bodies that send commands (`activate_workspace_context_action`,
  `activate_pane_context_action`, `activate_global_menu_item`) stay
  `impl ClientShellState` in the same files, because they call
  `push_endpoint_command`, `record_binding` and the rename constructors. They
  are what `apply_overlay_effect` calls for the menu commands.
- `widgets.rs`: `panel`, `popup`, `button`, `row` and `set_cell`, moved from
  `overlays/mod.rs`.
- `fixed_keys.rs` is deleted once its two tables have moved.
- `overlay_input.rs` is deleted. The openers (`open_navigator_overlay`,
  `open_new_workspace_overlay`, `open_rename_workspace_overlay`,
  `open_rename_pane_overlay`, `open_confirm_close_overlay`) need the
  snapshot and `submit`, so they move to `overlays/mod.rs` as
  `impl ClientShellState`. So do `workspace_action_id`, `save_rename_overlay`
  (now the `SaveRename` arm) and `accept_close_confirmation` (the
  `CloseWorkspace` arm).

`input/mouse.rs` keeps the gesture, chrome and pane code. Its four overlay
blocks, the `overlay_primary`/`overlay_clear` block and the Help and
Navigator scrollbar drag arms become one call:
`if self.overlay.is_some() { self.route_overlay_mouse(mouse, outcome); return;
}`. That call sits where the first overlay block is today, after the gesture,
toast, chrome-drag and release handling. A press while an overlay keeps a
scrollbar drag is a lost release: the overlay's `on_mouse` clears its `drag`
on any `Down`, the way the shell settles a chrome drag. `ClientChromeDrag`
loses `HelpScrollbar` and `NavigatorScrollbar`.

`insert_overlay_text` and the overlay arm of `modal_paste_target_active`
(`input/mod.rs`) call `on_text` and `accepts_modal_paste`.

### Notices leave `overlays/`

New `shell/notices/`: `mod.rs` (was `overlays/notices.rs`), `cards.rs` (was
`overlays/endpoint_notices.rs`, split into `notice_card_rect`,
`draw_notice_card`, `lifecycle_banner_rect` and `draw_lifecycle_banner`),
`machine_diagnostics.rs` and `transient_error.rs`. The rect functions are
what `resolve_frame` calls. The draw functions take the rect. The
`render_notification_card` body splits at "`let rect = ...`". `EndpointNotice`
and `EndpointNoticeKind` keep their `pub(crate)` re-export from `shell/mod.rs`.
`text_editor.rs` stays in `overlays/`, because the copy search prompt also
uses it through `view/draw.rs`.

### Final module tree

```
shell/
  mod.rs            module declarations, the crate-facing re-exports
  state.rs          ClientShellState, new_at, outcome vocabulary, timers, presentation_log_context
  config.rs         ClientShellConfig, ClientShellLayout, layout, initial_surface_size (from state.rs and presentation/config.rs)
  transitions.rs    apply_active_snapshot, reset_endpoint_projection, pair_surfaces, surface_presented, receive_tagged_pane_surface
  mode.rs           ModeState
  endpoints.rs      Endpoints, EndpointState, ActiveProjection, revision_is_older, activation
  ledger.rs         (D)
  copy/
    mod.rs          CopySession, CopyEntry, copy selection and search types, reconcile_snapshot, surface_presented, prune_evicted_search_matches
    pipeline.rs     CopyPipeline
    keys.rs         copy key routing, requests and completions (was input/copy_mode.rs)
  input/
    mod.rs  events.rs  hit_test.rs  mouse.rs  pointer.rs  selection.rs  scroll_lanes.rs  word_bounds.rs  word_selection.rs
  navigation/
    mod.rs  actions.rs  aggregate_navigation.rs  endpoint_navigation.rs  location.rs  workspace_navigation.rs
  view/
    mod.rs          ShellView, ResolvedFrame, DrawnFrame, hit types, compose, commit_frame
    resolve.rs      resolve_frame, copy-cursor cell and mode bar placement
    draw.rs         draw_frame, copy highlight painting, render_mode_bar
    list.rs         ListView, resolve_list, list scroll math and scrollbars
  presentation/
    mod.rs          Presentation
    surfaces.rs  surface_patch.rs  topology.rs  selection_render.rs  status.rs
    text.rs         put_text, put_right_text, put_spans, display_width, rendered_text_width, render_sidebar_background (was render.rs)
  sidebar/
    mod.rs  chrome.rs  preferences.rs (+ persist_chrome_preferences)  sidebar_tokens.rs  token_definitions.rs
    scroll.rs  layout.rs  endpoint_sidebar.rs  endpoint_agents.rs  agent_sidebar.rs
  overlays/
    mod.rs  widgets.rs  text_editor.rs  rename.rs  confirm_close.rs  help.rs  navigator.rs  context_menu.rs  global_menu.rs
  notices/
    mod.rs  cards.rs  machine_diagnostics.rs  transient_error.rs
  tests/            see the test strategy
```

`presentation/composition.rs` and `presentation/render.rs` are deleted, along
with `presentation/config.rs`, `navigation/scroll.rs`,
`overlays/overlay_input.rs`, `overlays/fixed_keys.rs` and
`input/copy_mode.rs`.

## Migration

Nine landings. Each one compiles, passes `brokkr check`, and is kept or
reverted as a unit. Landings 1 to 3 are the render-mutation entry (with the
presentation part of the state split, which falls out of landing 3).
Landings 4 to 6 are the rest of the state split. Landings 7 and 8 are the
overlay entry. Landing 9 is the visibility pass.

The order follows the dependencies:

- the view model (3) needs the sidebar (1) and the overlays (2) to stop
  mutating first;
- the transitions (6) need the domains (4, 5) and the view-owning
  `Presentation` (3);
- the per-overlay modules (7) need the overlay views (2) and `ShellView` (3);
- visibility (9) needs the final tree.

Landing 7 does not need landing 6, so it may go before it, and in the combined
order with D it does.

### Combined order with D

This spec's landings interleave with D's (`notes/spec-shell-requests.md`) in
one order, recorded identically in both specs. C*n* is this spec's landing
*n*, D*n* is D's:

| step | landing | why here |
|---|---|---|
| 1 | D1 lost endpoint drops lane-first | bug fix; touches only the endpoint-loss path (`reconcile.rs`, `shell_runtime.rs`, `transition_endpoint_status`) |
| 2 | C1 sidebar resolution | bug fixes |
| 3 | C2 overlay layout and views | bug fixes |
| 4 | C3 three-phase compose | bug fix (unpaired compose); `ShellView` |
| 5 | C4 `ModeState` | no overlap with D's fields |
| 6 | C5 `CopySession`, selection reactions, `Pointer` | defines the copy shapes D's tokens go into |
| 7 | C7 one module per overlay | defines `RenameTarget` and the label-lookup application D's token goes into |
| 8 | D2 continuation tokens | written once against the C5 and C7 shapes |
| 9 | D3 rollback type rule | `Rollback` written once with `copy: &mut Option<CopySession>` and `overlay: &mut Option<Overlay>` |
| 10 | C6 `ActiveProjection` and transitions | its free functions and reset are written once against tickets and `Rollback` |
| 11 | C8 notices, config, text helpers | independent |
| 12 | D4 endpoint hub | `present_projection` written once against C6's `activate_endpoint_projection` (no agent-scroll save and restore left to carry) |
| 13 | D5 `ClientLoop` closed | |
| 14 | C9 visibility pass | needs the final tree, including D's hub and `client_loop/` |

The rework this leaves is small and deliberate: C5 moves
`operation_generation: u64` into `CopySession` and D2 replaces it with
`rows: Ticket`; C7 carries `label_lookup: Option<RequestId>` into
`RenameTarget` and D2 changes its type. Every other D2 and D3 edit lands on
code that no later landing rewrites, and C6 and D4 are each written once.

Commit rules from AGENTS.md hold at every boundary: `brokkr fmt` before each
commit, `brokkr check` green, and `Cargo.lock` committed if it moved (it should
not; no dependency changes).

### Landing 1: sidebar resolution

Changes:

- Add `shell/sidebar/scroll.rs` (`SidebarScroll`, `WorkspaceReveal`,
  `SidebarScrollResolution`) and `shell/sidebar/layout.rs` (`SidebarView` and
  its parts, `SidebarInputs`, `resolve_sidebar`).
- Add `resolve_list` and `ListResolution` to `shell/navigation/scroll.rs`
  (they move to `view/list.rs` in landing 3).
- Rewrite `endpoint_sidebar.rs` and `endpoint_agents.rs` into draw functions
  over the views. Keep `agent_sidebar.rs`'s row drawing.
- `compose`, still `&mut self` in this landing, calls `resolve_sidebar` with
  `self.sidebar_scroll`, calls `self.sidebar_scroll.commit(..)` at once, and
  draws from the view. A temporary
  `fn sidebar_hits(view: &SidebarView, armed: bool) -> ShellHitMap`-shaped
  fill keeps `ShellHitMap`'s sidebar fields populated for the input readers.
  The fill is written in this landing and deleted in landing 3.
- Convert every call site in the sidebar table above. Change
  `endpoint_command_for_action` to `&self` returning `ActionCommand`, and
  update `record_binding` and `cycle_pane`. Add
  `ClientShellState::request_workspace_reveal`.
- `activate_endpoint_projection` keeps its save and restore in this landing,
  rewritten over `sidebar_scroll.agent_start()` and `scroll_agents_to`.
  Landing 6 deletes it.

Deletes: the fields `workspace_scroll`, `agent_scroll`,
`reveal_focused_workspace`, `reveal_navigation_workspace` and
`pending_agent_reveal`; `render::ShellRenderState`'s four `&mut` fields;
`render_shell`'s sidebar half; `reveal_endpoint_agent`; the index arithmetic
of `ClientShellState::reveal_workspace` (the function itself becomes
`request_workspace_reveal`); `agent_sidebar::render_agent_list` and its
`empty_message`; the agent-visibility check in `endpoint_command_for_action`.

Tests:

- Rewrite the field accesses in the existing tests. `state.workspace_scroll =
  x` becomes `state.sidebar_scroll.scroll_workspaces_to(x)`, and reads become
  `workspace_start()`; the same for agents. `state.reveal_focused_workspace =
  false` becomes a `#[cfg(test)] SidebarScroll::clear_reveals()`.
  `assert!(state.reveal_focused_workspace)` becomes
  `assert!(state.sidebar_scroll.workspace_reveal().focused_pending())`, a
  `#[cfg(test)]` accessor.
- New unit tests in `sidebar/layout.rs` (pure, no `ClientShellState`; build
  `SidebarInputs` from `crate::shell::tests::snapshot()` endpoints):
  - `selected_reveal_wins_over_focused_and_consumes_both`
  - `explicit_reveal_targets_the_named_endpoint_not_a_same_id_elsewhere`
  - `empty_workspace_body_keeps_start_and_reveals_pending`
  - `empty_agent_body_keeps_start_and_reveal_pending`
- New behaviour tests:
  - in `shell/tests/chrome_context.rs`:
    `collapsed_sidebar_keeps_a_reveal_until_its_workspace_area_is_visible`
    (compose collapsed at a height with an empty workspace area after a focus
    change, then at full height; the focused workspace is among the
    workspace hits);
    `an_empty_sidebar_body_keeps_both_list_scroll_positions` (scroll both
    lists, compose at height 2, compose at full height; both starts kept);
    `a_reveal_after_a_sidebar_toggle_uses_the_new_layout` (one input batch
    with ToggleSidebar and NextWorkspace on a long list; after one compose the
    target is among the workspace hits).
  - in `shell/tests/endpoints.rs`: `agent_reveal_waits_for_a_visible_agent_body`
    (NextAgent to an offscreen agent, compose with the agent body empty, then
    at full height; the agent is among the agent hits).
- Existing tests that pin parity and must pass unchanged in meaning:
  `focused_workspace_change_reveals_new_workspace_in_full_sidebar`,
  `revealing_an_active_workspace_ignores_a_same_id_on_another_endpoint`,
  `agent_navigation_reveals_offscreen_targets`,
  `agent_navigation_reveal_is_cancelled_by_another_selection`,
  `agent_navigation_keeps_scroll_when_target_is_visible`,
  `switching_machines_preserves_aggregate_agent_scroll_and_visible_rows`,
  `same_machine_reboot_still_resets_agent_scroll`,
  `aggregate_agent_scroll_still_clamps_when_rows_shrink_on_activation`,
  `expanded_machine_sidebar_reveals_newly_focused_workspace`,
  `aggregate_navigation_reveals_overflow_and_preserves_order`.

Each new behaviour test is written before its production change and must be
seen to fail against the old sidebar code:

```
brokkr test -p shepr-client collapsed_sidebar_keeps_a_reveal_until_its_workspace_area_is_visible
brokkr test -p shepr-client an_empty_sidebar_body_keeps_both_list_scroll_positions
brokkr test -p shepr-client a_reveal_after_a_sidebar_toggle_uses_the_new_layout
brokkr test -p shepr-client agent_reveal_waits_for_a_visible_agent_body
```

Gate: `brokkr check`.

### Landing 2: overlay layout and views

Changes:

- In `overlays/mod.rs`, split each `render_*_overlay`, `render_global_menu`
  and `render_context_menu` into `layout_*` (returns the variant's view and
  optional `OverlayScroll`, or `None` when it does not fit) and `draw_*`
  (infallible, returns `OverlayPaint`).
- Add `OverlayView`, the five view structs, `OverlayPaint` and
  `OverlayScroll`, and add one `layout_overlay` and one `draw_overlay` over
  `&ClientShellOverlay`.
- `compose` lays out the overlay before drawing, then commits
  `OverlayScroll` to the overlay state. Until landing 3 it also copies the
  view into the overlay half of `ShellHitMap` through one temporary fill,
  deleted in landing 3.
- Navigator: the stored scroll is the effective scroll.
  `scroll_navigator_to` takes the drawn `ListScroll`.
- Help: the clamp moves from `compose`'s tail to the commit of
  `OverlayScroll::Help`. The key and wheel handlers clamp against the drawn
  `HelpView` when one exists.

Deletes: `OverlayRender`; `render_client_overlay`'s `None` arm for the menus;
`compose`'s menu special case; the Help clamp in `compose`;
`scroll_navigator_to`'s own clamp formula.

Tests:

- New, in `shell/tests/copy.rs` next to the other navigator tests (they move
  in landing 7): `navigator_selection_moves_within_a_scrolled_viewport`. Open
  the navigator on 60 panes at 106x24, press Down until the start is
  non-zero, compose, press Up once, compose. The start is unchanged, and the
  selected row's rect is one row above the previous one. It must be seen to
  fail first:

  ```
  brokkr test -p shepr-client navigator_selection_moves_within_a_scrolled_viewport
  ```

- New, in `shell/tests/presentation_regressions.rs`:
  `help_scroll_survives_a_window_too_small_for_help` (scroll Help, compose at
  a size where it does not fit, compose at full size; same scroll).
- Parity: `navigator_scrollbar_click_and_drag_scroll_without_opening_a_destination`
  ("keyboard resumes after drag" stays `max_start - 1`),
  `client_presentation_regression_help_scrolls_to_its_last_entry_in_a_narrow_terminal`,
  `unavailable_small_popup_uses_the_common_hint_and_clears_hits`,
  `navigator_narrow_layout_and_long_search_stay_inside_the_popup`,
  `cursor_movement_preserves_filter_selection_and_scroll`.
- Unit tests next to the code in `overlays/mod.rs`:
  `layout_returns_none_where_render_gave_up` (each overlay at 3x3 and 10x5),
  `navigator_layout_keeps_the_selection_in_view`.

Gate: `brokkr check`.

### Landing 3: three-phase compose, `ShellView`, `Presentation`

Changes:

- Create `shell/view/` (`mod.rs`, `resolve.rs`, `draw.rs`, `list.rs`). Move
  `navigation/scroll.rs` into `view/list.rs`.
- Split `presentation/composition.rs`'s `compose` body into `resolve_frame`
  and `draw_frame`. Resolution covers: layout and fallback chrome layout,
  `valid_navigation_target`, the pending highlight filter, the dragged
  workspace and drop row, the sidebar, `active_lifecycle` and the banner
  rect, `healthy_local_chrome` and the placeholder message, pane hits and
  split hits, the copy cursor cell, `mode_bar_area`, the notice offset and
  card rect, and the overlay layout. Drawing is everything that writes the
  buffer or canvas.
- Make the hit types and `render_mode_bar` move as listed in the tree.
- Add `ShellView`, `ResolvedFrame`, `DrawnFrame`, `commit_frame`, and
  `Presentation` with the fields `surfaces`, `view`, `composition` and
  `composed_at`.
- Migrate every reader listed in the survey to `self.view()`,
  `self.pane_hits()`, `self.presentation.composition()` or
  `self.presentation.composed_at()`. `mode_bar_covers_copy_pane` reads
  `view.layout.pane_surface` (the drawn layout; today it recomputes the
  layout from the current chrome at the drawn size, and with
  `copy_hit` also from the drawn frame, the drawn layout is the consistent
  choice). `apply_tagged_pane_surface_patch` keeps
  `self.layout(view.size)`, because its comment's reason ("the layout can
  change after the last composition, so this reads the current area") still
  holds. Its fast path calls `presentation.patch_pane_hit`.
- `invalidate_pane_surface` (test helper) becomes `presentation =
  Presentation::default()` plus `pointer.host_mouse_pixels = None`; `Pointer`
  lands in landing 5, so until then `host_mouse_pixels = None`.
- Edit AGENTS.md, Principles, the "Render is pure" bullet. After its first
  sentence, add: "The client shell composes the same way:
  `ClientShellState::compose` resolves a `ShellView` (layout, scroll and hit
  rects) and draws it, both by shared reference, then commits the view and
  the resolved scroll positions in one step; drawing never writes shell
  state."

Deletes: `ShellHitMap` and both temporary fills; the fields `hits`,
`surfaces`, `last_composition`, `last_composed_size` and `last_composed_at`
on `ClientShellState`; `record_composed_frame`; `presentation/composition.rs`;
`render::render_shell`; `navigation/scroll.rs`.

Tests:

- Mechanical rewrite of `state.hits.X` in the test files (about 110 reads):
  `state.view().expect("drawn").X()` for accessors, `state.pane_hits()` for
  panes. For brevity, add `#[cfg(test)] fn drawn(&self) -> &ShellView` on
  `ClientShellState`, which panics with "no frame was drawn". The existing
  unit tests in `composition.rs` move to `view/draw.rs`
  (`copy_search_highlights_clip_surface_taller_than_frame`) and `view/mod.rs`
  (the two placeholder tests).
- New, in `shell/tests/presentation_regressions.rs`:
  `an_unpaired_compose_records_no_view` (compose a frame, receive a surface
  that waits for its snapshot, compose at another size; `None`, and the view
  keeps the first size); `navigate_reveal_follows_a_size_change` (Navigate
  with the preview off-screen, compose at a new size; the preview is among
  the workspace hits).
- Parity: every test in `chrome_context.rs`, `startup_overlays.rs` and
  `presentation_regressions.rs`, and `oversized_retained_surface_is_clipped_with_its_hits`.

Gate: `brokkr check`.

### Landing 4: `ModeState`

Changes:

- Add `shell/mode.rs`.
- Replace `mode` and `navigate_workspace_id` everywhere (about 75 production
  sites in `input/mod.rs`, `input/mouse.rs`, `input/copy_mode.rs`,
  `navigation/actions.rs`, `navigation/workspace_navigation.rs`,
  `overlays/overlay_input.rs`, `presentation/` and `state.rs`).
  `ClientInputContext::mode` takes `self.mode.kind()`.
  `host_keyboard_report_all_requested` reads `kind()`.

Deletes: the two fields; the paired "set mode, clear preview" statements.

Tests: rewrite `state.mode` / `state.navigate_workspace_id` reads in
`workspace_navigation.rs`, `copy.rs`, `input_domain.rs`, `endpoints.rs` and
`text_editing.rs` to `state.mode.kind()` / `state.mode.preview()`. Writes in
tests go through `set` / `enter_navigate`. New unit tests in `mode.rs`:
`leaving_navigate_drops_the_preview` and
`fill_preview_only_fills_an_empty_navigate_preview`.

Gate: `brokkr check`.

### Landing 5: `CopySession`, `MouseSelection` reactions, `Pointer`

Changes:

- Create `shell/copy/`. Move `input/copy_mode.rs` to `copy/keys.rs` and
  `CopyPipeline` to `copy/pipeline.rs`. Move the copy types and
  `prune_evicted_search_matches` from `state.rs` to `copy/mod.rs`.
- Replace `copy_mode` and `copy_pipeline` with `copy: Option<CopySession>`.
  `reset_copy_pipeline` becomes `session.pipeline_mut().reset()` where a
  session is kept (`abandon_copy_operation`, `enter_copy_mode`'s re-entry
  path). Elsewhere it disappears into `self.copy = None`. `enter_copy_mode`
  builds through `CopySession::start`. `sync_copy_selection` becomes
  `if let Some(s) = session.projected_selection() {
  self.mouse_selection.selection = Some(s) }`.
- `handle_key`'s queue check reads
  `self.copy.as_ref().is_some_and(|s| s.pipeline().in_flight())`.
  `release_input_leases` clears the keys through the session.
- Move `MouseSelection` and its types to `input/selection.rs`, and add
  `facts_in`, `reconcile_snapshot`, `surface_presented` and `frame_drawn`.
  Their bodies are cut out of `apply_active_snapshot` and
  `presented_surface_changed`, which call them, still as `&mut self` methods
  in `state.rs`.
- Add `input/pointer.rs` with `Pointer` and move the five fields and four
  types into it.

Deletes: `ClientCopyModeState`; `copy_pipeline`; `reset_copy_pipeline`;
`pane_facts_before`; the five pointer fields; the field-by-field copy session
constructions in the tests.

Tests:

- Add `#[cfg(test)]` builders on `CopySession` (`with_search`,
  `with_selection`, `with_cursor`). Rewrite the four field-by-field
  constructions (`input/mod.rs` tests, `surface_patch.rs` tests,
  `view/draw.rs` tests, `shell/tests/copy.rs`) to use them.
- Unit tests in `copy/mod.rs`: `ending_a_session_discards_its_queue` and
  `projected_selection_follows_anchor_and_cursor`.
- Parity: all of `copy.rs`, `mouse_selection.rs`, `endpoint_requests.rs` and
  `switching_machines_from_copy_mode_restores_terminal_input`.

Gate: `brokkr check`.

### Landing 6: `ActiveProjection` and the reshaped transitions

Lands after C7, D2 and D3 in the combined order, so `CopySession` already
carries `rows: Ticket`, the ledger has `Ledger::ticket()`, and `Work::dropped`
takes `Rollback`.

Changes:

- Add `ActiveProjection`, `ProjectionChange`, `ProjectionStep`,
  `ProjectionReset` and `revision_is_older` to `endpoints.rs`. Move
  `collapsed_endpoints`, `agent_panel_model` and `navigator_index` into
  `Endpoints`, and `snapshot`, `active_snapshot_generation`,
  `active_boot_key` and `previous_pane_id` into `Endpoints::active`.
  `cache_endpoint_snapshot_at_generation` uses `revision_is_older`.
- Create `transitions.rs` with `apply_active_snapshot`,
  `reset_endpoint_projection(ProjectionReset)`, `pair_surfaces`,
  `surface_presented` and `receive_tagged_pane_surface`, as specified above.
- Add `copy::reconcile_snapshot` and `copy::surface_presented` (cut from the
  old bodies). The `rows = self.ledger.ticket()` re-issue D2 put in
  `presented_surface_changed` moves with it, through the `ledger` parameter.
- `activate_endpoint_projection` as specified.
- The patch slow path calls `transitions::surface_presented`.

Deletes: the save and restore of the agent start;
`ClientShellState::presented_surface_changed`; both `std::mem::take(&mut
self.surfaces)` dances; the second `mouse_selection.clear()`; the duplicated
older-revision expressions; the seven moved fields.

Tests:

- The two tests that call `reset_endpoint_projection()` directly
  (`endpoint_requests.rs`, `presentation_regressions.rs`) pass
  `ProjectionReset::Rebooted`.
- New unit tests in `endpoints.rs`:
  `accept_reports_a_switch_and_a_reboot_apart` and
  `older_revision_rule_is_shared_by_cache_and_projection` (one table of
  `(generation, boot, revision)` cases run through both paths).
- New, in `shell/tests/endpoint_requests.rs`:
  `a_reset_drops_requests_before_resetting_features`. Hold a copy motion, a
  word-selection read and a label lookup, then reboot. Every request is gone,
  the copy session is gone, and the outcome holds no action.
- Parity: `switching_machines_preserves_aggregate_agent_scroll_and_visible_rows`
  and `same_machine_reboot_still_resets_agent_scroll` now pass without the
  save and restore. That is the evidence the workaround is gone. Also
  `inactive_endpoint_snapshot_cache_never_regresses_revision`,
  `new_connection_generation_accepts_a_lower_same_boot_projection_revision`,
  `reconnect_same_endpoint_accepts_new_generation_surface_revision`,
  `active_preview_is_not_retargeted_by_deletion_or_reboot`,
  `selection_without_a_previous_surface_is_dropped_by_the_next_surface`, and
  everything in `copy.rs`.

Gate: `brokkr check`.

### Landing 7: one module per overlay

Changes:

- Split `overlays/mod.rs` and `overlay_input.rs` into the per-overlay files
  as specified. Rename `ClientShellOverlay` to `Overlay`,
  `ClientShellOverlayKind` to `OverlayKind`, and `Client*Overlay`,
  `ClientRenameTarget`, `ClientContextMenu*` and `ClientGlobalMenuAction` to
  their unprefixed names in their modules.
- Add `OverlayContext`, `OverlayEffect`, `OverlayCommand`,
  `route_overlay_mouse` and `apply_overlay_effect`.
- Replace the overlay blocks in `input/mouse.rs` with the single call.
- Move the Help and Navigator scrollbar drags into their overlays.
- Derive the rename title.

Deletes: `overlays/overlay_input.rs`; `overlays/fixed_keys.rs`; the overlay
types in `state.rs`; `ClientChromeDrag::HelpScrollbar` and
`ClientChromeDrag::NavigatorScrollbar`; `ClientRenameOverlay::title`;
`ClientConfirmCloseOverlay::title`; the mouse blocks.

Tests:

- Move the navigator tests out of `shell/tests/copy.rs` into a new
  `shell/tests/navigator.rs`. They are everything from
  `navigator_workspace_headings_use_the_active_themes_primary_text` through
  `navigator_owns_search_mouse_selection_and_stable_target_focus`, plus
  `navigator_scale_snapshot` and
  `navigator_selection_moves_within_a_scrolled_viewport`. Move
  `pasted_help_and_copy_queries_normalize_single_line_text` with them only if
  it stops touching copy; it touches both, so it stays.
- Unit tests next to the code, pure, with no `ClientShellState`:
  `rename::tests::title_follows_the_target`,
  `help::tests::scroll_keys_clamp_to_the_drawn_view`,
  `help::tests::a_press_drops_a_lost_scrollbar_drag`,
  `navigator::tests::up_after_scrolling_moves_the_selection_not_the_view`,
  `context_menu::tests::items_follow_the_target`,
  `global_menu::tests::launcher_click_toggles`.
- Parity: `context_menus_capture_stable_targets_and_route_actions`,
  `global_menu_opens_from_sidebar_and_routes_client_actions`,
  `overlays_render_without_a_pane_surface_or_snapshot`,
  `unavailable_global_menu_renders_and_activates_without_snapshot`, all of
  `text_editing.rs`, `cancelled_close_returns_to_the_mode_it_was_opened_from`
  and `cancelled_close_does_not_restore_an_older_navigation_highlight`.

Gate: `brokkr check`.

### Landing 8: notices out of `overlays/`, config and text helpers

Changes:

- Create `shell/notices/` as specified and split the card and banner
  functions into rect and draw halves (`resolve_frame` already calls the rect
  halves from landing 3 onward; there the split is done inside
  `endpoint_notices.rs`, and this landing only moves the files).
- Move `ClientShellConfig` and `ClientShellLayout` from `state.rs` and the
  impls from `presentation/config.rs` to `shell/config.rs`.
- Move `persist_chrome_preferences` to `sidebar/preferences.rs`.
- Rename `presentation/render.rs` to `presentation/text.rs`.
- Fold the duplicate width helpers: `sidebar_tokens::display_width`,
  `agent_sidebar`'s private `display_width` and `put_text` wrappers go, and
  callers use `presentation::text::rendered_text_width` (usize) and
  `display_width` (u16).

Deletes: the five moved `overlays/` files; `presentation/config.rs`; the three
wrapper functions.

Tests: imports only. Gate: `brokkr check`.

### Landing 9: the visibility pass

Go module by module, bottom-up (`view/list.rs`, `presentation/*`,
`sidebar/*`, `overlays/*`, `notices/*`, `copy/*`, `input/*`, `navigation/*`,
`view/*`, `transitions.rs`, `state.rs`). For each item, the visibility is the
narrowest that compiles:

- private;
- `pub(super)` for a sibling in the same directory;
- `pub(in crate::shell::X)` for a subtree;
- `pub(in crate::shell)`;
- `pub(crate)` only for what `crate::` outside `shell/` names. This pass runs
  after D's landings 4 and 5, so that outside set is: `ClientShellState`'s
  `pub(crate)` methods called from `state.rs`, `client_loop/`,
  `shell_runtime.rs` and `endpoint/` (D's hub and view steps, including
  D's `endpoint_projection`, `present_projection` and `endpoint_failed`);
  `endpoints`, `Endpoints`, `choice`; the re-exports in `shell/mod.rs`; and
  what `crate::tests` uses.

Struct fields follow their type: a type private to a module has private
fields.

Tests in `shell/tests/` are children of `crate::shell`. An item they need is
`pub(in crate::shell)` or exposes a `#[cfg(test)]` accessor. The rule is that
a test which only exercises one module's internals moves into that module's
`#[cfg(test)] mod tests`, rather than widening the item for it. Candidates to
move this way, found by the pass: any test that reaches into
`CopySession`, `SidebarScroll` or an overlay's fields without going through
`ClientShellState`.

The overlays' `OverlayEffect` and `OverlayCommand` are
`pub(in crate::shell::overlays)` except where `input/` names them.
`ShellView`'s fields become private with the accessors above, except the
fields drawing reads, which are `pub(in crate::shell::view)`.

The cleanup entry's named items: `OverlayRender` (gone),
`render_client_overlay`, `render_global_menu` and `render_context_menu` (gone,
replaced by `pub(in crate::shell::overlays)` layout/draw halves called only
from `overlays/mod.rs`), `endpoints.rs`'s items (`MachineHit` moved; the
`ClientShellState` methods used only inside `shell/` narrow to
`pub(in crate::shell)`; `endpoint_status_presentation` and `local_endpoint`
narrow to their users), and `sidebar/token_definitions.rs`'s items (narrowed
to `pub(in crate::shell::sidebar)`; `sidebar_tokens.rs` re-exports what
`agent_sidebar.rs` and `sidebar/mod.rs` use).

Gate: `brokkr check`. The visibility pass also runs clippy's dead-code
lints, so an item whose last user went away in an earlier landing shows up
here and is deleted.

## Test strategy

- `brokkr check` gates every landing. The command list per landing is that
  one line plus the failing-first runs named in landings 1 and 2.
- Pure layout and state-machine logic gets unit tests next to the code, with
  no `ClientShellState`: `sidebar/layout.rs`, `view/list.rs`, `mode.rs`,
  `copy/mod.rs`, `endpoints.rs` (`ActiveProjection`), and each overlay
  module. That follows the AGENTS.md convention, and it is the cheap way to
  pin resolution rules (reveal priority, empty body, navigator start) without
  composing frames.
- Behaviour through the whole shell stays in `shell/tests/`, grouped by
  feature as today. Placement for new tests:
  - sidebar reveal and scroll: `chrome_context.rs` (single endpoint) and
    `endpoints.rs` (machines, aggregate agents);
  - composition and view lifetime: `presentation_regressions.rs`;
  - the navigator: the new `navigator.rs` from landing 7 (`copy.rs` until
    then);
  - reset ordering and requests: `endpoint_requests.rs`;
  - mode: `workspace_navigation.rs` and `input_domain.rs`;
  - copy: `copy.rs`.
- Test-only seams are `#[cfg(test)]` methods on the production types
  (`ClientShellState::drawn`, `SidebarScroll::clear_reveals`,
  `WorkspaceReveal::focused_pending`, `CopySession::with_*`), never a test
  feature. Tests write state through the domain's methods, not through
  fields, so the visibility pass does not have to widen anything for them.
- Purity is enforced by the compiler: `resolve_frame` and `draw_frame` take
  `&ClientShellState`. No test is needed for it. The commit-once property is
  pinned by `an_unpaired_compose_records_no_view` and the landing 1 tests.

## Behaviour changes

All intended. Each one is pinned by the named test.

1. The navigator viewport stays put while the selection moves inside it
   (`navigator_selection_moves_within_a_scrolled_viewport`).
2. The collapsed sidebar keeps a reveal requested while its workspace area is
   empty
   (`collapsed_sidebar_keeps_a_reveal_until_its_workspace_area_is_visible`).
3. A frame whose list body is empty keeps the workspace and agent starts
   (`an_empty_sidebar_body_keeps_both_list_scroll_positions`).
4. An agent reveal requested while the agent body is empty waits for it
   (`agent_reveal_waits_for_a_visible_agent_body`).
5. A reveal in the same batch as a sidebar toggle uses the new layout
   (`a_reveal_after_a_sidebar_toggle_uses_the_new_layout`).
6. Keyboard workspace switching on a multi-endpoint shell reveals the target
   in the flattened list at once, rather than when its focused snapshot
   arrives. The target is the same row either way.
7. An unpaired compose records nothing (`an_unpaired_compose_records_no_view`).
8. A Help overlay too large for the window keeps its scroll
   (`help_scroll_survives_a_window_too_small_for_help`).
9. A canvas refusal commits nothing. Not reachable from a test, since
   `Canvas::from_buffer` on a fresh `Buffer::empty` does not refuse. The
   structure guarantees it.
10. The copy-mode bar placement reads the drawn layout rather than one
    recomputed from current chrome. This only differs between a chrome change
    and its frame, when the pane hit it is paired with is also from the drawn
    frame.

## Risks

- Test churn. About 110 `state.hits` reads, about 30 scroll and reveal field
  pokes, about 75 production mode sites plus their test reads, and the
  overlay type renames touch most of the 11,000 lines in `shell/tests/`. A mechanical rewrite can change a test's
  meaning silently (for example, a `workspace_max_scroll` read that becomes
  the collapsed list's `max_start`, which today is a hand computation).
  Mitigation: landings 1 and 3 each list their parity tests, and a reviewer
  diffs assertions, not just compile fixes.
- The borrow shape of the transitions. `surface_presented` and
  `copy::reconcile_snapshot` take disjoint fields precisely so the surface
  can be borrowed. A later change that turns them back into `&mut self`
  methods brings the `mem::take` dance back. The free-function signatures are
  the guard; keep them.
- Resolution cost on the hot path. Compose runs once per presented frame.
  `resolve_frame` must allocate no more than today's render. The row-height
  and gap vectors are already built per frame. The navigator rows are built
  once per frame, as today, and move into the view rather than being built
  again in draw. Workspace token rows are built for heights and again for the
  drawn rows, as today. Do not cache tokens in the view (they would be cloned
  per frame).
- The reset order (requests first) interacts with D. D's tickets make it
  irrelevant to staleness, and D's rollbacks are no-ops on absent state, but a
  future rollback that wanted to restore something the reset had already
  cleared would act on nothing. Both specs keep the drop first.
- The overlay lost-release rule is new for Help and Navigator scrollbar
  drags. They used to be settled by the generic chrome-drag code, which did
  nothing for them on a lost release (only sidebar drags owe work). The new
  rule (clear the grab on any press) is equivalent.
- Parallel work with D in the same files: `input/copy_mode.rs` (moved in
  landing 5), `ledger.rs` (untouched here except imports),
  `overlays/overlay_input.rs` (split in landing 7, carrying `label_lookup`),
  `navigation/workspace_navigation.rs` (mode edits in landing 4). Sequence
  them as the combined order in the migration section gives.

## Findings

Found on the way. "In scope" items are fixed by a landing above; the rest are
out of this spec's scope and listed for the owner.

1. In scope, bug: the navigator viewport snaps after keyboard scrolling,
   because the effective start is never stored (landing 2).
2. In scope, bug: a frame with an empty list body resets the workspace scroll
   (expanded, through `metrics.start()`) and the agent scroll (explicit `= 0`)
   to 0. Not in the hunt entry (landing 1).
3. In scope, bug: `reveal_endpoint_agent` drops a reveal when the agent body
   height is 0, and both imperative reveals use the last frame's body
   heights, which are stale after a sidebar toggle in the same batch
   (landing 1).
4. In scope, bug: a Help overlay that does not fit has its scroll reset to 0
   by `compose`'s tail clamp (landing 2).
5. In scope, smell: `reset_endpoint_projection` clears `mouse_selection`
   twice (landing 6).
6. In scope, smell: `render_agent_list` is generic, with an `empty_message`
   only ever `None` (landing 1). There are four display-width helpers
   (`render::display_width`, `render::rendered_text_width`,
   `sidebar_tokens::display_width`, `agent_sidebar`'s private one) and a
   private `put_text` wrapper (landing 8).
7. In scope, smell: about 790 lines of navigator tests live in
   `shell/tests/copy.rs` (landing 7).
8. In scope, smell: `persist_chrome_preferences` lives in
   `presentation/config.rs` and `ClientShellConfig`'s impl is split from its
   struct (landing 8).
9. Out of scope: `ClientShellConfig::agent_panel_sort` is mutated at runtime
   (`new_at` and the sort toggle), duplicating `agent_panel_sort_chrome`.
   Validated config doubles as live state. The live sort belongs with the
   chrome, next to `ChromeLayout`.
10. Out of scope: `ClientState::write_frame` can fail after `compose`
    committed. The notice's drawn lifetime and the view then describe a
    frame that never reached the terminal. The next frame repaints in full,
    so the view self-heals, but a notice can expire unseen. A
    `ClientShellState::frame_presented()` called after a successful write
    would move the notice clock there; that needs `state.rs`.
11. Out of scope: `ClientLoop::handle_resize` assigns
    `state.reported_geometry = geometry` and then immediately calls
    `state.set_host_size(geometry.cols(), geometry.rows())`, which re-derives
    the grid from the same values.
12. Stale hunt detail: the state-split entry's "built field by field in
    several places" is one production site and four test sites, and its
    "`reported_geometry` repeats the size" is only partly true (see "Hunt
    claims checked").
13. Stale hunt detail: the visibility entry's "none narrower" is off by
    `overlays/fixed_keys.rs` and `presentation/selection_render.rs`.
14. Out of scope, observation: navigator PageUp and PageDown move by a fixed
    8 rows, and Help's paging by 8, rather than by the viewport height.
15. Out of scope: `endpoints` and `Endpoints::choice` are reached from
    `shell_runtime.rs`, `dispatch.rs` and `reconcile.rs`, so the visibility
    pass cannot narrow them. That belongs to the client move protocol entry
    in `notes/hunt-structure.md`.

## Interface with D (`notes/spec-shell-requests.md`)

### What this spec assumes of D

- D owns every request id named in the stopping rule and the `Work`
  dispatch. This spec moves the holders of those ids:
  `CopyPipeline::awaiting` into `CopySession`'s pipeline,
  `ClientRenameTarget::NewWorkspace::label_lookup` into
  `overlays/rename.rs::RenameTarget`, and `ClientWordSelection` stays in
  `input/word_selection.rs`. It never changes how they are compared.
- D keeps `submit` and `push_endpoint_command` callable from
  `impl ClientShellState` code. Until D2, `submit` returns
  `Option<RequestId>`; from D2 on it returns `Submitted` (`Opened` or
  `Refused`), and a caller that holds a ticket records it only on `Opened`.
  This spec calls them from `apply_overlay_effect`, the overlay openers and
  `record_binding`, never from overlay modules, which are pure state and
  return effects.
- D's rollbacks (`Work::dropped`) run during `reset_endpoint_projection`
  before any feature is reset (step 1). D does not need that order for
  correctness (tickets decide staleness, and each rollback is a no-op on
  absent state); both specs keep it.
- D's copy tokens go into the shapes landing 5 defines, because D2 lands
  after it: `rows: Ticket` replaces `operation_generation` in `CopySession`
  and is added to `CopyEntry` (so `CopySession::start` takes it), and the
  flight ticket lives in the session's `CopyPipeline`. `enter_copy_mode`,
  `CancelOrClear`, each search dispatch and a resize or screen switch
  re-issue `rows`; landing 6's `copy::surface_presented` therefore takes
  `&mut Ledger`.
- D's label-lookup token goes into landing 7's `RenameTarget`, because D2
  lands after it: `label_lookup: Option<Ticket>`, matched inside
  `RenameOverlay::apply_checkout_root(lookup, ..)`, and D3's drop is a
  rename-overlay operation reached through `Option<Overlay>`.
- D3 lands before landing 6, so `Rollback`'s field list is the complete set of
  state a reset's request drops can touch.
- D does not reintroduce reads of render output in request handlers. Reads of
  the drawn view (`request_selection_copy`'s linewise width from
  `pane_hits()`) are allowed.

### What this spec offers D

- `CopySession` with private fields, `start(CopyEntry)`, `pipeline()`,
  `pipeline_mut()` and `pane_id()`. A session is replaced, never mutated into
  a new one: `enter_copy_mode` on another pane drops the old session first,
  and the session owns its pipeline, so a replaced or ended session holds no
  flight ticket and every answer for it is stale.
- `RenameOverlay::apply_checkout_root(..)`, the label-lookup application
  without its request match. D supplies the match.
- `ProjectionReset` (`EndpointSwitched` / `Rebooted`) as the reason for a
  projection reset, available to D if dropped requests need to tell them
  apart (today both use `DropReason::Reset`).
- `transitions.rs` as the single home of `drop_all_requests(DropReason::Reset)`
  and of `scroll_lanes.retain_panes` and `clear`, so D finds each call site
  of its scroll-lane and ledger hooks in one file.

### Sequencing with D

Decided: the combined order in the migration section (D1, C1, C2, C3, C4, C5,
C7, D2, D3, C6, C8, D4, D5, C9), recorded identically in D. In short: the bug
fixes first; D's tokens and rollback type after this spec has defined the
copy and rename shapes they go into (C5, C7) and before the transitions (C6)
are rewritten as free functions; D's hub after C6, so `present_projection` is
written once; the visibility pass last.

D2 is not a pure field-type change, which is why it waits for C5 and C7
rather than going first: it replaces `Work`'s variants, changes `submit`'s
return type and the signatures of seven feature entry points, moves the copy
staleness check from `operation_generation` to tickets, and adds the
`Focus` work that releases the workspace highlight. Those edits sit in code
C5 and C7 move, and in the two shapes they define (`CopySession` and
`CopyEntry`, `RenameTarget`), so landing D2 after them writes each edit once.
