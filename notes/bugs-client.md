# Defects: client, TUI shell and terminal input

Filed from the defect hunt over `crates/shepr-client` (endpoint, transport,
handshake, loop, input, and the `shell/` presentation) and `crates/shepr-termio`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## CLIENT-023 - The client loop wakes every 100 ms while idle

Scope: client-shell and client-endpoint.

`timer_delay` (`crates/shepr-client/src/shell/state.rs`) knows only the
autoscroll and repaint deadlines; the notice, endpoint error, workspace highlight
and selection-clear deadlines rely on the `MAX_CLIENT_TIMER_DELAY` (100 ms) cap in
the client loop (`lib.rs`, `limits.rs`), so an idle client wakes ten times a
second forever. The same tick also drives endpoint health checks and reconnect
scheduling, whose deadlines live in `endpoint/registry.rs` and
`endpoint/supervisor.rs` and are not exposed, which is why the cap cannot simply
be lifted (a note at each site says so). Expose the next health and retry
deadline from the registry, fold every shell deadline into `timer_delay`, and let
the loop sleep until the earliest; one fixer needs the shell state, the loop and
the endpoint registry and supervisor together.
