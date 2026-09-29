# Flatten workspaces and tabs: landing spec

Decision (owner): drop tabs. A workspace becomes one pane layout, which is
what a single tab is today. The tab bar goes, and with it every tab-specific
concept: tab ids, tab creation, focus, rename, move and close, the tab
`EndpointCommand` variants, tab keybindings and config keys, the tab bar
widget and its right-hand status segments, tab-level persistence, the per-tab
geometry record, and the window-title and sidebar tokens that name tabs.
Nothing on disk migrates. Delete rather than adapt.

This is a transient plan. It was written against the tree while other agents
were editing it; see "In-flight edits" before starting.

## In-flight edits to land first

When this spec was written, uncommitted work was in progress in files this
change also touches. Land (or wait for) that work, then have every slice
re-read its files before editing:

- `crates/shepr-protocol/src/command.rs`: `PaneInfo` trimmed to
  `pane_id, focused, scroll`; `AgentSessionInfo` deleted.
- `crates/shepr-api/src/schema.rs`, `schema/tabs.rs`, `error.rs`: the
  `agents` and `session` schema modules, `TabInfo`, `SessionSnapshot` and the
  `External` error code are being removed. `schema/agents.rs` and
  `schema/session.rs` are still on disk but already out of the module tree;
  whoever finishes that work should delete the two files.
- `crates/shepr-server/src/app/creation.rs`: `collect_panes`, `tab_info` and
  the session-snapshot pane fields removed. `app/api/session.rs`
  (`session_snapshot`) and `app/agents.rs` (`collect_agent_infos`) still feed
  `server/client_shell.rs`; they are presumably next in that change.
- `crates/shepr-client/src/shell_runtime.rs`, `lib.rs`, `shell/endpoints.rs`:
  `resize_handoff_for_snapshot` and `snapshot_surface_size` were just added.
  They exist only because a snapshot can change the focused workspace's tab
  count and so the tab bar's presence. Flattening makes them dead (see the
  client slice).
- `shell/navigation/aggregate_navigation.rs`, `shell/sidebar/endpoint_*.rs`,
  `brokkr.toml`, `Cargo.lock`, `crates/shepr-client/Cargo.toml`,
  `endpoint/supervisor.rs`: unrelated edits in files the client and deps
  slices touch.

## 1. Inventory

Everything below is a place tabs exist today. Line numbers are deliberately
omitted.

### shepr-mux (data model, persistence)

