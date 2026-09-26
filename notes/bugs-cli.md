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
