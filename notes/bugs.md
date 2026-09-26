# Bugs

Defects and oddities the 2026-09-26 design hunt turned up on the way. Unverified:
each carries the hunter's own confidence where it gave one.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## BUG-004 - Config problems fall back instead of failing the launch

Decision (owner): any config problem fails the launch. No fallbacks, no
warn-and-continue.

Known violations:
- `parse_cjk_ime_agents` silently drops unknown names, so `["typo"]` gives
  `configured = true` with an empty list that matches nothing. (app-state)
- The diagnostics machinery is built around continuing: messages such as
  "using defaults", "unknown config key", and `collect_diagnostics` rewriting
  "using cyan"; the startup banner and the per-client keybinding diagnostic
  variants exist to report problems on a running server. (config-cli, server)
- `headless_size()` falls back when `invalid_headless_size_diagnostic()` fires;
  `validated_sidebar_bounds` returns `Option` and callers fall back; the sidebar
  `split_ratio` is clamped silently. (config-cli, ui)

This is a sweep of every validator in `src/config/`, not only the sites above.
It also shrinks CON-031 and CON-045: with no running-with-bad-config state,
diagnostics only need to be printed once, at the failed launch.

## BUG-005 - ensure_default_workspace emits no created events

It calls `create_workspace_with_options` and never emits `workspace.created`,
`tab.created` or `pane.created`, a gap for subscribers after the last workspace
dies and is replaced. Hunter: "Please verify." (app-state)

## BUG-006 - Pane-death close path skips alias cleanup

`handle_pane_died` does not clear aliases when the workspace closes; the API
path clears aliases, then `close_workspace_at`, then
`remove_unattached_terminal_ids`. See CON-020. (app-state)

## BUG-007 - Possible PaneId collision after restore

`PaneId::alloc()` uses a global counter starting at 1 while persistence uses
`from_raw`; the hunter did not check whether restore bumps the counter.
(app-state)

## BUG-008 - Stale pane alias can shadow a live public id

`ids.rs::parse_pane_id` checks the alias map before the canonical form; a stale
alias could shadow a reissued public id. (app-state)

## BUG-009 - PaneStateUpdate.ws_idx can go stale

If a workspace closes between the mutation and `emit_pane_state_update`, the
carried index points elsewhere. See STR-002. (app-state)

## BUG-024 - Unknown agent shows "idle" in one panel and "unknown" in another

`agent_sidebar.rs:370 sidebar_status_text` vs `shell.rs:182 status_text`; the
label override is keyed under "unknown". See CON-002. (ui)

Decision (owner): Unknown shows as Idle everywhere in the UI.

## BUG-025 - Aggregate status has mixed authority

`project_aggregate_status` overwrites the server's tab/workspace status only when
it contains an agent; otherwise the server value survives. See CON-003. (ui)

Decision (owner): remove "seen" and the Done status entirely. Presented states
are Working, Blocked and Idle (Unknown shows as Idle, BUG-024). This dissolves
CON-003 and simplifies CON-002.

## BUG-026 - Workspace numbering differs between local and endpoint sidebars

Local: `format!("{:<2}", index + 1)` (position); endpoint: `format!(" {}",
workspace.number)` (server number, leading space). See CON-057. (ui)

Decision: show the server's workspace number, which is stable and matches the
public ids.

## BUG-027 - put_text iterates chars, not display width

`agent_sidebar.rs:358`: wide characters overflow `width` and can misplace cells.
See CON-059. (ui)

## BUG-030 - Empty or relative HOME / XDG_*_HOME produce relative data paths

`config/io.rs` `config_dir()`/`state_dir()` accept `XDG_CONFIG_HOME` /
`XDG_STATE_HOME` when empty or relative (XDG says ignore); `platform_config_dir()`
with empty `HOME` gives `.config/shepr` relative to cwd, where sockets and session
files then go; unset `HOME` falls back to `temp_dir()` (shared tmp).
`pathutil::home_dir()` rejects empty `HOME` but is not used. See CON-048.
(config-cli)

Decision (owner): follow the XDG spec (use `XDG_*_HOME` when set and absolute,
otherwise `$HOME/.config` / `$HOME/.local/state`). Anything empty, relative or
missing fails the launch; no `temp_dir()` fallback.

## BUG-031 - KeysConfigOverlay.clear_pane lacks skip_serializing_if

The only such field; looks accidental. (config-cli)

## BUG-032 - load_history reads without the data-dir lock

It uses `path.exists()` then reads and does not claim the lock; works only
because `load` runs first. See CON-046. (config-cli)

## BUG-033 - PermissionDenied socket counts as "not running" in session commands

`session list`/`delete` treat an existing socket whose connect fails with
`PermissionDenied` as not running; the CLI treats it as a transport error. See
CON-043. (config-cli)

Decision (owner): it is an error everywhere.

## BUG-049 - Agent API response send ignores a dropped receiver

`app/api/agents.rs:89` ignores the result of a response send, while the other
server/API send sites now log when the caller has gone away. (server fixer,
wave 1)

## BUG-052 - Machine catalog still accepts a legacy "enabled" field

The catalog loader accepts and ignores `"enabled"` (with a test) after the flag
was removed. No on-disk catalogs exist; remove the field handling and its test
so an unknown field is rejected. (wave 1 reviewer)

## BUG-050 - send_api_response returns a bool every caller ignores

`src/api/mod.rs`. (wave 1 reviewer)

## BUG-051 - grapheme_text_into comment claims API compatibility

Its `bytes` parameter is documented as "kept for API compatibility", which does
not apply in this fork. (wave 1 reviewer)
