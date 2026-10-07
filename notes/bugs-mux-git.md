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

## MUX-015 - Chunked copy search can return partial results without marking them incomplete

Raised as a lateral by a wave 2 fixer.

The chunked search in `crates/shepr-mux/src/pane/terminal/backend.rs` can stop
on a resize or a screen switch and return the matches found so far, with
nothing telling the client the result is partial, so the "N of M" counter and
the match list present a short result as complete.

## MUX-016 - The Git filesystem access layer re-scans the mount table per path component

Raised as a lateral by the wave 2 reviewer.

`crates/shepr-git/src/access.rs` walks every accessed path one component at a
time (one `lstat` each) and announces each component; every announcement scans
the whole mount table (`crates/shepr-platform/src/mounts.rs`) twice and
allocates. `git_program()` walks `PATH` through the same machinery before every
Git command. On hosts with hundreds of mounts (snap, docker) that is noticeable
per refresh. Cheap fixes: resolve the Git executable once per refresh scope, and
compute the stall paths once per announcement.

Related sharp edge: when `/proc/self/mountinfo` cannot be read,
`MountTable::default()` makes every stall quarantine `/`, so one abandoned
thread stops Git status on every path until it finishes. Documented as
deliberately conservative.
