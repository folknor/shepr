# Defect hunt: agents (`crates/shepr-agent/src/`)

Scope covered: detection engine (`detect/manifest.rs`, `detect/mod.rs`),
process-tree probing (`detect/proc_tree.rs`), the bundled manifests, agent
descriptors and resume (`agent/`), and the integrations (`integration/`:
path resolution, presence, status, install, JSON/JSONC/TOML editing, config
locks, atomic replacement, version probe, every hook asset). Reports were
followed across into `shepr-mux/src/terminal/state/` and the server bootstrap
where the contract lives there.

Findings are ordered by how much they matter. Each names the claim it breaks.

---

## 1. Dev and release servers on one host overwrite each other's hook assets, so hooks of one build report to the other build's server

Claims broken:
- AGENTS.md, Principles: "Client and server are always the same build" and
  the JSON API's "cross-build JSON control surface is the `ping` response
  identity ... and the `server.stop_if_boot` request". Hook reports
  (`pane.report_agent`, `pane.report_agent_session`) are JSON API requests and
  are not part of that surface.
- AGENTS.md, "Running a dev build next to the installed one" presents a dev
  server beside the release one as a supported setup.
- `integration/registry.rs` (`integration_state_for_path`): "Exact bytes make
  dev and release builds share configs safely".

