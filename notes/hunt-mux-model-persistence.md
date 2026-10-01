# Defect hunt: mux model and persistence

Scope: `crates/shepr-mux/src/` `persist.rs` and `persist/`, `workspace.rs` and
`workspace/`, `git/`, `cwd.rs`, `events.rs`, `lib.rs`, `limits.rs`,
`logging.rs`. Findings are ordered by how much they matter. Each one names the
claim it breaks.

## 1. A shell sitting in a deleted directory saves `"<path> (deleted)"` as its cwd

Where: `persist/snapshot.rs` `PendingCwds::resolve` and `capture_workspace`,
fed by `PaneCwdProbe::read` (`pane/runtime.rs`), which reads the path from
`shepr_agent::detect::process_cwd` (`readlink /proc/<pid>/cwd`).

When the shell's working directory has been unlinked, the kernel's readlink of
`/proc/<pid>/cwd` returns `/old/path (deleted)`. That string is absolute, so it
passes the probe's only check (`is_absolute()`). It is then:

- written as the pane's `cwd`, and as the workspace `identity_cwd` when the pane
  is the root pane;
- remembered in `persistence_cwd`. `remembered_cwd()` prefers that value over
  the OSC 7 report, so later captures that cannot probe keep the bogus value.

On restore, `restore_workspace` stats `"/old/path (deleted)"`, gets `NotFound`
and brings the pane back `Unavailable(DirectoryUnavailable)` under that name.
The pane keeps "its saved state verbatim so the next start can try again", so
it never recovers, even when `/old/path` exists again.

Directories are commonly deleted and recreated under a shell: `rm -rf build;
mkdir build`, a branch checkout that removes and recreates a directory, a
tool's scratch directory. In every one of those cases the real path is valid
again, and the pane comes back dead instead of as a shell in that directory.

Claims broken:

- `restore`: "Each pane gets a fresh shell in its saved cwd."
- `PaneCwdProbe::read`: "The shell's absolute /proc cwd." What it returns is a
  display string that is not a path.

The same reader feeds `PaneRuntime::cwd`, and through it the workspace identity
cwd for Git refresh, so the sidebar label and Git status would follow the bogus
path too. That part is outside this scope.

Fix direction: have the persistence probe hand back only a `UsableCwd`, which
stats the path. That type already exists for exactly this and is unused here.
Alternatively, have the probe reject a readlink result that ends in
` (deleted)` and does not exist. Either way, a probe result that is not a usable
directory must not overwrite a good OSC 7 or remembered value.

## 2. Git discovery climbs past a broken gitfile into the enclosing repository

Where: `git/discovery.rs` `locate_git_dir` and
`git_repo_root_below_with_errors`.

Take a `.git` file whose `gitdir:` target is gone, the normal state of a linked
worktree after `git worktree prune` from the main checkout. `locate_git_dir`
returns `Some(target)`. `is_file_entry(target/HEAD)` is then `Ok(false)`, so
`found` is false and the walk pops to the parent. A `.git` file without a
`gitdir:` line, or one that is not UTF-8, also falls through to the bare-layout
check and then the walk ascends.

Git does not ascend from an invalid gitfile. `setup_git_directory_gently`
returns `GIT_DIR_INVALID_GITFILE` and the command fails. Worktrees kept inside
the main checkout (`repo/.worktrees/<name>`, a common layout) therefore get
attributed to the main repository. The sidebar shows the main checkout's
branch, ahead/behind and space key for a directory that is no longer a
checkout of it.

Claims broken:

- `git_repo_root_below_with_errors`: "A directory whose Git state cannot be
  read ... ends the walk with `None` ... rather than being passed over:
  ascending past it could attribute `start` to an enclosing checkout it is not
  part of."
- `entry_type`: discovery follows "as Git's discovery does".

A present-but-invalid gitfile is exactly that case: the code treats it as "no
marker here" rather than "unusable marker here".

