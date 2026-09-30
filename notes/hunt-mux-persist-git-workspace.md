# Defect hunt: shepr-mux persist, git, workspace, events, render_signal, cwd, lib

Scope: `crates/shepr-mux/src/persist*`, `git/`, `workspace*`, `events.rs`,
`render_signal.rs`, `cwd.rs`, `lib.rs`, plus the server call sites these hand
values to (`shepr-server/src/app/mod.rs` restore wiring, `app/session.rs` save
scheduling, `app/git_refresh.rs`, `app/api/panes.rs` split).

Findings are ordered by how much they matter. Each one names the claim it
breaks.

---

## 1. One bad pane field throws away the whole saved session, though the docs say restore drops only the bad workspace

**Claim broken.** `persist/snapshot.rs`, `parse_snapshot` says: "Deserializes
the saved shape only. Semantic checks stay in `restore`, so one invalid
workspace can be dropped while healthy ones survive". `restore.rs` repeats it
("An invalid saved split ratio drops this one workspace, like every other
per-workspace restore defect below, rather than refusing the whole session").

**What the code does.** Several semantic checks run inside serde, so they fail
the whole `SessionSnapshot` parse, not just one workspace:

- `PaneSnapshot.cwd` and `WorkspaceSnapshot.identity_cwd` go through
  `path_bytes::deserialize_saved_cwd`, which returns a serde error for a
  relative path.
- `PaneSnapshot.agent_session` is a `PaneAgentSessionSnapshot` with a typed
  `shepr_agent::agent::Agent`, whose `Deserialize` (`shepr-agent/src/agent/mod.rs`)
  fails with "unknown agent label" for any label the current build does not
  know. `AgentSource` and `AgentSessionRef` validate the same way.

When any of these fails, `io::load` logs `parse_error` and returns `None`, the
server restores nothing (`app/mod.rs` sets `protect_unloaded`), and every
workspace is gone from the live session. The file does get copied to
`session-backups` before the first save, but nothing restores it
automatically.

**Why it is realistic.** AGENTS.md says detection changes ship as new builds,
and the owner rebuilds often. Suppose a build renames or removes an `Agent`
variant, or changes what `AgentSource` accepts. Then the saved file from the
previous build fails to parse as soon as any pane had a hook-reported session.
Every workspace is lost to save one agent session reference. A hand edit
that makes one cwd relative does the same.

**Fix.** Parse permissively and validate in restore:

- Take `agent_session` as `Option<serde_json::Value>` (or a lenient newtype
  that turns any error into `None` plus a warning), and convert it in
  `restore_plan_for_snapshot` / `restored_terminal_agent_session`.
- Parse the cwds as plain `PathBuf`s (keeping the byte-sequence form), and
  check them for being absolute in `restore_workspace`. A bad pane there
  becomes `RestoredPaneStart::Unavailable`, and a bad `identity_cwd` falls
  back to the root pane's cwd.
- Only `SnapshotVersion` should reject the whole file. That check is the
  file's format identity.

## 2. Restore validates public pane numbers only after it has started every shell

**Claim broken.** `restore_workspace` says: "That happens before any pane
starts, so every shell starts at its size in the layout the workspace ends up
with". It also describes the per-workspace defects as things that drop the
workspace before anything runs.

**What the code does.** Two checks run only after the loop has called
`PaneRuntime::spawn_with_initial_history` for every surviving pane:

- the duplicate or zero public-number check, in `Workspace::from_restored` via
  `valid_panes`;
- the "restored pane has no public number" check.

When either one drops the workspace, the code has already:

- forked a shell for every pane. Each one got a `SHEPR_PANE_ID` launch env,
  duplicated across panes in the collision case. The runtimes are then
  dropped, and `PaneRuntime::drop` tears down the process sessions.
- inserted resume reservations into `resumed_agent_sessions` for panes of the
  dropped workspace. A later, healthy pane with the same saved session is then
  treated as a duplicate. `restored_terminal_agent_session` returns `None` for
  it, so that pane loses its agent session for good, and the next save writes
  it out without one.
- added `history_carry.carry_restored` entries. These are harmless: the first
  save prunes them.

**Fix.** Everything `valid_panes` checks is known before any spawn: public
numbers come from `assign_public_pane_numbers`, and the other checks are
structural. Run `valid_panes` (or an equivalent on the planned pane set) right
after pruning, and reserve resume keys only once the workspace is known to
survive. The best structure is a two-phase restore: first validate the whole
snapshot into plain restore plans with no side effects, then spawn. That
removes this ordering hazard for good.

## 3. A panic on the persister thread releases the data-directory lease while the server keeps running

**Claim broken.** `persist/actor.rs` module docs say: "The lease is released
only when the persister is retired, after every job submitted before has
finished". `persist.rs` says one server at a time owns a data directory.

**What the code does.** The lease sits inside `PersistState` on the
`shepr-persist` thread. If a job panics (for example in history formatting or
serialization, which run there), the thread unwinds and drops the state, and
the lease with it. `retire` only logs this later. Meanwhile:

- the server keeps running with every later job answered `abandoned`, and the
  save loop retries with backoff forever. The user gets no live persistence
  and no loud signal.
- the lease on `session.lock` is free. A second server on the same profile can
  acquire it and restore the stale file, while the first server still owns
  live panes.

**Fix.** Pick one:

- Make the lease outlive the worker. Hold the `DataDirLease` in
  `SessionPersister` itself, not in the thread's state, and release it only in
  `retire`.
- Treat a dead persister as fatal: on `abandoned`, the event loop logs an
  error and shuts down cleanly.

The first is the direct fix for the documented contract.

## 4. Git status passes unvalidated ref-file contents to `git rev-list` as argv

**Claim broken.** The git reader goes to some lengths not to trust repository
files: `GitReadError::FileRead` says "A repository file could not be read or
was not safe to trust", and `read_ref_oid_with_errors` refuses stale packed
fallbacks. But the OIDs it produces are never validated as object names.

**What the code does.** Here is how the OIDs reach `git`:

- `read_ref_oid_with_errors` returns the trimmed content of a loose ref file,
  or the first token of a packed-refs line, verbatim.
- `read_head_identity_from_files` does the same with a detached `HEAD`.
- `git_ahead_behind_between` formats `"{head_oid}...{upstream_oid}"` and runs
  `git rev-list --left-right --count <that>`, with no `--end-of-options` or
  `--`.

So a loose ref whose content starts with `-` becomes a git option. For example,
`.git/refs/heads/main` containing `--output=/path/x` makes git's revision
parser handle the diff option `--output=`, which opens that path for writing.

There are more ways to get arbitrary paths in:

- `full_ref` comes from `HEAD` (`ref: refs/heads/...`) or from the branch's
  `merge` config. `upstream_full_ref` returns `merge_ref` unchanged for
  `remote = "."`, and `common_dir.join(full_ref)` with an absolute or `..`
  path reads any file (up to the ref size cap) as an "oid".
- A legitimate symbolic loose ref (`ref: refs/heads/other`, used for branch
  aliases) is also passed through as an oid. rev-list then fails, and the
  refresh goes into a permanent retry loop.

**Threat model.** This needs someone else's `.git` directory on disk, from an
extracted tarball or a copied checkout, since `git clone` never writes these
files. Running `git` in such a directory already carries config risk. The
marginal risk is that shepr turns plain ref files, which Git itself would
treat as broken, into argv options.

**Fix.**

- Accept an oid only if it is 40 or 64 lowercase hex characters, and put
  `--end-of-options` before the range.
- Reject `full_ref` values that are absolute, contain `..`, or do not start
  with `refs/`, as Git's `check_refname_format` does.
- Follow `ref: ` indirection in loose refs, or report the ref as unavailable,
  rather than taking the text as an oid.

## 5. A drop by partial restore backs up the layout but not the history that pairs with it

**Claim broken.** `restore.rs` says of a dropped workspace: "The workspace is
not lost on disk: a nonzero `RestoredSession::dropped_workspaces` makes the
first save back the original file up before overwriting it."

**What the code does.** `SessionWriter::preserve_unloaded` copies only
`session.json` into `session-backups`. The first save also replaces
`session-history.json`, and the history in it belongs to the backed-up
layout: `layout_fingerprint` pairs them. Restoring from the backup therefore
brings the layout back without its screen history. The periodic
`session-snapshots` copies are layout-only too.

This is only a real loss with `experimental.pane_history` on. Either copy the
history file next to the layout backup (same timestamp and sequence name) or
narrow the doc to say only the layout is preserved.

## 6. `from_existing_pane`, `ExistingPane` and the "pane move" rationale are production surface with no production caller

**Claim broken.** AGENTS.md: "the goal is the smallest code surface that does
what the owner uses". Also the stale-doc rule.

**What the code does.**

- `Workspace::from_existing_pane` and `pane_tree::ExistingPane` are `pub`, but
  their only caller is `shepr-server/src/test_support.rs`.
- The comment on `NEXT_WORKSPACE_NUMBER` in `workspace.rs` justifies the
  process-global counter by saying an owned allocator "would have to be
  threaded into every workspace constructor, pane move and restore". No pane
  move exists anywhere in the tree.
- `detach_pane` returns a `DetachedPane` that its only caller, `remove_pane`,
  discards.
- `commit_new_pane` sets the public number twice: once in
  `commit_prepared_split` and again in `register_new_pane_with_number`.

These are small, but they are claims about capabilities that do not exist.
Move `from_existing_pane` behind `#[cfg(test)]`, or into test support as a
seam if the server tests need it. Drop "pane move" from the comment.

## 7. `events.rs` module doc is stale

It says background tasks include "future hook listeners". Hook state already
arrives through `AppEvent::HookStateReported` and `AgentSessionReported`.
Reword it as "PTY child watchers, detectors, hook reports, the git refresh".

## 8. Overflow on hand-edited public numbers (dev build panics on restore)

**Claim broken.** `PaneSnapshot.public_number` says a damaged file is handled:
"Restore gives a pane with none, or with zero ..., a fresh free number."

**What the code does.** In `restore_workspace`, `next_public_pane_number`
starts as `...max(snap.next_public_pane_number)`. If the file says
`next_public_pane_number: 18446744073709551615` and some pane lacks a number,
`assign_public_pane_numbers` runs `*next_public_pane_number += 1`. That is an
overflow panic in the dev profile (the server dies on every start until the
file is fixed), and a wrap in release. `valid_panes` would drop the workspace
anyway, but only if execution gets that far. Live splits have the same
pattern in `register_new_pane_with_number` (`number + 1`), though a live
counter cannot get there.

Use `checked_add` and treat exhaustion as a per-workspace defect.

## 9. Git config reimplementation: behaviour gaps against Git that the docs present as Git's semantics

`git/config.rs` and `git/discovery.rs` re-derive Git's config chain to find
the upstream without spawning git. Some of the docs promise Git-equivalent
behaviour: `git_dir_is_bare`: "Git's effective `core.bare` ... comes from the
whole config chain ... each with its includes". The implementation differs
from Git in ways that change the answer:

- `includeIf "gitdir:"`, `onbranch:` and `hasconfig:remote.*.url:` go through
  `wildcard_match`, where `*` matches across `/`, and `?`, `[...]` and `\`
  escapes are literal. Git uses wildmatch with `WM_PATHNAME`, where `*` stops
  at `/` and only `**` crosses it. For example,
  `gitdir:/work/*/.git` includes configs for any depth here, but for one level
  in Git, so the upstream (and ahead/behind) can come from a config Git would
  not read.
- `normalize_config_value` strips outer quotes only. It does not handle
  escapes (`\"`, `\\`, `\t`), mid-value quoting (`a"b c"d`), or continuation
  lines ending in `\`. It also skips the deprecated `[branch.main]`
  subsection syntax.
- packed-refs parsing in `read_ref_oid_with_errors` uses `parts.next()?` in a
  loop. One line with an oid but no name aborts the whole lookup, where it
  should skip the line. packed-refs is also read with no size cap, although
  loose refs are capped at `MAX_GIT_REF_FILE_BYTES`.

**Structural recommendation.** Stop reimplementing Git's config resolution.
The fingerprint needs only stat-level change detection, and that can be kept:
stamp `HEAD`, the branch's loose ref, `packed-refs`, and every config file Git
would read. When something changes, ask Git itself with one spawn:

`git for-each-ref --format='%(refname) %(objectname) %(upstream) %(upstream:track,nobracket)' refs/heads/<branch>`

or `git rev-parse --symbolic-full-name @{u}` followed by the existing
`rev-list`. That removes about 1200 lines of config grammar (`config.rs`)
whose correctness depends on matching Git's behaviour in every detail, fixes
all the fidelity gaps above at once, and closes finding 4's config-driven
paths. Git status is refreshed on an interval off the loop, so one extra spawn
per changed repository costs nothing that matters.

---

## Checked and found sound (so they need no re-audit)

- Two-file save atomicity. `session.json` and `session-history.json` are
  published separately, but the history carries the layout fingerprint and
  `restore` ignores a mismatched history. A crash between the two files
  therefore loses screen history, never pairs it with the wrong panes.
- `publish_private_file`: temp, fsync, rename, directory fsync; 0600 mode; a
  stale temp is removed under the lease; `NotDurable` is handled on both the
  replace and the no-replace paths.
- `SaveCompletion` fires the notify after the result is readable, on every
  way a job ends (reported, dropped, refused, retired), and `notify_one` keeps
  a permit.
- The `HistoryCarry` "Unchanged" shortcut. It is taken only when the file
  stamp matches the last write and the stamp of every pane revision and the
  layout fingerprint are equal. Failures and clears reset it.
- `serialize_history` trimming. The byte-identity with serde's pretty output
  and `fair_share` / `recent_cut` hold as documented (exhaustively tested
  against the assembled reference).
- `remap_saved_index` and `restored_workspace_id` / `reserve_workspace_ids`
  keep the saved bookmark and ID uniqueness across dropped workspaces.
- `RenderSignal`: every state change happens under the one mutex, `pending` is
  set under it too, and there are no lost wakes.
- The split path (`api/panes.rs` `handle_pane_split`) prepares and commits
  synchronously, so the `SHEPR_PANE_ID` number in the launch env always
  equals the number committed.
- `aggregate_state` picks between Idle and Unknown by HashMap order, but both
  present as Idle, and the only consumer maps it through
  `presentation_state()`.

## Lateral notes (outside scope, noticed on the way)

- `aggregate_state` returns `AgentState` where every consumer wants
  `PresentedAgentState`. Returning the presented state would make the
  "Unknown presents as Idle" rule a type fact, not a convention each caller
  must remember.
- `io::load` reads `session.json` with no size cap, and
  `snapshot_history_decision` reads both the live file and the newest recovery
  copy in full on every save outside the snapshot interval. Only the history
  file has a cap.
- `resolve_write_target` returns the path it reached after
  `MAX_SESSION_PATH_SYMLINK_HOPS` even if that path is still a symlink. The
  rename then replaces that link with a regular file, where it should report
  a loop.
- `SessionPersister::drop` joins the persister thread. If `App` is dropped on
  a tokio worker while a big history save is running, that worker blocks for
  the length of the save. That is fine at shutdown, but worth knowing if an
  `App` is ever dropped anywhere else.
