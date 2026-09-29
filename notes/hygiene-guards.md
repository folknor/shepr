# Hygiene: tests that prove nothing, guards and claims that stopped holding

This file consolidates the findings for questions 5 and 6 of the nine-scope
hygiene hunt over the shepr workspace: tests that depend on the environment they
run in rather than on anything this repository builds, tests that cannot fail,
checks that fail open when a name stops matching, and invariants asserted in
comments, documentation or commit messages that nothing in the build would
notice becoming false. It is a working document assembled from the raw hunter
reports; nothing here has been verified, and some entries may be phantoms. A fix
pass should expect that.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

---

## HYGG-150 - Unverified claim: a failed reactive host query delays an ambiguous Escape by only one flush

A comment in `shepr-termio/src/input/raw_input.rs`, at the transition where
focus gain opens the appearance reply window, says that when the query write
fails the input reader holds an ambiguous Escape for at most one flush. The
fixer who wrote it read that from the timeout path; the reviewer could not
confirm it. A test that fails the query write and asserts when the Escape is
delivered would settle it.

## HYGG-151 - Production files that open with an early `#[cfg(test)]` item

Every `skip_after` textlint releases the rest of a file at its first
`#[cfg(test)]`, so in a file where a test-only item sits above production code,
only `scripts/check_skip_after_scopes.py` still sees what follows. Files in that
shape: `shepr-server/src/server/client_transport.rs`,
`shepr-server/src/app/api/panes.rs`, `shepr-server/src/app/mod.rs`,
`shepr-server/src/server/headless.rs`, `shepr-platform/src/lib.rs`,
`shepr-agent/src/integration/mod.rs` and
`shepr-client/src/endpoint/activation.rs`. Moving the test-only items into the
trailing test module puts those files back under the textlints directly.
