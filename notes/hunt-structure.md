# Structure from the design hunt

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

Shape and placement: crate splits, moves, god objects, boundaries that do not
follow the seams, dependency edges pointing the wrong way, state split from the
behaviour acting on it, and test layouts.

## Workspace and persistence

## STR-029 - Constants beside their policy, against the limits lints (decision open)

The constants of shepr-mux, shepr-server and shepr-client now sit beside the
policy each parameterises, and those crates' `limits.rs` files are gone. This
contradicts the repo-wide textlints `numeric-consts-live-in-limits` and
`duration-and-capacity-literals-live-in-limits`, which want every bound in a
crate's limits module. As an interim, `brokkr.toml` excludes those three
crates from both lints. Decide: keep the placement and reshape or drop the
lints for every crate, or move the constants back into limits modules and
remove the exclusion. (owner decision)

## Tests

## STR-049 - Test layouts mirror accretion

Not re-verified since the specs landed, which moved many tests; the server's
headless tests are already split per subject.

- The client's `src/tests/mod.rs` held tests for `terminal_geometry`,
  `terminal_setup`, `errors` and `clipboard_forwarding`, forcing `lib.rs` to
  carry `#[cfg(test)] use` re-exports of their private functions;
  `src/tests/endpoint_choice.rs` builds a full `ClientLoop` fixture that
  `endpoint/view.rs`'s unit tests reach into.
- The client shell's `shell/tests/` groups by feature (`copy.rs` and
  `endpoints.rs` are thousands of lines) while unit tests also sit in
  production files.
- Tests should sit with the component they exercise and drive its API rather
  than writing `pub(super)` fields.

(client-core, client-shell)
