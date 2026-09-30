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

## CLIENT-027 - The idle-sleep test proves little, and some pending work still polls

Scope: client-endpoint (lateral from review).

- `no_deadline_leaves_the_loop_asleep_past_one_hundred_milliseconds`
  (`crates/shepr-client/src/lib.rs`) only exercises `std::future::pending` and
  costs 120 ms of wall time; it does not drive the client loop's timer selection.
  Replace it with a test on the deadline computation itself (no deadline yields
  no timer; the earliest of shell, health and retry deadlines wins).
- The loop keeps a 100 ms recheck while a workspace highlight, a client command
  or an activation is pending, because those deadlines are private to
  `shell/navigation/workspace_navigation.rs`, `endpoint/commands.rs` and
  `endpoint/activation/model.rs`. `EndpointCommands` could expose its exact
  expiry (the in-flight `sent_at` plus the command timeout), and the other two
  their deadlines, so the loop sleeps exactly until the earliest.
