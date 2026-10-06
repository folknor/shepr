# Defect hunt: shepr-mux and shepr-git

Scope: `crates/shepr-mux` (pane runtime, launch settlement, exit arbiter and
`PaneEnding`, workspaces and `PaneTree`, events, persistence, cwd, render
signal) and `crates/shepr-git` (discovery, runner, config tracking, status,
refresh, worker), followed into `shepr-server`'s app layer where the value
crosses. Findings are ordered by how much they matter. Each names the claim it
breaks.

## M1. A pane whose child wait failed is removed without a checkpoint, losing its agent session

Claim broken: `PaneEnding::needs_checkpoint` (`crates/shepr-mux/src/pane/exit_arbiter.rs`)
documents that a checkpoint is taken for "every ending the user did not ask
for (a signal, a reader panic or IO failure, a closed terminal), so its agent
session is kept for resume". AGENTS.md repeats that `needs_checkpoint()` is
"the one answer to whether the exit gets a final session checkpoint".

What the code does: `PaneEndReason::WaitFailed` returns `false`. The child
watcher (`pane/child_watcher.rs`, the `Err` arm of the spawned task) records
`WaitFailed` with `child_exit_confirmed: false` when `try_wait`/`waitid`
fails, i.e. exactly when the child may still be alive. The user did not ask
for that ending. On the server side `App::decide_pane_exit`
(`crates/shepr-server/src/app/events.rs`) then takes `CheckpointDecision::Unchecked`,
and `transition_pane_exit(needs_checkpoint = false)` in
`crates/shepr-detect/src/ownership/source/detection.rs` skips the branch that
writes the session identity back. The pane is removed, its runtime dropped
(teardown kills the possibly-live agent) and the next save omits it, so a
running agent's resume identity is lost with no durable record.

Which side is wrong: the code. `WaitFailed` is an involuntary ending of a
pane whose child may be alive; it belongs with `ReaderIoFailed` in the
checkpointed set. The `checkpoint_follows_the_reason` test pins the current
(wrong) value and would change with it.

## M2. A hung mount holds one abandoned Git worker thread per distinct cwd or checkout, not one per mount, and four of them stop Git status everywhere

