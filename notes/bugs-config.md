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

## CFG-003 - XDG handling contradicts "Directories follow the XDG spec"

`io.rs:160-176`, `215-219`. The spec says an empty `XDG_CONFIG_HOME` or `XDG_STATE_HOME` means the default applies, and relative values are ignored. shepr fails the launch in both cases, and the test at `io.rs:975-979` locks that in. Two ways to resolve it:
- implement the spec (treat empty as unset), or
- reword the claim to "XDG, strict: a set variable must be absolute".

## CFG-009 - terminal.new_cwd is only partly checked

`model.rs:160-173`, `io.rs:516-531`.
- `""` silently means `follow`.
- `" home "` is trimmed to `Home`, but `"  /x "` keeps its whitespace as a `Path`.
- Relative paths such as `"projects"` are accepted with no rule for what they are relative to.
- Only `~` and `home` are checked at load time. A non-existent absolute path passes, so any failure surfaces at pane spawn rather than launch.
- `~` is now expanded once at validation, but the PTY layer still falls back to HOME (with a warning) for a non-directory path. The typed config deliberately leaves `NewTerminalCwd::Path` as a plain path (documented at `model.rs` and `validated.rs`) because nothing enforces more; deciding the rule here would let it become a checked absolute directory.

## CFG-010 - ConfigAgent duplicates shepr_agent::agent::Agent

`agent.rs`. It is a second, hand-kept list of the same 24 agents. shepr-agent is a lower layer, so shepr-config could depend on it instead of copying it.
- The doc on `experimental.cjk_ime_agents` (`model.rs:598-600`) has already drifted: it omits agy, omp, mastracode and muse.
- `parse_label` accepts `.exe` and `.js` suffixes and `/usr/bin/claude`-style paths as config values, which is argv-matching logic leaking into config.
- `rows_by_agent` keys (canonical labels as strings) only work if they equal whatever id the consumer passes to `rows_for_agent`. The hunter could not verify that.

## CFG-011 - Specific error messages are lost

- `RawSidebarToken` is `#[serde(untagged)]` (`sidebar.rs:199-204`), so a bad `fg` or a bad rule in a styled token is reported as "data did not match any variant", not the specific error.
- When the config fails to parse, `load_for_check` skips the `new_cwd` home check, so `config check` hides that second error until the first is fixed.

## CFG-013 - Small config smells

- `ValidatedConfig`'s `PartialEq` serializes both sides to JSON and treats a failed serialization as "not equal".
- `ConfigProvenance::defaults` swallows its error and returns an empty record. Production now only reaches it for the placeholder a failed load carries, which never becomes a `ValidatedConfig`.
- `PathProvenance.current_dir` is always `Default`.
- `tab_bar_right_diagnostics` only checks the first 16 entries. That is harmless, because more than 16 entries is already an error.

## CFG-014 - ServerAddress::resolve accepts relative paths directly

`address.rs`. `AppPaths` now validates socket overrides (non-empty, absolute, UTF-8) before resolving them, but `ServerAddress::resolve` stays a public, infallible constructor that takes any path, relative included. Its only external callers found are tests in `crates/shepr-server/src/server/socket_paths.rs`, which also still say "legacy" in a client-override test name and fixture path. Either make the constructor crate-private or validate inside it.

## CFG-015 - AppPaths decoded off the wire skips resolve's checks

`AppPaths` derives `Deserialize`, so a value decoded from a peer never goes through the checks `AppPaths::resolve` makes (absolute paths, validated overrides). Today the client uses the decoded paths only for the home-path check when it re-validates an endpoint's config, but nothing stops a later caller trusting them.
