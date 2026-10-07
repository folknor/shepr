# Bugs: mux and Git (shepr-mux, shepr-git)

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Filed from the mux and Git hunt. The raw reports are in commit 6dc81572
(`notes/hunt-mux-git.md`, `notes/hunt-terminal-core.md`).

## MUX-002 - A hung mount holds one abandoned Git worker thread per distinct cwd or checkout, not one per mount, and four of them stop Git status everywhere

The comments in `crates/shepr-git/src/worker.rs`, `limits.rs` and
`config.rs::stamp` now describe this behaviour instead of claiming one thread
per hung mount. The behaviour itself is unchanged; fixing it needs per-access
path tracking in `shepr-git` and a mount or filesystem-device lookup in
`shepr-platform`, so a fixer should own both crates.

What the code does: the stuck-path list is what the step itself names, not
the path that hung. A discovery step records `[target.cwd]`
(`refresh::group_targets`); a checkout step records `[key, cwds...]`
(`refresh::compute_refresh`). `GitStatusWorker::is_stuck` only skips targets
whose cwd or key `starts_with` one of those paths. On a hung NFS mount at
`/net`, workspaces in `/net/a` and `/net/b` are unrelated by prefix: the first
stalls and is abandoned after `GIT_REFRESH_STALL_BOUND`, the replacement thread
walks into `/net/b` and stalls too, and so on. The same happens when the hang
is outside every named path, for instance an NFS `$HOME` whose `~/.gitconfig`
(stamped by `config::git_user_config_paths_at` / `config_deps` with a plain
`metadata`, no deadline) or whose includes every checkout step reads: each
distinct checkout costs one abandoned thread. After four, `abandon_if_stalled`
refuses to abandon and the current thread is "waited out", which on a mount
that stays hung means forever: no workspace on the server gets a Git status
again (`App::start_git_status_refresh_if_due` never leaves `InFlight`).

Which side is wrong: the code, or the doc overstates. A fix that keeps the
claim would record the path actually being touched at each blocking call (or
the mount point / filesystem device of it) rather than the step's nominal
inputs, and treat shared dependency paths (user config, includes, the common
dir) as their own step paths.

## MUX-003 - Repository discovery walks the logical (OSC 7) path and crosses filesystem boundaries, so it can attribute a cwd to a repository Git would not

Claim broken: `discovery.rs` presents the walk as Git's discovery
(`entry_type`: "following symlinks as Git's discovery does"; `GitCeilings`:
"read with Git's semantics"; the runner doc: "Git follows the user's own
configuration and discovery"). AGENTS.md: "Git status in the sidebar (branch,
ahead/behind)" for the workspace's cwd.

What the code does: the target cwd is the workspace's resolved identity cwd
(`Workspace::resolved_identity_cwd` -> `PaneRuntime::cwd` ->
`ReportedCwd::resolve`), which is deliberately the OSC 7 logical path ("OSC 7
carries what /proc cannot: a logical path through symlinks").
`git_worktree_location_below` then pops components of that spelling. Git
resolves the physical cwd (`getcwd`) and walks physical parents. The two
diverge whenever a symlink sits on the path:

- `~/link -> /data/proj/sub` with `/data/proj` a repository: shepr walks
  `~/link`, `~`, `/` and reports OutsideRepository (or `~`'s repository, see
  next); Git reports `/data/proj`.
- `~/link -> /data/work` (no repository there) with a dotfiles repository at
  `~/.git`: shepr reports the dotfiles branch; Git reports none.
- `GitCeilings::contains` compares the popped logical spelling against the
  ceiling's spelled and canonical forms, so a ceiling at the symlink's target
  is never met by a walk that goes through the link.

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

## MUX-007 - Reftable checkouts spawn Git on every refresh

Raised as a lateral observation.

`read_head_identity_from_git` runs `git symbolic-ref` and
`git rev-parse --verify`, and `read_upstream` another `rev-parse`, inside
`fingerprint`, which runs on every refresh of a cached hit. With
`GIT_REMOTE_STATUS_REFRESH_INTERVAL` at 1.5 s, and refresh no longer gated on
a presenting client, that is three Git processes per reftable checkout every
1.5 s for the server's lifetime, where a files-backend checkout costs only a
few stats. A stamp of the reftable directory (`reftable/tables.list`) as a
fingerprint dependency would let the hit path skip the probes.

## MUX-008 - An uncacheable `ConfigCtx` is rebuilt on every refresh

Raised as a lateral observation.

A `ConfigCtx` that is `Dependencies::Uncacheable` (a `~user` include, a refused
config environment variable, a config that changed between the two
`config --list` runs) is rebuilt on every refresh: two `git config --list` runs
and a `for-each-ref` per checkout every 1.5 s, indefinitely.

## MUX-014 - A save before a fresh launch settles can stop the settlement seeding its cwd

Raised as a lateral by the wave reviewer.

`App::handle_pane_launch_settled` (`crates/shepr-server/src/app/pane_launch.rs`)
seeds the stored cwd on `LaunchOutcome::Launched` only when the runtime holds
no conflicting cwd observation, so a shell's own OSC 7 is not overwritten. But
`runtime.remembered_cwd()` also returns the `/proc` observation a save takes.
A save that runs between the fork and the settlement, before the child has
done its chdir, remembers the server's own cwd; that reads as a conflict and a
fallback launch cwd is then not seeded. The window is tiny. Consulting only the
OSC 7 `reported` slot would close it.
