# Defects: mux model and persistence

Filed from the defect hunt over `crates/shepr-mux/src/` `persist.rs` and
`persist/`, `workspace.rs` and `workspace/`, `git/`, `cwd.rs`, `events.rs`,
`lib.rs`, `limits.rs`, `logging.rs`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## MUXP-001 - A shell sitting in a deleted directory saves `"<path> (deleted)"` as its cwd

Where: `persist/snapshot.rs` `PendingCwds::resolve` and `capture_workspace`, fed
by `PaneCwdProbe::read` (`pane/runtime.rs`), which reads
`shepr_agent::detect::process_cwd` (`readlink /proc/<pid>/cwd`).

When the shell's working directory has been unlinked, readlink of
`/proc/<pid>/cwd` returns `/old/path (deleted)`. That is absolute, so it passes
the probe's only check (`is_absolute()`). It is then written as the pane's
`cwd` (and the workspace `identity_cwd` when the pane is the root pane), and
remembered in `persistence_cwd`; `remembered_cwd()` prefers that over the OSC 7
report, so later captures that cannot probe keep the bogus value.

On restore, `restore_workspace` stats `"/old/path (deleted)"`, gets `NotFound`
and brings the pane back `Unavailable(DirectoryUnavailable)` under that name.
The pane keeps "its saved state verbatim so the next start can try again", so it
never recovers, even when `/old/path` exists again. Directories are commonly
deleted and recreated under a shell (`rm -rf build; mkdir build`, a branch
checkout that removes and recreates a directory, a tool's scratch directory).

Claims broken: `restore`: "Each pane gets a fresh shell in its saved cwd."
`PaneCwdProbe::read`: "The shell's absolute /proc cwd" (it returns a display
string that is not a path).

The same reader feeds `PaneRuntime::cwd`, and through
`Workspace::resolved_identity_cwd_from` -> `cwd_for_pane` -> `PaneRuntime::cwd`
(a `/proc` readlink per workspace on the event loop every Git refresh) the
workspace identity cwd for Git refresh, so a workspace whose root shell is in a
deleted directory gets Git status and a label for a path that does not exist.

Fix direction: have the persistence probe hand back only a `UsableCwd`, which
stats the path (the type already exists and is unused here), or reject a
readlink result ending in ` (deleted)` that does not exist. A probe result that
is not a usable directory must not overwrite a good OSC 7 or remembered value.

## MUXP-002 - Git discovery climbs past a broken gitfile into the enclosing repository

Where: `git/discovery.rs` `locate_git_dir` and `git_repo_root_below_with_errors`.

A `.git` file whose `gitdir:` target is gone (the normal state of a linked
worktree after `git worktree prune` from the main checkout): `locate_git_dir`
returns `Some(target)`, `is_file_entry(target/HEAD)` is `Ok(false)`, `found` is
false and the walk pops to the parent. A `.git` file without a `gitdir:` line,
or not UTF-8, also falls through to the bare-layout check and the walk ascends.
Git does not ascend from an invalid gitfile (`setup_git_directory_gently`
returns `GIT_DIR_INVALID_GITFILE` and the command fails). Worktrees kept inside
the main checkout (`repo/.worktrees/<name>`) get attributed to the main
repository: the sidebar shows the main checkout's branch, ahead/behind and space
key for a directory that is no longer a checkout of it.

Claims broken: `git_repo_root_below_with_errors`: "A directory whose Git state
cannot be read ... ends the walk with `None` ... rather than being passed over:
ascending past it could attribute `start` to an enclosing checkout it is not part
of." `entry_type`: discovery follows "as Git's discovery does".

Fix direction: once `.git` is a regular file, any outcome other than a gitdir
with a readable `HEAD` ends the walk with `None` and a `FileRead` error. A `.git`
directory without `HEAD` can keep ascending, because Git skips those too.

## MUXP-003 - `GitReadError::ConfigEnvironment` is never constructed

`git/mod.rs`: the variant ("Git's config environment is refused, incomplete, or
invalid") is never built. An error from reading Git's config environment
(`git_user_config_paths_at` -> `shepr_core::env::read_*`) surfaces as
`FileRead` on `<common dir>/config`, so the log names the wrong thing. Construct
the variant there or delete it.

## MUXP-004 - The doc comment on `enum SessionLoad` belongs to `load`

`persist/io.rs`: the doc on `SessionLoad` begins with "Reads the saved layout
while the caller owns the data directory." That line belongs to `load` and was
left above the enum.

## MUXP-005 - A trimmed history leaves out empty panes instead of writing `""`

`persist/io.rs` `serialize_history_within`: in the trimmed shape a pane whose
history is empty (size 0) is left out entirely, because `kept > 0` is false,
rather than written as `""`. Restore treats both the same, so harmless, but the
`Shape::Cut` doc says `None` means a trimmed pane.

## MUXP-006 - `SessionPersister.finished` promises a count `Notify` does not keep

`persist/actor.rs`: documented as "Fired once per submitted job". It is a
`Notify::notify_one`, so completions that land while nobody waits collapse into
one permit. The server reaps one save at a time, so correct today; the wording
promises a count the signal does not keep.

## MUXP-007 - Git refresh caches grow without bound on a long-lived server

Lateral, `crates/shepr-server/src/app/git_refresh.rs`: `reported_git_read_errors`
is a `HashSet<GitReadError>` that is never pruned, and the errors carry stderr
text and paths. `git_status_cache` drops only fingerprint-less entries
(`mark_due`), so an entry for every repository a workspace has ever visited stays
for the life of the server.
