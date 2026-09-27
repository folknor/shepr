# Structural

Shape and placement findings from the 2026-09-26 design hunt: axes that should
be types, and moves, splits and rewrites with the payoff each buys.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

# Axes that should be types

# Moves, splits and rewrites

## STR-024 - Rename src/ghostty to vt and split it

The libghostty shims and infallible `Result`s are gone. Remaining: `mod.rs`
still holds the colour model, palette, cell/style types, `Terminal`, the
`RenderState` snapshot, row/cell views and text readers; proposed `color.rs`,
`cell.rs`, `render.rs`, `read.rs` around `Terminal`, and the planned rename of
`src/ghostty` to `vt`. Several accessors are now `#[cfg(test)]`-only.

Reported by: terminal-core.

## STR-055 - Test layouts that mirror accidents

`app/mod.rs` has ~1,300 lines of API handler tests round-tripping JSON strings
(belong beside handlers, against typed results); `headless/tests/mod.rs` ≥3400
lines of whole-server tests (STR-036); `agent_resume.rs` table tests restate
`plan()` (STR-030); `stop_wait_timeout_allows_slow_graceful_shutdown` asserts a
constant equals itself; `nested_message_strings_no_longer_repeat_shepr_prefix`
tests joke strings; pairwise tests `help_advertises_only_commands_the_parser_accepts`,
`default_config_documents_every_keybinding_with_its_default`,
`live_keybinds_matches_the_separate_accessor` mark CON-044/CON-045;
`client/input.rs` `stdin_input_event_carries_raw_bytes` tests enum construction
only; the `saved.rs` "rejects invalid profiles" test becomes moot with typed
`ProfileId` (CON-052).

Reported by: app-state, server, pane-detection, config-cli, client, remote.