- `src/workspace.rs`: `Workspace` holds `tabs: FocusedTabs` and
  `next_public_tab_number`. `FocusedTabs` (non-empty tab vector plus focused
  index, with `focus`, `push`, `remove`, `move_tab`). `valid_tabs`.
  `PaneRemovalScope::Tab`, `PaneRemovalPlan.tab_index`,
  `PaneRemoval.tab_index`, `TabRemoval`, `TabCreationOutcome`. Methods:
  `from_restored_tabs`, `tabs`, `set_tab_custom_name`, `set_tab_zoomed`,
  `focus_pane_in_tab`, `swap_panes_in_tab`, `resize_focused_pane_in_tab`,
  `resize_pane_in_tab`, `set_tab_split_ratio_at`, `from_existing_pane`
  (takes a `tab_label`), `with_first_tab`, `new_with_tab`, `active_tab`,
  `active_tab_index`, `tab_display_name`, `switch_tab`, `create_tab`,
  `create_tab_with_runtime`, `commit_new_tab`, `close_tab`, `move_tab`,
  `split_pane*` (return a tab index), `commit_new_pane` (takes a tab index),
  `next_public_tab_number`, `public_tab_number`, `find_tab_index_for_pane`,
  `resolved_identity_cwd_from` (first tab's root pane). Test helpers:
  `test_add_tab`, `public_tab_number_for_pane`, the tab half of
  `test_adversarial_identity_state` and `assert_invariants_for_test`. Tests:
  `public_tab_and_pane_ids_share_one_canonical_format`,
  `tab_public_numbers_are_stable_and_not_reused_after_close`,
  `moving_tab_keeps_active_identity_and_stable_tab_numbers`,
  `focused_tabs_*`, `workspace_identity_follows_first_tab_root_pane_cwd`.
- `src/workspace/tab.rs`: `Tab` (custom_name, number, root_pane, layout,
  panes, zoomed), `TabPane`, `ExistingPane`, `NewPane`, `DetachedPane`, and
  the pane-tree operations (`split_pane_shell`, `commit_prepared_split`,
  `close_pane`, `promoted_root_if_needed`, `has_consistent_panes`,
  `focus_pane`, `swap_panes`, `resize_*`, `set_split_ratio_at`,
  `terminal_id`, `cwd_for_pane`, `foreground_cwd_for_pane`,
  `single_pane`, `from_existing_pane`, `is_auto_named`).
- `src/workspace/geometry.rs`: `PaneGeometry::tab_panes` and doc comments
  that say "tab".
- `src/workspace/aggregate.rs`: `Tab::aggregate_state`, the tab-walking
  `Workspace::aggregate_state`, test `tab_aggregate_state_covers_only_its_own_panes`.
- `src/persist.rs`: re-exports `TabSnapshot`.
- `src/persist/snapshot.rs`: `WorkspaceSnapshot` (`public_tab_numbers`,
  `next_public_tab_number`, `tabs`, `active_tab`), `TabSnapshot`,
  `WorkspaceHistorySnapshot.tabs`, `TabHistorySnapshot`, `capture_tab`,
  `root_pane_cwd(&[TabSnapshot])`, probe keys `(workspace, tab, pane)`,
  the fingerprint's `TabLayout`, history cuts nested workspace/tab/pane.
- `src/persist/restore.rs`: `RestoredSession.dropped_tabs`, `RestoredTab`,
  `assign_public_tab_numbers`, `next_free_public_tab_number`, `restore_tab`,
  the tab loop in `restore_workspace`, `restored_pane_size` doc, and tests
  (`restore_drops_only_the_tab_with_an_invalid_split_ratio`,
  `dropped_workspaces_and_tabs_do_not_shift_the_saved_selection`,
  `missing_zero_and_duplicate_tab_numbers_get_unique_free_numbers`,
  `tab_snapshot` helper, and every fixture building `tabs: vec![...]`).
- `src/persist/io.rs`: the streamed history writer's `"tabs"` level and its
  nested loops; doc comments.
- `src/persist/writer.rs`, `src/persist/actor.rs`: test JSON with
  `"tabs"`/`"active_tab"` and tests mutating `workspaces[0].tabs[0]`.
- `src/events.rs`: `AppEvent::TabBarCommandFinished`, `TabBarCommandError`,
  `TabBarCommandFailure` (tab bar status commands).
- `src/pane/launch.rs`: scrubs `ChildEnv::SheprActive*` (status-command-only
  variables).
- `src/limits.rs`: doc mention of the workspace/tab shape.

### shepr-core

- `src/layout.rs`: doc comments naming `Tab.panes` and tab constructors.
- `src/env.rs`: `ChildEnv::SheprActiveWorkspaceId`, `SheprActiveTabId`,
  `SheprActivePaneId`, `SheprActivePaneCwd`, all "for tab-bar status
  commands". `SheprBinPath` is also set for every pane; keep it, reword its
  doc.

### shepr-protocol

- `src/command.rs`: `TabTarget`, `TabCreateParams`, `TabRenameParams`,
  `TabMoveParams`; `EndpointCommand::{TabCreate, TabFocus, TabRename,
  TabMove, TabClose}` and their `traits()` rows; `WorkspaceInfo.tab_count`,
  `WorkspaceInfo.active_tab_id`; `LayoutSetSplitRatioParams.tab_id`; docs
  on `changes_topology` and `claims_shell_geometry`.
- `src/projection.rs`: `ClientShellSnapshot.{focused_tab_id, tab_bar_right,
  tab_bar_right_separator, tabs}`, `ClientShellTabStatusSegment`,
  `ClientShellWorkspace.active_tab_id`, `ClientShellTab`,
  `ClientShellPane.tab_id`, `ClientShellAgent.tab_id`.
- `src/ids.rs`: `PublicTabId = PublicChildId<'t'>`, the `'t'` arm in
  `PublicIdParseError`, id tests.
- `src/lib.rs`: re-exports `PublicTabId`.
- `src/endpoint.rs`, `src/wire_tests.rs`: snapshot fixtures.
- `src/surface.rs`, `src/message.rs`: doc wording ("active-tab surface").

### shepr-api

- `src/schema/tabs.rs` (re-exports the three tab param types),
  `src/schema.rs` (`mod tabs`, `pub use tabs::*`), `src/schema/common.rs`
  (re-exports `TabTarget`), `src/schema/tests.rs` (tab names in
  `client_shell_commands_are_not_api_methods`; `tab.list`/`tab.get` in
  `removed_methods_are_rejected` can stay), `src/error.rs`
  (`TabCloseFailed`, `TabCreateFailed`, `TabMoveFailed`, `TabNotFound`).

### shepr-config

- `src/keybinding_table.rs`: rows `new_tab`, `rename_tab`, `previous_tab`,
  `next_tab`, `move_tab_previous`, `move_tab_next`, `close_tab`,
  `switch_tab`; help group `"workspaces / tabs"`; `last_pane` doc.
  Everything else keybinding-shaped is generated from this table (config
  fields, resolved keybinds, wire mapping, `KeybindAction`, help).
- `src/model.rs`: `ui.prompt_new_tab_name`, `ui.hide_tab_bar_when_single_tab`,
  `ui.tab_bar_position` and `TabBarPositionConfig`, `ui.tab_bar_right`,
  `ui.tab_bar_right_separator`, defaults and tests; `new_cwd` doc.
- `src/tab_bar.rs`: whole file (entry config, validation, tests).
- `src/lib.rs`: `mod tab_bar`, re-exports of `TabBarPositionConfig`,
  `TabBarRightEntryConfig`, `ValidatedTabBarRightEntry`; the default-binding
  test list.
- `src/validated.rs`, `src/wire.rs`: the same ui fields, `WireTabBarRightEntry`,
  tab bar parse and its diagnostics; `WireAgentSidebarToken::Tab`.
- `src/limits.rs`: `MAX_TAB_BAR_RIGHT_ENTRIES` and the six tab-bar command
  interval/timeout constants; doc on indexed digits.
- `src/window_title.rs`: `WindowTitleToken::Tab`, `"tab"` parse, test.
- `src/sidebar.rs`: `AgentSidebarToken::Tab`, `"tab"`, default agent rows,
  tests.
- `src/io.rs`, `src/keybinds.rs`: tests using `tab_bar_right` and tab
  bindings as sample keys (`next_tab`, `new_tab`, `close_tab`, `switch_tab`).
- `src/theme.rs`: doc ("Background for the tab bar").
- `src/default.toml`: tab keys, `prompt_new_tab_name`, tab bar keys,
  `{tab}` token, `"tab"` in agent rows, cwd doc.
- `Cargo.toml`: `time` (used only by `tab_bar.rs`).

### shepr-platform

- `src/host.rs`: `local_datetime`, `datetime_from_tm` (only the tab bar
  clock uses them); `src/lib.rs` re-export; `Cargo.toml` `time`.

### shepr-termio

- `src/input/keybind_help.rs`: `"workspaces / tabs"` group, tests listing
  tab rows.
- `src/input/keybindings.rs`: tests using `NextTab` and `SwitchTab(0)`.

### shepr-server: app

- `src/app/state.rs`: `TabAreaKey`, `TabBarStatusSegment`,
  `AppState.{active_tab_id, tab_areas, tab_bar_right,
  tab_bar_right_separator}`, `refresh_active_tab_id`,
  `pane_geometry_for_tab`, `tab_layout_area`, `tab_area`, `record_tab_area`,
  `has_tab_without_area`, `retain_live_tab_areas`, `default_layout_area`,
  `test_record_all_tab_areas`, invariants and tests.
- `src/app/mod.rs`: `mod tab_bar_status`, `App.tab_bar_status`,
  `configure_tab_bar_status` call, `dropped_tabs` handling on restore,
  `active_tab_id`/`tab_areas`/`tab_bar_right` initialisers, tests
  (`tab_bar_command_events_render_only_when_visible_output_changes`,
  `tab_info_number_uses_stable_public_tab_number`,
  `bare_tab_position_is_rejected_even_when_public_numbers_differ`,
  `pane_close_request_closes_only_the_target_tab_when_other_tabs_exist`,
  split-in-background-tab test, and every `tabs()[0]` access).
- `src/app/tab_bar_status.rs`: whole file (status runtime, command spawner,
  env for status commands, datetime refresh, tests).
- `src/app/ids.rs`: `public_tab_id`, `tab_index_for_pane`,
  `parse_tab_id`, `resolve_tab_id`, tests.
- `src/app/actions.rs`: `public_tab_id_for_index`, `TabRemovalScope`,
  `TabRemovalPlan`, `TabRemovalOutcome`, `TabRemovalCommit`,
  `PaneCreationOutcome.tab_index`, `PaneContext.tab_index`, zoom docs.
- `src/app/actions/workspace.rs`: tab logging in `switch_workspace`,
  `switch_workspace_tab`, `prepare_tab_removal`, `commit_tab_removal`,
  test-only `switch_tab`, `remove_active_tab`, tab walks in
  `terminal_ids_for_workspace`, `pane_ids_for_workspace`,
  `remove_unattached_terminal_ids`, `prepare_pane_removal_by_id`,
  `commit_pane_removal` (`tab_index`, `refresh_active_tab_id`).
- `src/app/actions/focus.rs`: `resolve_pane_context` (tab index),
  `commit_workspace_creation` (`active_tab().root_pane()`),
  `commit_tab_creation`, `commit_pane_split` (tab index),
  `focus_pane_in_workspace` (via `switch_workspace_tab` and
  `focus_pane_in_tab`).
- `src/app/actions/pane.rs`: `apply_pane_zoom` (tab zoom),
  `focused_tab_layout`, `swap_pane`, `resize_pane` (test-only helpers).
- `src/app/actions/events.rs`: `TabBarCommandFinished` arm.
- `src/app/actions/tests.rs`: tab creation, close-tab tests and every
  tab-indexed access.
- `src/app/api.rs`: `mod tabs`, the five `EndpointCommand::Tab*` dispatch
  arms, pane-exit tests that add a second tab.
- `src/app/api/tabs.rs`: whole file.
- `src/app/api/layouts.rs`: `resolve_layout_target` (tab id, pane, or
  active tab), `set_tab_split_ratio_at`, tests.
- `src/app/api/panes.rs`, `api/panes/geometry.rs`, `api/panes/tests.rs`:
  tab index threading for split, focus direction, resize, swap
  (`swap_panes_in_tab`, `focus_pane_in_tab`, same-tab check), and tests
  (`pane_close_of_a_tabs_last_pane_closes_the_tab`,
  `pane_zoom_on_an_unfocused_pane_of_a_zoomed_tab_moves_focus`,
  `pane_focus_focuses_direct_target_across_tabs_and_workspaces`,
  `lay_out_first_tab`, `tab_pane_order`).
- `src/app/api/workspaces.rs`, `api/cwd.rs`, `api/detect.rs`,
  `api/session.rs`: `resolved_new_workspace_cwd_from_tab` call, tests using
  `handle_tab_create`, `test_add_tab`, `tabs()[0]`.
- `src/app/api_helpers.rs`: `tab_not_found` and its test.
- `src/app/creation.rs`: `launch_cwd_for_pane_in_workspace` (via tab),
  `resolved_new_workspace_cwd_from_tab`, `pane_info` focus check via
  `active_tab_index`, `workspace_info` (`tab_count`, `active_tab_id`).
- `src/app/agents.rs`: agent info walks tabs and emits `tab_id`.
- `src/app/agent_resume.rs`: per-tab candidate walk
  (`tab_has_pending_agent_resume`, `resume_layout_area(ws, tab)`,
  `pending_agent_resume_pane_infos(tab, area)`,
  `derived_pending_agent_resume_pane_infos(tab, ..)`), tests
  (`pending_agent_resume_launches_inactive_tab_panes_with_current_terminal_area`,
  `pending_agent_resume_launches_zoom_hidden_active_tab_panes`) and
  `test_record_all_tab_areas` calls.
- `src/app/session.rs`: test
  `a_restore_that_drops_a_tab_backs_up_the_saved_session_before_the_first_save`.
- `src/app/snapshot_tests.rs`: `TabSnapshot` fixtures,
  `capture_contract_tracks_workspace_and_tab_names_and_active_tab`,
  `capture_contract_tracks_tab_closure`, `active_tab_default_is_zero`.
- `src/app/window_title.rs`: `window_title_for(ws, tab)`,
  `window_title_for_target`, `WindowTitleToken::Tab`, test
  `renders_workspace_and_tab_names`.
- `src/app/events.rs`, `src/app/runtime.rs`, `src/app/terminal_titles.rs`:
  `TabBarCommandFinished` routing, `next_tab_bar_status_deadline`,
  `tabs()` walks and test accesses.
- `src/ui.rs`, `src/ui/tab_surface.rs`: `TabSurfaceTarget` (workspace id plus
  tab id, `from_indices`, `resolve -> (ws, tab)`), `TabSurfaceLayout`,
  `TabSurfaceView`, `compute_tab_surface_for`, `resize_tab_surface`,
  `render_tab_surface`, `tab_surface_hyperlinks`, `tab_surface_cursor`,
  test `target_tracks_tab_across_position_changes`.
- `src/ui/panes.rs`: `compute_pane_infos_for_tab`, `resize_pane_infos`
  (tab index), render lookups through the tab, the active-tab helper, tests
  using `focus_pane_in_tab`, `set_tab_zoomed`, `tabs()[0]`.
- `src/logging.rs`: `tab_focused`, `tab_closed`, `tab_renamed`.
- `src/limits.rs`: `DATETIME_REFRESH_INTERVAL`, `MAX_TAB_BAR_TEXT_CHARS`,
  `TAB_BAR_COMMAND_SHELL`, `TAB_BAR_COMMAND_SHELL_ARGS`,
  `TAB_BAR_STATUS_READ_BUFFER_BYTES`, `MAX_COMMAND_LINE_BYTES` if it has no
  other user.
- `src/test_support.rs`: `WorkspaceFixture::test_add_tab`, the tab parts of
  `test_adversarial_identity_state` and `assert_invariants_for_test`,
  `Tab`/`TabPane` imports.
- `Cargo.toml`: `time`.

### shepr-server: headless loop

- `src/server/clients.rs`: `geometry_controllers: HashMap<PublicTabId,
  ClientId>` and its accessors (`geometry_controller`,
  `set_geometry_controller`, `claim_geometry`, `claim_unowned_geometry`,
  `retain_geometry_controllers`); `ClientShellLocation.active_tab_ids`,
  `focused_tab_id`, `focus_tab`, `reconcile`; `ClientShellTopology.
  {active_tab_ids, tab_workspace_ids}`; tests.
- `src/server/headless/client_views.rs`: `TabGeometrySource`,
  `tab_geometry_source` (the PTY size rule), `default_shell_target`,
  `shell_target_for_client`, `tab_id_for_target`, `shell_tab_id_for_client`,
  `client_shell_topology`, `reconcile_client_shell_locations`,
  `focus_shell_client_on_tab`, `set_default_shell_target_from_client`
  (`switch_workspace_tab`), `focus_shell_client_on_default_target`,
  `focus_target_for_surface`, `shell_client_views_pane` (tab zoom),
  `tab_geometry`, `apply_tab_geometry`, `apply_all_tab_geometry`,
  `finish_shell_tab_geometry_change`, `is_geometry_source`,
  `reapply_controlled_shell_tab_geometry`, `claim_shell_tab_geometry`,
  `claim_unowned_shell_tab_geometry`, `resize_shell_tabs_sized_for`, module
  doc, test.
- `src/server/headless/endpoint_requests.rs`: `TabFocus` arm and the
  `PaneFocus` arm that goes through the pane's tab; geometry calls.
- `src/server/headless/render.rs`: focused-pane lookup through the tab,
  visible pane sets (tab zoom), `tab_has_synchronized_pane`,
  `has_tab_without_area`, `tab_geometry_source` gate.
- `src/server/headless/surface_interest.rs`, `internal_events.rs`,
  `src/server/headless.rs`: geometry calls, `window_title_for(ws, tab)`,
  `handle_tab_bar_status_tasks`, status deadline, docs.
- `src/server/client_shell.rs`: projection of tabs, `focused_tab_id`,
  per-workspace `active_tab_id`, `new_workspace_cwd` from the active tab,
  `zoomed` and `tab_bar_right`; `render_pane_surface` target; test
  `snapshot_state_fields_follow_ids_not_positions`.
- `src/server/render_stream.rs`: `RenderedTabSurface`,
  `render_tab_surface_virtual`.
- `src/server/headless/tests/mod.rs` (the largest: tab focus per client,
  background tab create, remembered tabs, per-tab geometry controllers,
  retained patches for a dirty tab, `TabRename` round trip),
  `tests/surface_interest.rs` (`focused_surface_reassertion_reclaims_tab_geometry`,
  `state.tab_area`).

### shepr-client

- `src/shell.rs`: imports `ClientShellTab`, `TabBarPositionConfig`.
- `src/shell/state.rs`: `ClientShellConfig.{tab_bar_position,
  hide_tab_bar_when_single_tab, prompt_new_tab_name}`,
  `ClientShellLayout.tab_bar`, `ShellHitMap.{tabs, new_tab,
  tab_scroll_left, tab_scroll_right}`, `ClientTabPress`,
  `ClientChromeDrag::Tab`, `ClientChromeDrag::PaneSplit.tab_id`,
  `ClientRenameTarget::{NewTab, Tab}`, `ClientContextMenuAction::NewTab`,
  `ClientContextMenuTarget::Tab`, `ClientTabCloseConfirmation`,
  `ClientConfirmCloseOverlay.tab_target`, state fields `tab_press`,
  `tab_scroll`, `reveal_focused_tab`, `last_tab_bar_width`,
  `layout_with_tab_count`, `surface_size_with_tab_count`, the
  `tab_layout_changed` check on snapshot receipt.
- `src/shell/presentation/tabs.rs`: whole file (`render_tab_bar`,
  `tab_bar_status_width`, tab strip, ZOOM marker).
- `src/shell/presentation/render.rs`: `mod tabs`, tab bar render call,
  `ShellRenderState.{tab_scroll, reveal_focused_tab,
  tab_drag_insert_index}`, hit clearing. (The `"tab"` key label in the
  navigate mode bar is the Tab key; keep it.)
- `src/shell/presentation/composition.rs`: tab bar width tracking,
  `ClientChromeDrag::Tab`, mode bar placed on a bottom tab bar, tab hit
  clearing, comments.
- `src/shell/presentation/config.rs`: config fields, `layout(.., tab_count,
  ..)` and the tab bar rect, test asserting `new_tab` label.
- `src/shell/presentation/surface_patch.rs`: comment.
- `src/shell/input/mouse.rs`: `pane_split_target_is_current` (focused tab),
  `tab_drop_index_at`, tab press, drag and drop (`TabMove`), split drag
  (`LayoutSetSplitRatio.tab_id`), tab right-click menu, wheel over the tab
  strip, new-tab button, tab scroll buttons.
- `src/shell/input/input.rs`: `KeybindAction::SwitchTab` direct handling.
- `src/shell/input/copy_mode.rs`: mode bar placement on a bottom tab bar.
- `src/shell/navigation/actions.rs`: `CloseTab`, `NewTab`, `RenameTab`
  intercepts, `TabFocus`/`TabCreate` in focus classification, command
  construction for `SwitchTab`, `PreviousTab`/`NextTab`,
  `MoveTabPrevious`/`MoveTabNext`, `NewTab`, and pane cycling filtered by
  `pane.tab_id == focused_tab`.
- `src/shell/navigation/aggregate_navigation.rs`: navigator rows grouped
  workspace, tab, pane; tab labels in pane rows.
- `src/shell/overlays/context_menu.rs`: tab menu (`open_tab_context_menu`,
  `activate_tab_context_action`, New tab/Rename/Close).
- `src/shell/overlays/overlay_input.rs`: `open_new_tab_overlay`,
  `open_rename_tab_overlay`, `TabCreate`/`TabRename` submission,
  `request_tab_close`, last-tab close confirmation.
- `src/shell/sidebar/agent_sidebar.rs`: row index kinds `Tab` and
  `TabWorkspace`, `tab()`, `tab_count()`, tab label resolution.
- `src/shell/sidebar/token_definitions.rs`, `sidebar_tokens.rs`:
  `ResolvedTokenKind::Tab`, `tab` context field.
- `src/shell/endpoints.rs`: `focused_tab_count`, `workspace_tab_count`,
  `endpoint_surface_size`, `snapshot_surface_size`.
- `src/shell_runtime.rs`, `src/lib.rs`: `endpoint_terminal_geometry`,
  `handoff_geometry` (per-side sizing), `resize_handoff_for_snapshot`, the
  empty-snapshot fixture.
- `src/endpoint/activation/model.rs`, `activation.rs`,
  `activation_tests.rs`: `HandoffGeometry {source, target}`, which exists
  because the two endpoints can lay out differently; the only source of that
  difference is the tab bar (the doc on `endpoint_surface_size` says so).
- `src/limits.rs`: tab strip widths and buttons.
- Tests: `shell/tests/close_tab.rs` (whole file), `shell/tests/mod.rs`
  (snapshot fixture), `chrome_context.rs`, `mouse_selection.rs`, `copy.rs`,
  `endpoints.rs`, `text_editing.rs`, `workspace_navigation.rs`,
  `src/tests/mod.rs` (`test_tab_id`), `src/tests/codec.rs` (config with tab
  keys, `{tab}` token, `"tab"` sidebar token, `__TAB_BAR_POSITION__`).

### Docs

- `AGENTS.md`: the opening paragraph ("Workspaces, tabs and panes"), Scope
  ("Workspaces, tabs, panes, layout, the tab bar"), the JSON API and CLI
  paragraphs ("workspaces, tabs or panes"), the "Render is pure" principle
  (`compute_tab_surface_for`, `ui/tab_surface.rs`, "one tab"), and the
  "Presentation is per client" principle (`tab_geometry_source`, "each
  tab's applied area").
- `notes/todo.md`: the "Flatten workspaces and tabs" item.
- No `docs/` or `reference/` folder exists yet.

Agent manifests (`crates/shepr-agent/src/detect/manifests/*.toml`) mention
the Tab key, not shepr tabs. Nothing to do there.

## 2. Target model

### Workspace

A workspace is exactly one pane tree. The fields of today's `Tab` move onto
`Workspace`; nothing else changes about its identity, Git and label caching.

```rust
pub struct Workspace {
    pub id: WorkspaceId,
    pub custom_name: Option<String>,
    pub identity_cwd: PathBuf,
    pub cached_identity_cwd: PathBuf,
    pub cached_auto_label: String,
    pub cached_git_status_key: PathBuf,
    pub cached_git_branch: Option<String>,
    pub cached_git_ahead_behind: Option<AheadBehind>,
    pub cached_git_space: Option<GitSpaceMetadata>,
    pub next_public_pane_number: usize,
    // formerly Tab:
    pub(crate) root_pane: PaneId,
    pub(crate) layout: TileLayout,
    pub(crate) panes: HashMap<PaneId, WorkspacePane>,
    pub(crate) zoomed: bool,
}
```

The tab's own `custom_name` and `number` are gone. A workspace's name is
its `custom_name` or its Git-derived label, as today. Identity follows the
workspace's root pane cwd (was: first tab's root pane).

### Ids

`PublicTabId` is deleted. Wherever a tab id was carried, the workspace id
takes its place: a location, a surface target, a geometry controller key, a
layout target, the recorded layout area. Public pane ids (`w1:p3`) are
unchanged. With only one child kind left, `PublicChildId<const KIND>`
should collapse to a plain `PublicPaneId` struct (the `'p'` letter stays in
the canonical text); every other crate only names `PublicPaneId`, so this is
local to `ids.rs`.

### Per-workspace geometry record

`AppState::tab_areas: HashMap<TabAreaKey, Rect>` becomes
`workspace_areas: HashMap<usize, Rect>` keyed by `WorkspaceId::number()`
(no allocation on lookup, the reason `TabAreaKey` avoided `PublicTabId`).
`TabAreaKey` is deleted. The PTY size rule (`tab_geometry_source`) becomes
`workspace_geometry_source`, keyed by `WorkspaceId`; its rule text is
unchanged with "tab" read as "workspace". The per-client location loses its
per-workspace remembered tab map: it is only the focused workspace id.

### TUI

The tab bar row goes. The pane surface is the whole main area (screen minus
sidebar), independent of anything in the snapshot. The mode bar already
falls back to the pane area's bottom row (or top row when the copy cursor
sits on the bottom one); only the "bottom tab bar" branch goes.

