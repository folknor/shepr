# Defects: shepr-agent

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

IDs AGT-001 through AGT-019 were used by an earlier edition of this file and are not reused.

## AGT-020 - Integration installs leave lock files in the agent's config directory

Agent config edits now hold a persistent sidecar flock (`integration/config_file.rs`), so every edited file gains a `<file>.lock` sibling in the user's agent directory (for example next to `~/.claude/settings.json`), and it stays after `shepr integration uninstall`. The lock inode must stay stable while editors may race, so deleting it is not free; options are a lock file under shepr's own runtime or state directory keyed by the target path, or removing the sidecar on uninstall once no other edit can be in flight.
