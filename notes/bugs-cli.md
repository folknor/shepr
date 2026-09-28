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

## CMD-008 - Two classifiers decide whether a server socket is listening

`crates/shepr-platform/src/ipc.rs` has `probe()`, which classifies a socket as live or stale, and `crates/shepr-remote/src/remote/local_server.rs` has `is_server_listening_at`, which now returns unexpected connect errors instead of treating them as not listening. They answer the same question with separate error handling; one platform function should serve both.