What happens: agent config locations are per host and shared by both profiles
(`integration/env.rs`, `resolve_config_update_lock_dir`: "Agent configs are
shared by dev and release builds"). Every server launch runs
`install_present_integrations`, which compares the installed asset with *its
own* bundled bytes and rewrites it when they differ. So whichever profile
launched last owns `~/.claude/hooks/shepr-agent-state.sh`,
`~/.codex/shepr-agent-state.sh`, the OpenCode/Kilo plugins, the Pi/OMP
extensions and so on. The panes of the *other* server run that script, and the
script reports to the pane's `SHEPR_SOCKET_PATH`, which is the other server.
The exact-bytes rule prevents a version-number contest, but it does not make
the sharing safe: it guarantees a flip-flop on every alternate launch, and in
between it guarantees a cross-build JSON conversation that the project
explicitly does not support. A dev change to a report param, a method name or
the seq unit silently breaks hook state and resume ids for every release pane
(and vice versa), and nothing logs it because hooks discard failures by design.

The registration side has the same split: a dev build that adds an event or a
changed action registers an entry that the release build's install never
removes (see finding 3).

Direction: make the installed artifacts per profile (profile-suffixed hook
file names and plugin ids, so two registrations coexist and each script
reports only when `SHEPR_BUILD_PROFILE` matches its own profile), or have a
non-release build skip integration install entirely and say so. Either way the
status check must stop treating "bytes differ" as "mine to overwrite" for a
file that a different build legitimately owns.

---

## 2. OpenCode status reads "Current" when `cli.json` is absent even though install would create it

Claim broken: `registry.rs` `hook_registration_is_current` doc ("Whether the
agent's own config still registers the installed hook, the way install wrote
it"), and the bootstrap claim in
`shepr-server/src/server/headless/bootstrap.rs` that "A server that stops
while the thread runs leaves at most a half-finished install, which the next
launch completes".

`opencode_tui_integration_is_valid` returns valid when `cli.json` does not
exist (`!cli_config_exists || cli_plugin_is_configured(..)`), regardless of
whether the V2 migration is pending. Install (`prepare_cli_plugin`) only
defers when `cli.json` is absent *and* `cli_migration_pending` (legacy
`tui.json` or `kv.json` present); otherwise it creates `cli.json`, because, as
its own comment says, "OpenCode will never do it for a fresh V2 install with
nothing to migrate". Status and install disagree on the
absent-and-not-pending case, which leaves the V2 TUI plugin permanently
unregistered in at least these cases:
- The server stops between `tui_config_edit.write()` and
  `cli_config_edit.write()` in `install_opencode` on a first install: every
  asset is current, `cli.json` does not exist, status says Current, the next
  launch does nothing.
- A V1 user had `tui.json` (migration pending, so install deferred), then
  removes `tui.json` without starting V2: status says Current, V2 later starts
  with nothing to migrate, never creates `cli.json`, and is never registered.
- The user deletes `cli.json`.

Direction: status should be valid only when `cli.json` is registered, or when
it is absent *and* `cli_migration_pending` holds, i.e. the same predicate
install uses. One shared function for "what install would do to cli.json"
would keep them from drifting again.

---

## 3. Install strips only the exact commands of the events it currently declares, so registrations it no longer declares are never removed

Claim broken: `registry.rs` `json_event_has_command` doc: "Install first
strips every entry carrying shepr's command from the event, whatever its
matcher or extra fields, and then writes the canonical one, so anything this
rejects a reinstall repairs." Also AGENTS.md "installs or updates them".

`remove_hook_commands`, `remove_direct_hook_commands`,
`remove_flat_command_hook` and the Claude remover are all called per declared
event with the exact command string `sh '<path>' <action>`. An entry whose
event is no longer in the descriptor's list, or whose action argument changed
(Devin's `UserPromptSubmit` going from `session` to `working`, say), is not
"shepr's command" by that definition and survives every reinstall. The status
check only looks for the expected entries and ignores extras, so it reads
Current with the stale entry still registered. Kimi is the exception (it owns
a marked block and rewrites it whole), and Grok and Antigravity own a whole
file or block.

Today the shell assets' `case "$action"` filter makes most stale entries
inert, but a stale entry with a still-valid action under an event whose
meaning differs reports the wrong state, and finding 1 makes "a build with a
different event list" an everyday occurrence rather than an upgrade edge.

Direction: identify shepr-owned entries by the hook path (the quoted
`sh '<path>'` prefix), strip all of them from every event, then write the
declared set. That turns install into a true "make the registration equal to
the descriptor" operation, and status can then also reject extras.

---

## 4. The Kilo plugin says Kilo's "startup" cannot replace a session; the descriptor makes it a replacement start

Claim broken: comment in `integration/assets/kilo/shepr-agent-state.js`
(`reportSession`): "shepr treats Kilo's 'startup' and 'resume' alike: both can
anchor a session, and neither lets Kilo replace it."

`agent/mod.rs` sets `HookSessionPolicy::KILO.replacement_starts =
[Startup]`, `shepr-mux` consults that in
`session_report_allows_session_replacement`, and the mux test
`recognized_kimi_and_kilo_session_starts_replace_identity_and_release_old_state`
asserts that a Kilo `startup` *does* replace identity and release the old
state. The plugin sends `session_start_source: "startup"` on every
`session.created` and `session.updated` of any non-child session, and on
every `session.status` without a recognised status. So the pane's resumable
session is replaced by whichever root session last emitted `session.updated`.
Unlike the OpenCode server plugin, the Kilo plugin has no `ownsLocalLifecycle`
gate, so under `kilo serve` or an attached TUI the event bus is server-global
and another client's session can take over the pane's identity.

One of the two is wrong. If replacement is intended (switching sessions in the
TUI should move the resume id), the comment must say so and the plugin needs
the same "this process owns this pane's lifecycle" gate the OpenCode plugin
has. If it is not intended, `KILO.replacement_starts` should be empty and the
mux test flipped.

---

## 5. Python `-S` is treated as an option that takes a value, so `python3 -S <agent>` is not identified

Claim broken: `detect/mod.rs` `script_arg_agent_name` /
`wrapped_agent_name_from_runtime_argv`, whose job is to find the script a
runtime runs.

`option_takes_value` is one list shared by node, bun, python and the shells.
It includes `-S`, `-L` and `-o`. For Python `-S` ("don't import site") takes
no value, so `python3 -S /path/bin/codex` skips the script path as `-S`'s
value and identifies nothing. Conversely bash's `-O shopt_name` takes a value
and is not listed, so `bash -O extglob /path/claude` reads `extglob` as the
script. The list should be per runtime (node: `-r --require --loader --import
--experimental-loader --inspect-port`; python: `-W -X -m -c`; shells: `-o -O`
and `+o +O`).

Smaller related point in the same parser: a short-flag cluster ending in `c`
for python (`python3 -Ic "code"`) is not recognised as `-c` (only shells get
the cluster check), so the code string is taken as the script path. It only
mis-identifies when the code string happens to be an agent name, so it is
cosmetic.

---

## 6. `PI_CONFIG_DIR` is not tilde-expanded, unlike every other agent directory override

Claim broken: consistency of `AgentIntegrationPaths` resolution, where every
`*_DIR`/`*_HOME` override goes through `config_dir_from_env_or_home` and its
`expand_tilde_path_with_environment`.

`omp_extension_dir` joins the raw `PI_CONFIG_DIR` onto `$HOME`, so
`PI_CONFIG_DIR=~/.omp2` resolves to `$HOME/~/.omp2/agent/extensions`, a
directory that does not exist, and OMP reads as absent. Low impact (the owner
sets none of these), but it is the one override that behaves differently. Note
also that `env.rs` documents that these overrides are read from the server's
environment, not the agent's, which is the bigger practical trap and is
already documented.

---

## 7. OMP reports `session_start_source: "startup"` on every turn

Not a broken written claim, but a behaviour that its sibling deliberately
avoids. In `assets/omp/shepr-agent-state.ts`, `reportSession` defaults its
argument to `"startup"`, and the `agent_start` handler calls `reportSession()`
on every turn. `HookSessionPolicy::OMP` lists `Startup` as a replacement
start, so every turn is presented to the server as a recognised fresh start
that may replace the pane's identity. The Pi extension, from which this one is
derived, sends no start source on `agent_start` (`reportSession()` with
`undefined`, dropped by `JSON.stringify`). If a turn ever runs with a
different session ref than the pane holds (a nested or attached OMP that
slipped past the `OMPCODE` guard), it replaces the identity instead of being
treated as cross-talk. Match Pi: no start source on per-turn refreshes, and
`"startup"` only from `session_start`.

---

## 8. Some hook assets exit without draining stdin

The Claude, Codex, Copilot, Devin, Droid, Grok and MastraCode assets `cat` the
payload into a temp file before any early exit. The Kimi, Cursor and
Antigravity assets exit before reading stdin whenever the pane is not a shepr
pane (`SHEPR_ENV` unset, which is the common case for an agent run outside
shepr) or python3 is missing. For Kimi this runs on every `PreToolUse`. A
hook that has exited before the agent writes its payload makes the agent's
write fail with `EPIPE`; Node agents surface that as an `error` event on the
child's stdin. Whether it is harmless depends on each agent handling that
event. Draining first (as the others do) costs nothing and removes the
dependency. Antigravity additionally promises "every exit path emits an empty
object", which holds, but the drain is missing on the same paths.

---

## 9. Config locks serialise shepr against shepr only; the agent's own concurrent writes can be lost

`config_file.rs` `lock_config_for_update`: "Serializes Shepr's
read-modify-write of a user config across processes ... to prevent concurrent
edits from overwriting one another." Accurate as written for shepr
processes. But Claude Code, Copilot, Droid and OpenCode rewrite their own
settings files at runtime (permission grants, model choice), and install
reads, edits in memory, then renames over the file with no check that the
file is unchanged since the read. An agent write landing in that window is
silently discarded. The window is small and install only runs when status is
not Current, but finding 1 makes installs happen on every alternate launch.
A cheap guard: re-read (or compare size+mtime+inode) immediately before the
rename, and retry the edit once if it changed.

---

## Lateral findings outside the scope

- `shepr-mux/src/pane/launch.rs`, comment above the `SheprSocketPath` export:
  "Under a client-socket-only override that costs a nested shepr client its
  client socket: the API variable takes precedence and derives the runtime
  client socket, which this server does not listen on." That is no longer
  true: `shepr-config/src/address.rs` `ServerAddress::resolve_paths` pairs a
  client-socket override with an API path equal to the runtime `shepr.sock`,
  exactly so the nested client keeps the right client socket, and AGENTS.md
  documents that. The comment should be reworded to describe the pairing.
- `integration/mod.rs` `KIMI_MIN_VERSION` gate: the version probe runs the
  server's `PATH` (`kimi --version` from `/`). A server started over SSH by a
  non-interactive shell often lacks the user's interactive `PATH` additions,
  so the probe fails, logs a warning and installs anyway. Behaviour matches
  the doc ("a warning when the version cannot be determined (install
  proceeds)"); noting it because the warning will appear on every install on
  such hosts.
- `build_kimi_config_with_hooks` refuses a config containing a top-level
  `hooks = []` or `[hooks]` table (tested, deliberate). If Kimi Code ever
  writes such a default itself, the integration fails on every launch with no
  path forward except hand-editing; and if Kimi rewrites `config.toml`
  through a TOML serializer, the `# >>> shepr kimi integration` marker
  comments are lost, after which a later reinstall appends a second set of
  `[[hooks]]` beside the unmarked first set (each event then fires twice).
  The marker-comment approach depends on Kimi never rewriting the file.
- `codex.toml` rule `screen_working_fallback` matches a literal em-dash in
  Codex's "Reconnect failed" text. It is a matcher for the agent's real
  output, so it is presumably exempt from the no-gremlins rule, but the
  gremlins check should be confirmed to allow it rather than this being an
  accident.
- `detect/manifest.rs` `DetectionInput` doc: "behavior is identical to the
  pre-OSC engine in that case". There is no pre-OSC engine in this fork; the
  sentence can just say empty OSC strings make OSC-region rules see empty
  text.
- The manifest validator accepts `visible_idle`/`visible_blocker`/
  `visible_working` on a rule whose `state` is a different state and silently
  drops the flag at evaluation (`rule_detection`). Every bundled manifest
  currently pairs them correctly (checked), but a mismatch is a manifest bug
  that the validator could reject the same way it rejects `skip_state_update`
  with a visible flag.

---

## Checked and found sound

- Manifest engine: priority-order detection agrees with `explain`'s
  first-wins tie break (stable sort); gate depth/count/matcher limits match
  their docs; region interning and lazy extraction prepare every region a
  rule's gate tree reads; `not` gate validation; counted-region parsing;
  `prompt_box_bounds` ordering; CRLF handling in `after_last_horizontal_rule`
  and the pointer-based line offsets; the KMP `contains` matcher with
  `str::to_lowercase` semantics including final sigma.
- `unknown_is_stable` and its consumer in `agent_detection.rs`: the screen
  output is a pure function of (screen, title, progress), so skipping a scan
  on unchanged content is safe for Idle and for manifests that can yield
  Unknown.
- Process probing: per-root budgets, round-robin frontiers and the
  truncated-token rule in `read_bounded_pid_list`; `D`/`Z` states never read
  `cmdline`; leader-first identification; relative argv paths resolved via
  `/proc/<pid>/cwd`; `/proc/<pid>/comm` truncation does not break any alias
  (muse's versioned binary still matches after truncation).
- Descriptor table: discriminant/index consistency is enforced at compile
  time; session ids are rejected when they could read as flags, both at
  construction and deserialisation; resume argv is data, never shell text.
- Install ordering: every config-editing target parses and edits in memory
  before writing the hook, so an unparseable config leaves no orphan hook;
  locks are taken in a consistent order (Codex hooks then config, OpenCode tui
  then cli), so two shepr processes cannot deadlock; managed assets are
  replaced by rename so a running `sh` keeps its old inode; symlinked configs
  are edited at their target and hard-linked configs refused.
- Claude settings editor: byte-exact no-op for a canonical entry, duplicate
  keys refused, compact/pretty layout preserved, result re-verified against
  the in-memory edit before it is returned.
- Every report-sending asset uses one source string per process family and
  one seq unit per source (nanoseconds for shell/python, microseconds-plus-one
  for JS), matching the mux's per-source ordering, and the OpenCode TUI's
  unsequenced selection report is handled as such by the mux.
- `SHEPR_ENV`, `SHEPR_PANE_ID` and `SHEPR_SOCKET_PATH`, which every asset
  gates on, are always exported by the pane launch; `CODEX_THREAD_ID` and
  `OMPCODE`, which the Codex and OMP assets use to detect nesting, are
  scrubbed from pane environments so only a genuinely nested agent carries
  them.