The tab bar's right-hand status (zoom marker, hostname, datetime, text,
command segments) has no remaining home. Recommended: delete the whole
subsystem (config, validation, wire, server runtime, command spawner,
`ChildEnv::SheprActive*`, `AppEvent::TabBarCommandFinished`,
`local_datetime`, and the `time` dependency). Owner choice, see questions.

The zoom state stays on the server (it changes layout and geometry). With
the tab bar gone the client has no zoom indicator. Recommended: none (a
zoomed workspace is visibly one full-size pane). Owner choice.

Since chrome is then entirely client-owned, every endpoint lays out the
same pane surface for a given host size. `HandoffGeometry {source, target}`
and the per-endpoint sizing (`endpoint_surface_size`,
`snapshot_surface_size`, `workspace_tab_count`, `focused_tab_count`,
`resize_handoff_for_snapshot`) can collapse to the one
`ClientShellState::surface_size`. Recommended as part of this change; see
the client slice for the fallback.

### Keybindings

Deleted: `new_tab`, `rename_tab`, `previous_tab`, `next_tab`,
`move_tab_previous`, `move_tab_next`, `close_tab`, `switch_tab`, and config
`ui.prompt_new_tab_name`. Help group `"workspaces / tabs"` becomes
`"workspaces"`.

Recommended repurposing (owner choice), taking over the freed defaults:

