# Hygiene: policy invented per call site, and code that is no longer load-bearing

This file consolidates the findings of the nine-scope hygiene hunt for two of the
eight questions the hunters were asked: question 7 (one rule implemented
independently wherever it was needed, ambient dependencies reached from logic,
shared mutable state whose safety rests on call order, unbounded resources,
secrets in diagnostics, test-only shortcuts production can reach) and question 8
(modules, functions, flags and configuration keys that are no longer
load-bearing). Findings about duplicated or unfindable values, output channels
and error handling, and tests, guards and stale claims are filed in sibling
documents; live defects are in `notes/bugs.md`. This is a working document
assembled from reading, not from running anything: entries may be wrong, and a
later fix pass is expected to find phantoms. Where two hunters read the same
thing differently, or where a hunter marked a claim as an unverified inference,
the entry says so.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

---

## HYGP-157 - Socket-path hazards and the already-running text

- `shepr-platform/src/ipc.rs::prepare_socket_path`: `probe()` treats a
  connect failing with `ConnectionRefused` as a stale socket, and Linux
  `connect()` to a regular file is believed to return ECONNREFUSED, so
  `bind_private_socket` may delete a regular file sitting at the socket path.
  Untested. Check `file_type().is_socket()` before calling a path stale, with a
  test that puts a regular file there.
- Saved-machine bridge sockets (`saved_bridge_path` in
  `shepr-remote/src/remote/saved.rs`) are named `shepr-ssh-<profile>.<token>.sock`,
  sharing the `shepr-ssh-` prefix with the ssh config dirs `shepr-ssh-<tag>`.
  Harmless today (the config-dir sweep needs a parseable tag and a directory);
  a distinct bridge prefix would remove the trap.
- "shepr server is already running" is written three times: the
  `ALREADY_RUNNING` constant in `src/cli/error.rs`, both arms of
  `RunServerError`'s `Display` in `shepr-server/src/server/headless/bootstrap.rs`,
  and the `tracing::error!` in `startup_error`. Leave the operator sentence to
  the binary and word the library `Display` neutrally.

## HYGP-156 - Dead checked-time fallbacks, and limit refusals that restate the limit

- `shepr-server/src/app/tab_bar_status.rs`:
  `now.checked_add(runtime.interval).unwrap_or(now)` cannot fail, since config
  caps the interval at `MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS`, and its fallback
  would be harmful if it ran (the command due again next tick). Use
  `now + runtime.interval` with a comment naming the config bound.
- `shepr-server/src/app/git_refresh.rs::mark_due`:
  `now.checked_sub(GIT_REMOTE_STATUS_REFRESH_INTERVAL).unwrap_or(now)` cannot
  fail on a monotonic `Instant`, and its fallback would mark the refresh not
  due, the opposite of the function's purpose. Use plain subtraction.
- `shepr-server/src/app/api_helpers.rs::normalize_metadata_ttl` hard-codes
  its limits in the refusal text ("must be at least 1", "must be 86400000 or
  less") instead of formatting `METADATA_TTL_MIN_MS` and `METADATA_TTL_MAX_MS`,
  so the message goes stale if a limit changes. It returns `&'static str`; the
  two callers in `app/api/workspaces.rs` and `app/api/panes/reports.rs` would
  take a `String`.
- `shepr-api/src/client.rs`: the `request_value_with_timeout` doc reads "an
  send timeout".
