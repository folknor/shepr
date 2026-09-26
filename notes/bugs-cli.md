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

Findings raised in this scope but filed elsewhere: `--raw` (API-004), build identity (WIRE-001), Windows key code (PLAT-003), scrollback limit not a maximum (TERM-005).

## CMD-003 - `--current` means different things on different pane commands

- Help shows `[--pane ID|--current]` everywhere.
- In `focus`, `resize`, `neighbor`, `swap` and `zoom`, `--current` sets `pane_id=None`, which the server treats as its focused pane (`src/cli/pane.rs`). An agent in a background pane running `pane resize --current` resizes some other pane.
- `layout`, `edges` and `process-info` fall back to the focused pane when `SHEPR_PANE_ID` is unset.
- `split` and `input` return an error instead ("--current requires SHEPR_PANE_ID").
- `split` defaults to the caller's pane even without `--current`; the others don't.
- `SHEPR_PANE_ID` is also used with `--session other` or a socket override, so it points at a pane on a different server.
- The clap rewrite kept all three meanings on purpose and commented them in `pane.rs`.

## CMD-006 - `shepr config reset-keys` is advertised but doesn't exist

Surfaced in two scopes: CLI/config, platform.

- It is listed under "Common commands" in the top-level help (`src/main.rs`); only `config check` exists. With clap as the parser it is now a usage error.

## CMD-008 - `ui.accent = "cyan"` is ignored, and "cyan" isn't really the default

- `palette_from_config` only applies the accent when `config.ui.accent != "cyan"` (`src/app/mod.rs:141`). Setting cyan explicitly keeps the theme's accent, and the unset default is the theme's accent, not cyan as `DEFAULT_CONFIG` documents.
- The new colour diagnostic reports a bad `ui.accent` as "using cyan" even when `theme.custom.accent` is set, in which case `ui.accent` is ignored entirely. Fixing the accent rule should make the diagnostic match it.

## CMD-009 - A preferences file overrides three config keys permanently

- `src/client/shell/config.rs:182-193` and `preferences.rs` store `sidebar_width`, `sidebar_collapsed` and `agent_panel_sort` in `state_dir()/client-shell/local-*.json`.
- After the first manual toggle or resize, the stored value wins over `ui.sidebar_width`, `ui.sidebar_start_collapsed` ("Changes take effect on the next launch") and `ui.agent_panel_sort`. Editing config then has no visible effect.

## CMD-010 - The keybinding help screen leaves out four bound actions

- `keybind_help_groups` (`src/input/keybind_help.rs:141-183`) omits `swap_pane_left`, `swap_pane_down`, `swap_pane_up` and `swap_pane_right` (defaults `prefix+shift+h/j/k/l`).
- `copy_mode` and `swap_pane_*` are also missing from `DEFAULT_CONFIG`.

## CMD-011 - Direct attach ignores the configured prefix and detach keys

- `AttachEscapeState::filter_input` (`src/client/attach.rs:50-65`) hardcodes Ctrl+B and `q`. With `keys.prefix = "ctrl+a"`, `terminal attach` and `agent attach` still intercept Ctrl+B from the pane app.
- Separately, in the legacy byte path a coalesced chunk like `abc\x02q` detaches and drops the `abc` (`:104-107`).

## CMD-013 - Relative `--cwd` is resolved against the server's working directory

- The CLI sends the path unresolved, and `handle_workspace_create` does `PathBuf::from(cwd)` (`workspaces.rs:58`). So `shepr workspace create --cwd .` uses the server process's cwd, not the caller's.
- The hunter only verified this for `workspace create`.

## CMD-015 - A test passes for the wrong reason

- `rejects_unknown_bare_and_malformed_custom_tokens` (`src/config/sidebar.rs:687`) builds TOML with a literal `\\n`. The input is invalid TOML regardless of the token, so the test can never fail.

## CMD-016 - "Every CLI subcommand goes through the JSON API" is not true

- `config check`, `session list/delete`, `integration *`, `machine *` and `agent explain --file` (runs manifest evaluation in the CLI process) work locally.
- `session stop` hand-builds a raw `server.stop` request without the protocol check.
- The hunter's view: most of these are inherently local, so the documented contract needs rewording rather than the code needing change.

## CMD-017 - The meaning of "meta" is inconsistent

- In keybindings `parse_modifier_token` maps it to ALT (`src/config/keybinds.rs:928`). `format_key_combo` prints the META flag as "meta", and `right_click_passthrough_modifier` maps "meta" to META.

## CMD-018 - Help text and documentation drift

- The `integration` usage string omits `antigravity-cli`, which is accepted.
- `server_not_running.rs:8` mentions a "plugin offline fallback", which doesn't exist.
- `#[allow(dead_code)]` comments in `input/parse.rs` and `input/encode.rs` say "reserved/unused", but the functions are used in production.

## CMD-019 - Duplicate keybinding diagnostics, missing key names, and a status probe per request

- `validated_keybinds` re-parses and re-logs every diagnostic each time `prefix_key()`, `keybinds()` or `collect_diagnostics()` is called, producing duplicate warnings.
- `pane send-keys` cannot send Home, End, PageUp, PageDown, Delete or Insert: `parse_key_combo` has no names for them, and the only tmux-style alias is `C-c`.
- Every CLI request first does a status probe (up to 15 s timeout under `--machine`). `agent start` polling therefore makes two round trips per poll.

## CMD-020 - `integration` still has its own hand parser

- Every other subcommand now reads typed values from the clap `ArgMatches`. `src/cli/integration.rs` receives argv that clap has validated and parses it again by hand.

## CMD-021 - Callers of the old argv forms may remain outside `src/cli/` (unverified)

- Global options (`--session`, `--machine`, `--remote`, `--remote-keybindings`) now only count before the subcommand, and `workspace close --group` is gone. Hook scripts in `src/integration/assets/`, and any `docs/`/`reference/` pages, were not checked for trailing global options or `--group`.
