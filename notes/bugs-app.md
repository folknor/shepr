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

## APP-010 - `seen` bookkeeping ignores which client is looking

Surfaced in two scopes: app core, headless server.

- The server now marks only the focused foreground client's own tab seen. The app side still uses the global `app.state.active`: `active_tab_is_seen` (`src/app/actions.rs`) respects `outer_terminal_focus` but reads the global active tab, and `Workspace::switch_tab` (reached by API `workspace focus`, `focus_pane_in_workspace`, and `apply_pane_zoom` even when the zoom is a no-op) marks every pane in the tab seen unconditionally. A scripted focus while the user is away clears "Done" markers.
- Not verified: whether `outer_terminal_focus` is reset when the last client detaches.

## APP-014 - `start_agent` rollback may not clear the managed phase

- `start_agent` rolls back a failed input with `clear_agent_name()` only. The retry test passing implies this clears the managed phase too; the hunter flagged it as worth confirming in `TerminalState`.

## APP-025 - Dead git helpers kept alive as test-only code

- `discover_workspace_git_identity`, the public `git_space_metadata` wrapper, `git_branch`, `git_symbolic_head_short`, `parse_git_head_branch` and `git_status_cache_key_for_space` (`src/workspace/git/`) have no production callers since identity and status flow through `git_status_snapshot_for_cwd_with_demand` / `repo_context`. They were gated `#[cfg(test)]` to keep their tests (HEAD parsing edge cases: oversized HEAD, worktree gitdir file, detached HEAD, reftable). Tests of code production never runs prove nothing; either port the edge-case tests onto the live path or delete the helpers with their tests.

## APP-023 - The zoomed-pane border rule exists twice

- "Zoomed pane borders = ALL when borders shown and outer borders on" is written in both `resize_tab_panes` and `compute_pane_infos_for_tab` (`src/ui/panes.rs`), and `PaneGeometry` (`src/workspace/geometry.rs`) has no zoomed case. Harmless while a split un-zooms first; same drift risk as the scrollbar rule that was just unified.
