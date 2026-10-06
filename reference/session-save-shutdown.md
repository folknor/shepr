# Session saves and server shutdown

This page describes when the server writes its saved layout and what can hold
shutdown open. There is no end-to-end shutdown deadline: filesystem operations,
the final save, and session-writer retirement can take as long as the storage
allows.

## Layout saves

The server notices layout changes in the event loop. Ordinary saves are
debounced by `SESSION_SAVE_DEBOUNCE` (`crates/shepr-server/src/limits.rs`). The
event loop captures the layout and submits it to the session persister, which
normally serializes writes on its own thread. If that thread cannot be started,
the persister logs the failure and runs writes inline, where filesystem work
can block the event loop. A save already in flight finishes before the next
save starts. Retryable autosave failures use
`SESSION_SAVE_RETRY_MIN` and `SESSION_SAVE_RETRY_MAX`, growing by the shared
`BACKOFF_MULTIPLIER` (`crates/shepr-core/src/limits.rs`, applied by
`shepr_core::backoff::Backoff`); the failure delay grows up to the autosave cap.

The persister publishes the session file atomically and maintains recovery
snapshots according to `SNAPSHOT_INTERVAL`, `SNAPSHOT_LIMIT`, and
`BACKUP_LIMIT` in `crates/shepr-mux/src/limits.rs`. A write error before
publication leaves the previous session file available. A directory-sync error
after publication is reported as not confirmed durable even though the new file
has already replaced the old one. If the required backup of an unreadable
existing session cannot be made, the server blocks persistence for the rest of
that boot; a non-retryable persistence failure stops it for that boot.

## Pane-exit checkpoints

When a pane exits, the server can hold the exit while a checkpoint makes
the corresponding layout durable. Checkpoint retries use
`CHECKPOINT_RETRY_MIN` and `CHECKPOINT_MAX_FAILURES` in
`crates/shepr-server/src/limits.rs`, with the same `BACKOFF_MULTIPLIER`
growth. The attempt count bounds the retry schedule,
but each save attempt includes filesystem work without a duration bound.

On success, the pane exit can be applied to the live layout while its durable
pre-removal layout remains a protected restore candidate. If no later session
mutation supersedes it, the final save recaptures that candidate with fresh
working directories. A later successful ordinary save records the current
layout and replaces the candidate. If the checkpoint exhausts its attempts,
the pane exit is released without a durable record of that removal; the last
saved layout may therefore still contain the pane.

## Host shutdown warning

The server watches logind for `PrepareForShutdown(true)` and holds a delay
inhibitor while waiting for a host-shutdown checkpoint. The checkpoint uses the
same retry policy as pane-exit checkpoints. Once its result is known, the server
freezes session saves and releases the inhibitor. A failed or unavailable
checkpoint is logged, then saves are still frozen so shutdown can continue.

The manager's configured `InhibitDelayMaxSec` is an external limit. The server
does not read it or turn it into a checkpoint deadline. A connection made
after logind has started shutdown preparation can report the warning but cannot
acquire an inhibitor. The checkpoint's retry schedule is bounded by
`CHECKPOINT_MAX_FAILURES`, but an individual filesystem attempt is not; logind
can proceed when its inhibitor limit expires while the server is still trying
to save. If logind cancels during the warning, the pending checkpoint is
cancelled while saves remain enabled. If it cancels after the server froze
saves, the server thaws them and marks the live layout dirty again.

`SHUTDOWN_RECONNECT_INITIAL_DELAY` and `SHUTDOWN_RECONNECT_MAX_DELAY` in
`crates/shepr-server/src/limits.rs` govern reconnect attempts whenever the
logind watch ends, whether it failed or its stream closed cleanly (an owner
change, a dropped bus connection), so a bus that keeps accepting and dropping
watches cannot spin the monitor. The failure streak resets only after a watch
stayed connected for `SHUTDOWN_RECONNECT_STABLE_TIME`. While a shutdown warning
is pending, each reconnect waits only the initial delay, without resetting the
streak. These delays do not bound checkpoint or shutdown duration.