| key | today | proposed |
|---|---|---|
| `prefix+n` | `next_tab` | `next_workspace` (today unset) |
| `prefix+p` | `previous_tab` | `previous_workspace` (today unset) |
| `prefix+1..9` | `switch_tab` | `switch_workspace` (today unset) |
| `prefix+c` | `new_tab` | unbound, or a second `new_workspace` trigger |
| `prefix+shift+t` | `rename_tab` | unbound (`rename_workspace` is `prefix+shift+w`) |
| `prefix+shift+x` | `close_tab` | unbound (`close_workspace` is `prefix+shift+d`) |

No `move_workspace_*` key is proposed; the sidebar drag and
`WorkspaceMove` already cover it.

### Tokens

`{tab}` in `ui.window_title` and the `"tab"` agent sidebar token are deleted
(a config using them fails the launch, as any unknown token does). Default
agent rows become `[["state_icon", "machine", "workspace"], ["agent"]]`.

### Session file

`WorkspaceSnapshot` absorbs `TabSnapshot`'s layout fields. History drops its
tab level. A layout defect (invalid split ratio, no surviving pane) now drops
the whole workspace; `RestoredSession.dropped_tabs` becomes
`dropped_workspaces` and still triggers the backup-before-first-save.

## 3. Slices

Five code slices, disjoint by file, plus the orchestrator's doc and
integration pass. Slices 1 and 2 define the interfaces; slices 3, 4 and 5
code against the signature lists below, so all five can run in parallel.
Integrate with one `brokkr check` (it regenerates `Cargo.lock`).

