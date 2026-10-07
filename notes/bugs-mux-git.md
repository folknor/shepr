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

## MUX-003 - Repository discovery crosses filesystem boundaries, and still differs from Git in smaller ways

Residue. Discovery in `crates/shepr-git/src/discovery.rs` now walks physical
parents and validates `objects/` and `refs/` through the common directory, as
Git does. What is left:

- Git stops at a filesystem boundary unless `GIT_DISCOVERY_ACROSS_FILESYSTEM`
  is set; the walk has no boundary check, so a cwd on its own mount under a
  `$HOME` repository shows `$HOME`'s branch where `git status` there says "not
  a git repository (... filesystem boundary)". Honouring the variable needs it
  registered in the central environment registry
  (`crates/shepr-core/src/env.rs`); the mount table
  (`crates/shepr-platform/src/mounts.rs`) can supply the boundary.
- Discovery still differs from Git's `setup.c` validation on `HEAD` contents
  and on search permissions.
- `commondir` parsing trims whitespace that can be part of the path.
- Discovery now returns the physical repository root, not the logical
  spelling; nothing found compares it as a prefix of the logical cwd, but
  anything that displays the root shows the resolved path.
