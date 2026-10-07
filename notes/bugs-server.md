# Bugs: server (shepr-server app, ui and serving, shepr-api, shepr-daemon)

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Filed from the server app and render hunt and the server serving and API hunt.
The raw reports,
including each one's list of areas checked and found sound, are in commit
6dc81572 (`notes/hunt-server-app.md`, `notes/hunt-server-serving.md`,
`notes/hunt-mux-git.md`).

## SRV-013 - A workspace created with an explicit cwd silently starts elsewhere when that directory cannot be entered, and keeps the name of the directory it is not in

Where: `App::handle_workspace_create` (`app/api/workspaces.rs`) together with
`create_workspace_outcome` (`app/creation.rs`).

What happens: `WorkspaceCreateSource::Cwd(raw)` is only checked lexically
(`launch_cwd`) and then launched with `LaunchKind::Fresh`. Fresh launches fall
back to `HOME`, the passwd home or `/` when the chdir fails
(`LaunchKind::requires_cwd` is false for `Fresh`). `prepare_workspace(cwd)` has
already named the workspace after the requested path. If the user typed a wrong
or nonexistent path:

- the command answers `Done`;
- the workspace appears under the requested directory's name but runs in
  `$HOME`;
- the only trace is a WARN `pane.cwd outcome = Fallback` in the server log.

Claim broken: `WorkspaceCreateSource` documents itself as "Where a new
workspace's first pane starts", with `Cwd` as "An explicit working directory".
Its sibling, `Default`, resolves through the server's new-terminal-cwd policy,
where a fallback is reasonable. An explicit path the user named is a different
case.

Direction: either launch an explicit `Cwd` with a required cwd (a failure
becomes the existing placeholder, "Pane directory is unavailable"), or report
the fallback to the requester. The workspace name should follow the cwd the
launch settled in, or the refusal.

A wave 3 fixer confirmed it and found the fix needs a distinct
required-cwd launch kind in `shepr-mux`, threaded through workspace creation
(`LaunchKind::Restored` already requires its cwd, but would mislabel the
launch). The boundary is noted at `app/creation.rs`.
