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

## AGNT-001 - Dev and release servers on one host overwrite each other's hook assets, so hooks of one build report to the other build's server

Claims broken:
- AGENTS.md: "Client and server are always the same build", and the JSON API's
  cross-build surface is only the `ping` identity and `server.stop_if_boot`.
  Hook reports (`pane.report_agent`, `pane.report_agent_session`) are JSON API
  requests outside that surface.
- AGENTS.md, "Running a dev build next to the installed one", presents a dev
  server beside the release one as supported.
- `integration/registry.rs` (`integration_state_for_path`): "Exact bytes make
  dev and release builds share configs safely".

Agent config locations are per host and shared by both profiles
(`integration/env.rs`, `resolve_config_update_lock_dir`: "Agent configs are
shared by dev and release builds"). Every server launch runs
`install_present_integrations`, which compares the installed asset with its own
bundled bytes and rewrites it when they differ. So whichever profile launched
last owns `~/.claude/hooks/shepr-agent-state.sh`, `~/.codex/shepr-agent-state.sh`,
the OpenCode/Kilo plugins, the Pi/OMP extensions and so on. The other server's
panes run that script, and it reports to the pane's `SHEPR_SOCKET_PATH`, the
other server. The exact-bytes rule prevents a version contest but guarantees a
flip-flop on every alternate launch and, in between, a cross-build JSON
conversation the project does not support. A dev change to a report param, a
method name or the seq unit silently breaks hook state and resume ids for every
release pane (and vice versa), and nothing logs it because hooks discard
failures by design. The registration side flip-flops the same way: install now
makes the registration equal to its own descriptor, so a dev build that adds an
event or changes an action and the release build rewrite each other's
registrations on alternate launches.

Direction: make the installed artifacts per profile (profile-suffixed hook file
names and plugin ids, so two registrations coexist and each script reports only
when `SHEPR_BUILD_PROFILE` matches its own profile), or have a non-release build
skip integration install and say so. Either way, status must stop treating
"bytes differ" as "mine to overwrite" for a file another build legitimately owns.

## AGNT-009 - Config locks serialise shepr against shepr only; the agent's own concurrent writes can be lost

`config_file.rs` `lock_config_for_update`: "Serializes Shepr's read-modify-write
of a user config across processes ... to prevent concurrent edits from
overwriting one another." Accurate for shepr processes. But Claude Code,
Copilot, Droid and OpenCode rewrite their own settings at runtime (permission
grants, model choice), and install reads, edits in memory, then renames over the
file with no check that it is unchanged since the read. An agent write in that
window is silently discarded. The window is small and install runs only when
status is not Current, but AGNT-001 makes installs happen on every alternate
launch. Cheap guard: re-read (or compare size+mtime+inode) just before the
rename, and retry once if it changed.

## AGNT-011 - The Kimi version probe uses the server's PATH

Lateral. The `KIMI_MIN_VERSION` gate (`integration/mod.rs`) runs `kimi
--version` from `/` with the server's `PATH`. A server started over SSH by a
non-interactive shell often lacks the user's interactive `PATH` additions, so the
probe fails, logs a warning and installs anyway. Matches the doc ("a warning when
the version cannot be determined (install proceeds)"); noted because the warning
appears on every install on such hosts.

## AGNT-016 - A Kimi hook table outside the marked block is neither stripped nor rejected

Lateral. The JSON targets now strip every entry invoking the installed hook path
from every event before writing the declared set, and status rejects extras. The
Kimi path only rewrites its `# >>> shepr kimi integration` block, and
`kimi_config_block_is_current` looks only inside it, so a `[[hooks]]` table
invoking shepr's hook path outside the block (a copy, or a block whose markers
were lost) survives install and reads Current. Each such event then fires twice.
Smaller: `kimi_hooks_registered` parses the TOML only to surface a parse error
(`let _config = ...`) with no comment saying so.

## AGNT-017 - The hook-path strip skips an event whose value is not an array

Lateral. `remove_hook_path_commands_preserving` (`integration/config_edit.rs`)
silently skips an event whose value is not an array, while the `ensure_*`
helpers reject that shape. Install then fails later with a less specific message;
nothing is corrupted.

## AGNT-018 - Python script detection treats `--check-hash-based-pycs` as valueless

Lateral, low. `script_arg_index` (`detect/mod.rs`) does not list
`--check-hash-based-pycs` as taking a value, so
`python --check-hash-based-pycs always agent.py` takes `always` as the script.
