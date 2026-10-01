# Defects: agents

Filed from the defect hunt over `crates/shepr-agent/src/`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## AGNT-011 - Delete the Kimi version gate

The Kimi install runs `kimi --version` with the server's `PATH` and refuses a
version below `KIMI_MIN_VERSION`; when the probe cannot find or parse the
binary it warns and installs anyway. A server started over SSH by a
non-interactive shell usually lacks the `PATH` entry where `kimi` lives, so
every launch on such a host logs that warning for nothing. Kimi is the only
agent with a version requirement, so `integration/version.rs` exists for it
alone.

Owner's decision: delete the gate. Remove `integration/version.rs`,
`KIMI_MIN_VERSION`, the version-probe limits it uses, the call site in the
install path, the "requires kimi code ... or newer" install notice, and their
tests. The owner installs and updates Kimi on each host by hand, so an
outdated Kimi is theirs to notice. Update any doc or comment that describes
the version check.
