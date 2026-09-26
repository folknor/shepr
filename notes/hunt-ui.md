Review of UI/render scope (src/ui*, src/metadata_tokens.rs, src/client/shell*). No files were edited. I did not run any shell commands; I used only Read and LSP, per the project rule. The review is partial: I read ui.rs, ui/sidebar.rs, ui/tab_surface.rs, the sidebar/tokens symbols, metadata_tokens.rs, and these files under client/shell: shell.rs, agent_sidebar.rs, endpoint_agent_state.rs, sidebar.rs, part of endpoint_sidebar.rs and part of aggregate_navigation.rs. I did not open ui/panes.rs, scrollbar.rs, status.rs, copy_mode, menus or input.

## Main structural finding
"Server-side render of the sidebar" no longer exists. `src/ui/` renders only the tab surface (the panes). `ui/sidebar.rs` and `ui/sidebar/tokens.rs` are a token-layout library that only the client shell calls (`crate::ui::sidebar_agent_rows`, `resolved_token_spans`, `expanded_sidebar_sections`). So `crate::ui` has two unrelated jobs: the server's pane compositor, and the client's sidebar widget kit.
- **Suggestion:** move the sidebar token/section code into `client/shell/` (or a `sidebar_layout` module the client owns), leaving `ui` as the pane/tab-surface renderer. The dependency edge `client::shell -> crate::ui` would then disappear.

## Decisions made in more than one place

1. **Which agent needs attention most (the priority ladder).**
   - `src/workspace/aggregate.rs:8` `pane_attention_priority(AgentState, seen)`: Blocked 4, Idle+unseen 3, Working 2, Idle+seen 1, Unknown 0.
   - `src/client/shell.rs:171` `status_priority(AgentStatus)`: the same ladder on a different enum.
   - They agree today; nothing ties them together.
   - The client ladder is used in 4 places: `agent_sidebar.rs:28`, `aggregate_navigation.rs:88`, `endpoint_agent_state.rs:185,196`.
   - Tie-breaks differ. The server prefers the unseen pane among equals. The client's `max_by_key` has no tie-break key, but equal statuses are identical values, so there is no visible difference yet.
   - **Owner:** a single `Ord` / `attention_rank()` on one status type.

2. **Is this completion seen, i.e. Done or Idle?**
   - The server decides it from `pane.seen`, mapped through `app/api_helpers.rs:86 pane_agent_status`.
   - The client re-decides it independently in `client/shell/endpoint_agent_state.rs` (`EndpointAgentPresentation`: its own acknowledged/completed/working maps, then `projected_status`).
   - `project_aggregate_status` then overwrites the server's tab and workspace `agent_status` with a client re-aggregation, but only when that tab or workspace has at least one agent. Otherwise the server's value survives. That is a mixed-authority bug surface: a workspace with no agents keeps whatever the server said.
   - The server's `seen` and aggregate still exist and are still sent, so two answers to "seen" coexist.
   - **Suggestion:** decide which side owns "seen". Since the design is per-client acknowledgement, the server should send raw `AgentState` plus a completion sequence, not `AgentStatus::Done`. Drop the server-side `seen`, `pane_agent_status` and aggregate for presentation.

3. **The status text of an agent.**
   - `shell.rs:182 status_text` maps Unknown to "unknown". It is used as the `state_labels` lookup key and as the space-row `state_text` (`sidebar.rs:494`).
   - `agent_sidebar.rs:370 sidebar_status_text` maps Unknown to "idle", and is the fallback for agent rows.
   - They already disagree: an Unknown agent shows "idle" in the agent panel and "unknown" in space rows. Its label override is keyed under "unknown".
   - **Owner:** one function on the status type, with an explicit "display" variant if the difference is intended.

4. **Two enums for one axis.** `detect::AgentState` (4 states) and `api::schema::AgentStatus` (5 states: Done = Idle+unseen) are hand-mapped in several places: `api_helpers.rs:75,86` and the client projection. I did not verify whether `api::schema::PaneAgentState` (the 4-state enum `detect_state_from_api` maps from) is a third copy of the same axis. A single `AgentState` plus a separate `Attention { seen }` would remove the Done/Idle ambiguity that the client has to re-derive.

5. **Workspace row presentation, answered by two sidebars.** `client/shell/sidebar.rs` (local) and `client/shell/endpoint_sidebar.rs` (multi-endpoint) each render collapsed and expanded rows independently.
   - The selection background is duplicated: `sidebar.rs:7 workspace_selection_background` is private, and `endpoint_sidebar.rs:125` inlines the same logic.
   - Numbering already disagrees: local is `format!("{:<2}", index + 1)` (position), endpoint is `format!(" {}", workspace.number)` (a server number, leading space).
   - Stale styling (DIM, overlay0 icon) exists only in the endpoint path.
   - The same pattern holds for agent rows: `agent_sidebar.rs` vs `endpoint_agents.rs` (seen only via references).
   - **Suggestion:** a single-endpoint view is the one-endpoint case of the multi-endpoint sidebar. Make local the degenerate case of the endpoint sidebar and delete `sidebar.rs`'s row rendering. This looks like the biggest payoff in the client shell.