Claim broken: the `crates/shepr-git/src/worker.rs` module doc: "the paths
its stalled step reads are left out of later refreshes until it finishes, so a
mount that stays hung holds one thread, not one per refresh". Also
`MAX_ABANDONED_GIT_REFRESH_THREADS` ("bounds the threads a hung mount can
hold; past it a stalled refresh is waited out").

What the code does: the stuck-path list is what the step itself names, not
the path that hung. A discovery step records `[target.cwd]`
(`refresh::group_targets`); a checkout step records `[key, cwds...]`
(`refresh::compute_refresh`). `GitStatusWorker::is_stuck` only skips targets
whose cwd or key `starts_with` one of those paths. On a hung NFS mount at
`/net`, workspaces in `/net/a` and `/net/b` are unrelated by prefix: the
first stalls and is abandoned after `GIT_REFRESH_STALL_BOUND`, the
replacement thread walks into `/net/b` and stalls too, and so on. The same
happens when the hang is outside every named path, for instance an NFS
`$HOME` whose `~/.gitconfig` (stamped by `config::git_user_config_paths_at` /
`config_deps` with a plain `metadata`, no deadline) or whose includes every
checkout step reads: each distinct checkout costs one abandoned thread. After
four, `abandon_if_stalled` refuses to abandon and the current thread is
"waited out", which on a mount that stays hung means forever: no workspace on
the server gets a Git status again (`App::start_git_status_refresh_if_due`
never leaves `InFlight`).

Which side is wrong: the code, or the doc overstates. A fix that keeps the
claim would record the path actually being touched at each blocking call (or
the mount point / filesystem device of it) rather than the step's nominal
inputs, and treat shared dependency paths (user config, includes, the common
dir) as their own step paths.

## M3. Repository discovery walks the logical (OSC 7) path and crosses filesystem boundaries, so it can attribute a cwd to a repository Git would not

Claim broken: `discovery.rs` presents the walk as Git's discovery
(`entry_type`: "following symlinks as Git's discovery does";
`GitCeilings`: "read with Git's semantics"; the runner doc: "Git follows the
user's own configuration and discovery"). AGENTS.md: "Git status in the
sidebar (branch, ahead/behind)" for the workspace's cwd.

What the code does: the target cwd is the workspace's resolved identity cwd
(`Workspace::resolved_identity_cwd` -> `PaneRuntime::cwd` ->
`ReportedCwd::resolve`), which is deliberately the OSC 7 *logical* path
("OSC 7 carries what /proc cannot: a logical path through symlinks").
`git_worktree_location_below` then pops components of that spelling. Git
resolves the physical cwd (`getcwd`) and walks physical parents. The two
diverge whenever a symlink sits on the path:

- `~/link -> /data/proj/sub` with `/data/proj` a repository: shepr walks
  `~/link`, `~`, `/` and reports OutsideRepository (or `~`'s repository, see
  next); Git reports `/data/proj`.
- `~/link -> /data/work` (no repository there) with a dotfiles repository at
  `~/.git`: shepr reports the dotfiles branch; Git reports none.
- `GitCeilings::contains` compares the popped logical spelling against the
  ceiling's spelled and canonical forms, so a ceiling at the symlink's
  target is never met by a walk that goes through the link.

Separately, Git stops at a filesystem boundary unless
`GIT_DISCOVERY_ACROSS_FILESYSTEM` is set; the walk has no boundary check, so a
cwd on its own mount under a `$HOME` repository shows `$HOME`'s branch where
`git status` in that directory says "not a git repository (... filesystem
boundary)".

A smaller divergence of the same kind: `locate_git_dir` accepts a `.git`
directory as soon as it has a regular `HEAD` file; Git's `is_git_directory`
also requires `objects/` and `refs/` (or a `commondir`), and skips past a
`.git` that lacks them.

Which side is wrong: the code. Discovery should start from the canonical
(physical) path, as Git does, and honour the filesystem boundary rule; the
logical path can still be kept for display and for the per-cwd admission in
`Workspace::apply_git_status`.

## L1. A launch settlement overwrites a cwd the shell already reported

`App::handle_pane_launch_settled` (`crates/shepr-server/src/app/pane_launch.rs`)
applies `StateEvent::TerminalCwdReported { cwd: <launch candidate> }` when a
launch settles `Launched`. The coordinator (`launch_status::coordinate`)
publishes that settlement with an awaited send after reading the status
socket's EOF; the PTY reader publishes the shell's own OSC 7 with `try_send`
from `publish_reported_cwd`. Nothing orders the two (the only ordering the
code promises, in `events.rs`, is settlement before `PaneDied`). If the
shell's first prompt OSC 7 (after an rc-file `cd`) is admitted first, the
settlement replaces the stored terminal cwd with the launch directory, and
the runtime's dedupe slot (`PaneCwdState::reported`) then suppresses the same
OSC 7 path on every later prompt, so the stored cwd stays wrong until the
shell changes directory. Impact is limited because `terminal_cwd` prefers the
runtime's live observation, but the stored cwd is what saves and identity
fall back to once the runtime has no observation. The settlement should only
seed the stored cwd when no report has been accepted for that runtime yet.

## L2. Stale comment: Git status refresh is not tied to connected clients

`GIT_REMOTE_STATUS_REFRESH_INTERVAL` in `crates/shepr-server/src/limits.rs`
says "Refresh Git ahead/behind status periodically while clients are
connected". `GitRefreshScheduler::deadline` keys only on
`has_workspaces`, and AGENTS.md says "A server always computes a workspace's
Git branch and ahead/behind, whatever any sidebar shows". The comment is the
wrong side; it should drop the client condition.

## L3. Stale comment: recovery copies are published exclusively

`RECOVERY_SEQUENCE_LIMIT` in `crates/shepr-mux/src/limits.rs` says "The
publish is not exclusive against a second writer and does not need to be."
`recovery::copy_recovery` publishes with `PublishTarget::CreateOnly`, which
`files::publish_private_file` documents as "an existing `target` is
atomically refused with `AlreadyExists`", and `preserve_opened_source` has an
`AlreadyExists` arm for exactly that. The comment is wrong; the code is
exclusive.

## L4. `ResumeFailed` renders two guidance sentences and two `Error:` prefixes

`PaneStartFailure::cause` for `ResumeFailed` formats the inner failure with
`Display` (`"{failure} Agent: ..."`), and the inner `Display` already writes
its own guidance and `" Error: ..."`. The outer `Display` then writes the
outer guidance, `" Error: "`, and that whole string, so the pane placeholder
reads "Could not resume the saved agent. ... Error: Could not start the pane
shell. Check ... Error: <errno> Agent: ...". `cause()` should use the inner
failure's own `cause()` (and, if wanted, its guidance once).

## Lateral observations (not defects against a written claim)

- Reftable checkouts spawn Git on every refresh. `read_head_identity_from_git`
  runs `git symbolic-ref` and `git rev-parse --verify`, and `read_upstream`
  another `rev-parse`, inside `fingerprint`, which runs on every refresh of a
  cached hit. With `GIT_REMOTE_STATUS_REFRESH_INTERVAL` at 1.5 s and no client
  gating, that is three Git processes per reftable checkout every 1.5 s for the
  server's lifetime, where a files-backend checkout costs only a few stats. A
  stamp of the reftable directory (`reftable/tables.list`) as a fingerprint
  dependency would let the hit path skip the probes.
- A `ConfigCtx` that is `Dependencies::Uncacheable` (a `~user` include, a
  refused config environment variable, a config that changed between the two
  `config --list` runs) is rebuilt on every refresh: two `git config --list`
  runs and a `for-each-ref` per checkout every 1.5 s, indefinitely.
- `run_git`'s pipe readers (`runner::read_until`) have no byte cap; every
  current probe is small, but `config --includes --show-origin --list` is
  user-controlled in size and read into memory whole, twice per rebuild.
- With a known key, `refresh::compute_refresh` rereads status from the key's
  path, not the cwd. A cwd that becomes its own repository in place (`git
  init` in the pane's directory) keeps the enclosing checkout's branch until
  the next rediscovery (`GIT_REPO_DISCOVERY_REFRESH_INTERVAL`, 5 minutes).
  Designed, but the rediscovery comment is the only place it is stated.
- `follow_cwd` and `foreground_cwd` (`pane/runtime/cwd.rs`) read
  `/proc/<pgid>/cwd` of the foreground group's leader pid without a pidfd: once
  that leader has exited while its group lives on (`a | b` with `a` gone), the
  pid can name an unrelated process, whose cwd a split would then inherit.
  Everything else in the pane runtime goes through `ChildLiveness::observe` to
  avoid exactly this.
- `osc7::parse_reported_cwd` trims the payload before percent-decoding and
  `percent_decode_utf8` refuses non-UTF-8 bytes, so a directory whose name
  ends in whitespace, or is not UTF-8, is never taken from OSC 7 (the saved
  schema goes out of its way to keep non-UTF-8 paths).
