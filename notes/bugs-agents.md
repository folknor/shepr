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

## AGNT-009 - Config locks serialise shepr against shepr only; the agent's own concurrent writes can be lost

`config_file.rs` `lock_config_for_update`: "Serializes Shepr's read-modify-write
of a user config across processes ... to prevent concurrent edits from
overwriting one another." Accurate for shepr processes. But Claude Code,
Copilot, Droid and OpenCode rewrite their own settings at runtime (permission
grants, model choice), and install reads, edits in memory, then renames over the
file with no check that it is unchanged since the read. An agent write in that
window is silently discarded. The window is small and install runs only when
status is not Current (a dev build no longer installs, so installs no longer
flip-flop between profiles). Cheap guard: re-read (or compare size+mtime+inode)
just before the rename, and retry once if it changed.

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
