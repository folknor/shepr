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

Findings raised in this scope but filed elsewhere: Windows key code (PLAT-003).

## CMD-008 - `ui.accent = "cyan"` is ignored, and "cyan" isn't really the default

- `palette_from_config` only applies the accent when `config.ui.accent != "cyan"` (`src/app/mod.rs`). Setting cyan explicitly keeps the theme's accent, and the unset default is the theme's accent, not cyan as `DEFAULT_CONFIG` documents.
- The colour diagnostic reports a bad `ui.accent` as "using cyan" even when `theme.custom.accent` is set, in which case `ui.accent` is ignored entirely. Fixing the accent rule should make the diagnostic match it.

## CMD-009 - A preferences file overrides three config keys permanently

- `src/client/shell/config.rs` and `preferences.rs` store `sidebar_width`, `sidebar_collapsed` and `agent_panel_sort` in `state_dir()/client-shell/local-*.json`.
- After the first manual toggle or resize, the stored value wins over `ui.sidebar_width`, `ui.sidebar_start_collapsed` ("Changes take effect on the next launch") and `ui.agent_panel_sort`. Editing config then has no visible effect.

## CMD-011 - Direct attach ignores the configured prefix and detach keys

- `AttachEscapeState::filter_input` (`src/client/attach.rs`) hardcodes Ctrl+B and `q`. With `keys.prefix = "ctrl+a"`, `terminal attach` and `agent attach` still intercept Ctrl+B from the pane app.
- Separately, in the legacy byte path a coalesced chunk like `abc\x02q` detaches and drops the `abc`.

## CMD-024 - Right-click passthrough accepts modifiers mouse reports can't carry

- `right_click_passthrough_modifier` still accepts cmd/super/hyper, but SGR mouse reports only carry ctrl and alt, so those settings never match. Reject them with a diagnostic. (`DEFAULT_CONFIG` no longer advertises them; "meta" is now an alias for alt.)

## CMD-025 - Endpoint keybinding profile diagnostics are silent

- `keybindings_from_profile_toml` discards its non-fatal diagnostics. They used to reach the log through `warn!` calls inside keybinding validation, which now only returns diagnostics (logged once by `Config::load_from_str`). Log or surface the profile's diagnostics.
- In `src/client/shell/config.rs`, when keybinding validation fails the fallback runs it twice more (`prefix_key()` and `keybinds()`); one result could be reused.