Fix direction: once `.git` is a regular file, any outcome other than a gitdir
with a readable `HEAD` ends the walk with `None` and a `FileRead` error. A
`.git` directory without `HEAD` can keep ascending, because Git skips those
too.

## 3. The inline persister does not keep the module's panic contract

Where: `persist/actor.rs` `Worker::Inline`, `SessionPersister::submit`.

The module doc says: "If a worker job panics, it fails closed: it reports that
job and later submissions as abandoned, keeps the writer (and lease) alive, and
releases the lease only after the persister is retired." `persist.rs` repeats
the claim.

Only the thread worker implements it. `Worker::Inline` calls
`done.complete(state.run(job, now))` with no `catch_unwind` and no
`accepting_jobs` latch, so the panic unwinds into the caller, which is the
server's event loop. If something above catches it, later jobs keep running
against a `PersistState` that may be half updated (carry stamps, writer
caches).

Inline is not only the "persists nothing" case. It is also the fallback when
the persister thread cannot be spawned for a production app
(`spawn_error` -> `Worker::Inline`), and that app does submit real saves. In
that fallback every save also does the expensive work on the event loop:
`/proc` cwd reads, scrollback formatting, serializing and fsync. The module
doc's opening paragraph says that work never runs there; the spawn-error log
line admits it.

Structural note: a `Suspended` app constructs a full `SessionWriter`, with
`protect_unloaded` and a `HistoryCarry`, only to hold a lease. Nothing in the
type stops it from writing; the server's policy checks are the only guard. An
owner that persists nothing should hold the `DataDirLease` alone. That would
leave one worker shape (thread), with the panic latch in one place.

## 4. "Created exclusively" recovery copies are not exclusive

Where: `limits.rs` `RECOVERY_SEQUENCE_LIMIT`, `persist/io.rs`
`publish_private_file` with `replace == false`, `persist/writer.rs`
`preserve_existing_in`.

`RECOVERY_SEQUENCE_LIMIT` says: "The copy is created exclusively, so a
concurrent writer that picked the same timestamp moves on to the next one." The
code does not do that:

- `publish_private_file` first unlinks whatever sits at the pending name
  (`remove_stale_temporary`), so a second writer would delete the first
  writer's in-progress temporary.
- The `replace == false` "refusal" is a `symlink_metadata` check followed later
  by a plain `rename(2)`, which overwrites. With two writers, one can publish
  the other's partly written temporary under the recovery name. That is the
  truncated copy `publish_private_file` promises never to leave.

Under the data-directory lease there is only one writer, so this cannot happen
today. The sequence loop and the doc then defend against nothing, and the
`AlreadyExists` branches in `preserve_existing_in` are effectively dead.
`publish_private_file`'s own doc does state the lease precondition correctly.

Fix direction: either make the no-replace publish actually exclusive
(`renameat2(RENAME_NOREPLACE)`, or `link(2)` followed by unlinking the
temporary), or drop the sequence loop and fix the limit's doc to say the lease
is what makes names unique.

## 5. A split's public number and layout are re-derived at commit, not carried from prepare

Where: `workspace.rs` `split_pane` / `commit_new_pane`, `workspace/pane_tree.rs`
`commit_prepared_split`.

`split_pane` builds the child's `SHEPR` pane identity from
`self.next_public_pane_number` at prepare time. `commit_new_pane` reads
`self.next_public_pane_number` again and registers the pane under whatever it
holds then. `NewPane` does not carry the number the child was launched with.

`commit_prepared_split` validates only that the prepared layout's pane-ID set
is the current set plus the new pane. A prepared layout taken before an
intervening resize, swap, ratio change or focus change passes that check and
silently replaces the current tree, reverting the change.

