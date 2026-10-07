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

## FUP-001 - A failed mountinfo read makes Git discovery cross filesystems silently

`access::scoped` (`crates/shepr-git/src/access.rs`) gives an empty
`MountTable` when `/proc/self/mountinfo` cannot be read. The boundary check
then returns `None`, so discovery crosses filesystems without saying so, and
the stall quarantine loses its mount mapping too. Only the non-worker path logs
the read failure.

## FUP-002 - An unreadable HEAD loses its errno in Git discovery

`validate_git_head` (`crates/shepr-git/src/discovery.rs`) maps every error but
`WouldBlock` to "invalid". A gitfile target whose HEAD cannot be read (EACCES)
now reports "gitfile target has no valid HEAD" (`InvalidData`) and loses the
errno it used to carry. A plain `.git` with an unreadable HEAD now ascends to
an enclosing checkout, as Git does; that half is intended.

## FUP-003 - An operator Connect against a hung host can show Starting... for minutes

`SSH_START_ATTEMPT_BUDGET` (`crates/shepr-remote/src/limits.rs`) now covers
executable re-verification and up to three discovery candidates, six SSH
command timeouts plus the bridge phase, so a Connect against a host that hangs
at every step shows Starting... for several minutes before it fails. After a
bridge attempt fails on a stale path, `MachineSshConnector::connect` also
resolves again and runs a second bridge, which neither operator budget counts
(it is deadline-bound, so it fails as a deadline pass rather than overrunning).
Decide whether the operator budgets should be shorter, or the Starting entry
should show progress.
