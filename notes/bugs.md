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

## BUG-053 - Unreachable config fallbacks survive the fail-at-launch sweep

`parse_color` still warns and falls back to cyan, `palette_from_config` falls
back to catppuccin, and `validated_sidebar_bounds(..).unwrap_or((18, 36))`
remains in `client/shell/{config,state}.rs`. All only see validated config, so
the branches are dead; validated config should carry typed values instead.
(wave 2 reviewer)

## BUG-054 - Path helpers call process::exit

`config_dir()` / `state_dir()` / `config_path()` exit the process when they
cannot resolve, reachable from library code on a running server. Launch
validates first, so it fires only if the environment changes mid-process; the
resolved directories should be carried in validated config rather than
re-resolved. (wave 2 reviewer)

## BUG-055 - Test exercises a config launch now rejects

`local_keybindings_profile_publishes_the_effective_prefix_fallback` builds an
invalid-prefix config that can no longer reach that code. (wave 2 reviewer)
