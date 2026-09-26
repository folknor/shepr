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

## BUG-001 - Kitty unicode placeholder leaks into history reads and render

`cell_text_into` blanks `KITTY_UNICODE_PLACEHOLDER` cells, but `cell_graphemes`
(used by `screen_cell` and `screen_text_rows`) and
`RowCellIter::grapheme_text_into` (render path) do not, although the constant's
comment says it is filtered. See CON-009. (terminal-core)

## BUG-002 - TerminalTheme::is_empty ignores palette

A palette-only theme counts as empty. Hunter: "possibly a bug; worth checking
the callers". (terminal-core)

## BUG-003 - Copy-mode column maths disagree with the grid for U+FF9E/U+FF9F

`copy_mode.rs` uses `ghostty::unicode_codepoint_width` while the core applies
the halfwidth katakana voiced-mark special case. See CON-059. (terminal-core)

## BUG-004 - A typo in cjk_ime_agents disables the feature

`parse_cjk_ime_agents` silently drops unknown names, so `["typo"]` gives
`configured = true` with an empty list that matches nothing - the opposite of the
doc comment. (app-state)

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

## BUG-010 - API sends "{}" when response serialisation fails

`headless.rs:2243` - the caller gets no id and no error. `respond_to.send`
results are ignored everywhere, so caller disconnects go unrecorded. (server)

## BUG-011 - Mutating methods missing from request_changes_ui do not repaint

A new mutating method not added to the allowlist does not repaint until
something else marks the view dirty. See CON-025. (server)

## BUG-012 - PaneSurfacePatch before any full surface is accepted

Passed through with no error (`if let Some(base)`); every other update kind fails
without a baseline. See CON-032. (protocol)

## BUG-013 - Patch spans may overlap or be empty; BlitEncoder can wrap rows

The patch path accepts overlapping or zero-length spans while the delta path
rejects them. `BlitEncoder::commit_patch` bounds-checks only against the flat
slice, so `x + len > width` would wrap into the next row, relying on callers.
See CON-033. (protocol)

## BUG-014 - Surface cell limits disagree by ~7.6x

`MAX_GRID_CELLS = 1_000_000` in `decode.rs` vs `MAX_SURFACE_CELLS = 131072` in
`wire.rs`. See CON-034. (protocol)

## BUG-015 - CellData.fg doc comment is wrong

It says "0xAARRGGBB", contradicting the tag encoding. (protocol)

## BUG-016 - decode.rs compares base64 length against MAX_FRAME_SIZE

Wrong unit (base64 chars, not frame bytes); harmless only because the outer
frame is already bounded. (protocol)

## BUG-017 - render_signal: newly visible queued pane may not wake

`request_pty` wakes an immediate source only on insertion; a pane queued while
hidden and made visible via `set_immediate_pty_sources` does not wake unless
someone polls `has_immediate_work`. `request_terminal_title` returns true for
every new pane, unlike hidden PTY work coalescing. (protocol)

## BUG-018 - Host tty write failures reported as "failed to connect to server"

`set_mouse_capture(...).map_err(ClientError::ConnectionFailed)` at
`client/mod.rs:756,1217,1234` and `shell_runtime.rs:85`; the user sees "Is shepr
server running?". Proposed `HostTerminal(io::Error)`. (client)

## BUG-019 - SGR-pixels not re-enabled when exact geometry returns

The `Resize` handler drops SGR-pixels on inexact geometry but does not recompute
from `endpoint_sgr_pixels_requested`, so it stays off until the next server
`MouseCapture`. Hunter: "looks like a latent bug". See CON-006. (client)

## BUG-020 - Direct-attach Resize does not clamp cell size

`client/mod.rs:791-797`, unlike the three other sites. See CON-004. (client)

## BUG-021 - Any keybinding-source env value other than "server" is accepted

`handshake.rs:30-39`: `Some(_)` and `None` both map to `RemoteLocal`. (client)

## BUG-022 - Server reader collapses EOF, decode and IO errors

All become `ServerDisconnected`, losing the cause the loop needs for its
"reconnecting" message. (client)

## BUG-023 - Direct notices print on successful detach

`client/mod.rs:292` prints `direct_notices` to stderr even on clean exit;
hunter: "presumably intended", but notices from before a successful detach also
print. (client)

## BUG-024 - Unknown agent shows "idle" in one panel and "unknown" in another

`agent_sidebar.rs:370 sidebar_status_text` vs `shell.rs:182 status_text`; the
label override is keyed under "unknown". See CON-002. (ui)

## BUG-025 - Aggregate status has mixed authority

`project_aggregate_status` overwrites the server's tab/workspace status only when
it contains an agent; otherwise the server value survives. See CON-003. (ui)

## BUG-026 - Workspace numbering differs between local and endpoint sidebars

Local: `format!("{:<2}", index + 1)` (position); endpoint: `format!(" {}",
workspace.number)` (server number, leading space). See CON-057. (ui)

