# Bugs: follow-ups from the resolution waves

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Laterals raised while resolving the defect hunt filed in commit 21dfcea4.

## FUP-004 - A stall recorded while mountinfo was unreadable stops being quarantined once it is readable again

Raised as a lateral by the wave 6 reviewer.

With `/proc/self/mountinfo` unreadable, Git access falls back to an empty mount
table (`crates/shepr-git/src/access.rs`), and a stall is recorded as stuck on
`/`. Once mountinfo is readable again, `/` names only the root mount's device,
so a hung non-root mount stops being quarantined and a replacement refresh
thread can block on it again. Bounded by `MAX_ABANDONED_GIT_REFRESH_THREADS`.
A fix would record an "unknown mount" stall that stays global until its thread
finishes, rather than `/`.
