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
failures by design. The registration side has the same split: a dev build that
adds an event or changes an action registers an entry the release build's install
never removes (AGNT-003).

Direction: make the installed artifacts per profile (profile-suffixed hook file
names and plugin ids, so two registrations coexist and each script reports only
when `SHEPR_BUILD_PROFILE` matches its own profile), or have a non-release build
skip integration install and say so. Either way, status must stop treating
"bytes differ" as "mine to overwrite" for a file another build legitimately owns.

## AGNT-002 - OpenCode status reads "Current" when `cli.json` is absent even though install would create it

Claims broken: `registry.rs` `hook_registration_is_current` doc ("Whether the
agent's own config still registers the installed hook, the way install wrote
it"), and the bootstrap claim in `shepr-server/src/server/headless/bootstrap.rs`
that "A server that stops while the thread runs leaves at most a half-finished
install, which the next launch completes".

`opencode_tui_integration_is_valid` returns valid when `cli.json` does not exist
(`!cli_config_exists || cli_plugin_is_configured(..)`), regardless of whether
the V2 migration is pending. Install (`prepare_cli_plugin`) defers only when
`cli.json` is absent and `cli_migration_pending` (legacy `tui.json` or `kv.json`
present); otherwise it creates `cli.json`, because "OpenCode will never do it for
a fresh V2 install with nothing to migrate". Status and install disagree on the
absent-and-not-pending case, leaving the V2 TUI plugin permanently unregistered
when: the server stops between `tui_config_edit.write()` and
`cli_config_edit.write()` in `install_opencode` on a first install; a V1 user
had `tui.json` (install deferred), then removes it without starting V2; or the
user deletes `cli.json`.

Direction: status valid only when `cli.json` is registered, or absent and
`cli_migration_pending` holds, the same predicate install uses. One shared
function for "what install would do to cli.json" keeps them from drifting.

## AGNT-003 - Install strips only the exact commands of the events it currently declares

Claim broken: `registry.rs` `json_event_has_command` doc: "Install first strips
every entry carrying shepr's command from the event, whatever its matcher or
extra fields, and then writes the canonical one, so anything this rejects a
reinstall repairs." Also AGENTS.md "installs or updates them".

`remove_hook_commands`, `remove_direct_hook_commands`, `remove_flat_command_hook`
and the Claude remover are called per declared event with the exact command
string `sh '<path>' <action>`. An entry whose event is no longer in the
descriptor's list, or whose action argument changed (Devin's `UserPromptSubmit`
going from `session` to `working`, say), is not "shepr's command" by that
definition and survives every reinstall. Status looks only for expected entries
and ignores extras, so it reads Current with the stale entry registered. Kimi is
the exception (it owns a marked block and rewrites it whole); Grok and
Antigravity own a whole file or block. The shell assets' `case "$action"` filter
makes most stale entries inert, but a stale entry with a still-valid action under
an event whose meaning differs reports the wrong state, and AGNT-001 makes "a
build with a different event list" everyday.

Direction: identify shepr-owned entries by the hook path (the quoted
`sh '<path>'` prefix), strip all of them from every event, then write the
declared set, so install makes the registration equal to the descriptor and
status can reject extras.

## AGNT-004 - The Kilo plugin says Kilo's "startup" cannot replace a session; the descriptor makes it a replacement start

Claim broken: comment in `integration/assets/kilo/shepr-agent-state.js`
(`reportSession`): "shepr treats Kilo's 'startup' and 'resume' alike: both can
anchor a session, and neither lets Kilo replace it."

`agent/mod.rs` sets `HookSessionPolicy::KILO.replacement_starts = [Startup]`,
shepr-mux consults it in `session_report_allows_session_replacement`, and the mux
test `recognized_kimi_and_kilo_session_starts_replace_identity_and_release_old_state`
asserts a Kilo `startup` does replace identity. The plugin sends
`session_start_source: "startup"` on every `session.created` and
`session.updated` of any non-child session, and on every `session.status` without
a recognised status, so the pane's resumable session is replaced by whichever
root session last emitted `session.updated`. Unlike the OpenCode server plugin,
the Kilo plugin has no `ownsLocalLifecycle` gate, so under `kilo serve` or an
attached TUI another client's session can take over the pane's identity.

One of the two is wrong. If replacement is intended, the comment must say so and
the plugin needs the OpenCode plugin's "this process owns this pane's lifecycle"
gate. If not, `KILO.replacement_starts` should be empty and the mux test flipped.

## AGNT-005 - Runtime option parsing shares one value-taking list across node, python and the shells

