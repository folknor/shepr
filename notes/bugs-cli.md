# Defects: the root shepr binary (CLI)

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

IDs CMD-001 through CMD-006 were used by an earlier edition of this file and are not reused.

## CMD-007 - A saved-machines catalog that fails to load reads as "no saved machines" at startup

`src/main.rs`, the local startup decision: `EndpointCatalog::load(paths).is_ok_and(...)` turns a catalog load error into "no saved machines". The client itself now fails the launch on a catalog that cannot be loaded, but this earlier decision (whether losing Local ends the client, and how the local server is started) silently takes the empty-catalog path. Propagate the error so the launch fails with the catalog message.

## CMD-008 - Two classifiers decide whether a server socket is listening

`crates/shepr-platform/src/ipc.rs` has `probe()`, which classifies a socket as live or stale, and `crates/shepr-remote/src/remote/local_server.rs` has `is_server_listening_at`, which now returns unexpected connect errors instead of treating them as not listening. They answer the same question with separate error handling; one platform function should serve both.
