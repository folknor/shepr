# App core defects

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
```

## APP-001 - `AppState` is not pure data, and the test build takes a different runtime lookup than production

- **Claim:** AGENTS.md, "`AppState` is pure data, testable without PTYs or async".
- `Tab` (`src/workspace/tab.rs:27-41`) holds `events: mpsc::Sender<AppEvent>`, `render_notify: Arc<Notify>` and `render_dirty: Arc<RenderSignal>`. These are runtime handles inside the "pure data" state. They exist only so later splits and new tabs can spawn runtimes.
- The doc comment "All application state - pure data, no channels or async runtime" sits on `TabBarStatusSegment` (`state.rs:617`), not on `AppState`.
- `AppState::runtime_for_pane_in_workspace` (`state.rs:724-746`) has `#[cfg(test)]` branches that look up `Workspace.test_runtimes` and `Tab.runtimes` before the real `TerminalRuntimeRegistry` lookup by terminal id. Any test that injects runtimes that way never exercises the production path, so a broken registry or terminal-id lookup would go unnoticed. Its doc comment ("Returns true when … focused pane") also describes a different function.
- **Suggested fix:** move the spawn context (events, notify, render signal) into `App`, delete the test-only runtime maps, and have tests insert into `terminal_runtimes`.

## APP-002 - `close_selected_workspace` leaves stale entries and doesn't do what `handle_pane_died` does

