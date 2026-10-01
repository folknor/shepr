# Defects: terminal core (shepr-vt, shepr-pty)

Filed from the defect hunt over `crates/shepr-vt/src/` and `crates/shepr-pty/src/`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## TCORE-004 - Pane spawn does blocking filesystem work on the server event loop

Lateral, filed by the hunter as a design observation rather than a broken
claim. `PtyCommand::to_std_command` stats the requested cwd (`usable_directory`)
and walks `PATH` (`resolve_executable` with `classify_candidate`, a `stat` plus
`access` per candidate) on the calling thread, and the parent's
`Command::spawn` waits for the child's chdir and exec. Every pane spawn and
restore runs this on the server's event loop, so a hung mount in the cwd,
`HOME` or any `PATH` entry stalls the whole server, not just resume. This is
wider than the note in `agent_resume.rs` acknowledges.
