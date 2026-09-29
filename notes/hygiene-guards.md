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

## HYGG-151 - Production files that open with an early `#[cfg(test)]` item

Every `skip_after` textlint releases the rest of a file at its first
`#[cfg(test)]`, so in a file where a test-only item sits above production code,
only `scripts/check_skip_after_scopes.py` still sees what follows. Files still in
that shape: `shepr-server/src/app/mod.rs` and
`shepr-server/src/server/headless.rs`. Moving the test-only items into the
trailing test module puts those files back under the textlints directly.
