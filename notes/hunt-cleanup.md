# Cleanup from the design hunt

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

Dead code, dead state, test-only twins of production rules, public surface
that exists only for tests, unused dependencies and stale documentation. Each
entry is a deletion or a rewording rather than a redesign.

## Dead code and dead state

## CLN-023 - The client shell's visibility pass is unfinished

The narrowing pass covered `endpoints.rs`, `state.rs`, `ledger.rs`, `copy/`,
`overlays/mod.rs` and `sidebar/`. Still open: `view/`, `presentation/`,
`notices/`, `navigation/`, `input/` and `transitions.rs` keep blanket
`pub(in crate::shell)` items, item by item, and several `CopySession` fields
stay shell-wide only because tests in `shell/tests/` read them (moving those
tests into `copy/` lets them narrow). No dead-code sweep followed the pass.

## Test-only twins and test seams in production
