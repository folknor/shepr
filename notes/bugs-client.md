# Defects: client, TUI shell and terminal input

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

## CLIENT-029 - The loop tests restate the ClientLoop struct literal

Scope: client (lateral from review).

`test_client_loop` in `crates/shepr-client/src/lib.rs` builds a `ClientLoop` with
its own struct literal, restating the one in `run_client`. A field added with a
non-trivial default compiles in both but can drift in meaning, so the tests would
drive a loop shaped differently from production. Give `ClientLoop` one
constructor taking the pieces, and have both `run_client` and the tests use it.
