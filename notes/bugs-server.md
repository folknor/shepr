# Defects: server application, serving and persistence

Filed from the defect hunt over `crates/shepr-server` and the rest of
`crates/shepr-mux` (persist, git, workspace, events), and from the reviews of the
waves that resolved it. IDs continue the original series.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## SRV-038 - A resume into a hung cwd can still block the loop at spawn

Scope: server-app, pty (lateral from review).

Agent resumes now launch with a required cwd, so the loop no longer stats it.
The child still `chdir`s into that directory before exec, and
`std::process::Command::spawn` waits for the child's exec status, so a cwd on a
hung network mount still blocks the headless loop for the length of the spawn.
Only spawning off the loop (a worker that hands the spawned runtime back) removes
it. Decide whether that is worth the extra runtime-registration step.

## SRV-039 - No headless test replays a runtime-tagged pane exit held for a checkpoint

Scope: server-serving (lateral from review).

Runtime-originated events now carry a runtime generation that App and the
headless loop check before acting, and a pane exit held for a session checkpoint
is re-wrapped with its origin and checked again on replay
(`server/headless/internal_events.rs`). The headless tests still use untagged
`PaneDied`, and the App-level test checks the replayed envelope only through
`handle_prepared_pane_exit`. Add a headless test that holds a live tagged exit for
a checkpoint, replays it, and asserts the pane is removed; and one where the
runtime was replaced before replay and the stale exit is dropped.
