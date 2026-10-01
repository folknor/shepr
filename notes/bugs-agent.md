# Defects: agent detection, integrations and agent state arbitration

Filed from the reviews of the waves that resolved the defect hunt. IDs continue
the original series.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## AGENT-041 - The contract harness restates the App's report dispatch

Scope: server-app, agent-integration (lateral from review).

`crates/shepr-server/tests/agent_integration_contract.rs` parses each request
through the server's own `App::parse_agent_report_identity`, then maps each
method onto `TerminalState` calls itself (`set_agent_session_ref_for_typed_start_source_at`,
`set_hook_report_at`), restating the dispatch `App` performs for
`AgentSessionReported` and `HookStateReported`. A divergence there (an extra gate
in the event handler) would not be seen. Add a seam that applies a parsed API
request to an `App` holding one test pane and exposes its terminal state, and
replay whole requests through `handle_pane_report_agent*`.

## AGENT-042 - Hook-capture code is written three times

Scope: agent-integration tests (lateral from review).

`capture_shell_asset` in the contract harness, `capture_broken_asset_request` in
`crates/shepr-server/src/app/api/panes/reports.rs` and `run_kimi_hook` in
`crates/shepr-agent/src/integration/tests.rs` each run a hook asset against their
own fake socket, with their own reply tolerance and their own scrub of inherited
agent variables (`CODEX_THREAD_ID`, `CURSOR_VERSION` and friends), and the
scrubbed subsets already differ. Put one helper in `shepr-test-fixtures` that
runs a hook asset with a scrubbed agent environment against a fake socket and
returns the captured request lines, and use it in all three.

## AGENT-043 - Seeded-row marking reads every row first and duplicates the row visitor

Scope: vt, pane terminal (lateral from review).

- `seed_history_ansi` (`crates/shepr-mux/src/pane/terminal/backend.rs`) marks only
  non-blank seeded rows, reading each through a full row-text pass first. Marking
  every row the seed wrote (blank rows read as blank either way, and the live-row
  rule already handles later writes into marked blanks) would drop that pass.
- `visit_screen_row_text_with_seeded` (`crates/shepr-vt/src/read.rs`) duplicates
  `visit_screen_row_text`'s loop, and `crates/shepr-mux/src/pane/terminal/helpers.rs`
  duplicates `terminal_screen_row_into` the same way, kept separate so the
  non-detection readers pay nothing for the flag. A shared private loop generic
  over a per-cell hook would remove both copies at no runtime cost.