6. **Status glyph and colour.** `status_icon` / `status_color` in `shell.rs` are the single owner, which is fine. But every call site pairs them by hand (`status_icon(s, …)` next to `Style::default().fg(status_color(s, palette))`) in 5 or more places, and endpoint_sidebar overrides the colour for stale. A `StatusGlyph { text, style }` built from (status, indicator style, palette, stale) would give that one home.

## Axes that should be types
- **Client IDs are `String`s.** `pane_id`, `workspace_id` and `tab_id` in ClientShellAgent/Workspace/Tab, `AgentRow.pane_id`, `hits.agents: Vec<(Rect, String)>`, and `dragged_workspace_id: Option<&str>`. They are easy to swap (`agent.workspace_id == workspace.workspace_id` is only checked by field name). The server has a typed `PaneId`; the client should get newtypes for all three.
- **Index-based targets.** `TabSurfaceTarget { workspace_index: usize, tab_index: usize }` (`ui/tab_surface.rs:10`) and `resize_tab_surface(app, _, workspace_index, tab_index, …)` pass bare, positional, swappable indices into a mutable Vec. IDs, or at least `WorkspaceIndex`/`TabIndex` newtypes, would help.
- **Agent row lookups.** `agent_row` finds the agent, workspace, tab and pane by linear string search on every row, for every frame. A keyed snapshot (maps by typed id) removes both the O(n²) cost and the silent `?` drops.
- **Loose `agent_row` inputs.** `state_labels` and `tokens` travel as `Vec<(String, String)>` and are rebuilt into a `HashMap` per row per frame (`agent_sidebar.rs:255-260`, `sidebar.rs:488`). The label key is a status string produced by `status_text`, so it should be keyed by the status enum.
- **`metadata_tokens::accept_sequence -> Result<bool, ()>`.** Three outcomes are hidden in a bool plus a unit error. Use `enum SequenceOutcome { Accepted, Stale, TooManySources }`.
- **Lone primitives.** `split_ratio: f32`, clamped silently in `sidebar_section_heights`, should be a `SectionSplit` newtype validated once at config and drag time. `contains(rect, (u16, u16))` takes a bare tuple point. `acknowledge_surface(…, outer_focused: Option<bool>)` is a tri-state bool.
- **`resolved_token_spans` takes 5 positional `Style` parameters.** state_icon, state_text, workspace, secondary and custom are the same type in a row, and call sites already pass `secondary` twice. A `TokenStyles` struct would stop them being swapped.

## Structure
- **`metadata_tokens.rs` is not UI.** It is server-side report sequencing and TTL state, and depends on `terminal::state::MetadataReportSeq` / `HOOK_SEQUENCE_REANCHOR_AFTER`. It belongs next to `terminal/state` report sequencing, or in `workspace/`. The doc says it follows "the same rule as" `report_seq_superseded`; it does delegate to `last.supersedes`, so that is one answer, not a copy.
- **"Compute" functions resize PTYs.** `compute_view_*` and `compute_tab_surface(resize_panes: bool)` perform PTY resizes as a side effect, including every background tab. That contradicts the stated "compute_view handles geometry and mutations / render is pure" split only mildly, but the bool flag is the smell. Return the layout, and have a separate `apply_pane_sizes(layout)` step do the resizing.
- **`client/shell.rs` is a grab-bag.** It mixes the pane-input batching, an inline FNV topology hash, the status presentation table, and frame blitting.
- **`client/shell/` has about 27 flat modules** with overlapping axes (`endpoint_*` × {agents, sidebar, navigation, notices, agent_state} next to agent_sidebar, sidebar and aggregate_navigation). The local/endpoint split mirrors history, not the design. The proposed shape is `presentation/` (the status projection plus the glyph/text/priority table), `sidebar/` (one implementation over N endpoints), `navigation/`, `input/` and `overlays/`.

## Other smells
- `displayed_workspace_status` (`sidebar.rs:477`) is an identity function. It is a leftover hook where a decision used to live.
- `workspace_entries` builds `WorkspaceEntry { index }` for 0..n, also a vestige.
- `put_text` in `agent_sidebar.rs:358` iterates by `chars()`, not by display width, so wide characters overflow `width` and can misplace cells. `display_width` and `put_text` are also redefined locally even though `render::display_width` exists.
