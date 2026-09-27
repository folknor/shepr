# Defects: shepr-config

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Hunter coverage: every source file in the crate except `theme.rs`, `diagnostic.rs`, `default.toml` and the test tail of `keybinds.rs`. The hunter could not follow values into shepr-server or shepr-client; findings that depend on a consumer say so.

## CFG-009 - terminal.new_cwd is only partly checked

`model.rs`, `io.rs`.
- `""` silently means `follow`.
- `" home "` is trimmed to `Home`, but `"  /x "` keeps its whitespace as a `Path`.
- Relative paths such as `"projects"` are accepted with no rule for what they are relative to.
- Only `~` and `home` are checked at load time. A non-existent absolute path passes, so any failure surfaces at pane spawn rather than launch.
- `~` is now expanded once at validation, but the PTY layer still falls back to HOME (with a warning) for a non-directory path. The typed config deliberately leaves `NewTerminalCwd::Path` as a plain path (documented at `model.rs` and `validated.rs`) because nothing enforces more; deciding the rule here would let it become a checked absolute directory.
