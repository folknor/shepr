# Defects: shepr-agent

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Hunter coverage: detection, manifests, the manifest registry and reload, integrations and config editing, and the reload call sites in shepr-server.

## AGT-010 - Concurrent installs can lose settings edits

Every settings edit is read-modify-write with no lock, so two concurrent `shepr integration install` runs for the same target can lose one update. The rename is atomic; the edit is not.

## AGT-017 - Manifest compilation runs on the event loop

Each regex now compiles once per load and the first registry load uses the config directory, but `reload_manifests` still compiles every bundled manifest synchronously on the tokio loop, both at startup and on the `server.reload-agent-manifests` API call. Moving it off needs startup preparation in `server/headless/bootstrap.rs` before `rt.block_on`, and a deferred completion for the reload request in the headless request loop or dispatcher. Comments at the call sites in `app/mod.rs` and `app/api.rs` record this.