Renames in this spec are part of the contract; a slice must not keep an old
name as an alias.

### Contract A: shepr-mux `workspace` API (owned by slice 1)

Kept or moved types:

```rust
pub struct PaneSpawnHandles { .. }                  // unchanged
pub struct ExistingPane { pub pane_id: PaneId, pub pane: WorkspacePane }
pub struct WorkspacePane { pub pane_state: PaneState, pub public_number: usize }
    // was TabPane; Deref/DerefMut to PaneState; fn new(PaneState) -> Self
pub struct NewPane { pub pane_id, pub terminal, pub runtime, pub prepared_layout }
pub enum PaneRemovalScope { Pane, Workspace }       // Tab deleted
pub struct PaneRemovalPlan { pub pane_id: PaneId, pub scope: PaneRemovalScope, .. }
pub struct PaneRemoval {
    pub workspace_id: WorkspaceId, pub pane_id: PaneId, pub scope: PaneRemovalScope,
    pub pane_ids: Vec<PaneId>, pub terminal_ids: Vec<TerminalId>,
}                                                    // tab_index deleted
pub use geometry::{PaneChromeInfo, PaneGeometry, apply_pane_chrome, layout_rect,
                   pane_inner_rect, terminal_content_rect};
```

`PaneGeometry::tab_panes(&self, &TileLayout, zoomed: bool)` is renamed
`visible_panes` (same signature). `pane_size` and `sole_pane_size` unchanged.

Deleted: `Tab`, `TabPane`, `TabRemoval`, `TabCreationOutcome`,
`FocusedTabs`, `workspace/tab.rs` (its pane-tree code moves into
`Workspace`; a helper module such as `workspace/pane_tree.rs` is the slice's
call).

`impl Workspace`:

```rust
// construction
pub fn new_with_extra_env(initial_cwd: &Path, rows: u16, cols: u16,
    scrollback_limit_bytes: usize, host_terminal_theme: TerminalTheme,
    host_terminal_appearance: Option<HostAppearance>, shell_config: PaneShellConfig<'_>,
    spawn: &PaneSpawnHandles, extra_env: Vec<(String, String)>,
) -> io::Result<(Self, TerminalState, PaneRuntime)>          // unchanged
pub fn from_existing_pane(label: Option<String>, identity_cwd: &Path,
    existing: ExistingPane) -> Self                          // tab_label dropped
pub(crate) fn from_restored(id: WorkspaceId, custom_name: Option<String>,
    identity_cwd: PathBuf, root_pane: PaneId, layout: TileLayout,
    panes: HashMap<PaneId, WorkspacePane>, zoomed: bool,
    next_public_pane_number: usize) -> Option<Self>          // replaces from_restored_tabs

// reads
pub fn root_pane(&self) -> PaneId
pub fn layout(&self) -> &TileLayout
pub fn panes(&self) -> &HashMap<PaneId, WorkspacePane>
pub fn zoomed(&self) -> bool
pub fn focused_pane_id(&self) -> PaneId                      // unchanged
pub fn contains_pane(&self, pane_id: PaneId) -> bool         // replaces find_tab_index_for_pane(..).is_some()
pub fn shows_pane(&self, pane_id: PaneId) -> bool            // in layout, and focused when zoomed
pub fn pane_state(&self, PaneId) -> Option<&PaneState>       // unchanged
pub fn pane_state_mut(&mut self, PaneId) -> Option<&mut PaneState>
pub fn terminal_id(&self, PaneId) -> Option<&TerminalId>
pub fn public_pane_number(&self, PaneId) -> Option<usize>
pub fn pane_id_for_public_number(&self, usize) -> Option<PaneId>
pub fn pane_count(&self) -> usize
pub fn next_public_pane_number(&self) -> usize
pub fn cwd_for_pane(&self, PaneId, &HashMap<TerminalId, TerminalState>,
    &PaneRuntimeRegistry) -> Option<PathBuf>                 // moved from Tab
pub fn foreground_cwd_for_pane(&self, PaneId, &PaneRuntimeRegistry) -> Option<PathBuf>
pub fn resolved_identity_cwd_from(&self, ..) -> Option<PathBuf>  // root pane now
pub fn display_name(&self) -> String                         // unchanged
pub fn branch(&self) / git_ahead_behind(&self) / set_custom_name / mark_identity_undiscovered
pub fn aggregate_state(&self, &HashMap<TerminalId, TerminalState>) -> AgentState

// layout mutations (were *_in_tab with a tab index)
pub fn set_zoomed(&mut self, zoomed: bool)
pub fn focus_pane(&mut self, pane_id: PaneId) -> bool
pub fn swap_panes(&mut self, first: PaneId, second: PaneId) -> bool
pub fn resize_focused_pane(&mut self, NavDirection, delta: f32, area: core::Rect) -> bool
pub fn resize_pane(&mut self, PaneId, NavDirection, delta: f32, area: core::Rect) -> bool
pub fn set_split_ratio_at(&mut self, path: &[SplitBranch], ratio: f32) -> bool

// panes
pub fn split_pane(&self, pane_id, direction, geometry: &PaneGeometry, cwd, default_cwd,
    scrollback_limit_bytes, host_terminal_theme, host_terminal_appearance,
    shell_config, extra_env, focus_new_pane: bool, spawn: &PaneSpawnHandles,
) -> Option<io::Result<NewPane>>                             // tab index dropped
pub fn split_pane_with_ratio(&self, pane_id, direction, ratio: f32, ..same..)
    -> Option<io::Result<NewPane>>
pub fn commit_new_pane(&mut self, pane_id: PaneId, prepared_layout: TileLayout,
    terminal_id: TerminalId, focus: bool) -> Option<()>      // tab index dropped
pub fn prepare_pane_removal(&self, PaneId) -> Option<PaneRemovalPlan>
pub fn remove_pane(&mut self, &PaneRemovalPlan) -> Option<PaneRemoval>
```

