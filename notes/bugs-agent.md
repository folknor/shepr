# Defects: agent detection, integrations and agent state arbitration

Filed from the defect hunt over `crates/shepr-agent/src/detect/`,
`crates/shepr-agent/src/agent/`, `crates/shepr-mux/src/pane/agent_detection.rs`,
`crates/shepr-agent/src/integration/` (with its assets), and
`crates/shepr-mux/src/terminal` (the `TerminalState` arbitration between
detection and hook reports).

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## AGENT-009 - Seeded-row provenance does not survive column reflow or zero scrollback

Scope: vt, pane runtime.

Restored history seeded into a plain shell is now marked by absolute row id in
the pane terminal, so detection excludes those rows when an agent is started
there later. Absolute row ids cannot follow a seeded row through a column-width
reflow (rows are rewrapped) or through scrolling with zero scrollback (rows leave
without a history index), so in those two cases seeded chrome can again read as
live. Complete provenance needs the terminal core in `shepr-vt` to carry a
per-row seeded mark that survives reflow and eviction; a note in the pane
terminal records the limitation.

Related, same map: it keeps a full `String` per seeded row, and entries whose
rows scroll out of history are never pruned. Memory is bounded by the saved
history, but a row hash would cut it and pruning on eviction would keep it
tidy.

## AGENT-039 - The contract harness replays into a copy of the server validation

Scope: agent-integration, server-app (from the review of the contract harness).

`crates/shepr-server/tests/agent_integration_contract.rs` replays each asset's
requests into `TerminalState`, but it decodes report identity with its own
`decode_report_identity`, a restatement of the server's
`parse_report_session_ref`, because the App is private to the server crate. The
mutation probe therefore proves the harness's copy rejects a broken asset, not
the server's own handler. Move the replay into the server crate's own tests (or
expose a narrow test seam) so it drives the real report path. Separately, a pi
bun test was switched from a session file (`/tmp/pi-new.jsonl`) to an id-only
session so its trace matches the id-based contract, which lost coverage of pi's
path-form session; teach the pinned trace contract path sessions.

## AGENT-040 - Pi and omp re-send blocked on a changed prompt text the server no longer reads

Scope: agent-integration (from the review of dropping the report message).

The pi and omp reporters no longer send `message`, but `desiredState` still
pairs a message with blocked states and `publishState` de-duplicates on it, so a
second blocked prompt with different text re-sends an identical blocked report.
Either drop the message from the de-duplication (a same-state resend carries no
new information to the server) or keep it deliberately as a keepalive and say
so in the asset.