## BUG-027 - put_text iterates chars, not display width

`agent_sidebar.rs:358`: wide characters overflow `width` and can misplace cells.
See CON-059. (ui)

## BUG-028 - set_history_lines truncates events queued during set_options

Correct only because nothing else pushes concurrently. (terminal-core)

## BUG-029 - Terminal::scroll_viewport_row arithmetic is fragile

`history - row.min(history)` is safe, but bare `-` with `i32` casts elsewhere.
Hunter: "fine, just fragile". (terminal-core)

## BUG-030 - Empty or relative HOME / XDG_*_HOME produce relative data paths

`config/io.rs` `config_dir()`/`state_dir()` accept `XDG_CONFIG_HOME` /
`XDG_STATE_HOME` when empty or relative (XDG says ignore); `platform_config_dir()`
with empty `HOME` gives `.config/shepr` relative to cwd, where sockets and session
files then go; unset `HOME` falls back to `temp_dir()` (shared tmp).
`pathutil::home_dir()` rejects empty `HOME` but is not used. See CON-048.
(config-cli)

## BUG-031 - KeysConfigOverlay.clear_pane lacks skip_serializing_if

The only such field; looks accidental. (config-cli)

## BUG-032 - load_history reads without the data-dir lock

It uses `path.exists()` then reads and does not claim the lock; works only
because `load` runs first. See CON-046. (config-cli)

## BUG-033 - PermissionDenied socket counts as "not running" in session commands

`session list`/`delete` treat an existing socket whose connect fails with
`PermissionDenied` as not running; the CLI treats it as a transport error. See
CON-043. (config-cli)

## BUG-034 - AgentSessionRef validation can be bypassed

Public fields let callers skip the validating constructors; the letta test builds
`value: "default:--yolo"` directly and `plan()` trusts `value`. See STR-014.
(pane-detection)

## BUG-035 - shepr --remote accepts SSH targets with control characters

`validate_remote_target` rejects only empty and leading `-`; the saved catalog
rejects control characters. See CON-051. (remote)

## BUG-036 - Discovered remote executable can be remembered but never cached

Discovery can accept a `RemoteShepr` whose `machine_metadata()` returns `None`
under the stricter `SshMachineMetadata::is_valid`. See CON-050. (remote)

## BUG-037 - Connector failure invalidates the API bridge's metadata cache

The connector calls `metadata_cache.invalidate()` on any non-link failure, which
throws away the shared per-profile cache. See CON-050. (remote)

## BUG-038 - SSH failure classifiers misclassify

`"protocol"` as a bare substring matches unrelated ssh stderr (e.g. "Protocol
mismatch" noise), turning transient failures into Attention.
`is_remote_auth_error` is case-sensitive and lacks `"signing failed"`, unlike
`ssh_error_requires_authentication`. `attach.rs:999` hard-codes `255`. See
CON-049. (remote)

## BUG-039 - Misleading remote error text and hints

`ssh_bridge_exit_error` labels any non-zero exit with stderr as "remote SSH
connection failed", including the remote script's `exit 78` (stale metadata) and
remote command failures - load-bearing because classification is textual.
`print_remote_error_hint` tells saved-machine users to load keys "before running
`shepr --remote`". (remote)

## BUG-040 - machine add validates one profile and saves another

`add_ssh` runs twice (validate, prepare, reload, add), so the saved profile has a
different id from the validated one; a label added concurrently in between is not
rechecked for ambiguity. Hunter: harmless apart from that. (remote)

## BUG-041 - machine enable cannot take a label

`enable` accepts only a raw id, and `resolve_machine` rejects disabled machines,
so `machine enable <label>` could never work through it. See CON-056. (remote)

## BUG-042 - ClientEndpointStatus::Disabled may be dead

Not seen set in the files read; hunter suggests checking `registry.rs`.
(remote)

## BUG-043 - Resume argv shell safety is unverified

The resume argv is typed into an interactive shell; the only guard against flag
injection is the leading-dash check; shell metacharacters rely on quoting the
hunter did not verify, and `ids_are_data_not_shell_text` checks only the argv
vector, not the typed text. (pane-detection)

## BUG-044 - _agent_session_path is used despite its underscore prefix

In `session_ref_from_report`. (pane-detection)

## BUG-045 - Host mouse capture re-emitted on every resize

`refresh_host_mouse_capture` runs on every Resize event even when nothing
changed. (client)

## BUG-046 - remember_direct_notice uses Vec::remove(0)

A `VecDeque` fits. (client)

## BUG-047 - host_cell_size_query_required parameter name is stale

Declared as `kitty_graphics_enabled`, passed `pixel_geometry_enabled`. (client)

## BUG-048 - ProfileId::generate is not random

SHA-256 of pid, time and a counter - unique, but not the "opaque" the tests call
it. (remote)
