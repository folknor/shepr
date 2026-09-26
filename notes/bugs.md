# Bugs

Defects and oddities surfaced while resolving earlier findings.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## BUG-059 - ServerCapabilities fields are always true and unread

`ServerCapabilities.surface_interest` / `health_check` in the API schema are
always true and nothing on the remote side reads them since the compatibility
fossils were removed. Drop them. (wave B reviewer)

## BUG-061 - Cached keybinding validation can go stale

Validation is cached in a `OnceLock` while `Config.keys` is a public field;
nothing mutates it after load today. Dissolves with the immutable
`ValidatedConfig` (CON-045). (wave B reviewer)

## BUG-063 - Planning note in a Config doc comment

A `Config` doc comment in `config/model.rs` is a deferral note about migrating
readers rather than documentation. Dissolves with CON-045. (wave B reviewer)
