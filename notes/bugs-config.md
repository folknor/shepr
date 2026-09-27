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

## CFG-005 - Config::headless_size() panics on unvalidated configs

`lib.rs:110-114`, uses `.expect`. This breaks the "no unwrap in production" rule, and it is only safe if every `Config` was validated. That is not guaranteed:
- `Config` has public fields and a public `Default`.
- `ValidatedConfig`'s `Deserialize` (`validated.rs:414-445`) rebuilds the config and keybinds from the wire with no validation, and clears `prefix_diag` and `keybind_diags`.
- `KeybindValidation`, `Palette` and sidebar bounds cross the wire the same way.

The hunter's structural recommendation (parse, don't validate): the core design flaw is that `ValidatedConfig` is a `Deref<Target = Config>` over raw strings and integers, with validity tracked by a separate diagnostics pass. Because of this:
- consumers re-parse `window_title`, the `tab_bar_right` datetime formats and `accent`;
- they re-derive sidebar bounds (`validated_sidebar_bounds` exists only so callers do not trip `u16::clamp`'s panic) and headless size;
- `KeybindValidation` still has to carry the fallback values;
- `Deserialize` can build a "validated" config that was never validated.

Proposed rewrite:
1. `ValidatedConfig` holds typed, already-parsed fields: `GridSize`, `SidebarBounds` with the width clamped inside it, `Option<WindowTitleTemplate>`, `OwnedFormatItem`s, `Palette`, `Keybinds`, and a typed `NewTerminalCwd`.
2. `Config` becomes a private TOML DTO.
3. Validation returns either this struct or the diagnostics, with no fallbacks left in the parsers.
4. The wire form sends that typed struct (or re-runs validation on receipt).
5. `headless_size()` can then no longer fail and the `expect` disappears.

Related gap: `ui.sidebar_width` is never checked against min and max. What happens to an out-of-range width is decided by the consumer, which the hunter could not trace.

Related: SRV-002 (the server re-encodes `ValidatedConfig` per render with an `expect`).

## CFG-006 - Keybinding diagnostics say "disabling binding", but the launch fails

`keybinds.rs` lines 606, 611, 642, 647, 685, 702, 728, 737, 769 and 789. Every one of them is turned into a fatal launch error (`io.rs:477-482` → `into_validated`), so the wording describes the fallback behaviour the project says it removed. The fallback code is also still there: an invalid prefix falls back to `ctrl+b` (`keybinds.rs:363-367`), and invalid entries are skipped when building `Keybinds`. Only the diagnostics gate stops them.

## CFG-007 - One user binding silently removes a default binding on another action

`keybinds.rs:744-747` and `776-779`. For example, `new_tab = "prefix+z"` quietly unbinds `zoom`, and a user `prefix = "h"` quietly drops `navigate_pane_left`. No diagnostic is produced, so `config check` passes and the default action silently ends up with no key.

## CFG-008 - parse_key_combo accepts keys that do not exist

`keybinds.rs:997`. `f0` and `f200` parse to `KeyCode::F(0)` and `KeyCode::F(200)`, so a mistyped binding passes validation and can never fire.

## CFG-009 - terminal.new_cwd is only partly checked

`model.rs:160-173`, `io.rs:516-531`.
- `""` silently means `follow`.
- `" home "` is trimmed to `Home`, but `"  /x "` keeps its whitespace as a `Path`.
- Relative paths such as `"projects"` are accepted with no rule for what they are relative to.
- Only `~` and `home` are checked at load time. A non-existent absolute path passes, so any failure surfaces at pane spawn rather than launch. The hunter could not see how the pane spawner handles that.

## CFG-010 - ConfigAgent duplicates shepr_agent::agent::Agent

`agent.rs`. It is a second, hand-kept list of the same 24 agents. shepr-agent is a lower layer, so shepr-config could depend on it instead of copying it.
- The doc on `experimental.cjk_ime_agents` (`model.rs:598-600`) has already drifted: it omits agy, omp, mastracode and muse.
- `parse_label` accepts `.exe` and `.js` suffixes and `/usr/bin/claude`-style paths as config values, which is argv-matching logic leaking into config.
- `rows_by_agent` keys (canonical labels as strings) only work if they equal whatever id the consumer passes to `rows_for_agent`. The hunter could not verify that.

## CFG-011 - Specific error messages are lost

- `RawSidebarToken` is `#[serde(untagged)]` (`sidebar.rs:199-204`), so a bad `fg` or a bad rule in a styled token is reported as "data did not match any variant", not the specific error.
- When the config fails to parse, `load_for_check` skips the `new_cwd` home check, so `config check` hides that second error until the first is fixed.

## CFG-013 - Small config smells

- `ValidatedConfig`'s `PartialEq` ignores the palette and the keybind cache, and treats a failed serialization as "not equal".
- `ConfigProvenance::defaults` swallows its error and returns an empty record.
- `PathProvenance.current_dir` is always `Default`.
- `tab_bar_right_diagnostics` only checks the first 16 entries. That is harmless, because more than 16 entries is already an error.

## CFG-014 - ServerAddress::resolve accepts relative paths directly

`address.rs`. `AppPaths` now validates socket overrides (non-empty, absolute, UTF-8) before resolving them, but `ServerAddress::resolve` stays a public, infallible constructor that takes any path, relative included. Its only external callers found are tests in `crates/shepr-server/src/server/socket_paths.rs`, which also still say "legacy" in a client-override test name and fixture path. Either make the constructor crate-private or validate inside it.