## Server stop and final save

When the server begins orderly shutdown, it sends shutdown notices to connected
clients and waits for registered client writers under the shared
`SHUTDOWN_FLUSH_TIMEOUT` deadline in `crates/shepr-server/src/limits.rs`.
`UNREGISTERED_SHUTDOWN_FLUSH_TIMEOUT` and
`UNREGISTERED_SHUTDOWN_FLUSH_POLL_INTERVAL` cover transport threads waiting for
a shutdown notice to reach a client that was not registered. The server then
performs the final session save before pane teardown. The final save first
settles any save already in flight,
then captures and submits the current layout immediately; it does not wait for
the autosave debounce. A termination signal also lets the server adopt pane
exit candidates observed at the signal time before this capture.

The final save is skipped while host-shutdown saves are frozen, because the
checkpoint from the warning is the layout to restore. The `persist.save`
log with `kind = "final"` records the save's outcome (`ok`, `error`, `stopped`,
`blocked_on_backup` or `frozen`) and its duration; a failed save's cause is the
`error` field of the error-level `persist.save` event logged when the save
fails. A failed final save is reported as an unclean exit, and to every
client whose stop request the server accepted: the answer to `server.stop` and
`server.stop_if_boot` waits for the final save and carries its error, if any.
That wait is bounded by `FINAL_SAVE_ANSWER_TIMEOUT` in
`crates/shepr-api/src/limits.rs`; past it the answer says the server has not
reported its final save and may still be saving, though the save itself goes
on. A server that exits without reaching its final save (its run failed
first) answers every waiting stop with an explicit error rather than an empty
answer. Before removing its socket, the server waits a short bound
(`STOP_ANSWER_WAIT`) for those answers to be written. The final save is deliberately a
single attempt with no retry schedule, unlike autosaves and checkpoints: the
event loop no longer runs, and retries would hold the lease, the socket and
every stopping client through more unbounded filesystem work. The final save
has no deadline, and there is no forced stop; `SIGKILL` is the external escape
hatch.

After the save, the server drops pane runtimes and starts session teardown on
background threads. `PANE_TEARDOWN_STEPS` in
`crates/shepr-mux/src/limits.rs` owns the signal escalation sequence and grace
periods. When asynchronous pidfd waiting is unavailable,
`CHILD_WAIT_FALLBACK_POLL_INTERVAL` spaces nonblocking child-exit probes; it
does not bound the time to observe an exit. `PANE_TEARDOWN_BUDGET` sums the
signal grace periods and excludes scans. `PANE_TEARDOWN_WAIT` in
`crates/shepr-server/src/limits.rs` uses the mux's derived shutdown allowance,
which includes a scan allowance based on the signal steps. `/proc` session
scans and system calls are not time-bounded, so this wait can expire with pane
sessions still running; the server logs those pane identities and continues.

The session writer is retired after pane teardown. Retirement drains queued
work, joins the persister thread, and releases the data-directory lease. The
server removes its socket only after the lease is released. Writer retirement
has no deadline, so a slow or stuck filesystem can keep the server and socket
present after the final-save log.

## Client-side stop waits

`shepr stop` uses `STOP_WAIT_TIMEOUT` while waiting for the named server to stop
answering, then `STOP_LEASE_WAIT_TIMEOUT` if the data-directory lease remains
held. `STOP_STATUS_PROBE_TIMEOUT` and `STOP_WAIT_POLL` bound the observations
within those waits; `STOP_WORST_CASE` summarizes the stop client's own wait
budget in `crates/shepr-launch/src/limits.rs`. The wait for the stop request's
answer, which arrives once the final save is done, shares the
`STOP_WAIT_TIMEOUT` deadline. A server reporting a failed final save fails the
stop even when the server then goes. A client timeout does not kill
the server. In particular, it can expire while the server is still completing
its unbounded final save or retiring the writer.