Scope rule: `Pane` when the workspace has more than one pane, else
`Workspace` (the caller removes the workspace, as today).

Deleted methods: `tabs`, `active_tab`, `active_tab_index`,
`tab_display_name`, `switch_tab`, `create_tab`, `commit_new_tab`,
`close_tab`, `move_tab`, `set_tab_custom_name`, `set_tab_zoomed`,
`focus_pane_in_tab`, `swap_panes_in_tab`, `resize_focused_pane_in_tab`,
`resize_pane_in_tab`, `set_tab_split_ratio_at`, `find_tab_index_for_pane`,
`public_tab_number`, `next_public_tab_number`, `Tab::aggregate_state`.

Crate-internal test helpers in shepr-mux (`#[cfg(test)]`): `test_new`,
`test_split`, `test_adversarial_identity_state` (pane-number divergence
only), `assert_invariants_for_test` (pane checks only), `close_pane`,
`resolved_identity_cwd`. `test_add_tab` is deleted. The server's
`WorkspaceFixture` trait mirrors these on the public API (slice 3).

Persistence types (public, used by slice 3 tests):

```rust
pub struct WorkspaceSnapshot {
    pub id: Option<String>,
    pub custom_name: Option<String>,
    pub identity_cwd: PathBuf,
    pub public_pane_numbers: HashMap<u32, usize>,
    pub next_public_pane_number: usize,
    pub layout: LayoutSnapshot,
    pub panes: HashMap<u32, PaneSnapshot>,
    pub zoomed: bool,
    pub focused: Option<u32>,
    pub root_pane: Option<u32>,
}
pub struct WorkspaceHistorySnapshot { pub panes: HashMap<u32, PaneHistorySnapshot> }
pub struct RestoredSession { .., pub dropped_workspaces: usize }   // was dropped_tabs
```

`TabSnapshot`, `TabHistorySnapshot` are deleted. `restore(..)` keeps its
signature.

Events: `AppEvent::TabBarCommandFinished`, `TabBarCommandError`,
`TabBarCommandFailure` are deleted (only if the owner confirms deleting the
status segments; otherwise keep and rename in a later pass).

### Contract B: wire and config (owned by slice 2)

shepr-protocol `command.rs`:

- Deleted: `TabTarget`, `TabCreateParams`, `TabRenameParams`,
  `TabMoveParams`, `EndpointCommand::{TabCreate, TabFocus, TabRename,
  TabMove, TabClose}`.
- `WorkspaceInfo { workspace_id, number, label, focused, pane_count,
  agent_status }` (`tab_count`, `active_tab_id` deleted).
- `LayoutSetSplitRatioParams { workspace_id: Option<String>, pane_id:
  Option<String>, path: Vec<bool>, ratio: f32 }` (`tab_id` renamed).
- Trait docs say "workspace or pane" and "the workspace the command acted
  on".

shepr-protocol `projection.rs`:

```rust
pub struct ClientShellSnapshot {
    pub boot_id, pub revision, pub resolved_config,
    pub focused_workspace_id: Option<WorkspaceId>,
    pub focused_pane_id: Option<PublicPaneId>,
    pub workspaces: Vec<ClientShellWorkspace>,
    pub panes: Vec<ClientShellPane>,
    pub agents: Vec<ClientShellAgent>,
}
pub struct ClientShellWorkspace {           // active_tab_id deleted
    workspace_id, new_workspace_cwd, number, label, custom_label,
    branch, git_ahead_behind, focused, agent_status,
}
pub struct ClientShellPane { pane_id, workspace_id, label, cwd, foreground_cwd,
    focused, right_click_passthrough }      // tab_id deleted
pub struct ClientShellAgent { pane_id, workspace_id, agent, terminal_title,
    terminal_title_stripped, agent_status, state_change_seq, focused }  // tab_id deleted
```

`ClientShellTab`, `ClientShellTabStatusSegment`, `PublicTabId` deleted. If
the owner wants a zoom indicator, add `zoomed: bool` to
`ClientShellWorkspace` instead.

shepr-api: delete `schema/tabs.rs`, its `mod`/`pub use`, `TabTarget`
re-export, the four `Tab*` error codes; drop the five `tab.*` names from
`client_shell_commands_are_not_api_methods`.

shepr-config (all generated from the keybinding table, so the table is the
contract): the eight tab rows are deleted, `KeybindAction::{NewTab,
RenameTab, PreviousTab, NextTab, MoveTabPrevious, MoveTabNext, CloseTab,
SwitchTab}` disappear, and the resolved `LiveKeybindConfig.keybinds` loses
the matching fields. If the owner accepts the repurposing, `next_workspace`,
`previous_workspace` and `switch_workspace` get the defaults in the table
above. `UiConfig`/validated ui/wire ui lose `prompt_new_tab_name`,
`hide_tab_bar_when_single_tab`, `tab_bar_position`, `tab_bar_right`,
`tab_bar_right_separator`; `TabBarPositionConfig`, `TabBarRightEntryConfig`,
`ValidatedTabBarRightEntry` are deleted. `WindowTitleToken::Tab` and
`AgentSidebarToken::Tab` are deleted.

### Contract C: server app and ui (owned by slice 3, used by slice 4)

`AppState` (`app/state.rs`):

```rust
// deleted: active_tab_id, tab_areas, tab_bar_right, tab_bar_right_separator,
//          TabAreaKey, TabBarStatusSegment, refresh_active_tab_id,
//          pane_geometry_for_tab, tab_layout_area, tab_area, record_tab_area,
//          has_tab_without_area, retain_live_tab_areas, test_record_all_tab_areas
pub(crate) workspace_areas: HashMap<usize, Rect>     // key: WorkspaceId::number()
pub(crate) fn workspace_index(&self, id: &WorkspaceId) -> Option<usize>
pub(crate) fn pane_geometry(&self) -> PaneGeometry   // unchanged meaning
pub(crate) fn pane_geometry_for_workspace(&self, ws_idx: usize) -> PaneGeometry
pub(crate) fn workspace_layout_area(&self, ws_idx: usize) -> Rect
pub(crate) fn workspace_area(&self, ws_idx: usize) -> Option<Rect>
pub(crate) fn record_workspace_area(&mut self, id: &WorkspaceId, area: Rect)
pub(crate) fn has_workspace_without_area(&self) -> bool
pub(crate) fn retain_live_workspace_areas(&mut self)
#[cfg(test)] pub fn test_record_all_workspace_areas(&mut self, area: Rect)
pub fn switch_workspace(&mut self, idx: usize) -> bool   // replaces switch_workspace_tab; true when idx is valid
```

Actions: `TabRemoval*`, `prepare_tab_removal`, `commit_tab_removal`,
`remove_active_tab`, `commit_tab_creation`, `switch_workspace_tab` deleted.
`PaneContext` and `PaneCreationOutcome` lose `tab_index`;
`commit_pane_split` loses its `tab_index` parameter.

`App`:

