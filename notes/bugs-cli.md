# CLI, config and keybinding defects

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

Findings raised in this scope but filed elsewhere: build identity (WIRE-001), Windows key code (PLAT-003).

## CMD-008 - `ui.accent = "cyan"` is ignored, and "cyan" isn't really the default

- `palette_from_config` only applies the accent when `config.ui.accent != "cyan"` (`src/app/mod.rs`). Setting cyan explicitly keeps the theme's accent, and the unset default is the theme's accent, not cyan as `DEFAULT_CONFIG` documents.
- The colour diagnostic reports a bad `ui.accent` as "using cyan" even when `theme.custom.accent` is set, in which case `ui.accent` is ignored entirely. Fixing the accent rule should make the diagnostic match it.

## CMD-009 - A preferences file overrides three config keys permanently

- `src/client/shell/config.rs` and `preferences.rs` store `sidebar_width`, `sidebar_collapsed` and `agent_panel_sort` in `state_dir()/client-shell/local-*.json`.
- After the first manual toggle or resize, the stored value wins over `ui.sidebar_width`, `ui.sidebar_start_collapsed` ("Changes take effect on the next launch") and `ui.agent_panel_sort`. Editing config then has no visible effect.

## CMD-010 - The keybinding help screen leaves out four bound actions

- `keybind_help_groups` (`src/input/keybind_help.rs`) omits `swap_pane_left`, `swap_pane_down`, `swap_pane_up` and `swap_pane_right` (defaults `prefix+shift+h/j/k/l`).
- `copy_mode` and `swap_pane_*` are also missing from `DEFAULT_CONFIG`.

## CMD-011 - Direct attach ignores the configured prefix and detach keys

- `AttachEscapeState::filter_input` (`src/client/attach.rs`) hardcodes Ctrl+B and `q`. With `keys.prefix = "ctrl+a"`, `terminal attach` and `agent attach` still intercept Ctrl+B from the pane app.
- Separately, in the legacy byte path a coalesced chunk like `abc\x02q` detaches and drops the `abc`.

## CMD-015 - A test passes for the wrong reason

- `rejects_unknown_bare_and_malformed_custom_tokens` (`src/config/sidebar.rs`) builds TOML with a literal `\\n`. The input is invalid TOML regardless of the token, so the test can never fail.

## CMD-016 - "Every CLI subcommand goes through the JSON API" is not true

- `config check`, `session list/delete`, `integration *`, `machine *` and `agent explain --file` (runs manifest evaluation in the CLI process) work locally.
- `session stop` hand-builds a raw `server.stop` request without the protocol check.
- The hunter's view: most of these are inherently local, so the documented contract needs rewording rather than the code needing change.

## CMD-017 - The meaning of "meta" is inconsistent

- In keybindings `parse_modifier_token` maps it to ALT (`src/config/keybinds.rs`). `format_key_combo` prints the META flag as "meta", and `right_click_passthrough_modifier` maps "meta" to META.

## CMD-019 - Duplicate keybinding diagnostics, missing key names, and a status probe per request

- `validated_keybinds` re-parses and re-logs every diagnostic each time `prefix_key()`, `keybinds()` or `collect_diagnostics()` is called, producing duplicate warnings.
- `pane send-keys` cannot send Home, End, PageUp, PageDown, Delete or Insert: `parse_key_combo` has no names for them, and the only tmux-style alias is `C-c`.
- Every CLI request first does a status probe (up to 15 s timeout under `--machine`). `agent start` polling therefore makes two round trips per poll.

## CMD-020 - `integration` still has its own hand parser

- Every other subcommand reads typed values from the clap `ArgMatches`. `src/cli/integration.rs` receives argv that clap has validated and parses it again by hand (its target list is now a single `INTEGRATION_TARGET_LABELS`).

## CMD-021 - Callers of changed CLI forms may remain outside `src/cli/` (unverified)

- Global options (`--session`, `--machine`, `--remote`, `--remote-keybindings`) only count before the subcommand; `workspace close --group` and `pane read --raw` are gone; `--current` now errors when the caller's pane is unknown or on another server, and no selector defaults to the caller's pane; relative `--cwd` is resolved against the caller. Hook scripts in `src/integration/assets/` were not checked for any of these.

## CMD-023 - Test-only key encoders are exported

- `src/input/mod.rs` re-exports `encode_key`, `encode_cursor_key`, `encode_mouse_scroll` and `encode_mouse_button` under `#[allow(unused_imports)]`; production uses `encode_terminal_key_with_modes` and `encode_mouse_event`. Make them `#[cfg(test)]` or delete them.
- `pane read --ansi` sends `strip_ansi: true` while `agent read --ansi` sends `false`; the output is the same, but the params disagree.
