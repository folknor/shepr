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

## FUP-005 - A stall can be abandoned under the wrong step

Raised as a lateral by the wave 7 reviewer, who judged it not worth fixing
unless seen.

`abandon_if_stalled` (`crates/shepr-git/src/worker.rs`) reads the stalled
step, then marks the phase abandoned and sets `cancelled`. If the thread makes
a step on another mount in that window and blocks there, the abandoned record
names the earlier step, and the replacement is not kept off the mount that
actually hung. `cancelled` closes the window for every later announce, so it
needs a thread silent for the whole stall bound to wake and block within
microseconds; it is bounded by `MAX_ABANDONED_GIT_REFRESH_THREADS`. Either
close it (read the step and mark the phase under one lock) or record at the
code site why it is accepted.