```rust
// deleted: public_tab_id, tab_index_for_pane, parse_tab_id, resolve_tab_id,
//          handle_tab_* handlers, configure_tab_bar_status,
//          handle_tab_bar_status_tasks, next_tab_bar_status_deadline,
//          handle_tab_bar_command_finished, resolved_new_workspace_cwd_from_tab
pub(crate) fn resolved_new_workspace_cwd(&self, ws_idx: usize) -> PathBuf
pub(crate) fn window_title_for(&self, workspace_index: usize) -> Option<String>
```

Whatever `App` method feeds `server/client_shell.rs` after the in-flight
session-snapshot removal must emit no tab data; slice 4 reads workspaces,
panes and agents from `app.state` directly if that change has already
inlined them.

ui (`src/ui.rs`, `ui/tab_surface.rs` renamed `ui/surface.rs`):

```rust
// TabSurfaceTarget deleted; the target is a WorkspaceId.
pub(crate) struct SurfaceLayout {
    pub(crate) target: Option<WorkspaceId>,
    pub(crate) pane_infos: Vec<PaneChromeInfo>,
    pub(crate) split_borders: Vec<SplitBorder>,
}
#[derive(Clone, Copy)]
pub(crate) struct SurfaceView<'a> {
    pub(crate) target: Option<&'a WorkspaceId>,
    pub(crate) pane_infos: &'a [PaneChromeInfo],
    pub(crate) split_borders: &'a [SplitBorder],
}
pub(crate) fn compute_surface_for(app: &AppState, runtimes: &PaneRuntimeRegistry,
    target: Option<WorkspaceId>, area: Rect) -> SurfaceLayout
pub(crate) fn resize_surface(app: &AppState, resizer: &PaneResizer<'_>,
    workspace_index: usize, area: Rect, cell_size: HostCellSize)
pub(crate) fn render_surface(app, runtimes, SurfaceView<'_>, &mut Frame<'_>)
pub(crate) fn surface_hyperlinks(app, runtimes, SurfaceView<'_>) -> Vec<((u16, u16), String, String)>
pub(crate) fn surface_cursor(app, runtimes, SurfaceView<'_>) -> Option<CursorState>
```

`crate::server::render_stream::RenderedTabSurface` and
`render_tab_surface_virtual` are in slice 4 and become `RenderedSurface` and
`render_surface_virtual`.

`crate::logging::{tab_focused, tab_closed, tab_renamed}` deleted.

### Slice 1: shepr-mux model and persistence (plus core)

Owns: `crates/shepr-mux/src/workspace.rs`, `workspace/tab.rs` (delete),
`workspace/geometry.rs`, `workspace/aggregate.rs`, any new
`workspace/*.rs`, `persist.rs`, `persist/snapshot.rs`, `persist/restore.rs`,
`persist/io.rs`, `persist/writer.rs`, `persist/actor.rs`, `events.rs`,
`pane/launch.rs`, `limits.rs`; `crates/shepr-core/src/layout.rs`,
`crates/shepr-core/src/env.rs`.

Delete: everything under "shepr-mux" and "shepr-core" in the inventory
that names a tab; `ChildEnv::SheprActive*` and their scrub arms (if the
status segments go).

Change: implement Contract A. Restore: one `restore_workspace` that maps the
old `restore_tab` body onto the workspace (layout validation, pruning, zoom
rules, resume planning, history per pane); drop public tab numbering; count
dropped workspaces. Capture: fold `capture_tab` into the workspace capture,
probe key `(workspace_index, pane_raw)`, fingerprint and history cut drop the
tab level, `io.rs` history writer drops the `"tabs"` level. Optional,
slice's call: fold `public_pane_numbers` into `PaneSnapshot`.

Tests: delete the tab-number, tab-move, `focused_tabs_*` and
`tab_aggregate_state` tests; rewrite restore tests so a bad layout drops its
workspace and the selection remaps past it
(`dropped_workspaces_do_not_shift_the_saved_selection`); keep the zoom,
pruning, public pane number, history round-trip and agent-resume tests with
the new snapshot shape; update writer/actor JSON fixtures.

### Slice 2: wire, config and dependencies

Owns: `crates/shepr-protocol/src/{command.rs, projection.rs, ids.rs,
lib.rs, endpoint.rs, wire_tests.rs, surface.rs, message.rs}`;
`crates/shepr-api/src/{schema.rs, schema/tabs.rs (delete),
schema/common.rs, schema/tests.rs, error.rs}`; all of
`crates/shepr-config/src/` and `crates/shepr-config/Cargo.toml`;
`crates/shepr-termio/src/input/{keybind_help.rs, keybindings.rs}`;
`crates/shepr-platform/src/{host.rs, lib.rs}` and its `Cargo.toml`; the
workspace `Cargo.toml` `time` line; `brokkr.toml` (drop `"time"` from the
platform, config and server allow lists).

Delete and change: Contract B. The platform half (`local_datetime`, `time`)
only if the status segments go.

Tests: rewrite `keybinds.rs` tests that use tab bindings as sample fields
onto other bindings (`new_workspace`, `next_workspace`, `split_vertical`),
keeping each test's point (conflict, reserved key, indexed syntax, direct
binding safety); update `default_keymap_is_prefix_first_and_tab_centered`
(rename) to assert the repurposed defaults; delete `tab_bar.rs` tests and
the tab bar cases in `model.rs`, `io.rs`, `validated.rs`; update
`sidebar.rs` default-row tests and `window_title.rs` token test (assert
`{tab}` is rejected); `keybind_help.rs` expected rows; `keybindings.rs`
examples onto `NextWorkspace`/`SwitchWorkspace`; `wire_tests.rs` and
`endpoint.rs` fixtures; `ids.rs` tests keep only pane ids.

### Slice 3: server app and ui

Owns: `crates/shepr-server/src/app/**` (including deleting
`app/tab_bar_status.rs` and `app/api/tabs.rs`), `src/ui.rs`, `src/ui/*.rs`
(renaming `tab_surface.rs` to `surface.rs`), `src/logging.rs`,
`src/limits.rs`, `src/test_support.rs`, `crates/shepr-server/Cargo.toml`.

Delete: everything under "shepr-server: app" that names a tab, the status
runtime, the `Tab*` dispatch arms.

Change: implement Contract C against Contract A. Notable rewrites:
`resolve_layout_target(workspace_id, pane_id)`; pane swap requires the same
workspace only; zoom goes through `Workspace::set_zoomed`;
`focus_pane_in_workspace` switches workspace then `Workspace::focus_pane`;
agent resume walks workspaces (`workspace_has_pending_agent_resume`,
`resume_layout_area(ws_idx)`); `workspace_info` drops tab fields;
`window_title_for(ws)`; restore logging keys on `dropped_workspaces`.

Tests: delete the tab-only tests named in the inventory (tab create, close,
move, rename, public tab number, bare tab position, background tab split,
tab closure capture, `active_tab_default_is_zero`, `inactive_tab` resume,
`target_tracks_tab_across_position_changes`,
`tab_bar_command_events_render_only_when_visible_output_changes`,
`tab_not_found`); rewrite the ones whose point survives on workspaces
(pane exit removing the workspace it empties, zoom-hidden panes resuming,
directional focus across workspaces, `a_restore_that_drops_a_workspace_backs_up_..`,
geometry follows the focused workspace's recorded area, window title
workspace token); `WorkspaceFixture` loses `test_add_tab`.

### Slice 4: server headless loop

Owns: `crates/shepr-server/src/server/**` (`clients.rs`, `client_shell.rs`,
`render_stream.rs`, `headless.rs`, `headless/*.rs`, `headless/tests/*.rs`).

