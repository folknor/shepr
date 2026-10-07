# Bugs: client endpoints and remote (shepr-client endpoint lifecycle, shepr-remote)

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Filed from the client endpoints and remote hunt. The raw report is in commit
6dc81572 (`notes/hunt-client-endpoints.md`).

## RMT-015 - The launch connector admission check runs before client logging exists

Raised as a lateral by the wave 3 reviewer.

`preflight::check_connector_admission` (`src/preflight.rs`) now builds a
`MachineSshConnector` per machine before preflight, so a launch-fatal SSH
setup fails before anything acts. But it runs before client logging is
initialised: each connector writes and then drops a managed SSH config
directory, and a transient setup failure's WARN goes nowhere. Either initialise
logging first or check admission without building and discarding connectors.

## RMT-017 - Restart's attempt budget no longer lists everything Restart runs

Raised as a lateral by the wave 4 reviewer.

The `SSH_RESTART_ATTEMPT_BUDGET` doc counts the conditional stop plus an
ordinary connection attempt. Restart now runs, each under the same deadline, a
verification round trip (more when it falls back to discovery), the status
read inside `stop_server_of_another_build`, the stop, and the bridge. On a cold,
slow link the time left for the start can shrink. Check the budget arithmetic
against that sequence.

## RMT-018 - The endpoint reader's non-EOF error branch ignores a local stop

Raised as a lateral by the wave 4 reviewer.

In `server_reader_thread` (`crates/shepr-client/src/endpoint/connection_io.rs`)
the EOF branch now checks `transport_stopped`, but the non-EOF `Err` branch
does not, so after a local stop it can still post a `ServerDisconnected` for
the stopped generation and run `ended()`. Bounded and harmless, since
`should_stop` is already set, but inconsistent with the EOF branch.

## RMT-019 - Small leftovers of the bridge restructure

Raised as laterals by the wave 4 reviewer.

- `NativeEndpointTransport::with_lifetime` is only called with `()` in
  production; its generic lifetime parameter survives for one writer test.
- `wait_for_server` keeps a 16 KiB stdout tail using `SSH_STDERR_CAPTURE_LIMIT`;
  name a stdout constant or comment the reuse.
- The host.rs test `an_attach_only_bridge_starts_no_server_and_reports_none`
  writes the raw preamble to the test process's fd 1, which shows in test
  output; a stdout writer seam would keep it out.