- `actions.rs:536-576`, called in production from `api/workspaces.rs:327`.
- It never prunes `pane_id_aliases` or `public_pane_id_aliases` for the closed workspace's panes. `handle_pane_died` does (`actions.rs:1508-1510`). This breaks the invariant stated in `assert_invariants_for_test` (aliases must reference live panes).
- It never resets `mode` to Navigate when the last workspace goes; `handle_pane_died` does.
- It never drops `direct_attach_resize_locks` for the removed terminals (`remove_unattached_terminal_ids` doesn't either), so those entries leak.
- The handler also sets `state.selected = index` as a side channel just to reuse this function.

## APP-003 - Metadata expiry skips the agent state-change sequence bookkeeping

- `expire_agent_metadata_at` (`actions.rs:152-204`) applies an `EffectiveStateChange` but never bumps `next_agent_state_change_seq`, `last_agent_state_change_seq` or `last_agent_completion_seq`. `update_terminal_state_with_completion_policy` does all three.
- So a state change caused by a TTL expiring is invisible in the API's `AgentInfo.state_change_seq` / `completion_seq`, and anything waiting on those sequences misses it. Both paths should go through one function.

## APP-004 - Initial PTY sizes ignore the documented `headless_size` and the real split geometry

- `App::new` restores with hard-coded `24, 80` (`mod.rs:173-177`), although `AppState.headless_size` is documented as the size used when no client is attached.
- `estimate_pane_size` (`state.rs:714-720`) returns the outer `rect` (borders included) of whichever pane is first in the current view. Splits (`api/panes.rs:58`) and new workspaces (`creation.rs:132`) spawn their child at that size, not at the new pane's own size. Argv/agent panes see a wrong initial size until the next resize.
- Restored panes also get `TerminalTheme::default()`.

## APP-005 - The workspace label has two different sources

- `display_name_from` (API `workspace_info`) resolves the cwd through the runtime (`/proc`) first.
- `display_name_from_terminals` (window title, `window_title.rs:89`) uses `terminal.cwd`, which is only updated by OSC 7.
- `automatic_display_name_for_cwd` falls back to the basename whenever the cwd differs from `cached_identity_cwd`.
- `git_refresh_deadline` returns `None` when the sidebar has no Branch/GitStatus token (a test asserts this), and identity refresh is only requested on `TerminalCwdReported`. So with such a sidebar config and a shell that doesn't emit OSC 7, `cd` into a repo subdirectory permanently shows the subdirectory name instead of the repo name. (Related: TERM-016, OSC 7 from standard shell integrations is rejected.)

## APP-006 - Blocking work on the server's main loop

- `Workspace::new_with_tab`, `from_existing_pane` and `persist::restore_workspace` each call `discover_workspace_git_identity` and `git_branch` synchronously. Those walk up to `/`, and `git_branch` spawns `git` for reftable repos.
- `App::new` then calls `git_branch` again for every restored workspace (`mod.rs:270-275`), duplicating what restore just did.
- `TerminalCwdReported` does `cwd.is_dir()` on every OSC 7 (`actions.rs:1294`).
- None of this belongs on the loop that fans frames out to every client.

## APP-007 - The git refresh can wedge permanently

- `start_git_status_refresh_if_due` spawns a bare `std::thread` (`git_refresh.rs:67`). If it panics, `GitStatusRefreshed` is never sent, `git_refresh_in_flight` stays true, and `git_refresh_deadline` returns `None` for the rest of the process.

## APP-008 - Leftover config-reload machinery

- The tab-bar generation counter, the `Drop` that kills process groups "on the reconfiguring thread", and the tests `stale_command_result_does_not_replace_reloaded_status` / `reload_aborts_an_in_flight_command_task_and_its_descendants` all exist for reconfiguration.
- `configure_tab_bar_status` is only called from `App::new`, and AGENTS.md says there is no reload. These tests cover a path that can't happen.

## APP-009 - Leftover herdr-compat ID parsing can silently target the wrong thing

Surfaced in two scopes: app core, JSON API.

- In `src/app/ids.rs`: `parse_workspace_id` accepts `w_N` and bare `N` as positional indexes. `parse_tab_id` accepts `t_…` and `ws:N` positional forms. `parse_pane_id` accepts `p_<raw>` (raw ids restart every process, so after a restart this refers to a different pane) and `ws-N`.
- A mistyped or index-style numeric id resolves to some workspace by position instead of failing. This contradicts the "stable public identity, independent of display order" comment on `Workspace.id` and the "no compatibility with upstream herdr installs" stance.
- Unverified by the app-core hunter: whether anything still writes `pane_id_aliases`. If nothing does, the alias maps and `remove_alias_shadowed_by_new_pane` are dead.

## APP-010 - `seen` bookkeeping disagrees with itself

Surfaced in two scopes: app core, headless server.

- `active_tab_is_seen` respects `outer_terminal_focus == Some(false)`, but `Workspace::switch_tab` (reached by API `workspace focus`, `focus_pane_in_workspace`, and `apply_pane_zoom` even when the zoom is a no-op) marks every pane in the tab seen unconditionally. A scripted focus while the user is away clears "Done" markers.
- Server side: `sync_foreground_client_state` (headless.rs:634-636) calls `mark_active_tab_seen()`. That marks the global `app.state.active` tab, not the foreground client's `shell_location` tab. It runs on every `StateChanged`/`HookStateReported` event and every API request. With several clients, endpoint requests from client A move `app.state.active` (`set_default_shell_target_from_client`) while B is the focused foreground; B's focus then clears "done" state on A's tab.
- Not verified (app-core hunter): whether `outer_terminal_focus` is reset when the last client detaches. If it isn't, completions in the active tab are marked seen with nobody watching.

## APP-011 - `pane_exposes_host_cursor` ignores its arguments; CJK IME config may be parsed and ignored

- `pane_exposes_host_cursor` (`state.rs:702-708`) ignores both arguments and always returns true, while `reveal_hidden_cursor_for_cjk_ime` / `cjk_ime_agents` are parsed into state.
- The hunter could not confirm whether render reads them; if not, those config keys are parsed and ignored.

## APP-012 - `encode_public_number(0)` round-trips to a different number

- `encode_public_number(0)` returns `"0"`, which decodes to 32.

## APP-013 - Restored panes without a saved public pane number get no SHEPR identity env

- Restored panes with no saved public pane number get `PaneLaunchEnv::default()`, meaning no SHEPR identity env, so their hooks can't report back (`persist/restore.rs:358-366`).

## APP-014 - `start_agent` rollback may not clear the managed phase

- `start_agent` rolls back a failed input with `clear_agent_name()` only. The retry test passing implies this clears the managed phase too; the hunter flagged it as worth confirming in `TerminalState`.

## APP-015 - Duplicate code in workspace and tab removal

- `Workspace::close_pane` vs `remove_pane`; `Tab::close_pane` vs `remove_pane`; three copies of the active-tab-after-removal adjustment; `tab_attention_priority` vs `pane_attention_priority`.

## APP-016 - `aggregate_state` tie-break depends on `HashMap` order

- The `aggregate_state` `max_by_key` tie-break depends on `HashMap` order. It's harmless today only because `pane_agent_status` ignores `seen` for Working and Blocked.

## APP-017 - `word_bounds_at_column` lives in the actions module

- `word_bounds_at_column` (double-click text logic) lives in `actions.rs`, whose module doc says it holds state mutations.
