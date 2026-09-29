# Hygiene: channels and errors

This file collects the findings for two of the eight questions put to the nine
hygiene hunters who each read one scope of the workspace: question 3, "one
channel, one implementation" (output and diagnostics written directly where the
project has a channel for them, operator text assembled at the call site, and
the quality of what goes through the channel - levels, missing identifiers,
unreadable lines, and events that are logged nowhere), and question 4, "errors"
(failures swallowed where they should travel to a caller that can decide,
failures that travel but shed the context that made them actionable, and code
that aborts the process where a refusal was owed). Entries gather every scope
that reported the same thing. This is a working document produced by reading,
not by running anything; individual claims may be wrong, and a later fix pass is
expected to find phantoms among them.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

---

## HYGC-055 - Busy-socket errors lost their subject, and the already-running path has no test

- `shepr-platform/src/ipc.rs`: `acquire_socket_startup_lock` now returns a
  bare `AddrInUse` with no path, and `bind_private_socket` replaces the
  listener's "socket busy at <path>" message with a bare kind. Every caller
  wraps them today, but a new caller gets only "address in use"; carry the
  path in the error.
- Nothing tests that a busy client or API socket comes out of
  `shepr-server`'s `run_server` as `RunServerError::AlreadyRunning`; only the
  raw `AddrInUse` from `bind_private_socket` is tested.
- The `library-crates-do-not-write-stderr` textlint does not cover
  `io::stdout()`. Current library uses are fd handoffs in the client terminal
  setup and the bridge relay; decide whether stdout needs the same rule with a
  marker for those.

## HYGC-013 - Structured field names for the same thing differ across sites

`shepr-client` keys every failure field `error`. Every other crate mixes `err`
and `error` for the same thing in `tracing` fields: a count across the Rust
sources found both spellings in `shepr-agent`, `shepr-api`, `shepr-mux`,
`shepr-platform`, `shepr-remote` and `shepr-server`, and only `err` in
`shepr-pty`. Pick one name and convert the other crates.

Enforcement named: a text rule on the field name, or funnelling failures through
one helper.