Delete: `TabGeometrySource` becomes `GeometrySource`; the per-workspace
remembered tab map, `focus_tab`, `focused_tab_id`, `tab_id_for_target`,
`shell_tab_id_for_client`, the `TabFocus` navigation arm, tab bar status
tick, `tab_bar_right` projection.

Change:

- `clients.rs`: `geometry_controllers: HashMap<WorkspaceId, ClientId>`;
  `ClientShellLocation { focused_workspace_id: Option<WorkspaceId> }` with
  `focus_workspace` and `reconcile`; `ClientShellTopology {
  focused_workspace_id, fallback_workspace_id, live_workspace_ids:
  HashSet<WorkspaceId> }`.
- `client_views.rs`: `workspace_geometry_source(clients, &WorkspaceId)`;
  `shell_target_for_client -> Option<WorkspaceId>`;
  `focus_shell_client_on_workspace`; `apply_workspace_geometry`,
  `apply_all_workspace_geometry`,
  `reapply_controlled_shell_workspace_geometry`,
  `claim_shell_workspace_geometry`,
  `claim_unowned_shell_workspace_geometry`,
  `resize_shell_workspaces_sized_for`; `shell_client_views_pane` uses
  `Workspace::shows_pane`. Module doc and rule doc say "workspace".
- `endpoint_requests.rs`: `PaneFocus` moves the client to the pane's
  workspace.
- `render.rs`: visible panes via `Workspace::shows_pane` /
  `visible_panes`; `has_workspace_without_area`.
- `client_shell.rs`: project Contract B's snapshot; `new_workspace_cwd`
  from `resolved_new_workspace_cwd(ws_idx)`; delete zoom and status
  projection.
- `headless.rs`: `window_title_for(ws)`, drop the status tick and deadline.

Tests: delete tab-only tests (`client_shell_tab_focus_changes_only_the_source_connection`,
`navigation_moves_pane_focus_between_tabs_once`,
`background_tab_create_preserves_client_locations`, the "keeps remembered
tabs" half of `workspace_focus_moves_only_its_client_..`, the `TabRename`
round trip); rewrite the geometry-controller, retained-patch,
independent-resize and surface-interest tests on two workspaces instead of
two tabs (`retained_patches_only_reach_shells_viewing_the_dirty_workspace`,
`client_shell_workspaces_render_accept_input_and_resize_independently`,
`geometry_reapply_replaces_a_controller_that_left_the_workspace`,
`focused_surface_reassertion_reclaims_workspace_geometry`); the
`pty_size_rule_*` and `clients.rs` controller tests key on a workspace id.

### Slice 5: client

Owns: `crates/shepr-client/src/**` (including deleting
`shell/presentation/tabs.rs` and `shell/tests/close_tab.rs`).

Delete: everything under "shepr-client" in the inventory that names a tab:
config fields, layout `tab_bar`, hits, press/drag/scroll state, rename and
context-menu targets, new-tab and rename-tab overlays, tab close
confirmation (workspace close confirmation stays), the tab strip renderer
and limits, `SwitchTab`/`NextTab`/... dispatch, the tab level of the
navigator, the `"tab"` sidebar token, `test_tab_id`.

Change: `ClientShellConfig::layout(cols, rows, sidebar_collapsed,
sidebar_width)` with the pane surface filling the main area; the mode bar
without the bottom-tab-bar branch (composition and copy mode); split drags
carry `workspace_id` and send `LayoutSetSplitRatio { workspace_id, .. }`;
`pane_split_target_is_current` compares the focused workspace; pane
cycling filters by `pane.workspace_id`; agent sidebar resolves no tab
label; navigator rows are machine, workspace, pane (a lone-pane workspace
labels its pane by name, agent kind or title as today's single-pane tab
did). If the owner repurposes keys, nothing extra is needed here: the
existing `NextWorkspace`/`PreviousWorkspace`/`SwitchWorkspace` handling
picks up the new defaults.

Handoff: with the tab bar gone every endpoint lays out alike. Preferred:
collapse `HandoffGeometry` to one `TerminalGeometry`, delete
`endpoint_surface_size`, `snapshot_surface_size`, `focused_tab_count`,
`workspace_tab_count`, `endpoint_terminal_geometry`'s per-endpoint branch
and `resize_handoff_for_snapshot` (and its call in `lib.rs`), and rewrite the
activation test that relied on "the target's projection shows a tab bar the
source's does not". Fallback if the in-flight handoff work makes that
awkward: keep the types, make both sides the active `surface_size`, and file
the collapse in `notes/todo.md`.

Tests: delete `close_tab.rs`, the tab strip hit and drag tests in
`chrome_context.rs` and `mouse_selection.rs`, the new-tab overlay test,
`text_editing.rs` cases 2 and 3 (new tab, rename tab); rewrite the
snapshot fixture in `shell/tests/mod.rs`, `activation_tests.rs` and
`shell_runtime.rs`; `endpoints.rs` surface-size test becomes "every
endpoint gets the same surface"; `copy.rs` and `mouse_selection.rs` tests
that place the tab bar at the bottom lose that case; `codec.rs` config
drops tab keys and tokens.

### Orchestrator: docs and integration

After the slices: `AGENTS.md` (the paragraphs listed in the Docs inventory:
"Workspaces and panes", Scope drops "tabs" and "the tab bar", the Render
principle names `compute_surface_for` in `ui/surface.rs`, the Presentation
principle names `workspace_geometry_source` and "each workspace's applied
area"); `notes/todo.md` (remove the flatten item). Then one `brokkr check`,
`brokkr fmt`, and a sweep:
`rg -n "\b[Tt]abs?\b|[Tt]ab_|_tabs?\b|Tab[A-Z]|[a-z]Tab\b" crates src`
should leave only key-code `Tab`/`BackTab`, `KeyCode::Tab`, the navigate
bar's Tab-key hint, `reftable`, and removed-method names in API rejection
tests.

## 4. Risks

- In-flight edits (listed at the top) touch several files every slice
  owns. Starting before they land guarantees conflicts.
- Restore granularity: a single layout defect now loses a whole workspace
  instead of one tab. The backup-before-first-save protection still covers
  it; the loss is only what one tab used to contain.
- Contract drift between slices 3 and 4 is the likeliest integration
  failure (`AppState` geometry names, `ui` surface names). The lists above
  are the contract; do not improvise names.
- Deleting the status segments also deletes the only consumer of the
  `time` crate and of `shepr_platform::local_datetime`; `brokkr.toml`
  dependency allow lists must follow or the dependency rules fail.
- The repurposed keys change muscle memory (`prefix+n`, `prefix+p`,
  `prefix+1..9` switch workspaces, not tabs) and the reserved-prefix check
  now names `keys.next_workspace`.
- Adversarial identity tests lose the tab-number dimension; keep the
  raw-vs-public pane number divergence so id confusion is still caught.

## 5. Open questions for the owner

1. Tab bar status segments (zoom, hostname, datetime, text, command): delete
   the whole subsystem, including `time` and the `SHEPR_ACTIVE_*` status
   env (recommended), or move them somewhere (for example a sidebar footer)?
2. Zoom indicator: none (recommended), a marker on the sidebar workspace
   row, or in the pane border?
3. Keybindings: take over the freed defaults as proposed (`prefix+n`/`p`
   next/previous workspace, `prefix+1..9` switch workspace)? Should
   `prefix+c` become a second trigger for new workspace, or stay unbound?
4. With tabs gone, new workspaces are the frequent "new layout" action.
   Should `ui.prompt_new_workspace_name` default to true, as
   `prompt_new_tab_name` did?
5. Collapse `HandoffGeometry` to one geometry in this change (recommended),
   or leave it for a follow-up?