Claim broken: `detect/mod.rs` `script_arg_agent_name` /
`wrapped_agent_name_from_runtime_argv`, whose job is to find the script a
runtime runs. `option_takes_value` is one list for node, bun, python and the
shells, including `-S`, `-L` and `-o`. For Python `-S` takes no value, so
`python3 -S /path/bin/codex` skips the script path and identifies nothing.
Conversely bash's `-O shopt_name` takes a value and is not listed, so
`bash -O extglob /path/claude` reads `extglob` as the script. The list should be
per runtime (node: `-r --require --loader --import --experimental-loader
--inspect-port`; python: `-W -X -m -c`; shells: `-o -O` and `+o +O`). Smaller:
a python short-flag cluster ending in `c` (`python3 -Ic "code"`) is not
recognised as `-c` (only shells get the cluster check), so the code string is
taken as the script path; cosmetic unless the code string is an agent name.

## AGNT-006 - `PI_CONFIG_DIR` is not tilde-expanded, unlike every other agent directory override

`omp_extension_dir` joins the raw `PI_CONFIG_DIR` onto `$HOME`, so
`PI_CONFIG_DIR=~/.omp2` resolves to `$HOME/~/.omp2/agent/extensions` and OMP
reads as absent. Every other `*_DIR`/`*_HOME` override goes through
`config_dir_from_env_or_home` and `expand_tilde_path_with_environment`.

## AGNT-007 - OMP reports `session_start_source: "startup"` on every turn

Not a broken written claim; a behaviour its sibling deliberately avoids. In
`assets/omp/shepr-agent-state.ts`, `reportSession` defaults its argument to
`"startup"`, and the `agent_start` handler calls `reportSession()` every turn.
`HookSessionPolicy::OMP` lists `Startup` as a replacement start, so every turn is
presented as a recognised fresh start that may replace the pane's identity. The
Pi extension it derives from sends no start source on `agent_start`. If a turn
ever runs with a different session ref than the pane holds (a nested or attached
OMP past the `OMPCODE` guard), it replaces the identity instead of being treated
as cross-talk. Match Pi: no start source on per-turn refreshes, `"startup"` only
from `session_start`.

## AGNT-008 - Some hook assets exit without draining stdin

The Claude, Codex, Copilot, Devin, Droid, Grok and MastraCode assets `cat` the
payload into a temp file before any early exit. The Kimi, Cursor and Antigravity
assets exit before reading stdin whenever the pane is not a shepr pane
(`SHEPR_ENV` unset, the common case for an agent run outside shepr) or python3 is
missing; for Kimi on every `PreToolUse`. A hook that exits before the agent
writes its payload makes the write fail with `EPIPE`; Node agents surface that as
an `error` event on the child's stdin, harmless only if each agent handles it.
Draining first costs nothing. Antigravity additionally promises "every exit path
emits an empty object", which holds, but the drain is missing on the same paths.

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

## AGNT-010 - A stale comment above the `SheprSocketPath` export

Lateral, `shepr-mux/src/pane/launch.rs`: "Under a client-socket-only override
that costs a nested shepr client its client socket: the API variable takes
precedence and derives the runtime client socket, which this server does not
listen on." No longer true: `shepr-config/src/address.rs`
`ServerAddress::resolve_paths` pairs a client-socket override with an API path
equal to the runtime `shepr.sock` so the nested client keeps the right client
socket, as AGENTS.md documents.

## AGNT-011 - The Kimi version probe uses the server's PATH

Lateral. The `KIMI_MIN_VERSION` gate (`integration/mod.rs`) runs `kimi
--version` from `/` with the server's `PATH`. A server started over SSH by a
non-interactive shell often lacks the user's interactive `PATH` additions, so the
probe fails, logs a warning and installs anyway. Matches the doc ("a warning when
the version cannot be determined (install proceeds)"); noted because the warning
appears on every install on such hosts.

## AGNT-012 - The Kimi integration depends on Kimi never rewriting `config.toml`

Lateral. `build_kimi_config_with_hooks` refuses a config with a top-level
`hooks = []` or `[hooks]` table (tested, deliberate). If Kimi Code ever writes
such a default itself, the integration fails on every launch with no path
forward but hand-editing. If Kimi rewrites `config.toml` through a TOML
serializer, the `# >>> shepr kimi integration` marker comments are lost, and a
later reinstall appends a second set of `[[hooks]]` beside the unmarked first
(each event then fires twice).

## AGNT-013 - `codex.toml` matches a literal em-dash

Lateral. Rule `screen_working_fallback` matches a literal em-dash in Codex's
"Reconnect failed" text. It matches the agent's real output, so presumably exempt
from the no-gremlins rule; confirm the gremlins check allows it deliberately.

## AGNT-014 - `DetectionInput` doc cites a pre-OSC engine

Lateral, `detect/manifest.rs`: "behavior is identical to the pre-OSC engine in
that case". There is no pre-OSC engine in this fork; say that empty OSC strings
make OSC-region rules see empty text.

## AGNT-015 - The manifest validator accepts a visible flag on a rule of a different state

Lateral. The validator accepts `visible_idle`/`visible_blocker`/`visible_working`
on a rule whose `state` is a different state and silently drops the flag at
evaluation (`rule_detection`). Every bundled manifest pairs them correctly today,
but a mismatch is a manifest bug the validator could reject, as it rejects
`skip_state_update` with a visible flag.
