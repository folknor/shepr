# Defects: client, TUI shell and terminal input

Filed from the defect hunt over `crates/shepr-client` and `crates/shepr-termio`,
and from the reviews of the waves that resolved it. IDs continue the original
series.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## CLIENT-028 - No test drives the client loop's use of its next deadline

Scope: client-endpoint (from the review of the deadline query).

The client loop now wakes from one next-deadline query across shell, command,
activation, highlight, health and retry deadlines. The tests check each exposed
deadline and the earliest-of helper, but nothing drives the loop itself: a
regression that stopped arming the timer from the query, or armed it from a
stale value, would pass. Add a loop-level test with a paused tokio clock that
sets one pending deadline, advances past it, and asserts the expiry was handled
exactly once, and one with no deadline that asserts no timer fires.