The doc comments ("prepared on a clone and installed by `commit_new_pane`",
"`false` ... when the prepared layout is not this layout plus exactly
`pane_id`") present this as a safe two-phase operation. Today it is safe only
because the one production caller (`handle_pane_split`) runs both halves in
one synchronous handler.

The prepare/commit split exists so a PTY spawn can sit between them. Either:

- carry the reserved number and a layout generation in `NewPane` and refuse a
  stale commit, or
- collapse the two phases into one `&mut self` call. The spawn already happens
  synchronously inside `split_pane_shell`.

## 6. `resolve_write_target` reads every stat error as "not a symlink"

Where: `persist/io.rs` `resolve_write_target`.

`Err(_) => return Ok(current)` treats `EACCES`, `EIO` and the like the same as
`NotFound`. A link the writer cannot inspect is then written over as a plain
file path. Writes later fail on the same error in most cases, so the impact is
small.

It does contradict the discipline stated two functions below in
`missing_directory_chain`: "A stat error other than `NotFound` is returned:
read as absence it would ... [be] wrong." `clear_path` uses the same resolver,
so a clear can likewise target the link's own path instead of its target.

## 7. History and layout are paired by layout shape, not by save

Where: `persist/snapshot.rs` `layout_fingerprint`, `persist/restore.rs` history
filter.

The history file is accepted when its fingerprint equals the layout's. The
fingerprint covers the tree and the pane IDs, and pane IDs are a per-process
counter that restore reassigns deterministically in layout order. So a layout
restored and left unchanged has the same fingerprint in every boot.

If one save commits the layout and then fails to write the history (a failure
the writer reports but tolerates), the history file left on disk can be from
an earlier boot. Restore still accepts it as "matching", and replays
scrollback older than the layout it is paired with.

The writer's comments call history "the history that pairs with it [the
committed layout]". Restore's filter cannot tell that pairing apart from "a
history written for an identically shaped layout some time earlier".

This is low severity: the content still belongs to the same panes. It is a
place where a structural change would remove a class of reasoning:

- give each save a generation id stamped into both files and pair on that; or
- write layout and history as one directory published by a single rename.

## 8. Smaller items

- `git/mod.rs`: `GitReadError::ConfigEnvironment` ("Git's config environment
  is refused, incomplete, or invalid") is never constructed. An error from
  reading Git's config environment
  (`git_user_config_paths_at` -> `shepr_core::env::read_*`) surfaces as
  `FileRead` on `<common dir>/config`, so the log names the wrong thing.
  Either construct the variant there or delete it.
- `persist/io.rs`: the doc comment on `enum SessionLoad` begins with "Reads the
  saved layout while the caller owns the data directory." That line belongs to
  `load` and was left above the enum.
- `persist/io.rs` `serialize_history_within`: in the trimmed shape, a pane
  whose history is empty (size 0) is left out entirely, because
  `kept > 0` is false, rather than written as `""`. Restore treats both the
  same, so this is harmless, but the `Shape::Cut` doc says `None` means a
  trimmed pane.
- `persist/actor.rs` `SessionPersister.finished` is documented as "Fired once
  per submitted job". It is a `Notify::notify_one`, so completions that land
  while nobody waits collapse into one permit. The server reaps one save at a
  time, so this is correct today; the wording promises a count the signal does
  not keep.

## Lateral findings outside the scope

- `crates/shepr-server/src/app/git_refresh.rs`: `reported_git_read_errors` is
  a `HashSet<GitReadError>` that is never pruned, and the errors carry stderr
  text and paths. `git_status_cache` drops only fingerprint-less entries
  (`mark_due`), so an entry for every repository a workspace has ever visited
  stays for the life of the server. Both grow without bound on a long-lived
  server.
- `Workspace::resolved_identity_cwd_from` -> `cwd_for_pane` ->
  `PaneRuntime::cwd` does a `/proc` readlink per workspace on the event loop
  every Git refresh. The doc admits it ("App-side convenience"). It is the same
  `(deleted)`-blind reader as finding 1, so a workspace whose root shell is in
  a deleted directory gets Git status and a label for a path that does not
  exist.
- `PaneRuntime::remembered_cwd` prefers the last persistence probe over a
  newer OSC 7 report. When a later probe fails (for example, the shell has just
  exited), the save writes the older probed cwd even though the shell reported
  a newer one.
