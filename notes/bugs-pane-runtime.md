# Defects: pane runtime

Filed from the defect hunt over `crates/shepr-mux/src/` `pane.rs`, `pane/`,
`terminal/` and `render_signal.rs`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## PRUN-019 - Hook authority and the persisted session are still public fields

Lateral, `terminal/state/`. Hook arbitration is one per-source state machine,
and in production `hook_authority` and `persisted_agent_session` are written only
by its effects. But both are `pub` on `TerminalState`, so any crate can write them
past the machine; only tests do today. Narrow them to read accessors plus test
seams, so routing only through the machine is enforced, not a convention.
Related: the `transition_tests` fixtures in `terminal/state/source.rs`
(`session()`, `release()`, `record()`) build `shepr:claude` records, but Claude
is not a full-lifecycle agent, so they model state production never reaches for
Claude; use a full-lifecycle agent (Pi, Kimi, Kilo) so the fixtures read true.
