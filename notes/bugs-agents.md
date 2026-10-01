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

## AGNT-011 - The Kimi version probe uses the server's PATH

Lateral. The `KIMI_MIN_VERSION` gate (`integration/mod.rs`) runs `kimi
--version` from `/` with the server's `PATH`. A server started over SSH by a
non-interactive shell often lacks the user's interactive `PATH` additions, so the
probe fails, logs a warning and installs anyway. Matches the doc ("a warning when
the version cannot be determined (install proceeds)"); noted because the warning
appears on every install on such hosts.
