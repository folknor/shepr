# Hunt: agent integrations

Scope: everything under `crates/shepr-integration/` (Rust sources, templates,
decoders, generated assets, bun tests, contract traces) and the launch call
site `spawn_integration_install` in
`crates/shepr-server/src/server/headless/bootstrap.rs`. Followed out to
`shepr-agent` (descriptor table, session policy), `shepr-core::env`,
`shepr-platform::publish_file`, `shepr-test-support::hook_capture`,
`shepr-detect` limits, the server's `agent_integration_contract_tests.rs`,
`scripts/check_agent_asset_tests.py`, `brokkr.toml` and `clippy.toml`.

Findings are grouped by the nine questions. Items under 2-9 carry an
"Enforce:" line saying whether the fixed form can be held mechanically.

---

## 1. Defects

### D1. Every JSON agent config except Claude's is fully re-serialized: keys re-sorted, re-indented, numbers and escapes normalized

`targets::prepare_json` (Codex `hooks.json`, Copilot `settings.json`, Devin
`config.json`, Droid `settings.json`, Cursor `hooks.json`, MastraCode
`hooks.json`) and the Antigravity branch of `targets::install` parse the
user's file with `serde_json::from_str` into a `Value` and write it back with
`serde_json::to_string_pretty`. The workspace builds `serde_json` without
`preserve_order` (`Cargo.lock` lists no `indexmap` dependency for
`serde_json`), so `serde_json::Map` is a `BTreeMap`. Whenever a hook needs
installing or repairing (first install, any asset change, any user edit to
shepr's entries), the user's whole file comes back:

- with every object's keys in alphabetical order, at every depth
  (permissions, model settings, MCP server blocks, everything),
- re-indented to two spaces,
- with numbers re-spelled (`1e+02` becomes `100.0`), `\uXXXX` and `\/`
  escapes rewritten,
- with duplicate keys silently collapsed to the last one.

For Droid this is `~/.factory/settings.json` and for Copilot and Devin the
agent's main settings file, not a shepr-owned side file. The claim it breaks
is written in `config_edit.rs` above `ensure_flat_command_hook`: "Keep the
helpers separate so install preserves unrelated hooks in each agent's native
format instead of normalizing user configuration." The Claude path
(`claude_settings.rs`) shows the project already knows how to do this right:
a CST edit of only the touched containers, a duplicate-key refusal, and
`verify_updated`, which re-parses the edited text and refuses unless it equals
the intended value. None of the other JSON targets has any of that, and no
test checks byte preservation for them (`install_copilot_writes_hook_and_updates_settings`,
`install_droid_writes_hook_to_settings` etc. compare parsed values only).

Fix: one JSON edit engine for every JSON target, the CST engine Claude uses
(generalized from `claude_settings.rs`: remove-by-hook-path, append, verify),
with `prepare_json` and the Antigravity branch deleted. Enabling
`serde_json/preserve_order` is only a half fix (it keeps key order but still
re-indents and re-spells numbers). A byte-preservation test per JSON target
(the shape of `install_preserves_untouched_formatting_and_complete_trailing_suffix`)
would hold it.

### D2. Codex: every launch forces `features.hooks = true` over an explicit `false`, and deletes `features.codex_hooks`

`config_edit::build_codex_config_with_hooks` unconditionally sets
`features.hooks = true` and removes `codex_hooks`, and
`registry::codex_hooks_feature_enabled` reads anything but `true` as
Outdated, so a user who sets `hooks = false` gets it flipped back at every
release server launch. `features.hooks` is Codex's global hook switch, not
shepr's: turning it on also enables every hook the user registered in
`hooks.json` and deliberately switched off with that flag. The test
`codex_features_hooks_follow_the_users_features_shape` pins the override
(`"features.hooks = false\n"` must come out `true`). The AGENTS.md contract is
that integrations are "hooks installed into each agent's own config"; turning
on the agent's whole hook system and deleting a user key is wider than that.
Nothing is logged beyond "ensured codex config at ...". At minimum a `false`
should be a refusal that is logged (as Kimi does for a hook outside its block),
not something to overwrite every launch.

### D3. A corrupted Grok `hooks/shepr.json` is never repaired, although it is shepr's own file

`targets::install` treats `hooks/shepr.json` as "wholly Shepr-owned: its old
contents need not be valid JSON (only UTF-8)". But the launch path
(`actions::install_if_present`) runs `integration_status` first, and
`registry::grok_hook_config_is_valid` returns a `ConfigUnparseable` error on
invalid JSON; `install_if_present` returns that error and never calls
`install_target`. So a truncated or hand-mangled `shepr.json` is logged as a
warning at every launch and stays broken forever, and the Grok session hook
never runs. The test `grok_status_distinguishes_missing_malformed_and_drifted_hook_config`
hides this: after asserting the status error it repairs by calling
`install_grok` directly, a path production never takes after a status error.
Fix: for shepr-owned registration files, an unparseable file is Outdated, not
an error (or: a status error on a shepr-owned file proceeds to install).

### D4. Every shell-hook integration is silently inert on a host without `python3`

All ten shell hooks end with `command -v python3 >/dev/null 2>&1 || finish`.
On a host (or a pane `PATH`) without `python3`, Claude, Codex, Copilot,
Cursor, Devin, Droid, Grok, Kimi, MastraCode and Antigravity never report, the
installer reports success, status reads Current, and nothing anywhere says so.
AGENTS.md says integrations are "hooks installed into each agent's own config
that report state and session IDs back to shepr"; on such a host they do not,
and session resume silently stops working for every one of those agents. The
installer could at least log a warning when it installs a python-dependent
hook and `python3` does not resolve on the server's `PATH` (imperfect, since
the pane `PATH` can differ, but it turns a silent failure into a logged one).

### D5. The seq note shipped in every JavaScript asset contradicts the server's actual rule

`templates/seq_units.txt`, generated into the OpenCode, OpenCode TUI, Kilo,
Pi and OMP assets, says: "after a backwards clock step, shepr accepts any seq
from a source that has been silent for a few seconds." The server's rule
(`shepr-detect/src/limits.rs`, `HOOK_SEQUENCE_REANCHOR_AFTER`) says the
opposite about silence: "silence is not evidence of anything"; a report
re-anchors only when the wall clock reads earlier than at the last acceptance
or has fallen 5 s behind the monotonic clock. The asset prose is false today.

### D6. Antigravity hook: "every exit path emits an empty object" is false on the signal path

`bundle.rs` `EMPTY_OBJECT` makes `finish()` print `{}` "so every exit path
emits an empty object", but the shared template's `trap 'exit 0' HUP INT TERM`
exits without calling `finish`, so a signalled Antigravity hook prints
nothing. Fix: `trap 'finish' HUP INT TERM`.

### D7. Kimi: removing shepr's block rewrites the user's line endings and trailing blank lines

`config_edit::remove_kimi_config_block` splits with `str::lines()` (which
drops the `\r` of `\r\n`) and rejoins with `\n`, then strips every trailing
blank line (`while result.ends_with("\n\n")`). A CRLF `config.toml` therefore
becomes LF on the first update, and the user's trailing blank lines go.
`kimi_config_block_with_timeout_is_current` compares the CRLF block bytes
with an LF expectation, so a CRLF file always reads Outdated until that
conversion has happened. Small, but it is user content changed for no reason.

### D8. Relative agent config-dir overrides resolve against the server's working directory, except OMP's

`env::config_dir_from_env_or_home` returns a relative `CLAUDE_CONFIG_DIR`,
`CODEX_HOME`, `COPILOT_HOME`, `CURSOR_CONFIG_DIR`, `KIMI_CODE_HOME`,
`GROK_HOME`, `PI_CODING_AGENT_DIR` or `ANTIGRAVITY_CLI_CONFIG_DIR` unchanged
(these are `EnvKind::Path`, which accepts relative values), so every later
`fs` call resolves it against the server process's cwd. `omp_extension_dir`
instead joins a relative `PI_CONFIG_DIR` onto `HOME` explicitly. The agent
itself resolves the same value against its own cwd (the pane's). The doc on
`AgentIntegrationPaths` claims install and status "never consult the process
environment while choosing files"; the cwd is process environment. Fix: one
rule for every override (refuse relative, or join `HOME` as OMP does), applied
in `config_dir_from_env_or_home`.

### D9. Stale claims that state facts which are false today

- `shepr-agent/src/lib.rs`, `IntegrationHookAction`: "The action word an
  installed hook passes to the shepr report command". There is no report
  command; hooks write to the socket (`lib.rs` of this crate says "no
  reporter command belongs in the CLI").
- `shepr-core/src/env.rs`, `SHEPR_ASSET_INTERNAL_NAMES`: "Header markers
  install and status code parse out of an asset's text". Only
  `SHEPR_INTEGRATION_VERSION=` is parsed (`registry::parse_integration_version`);
  `SHEPR_INTEGRATION_ID` is read by nothing.
- `registration.rs`, `JsonShape::expected_events`: "Copilot, Devin and Droid
  call their payload-decoding hooks for every event." Devin's and Droid's
  descriptor events all carry `Some(IntegrationHookAction::Session)`; only
  Copilot has an action-less event (see 9).
- `config_edit.rs`: the comment "Copilot uses the flatter settings shape
  `{ type, matcher, bash }`" sits above `ensure_flat_command_hook`, which is
  MastraCode's (it hard-codes `MASTRACODE_HOOK_DESCRIPTION`); Copilot goes
  through `ensure_direct_command_hook`.
- `registration.rs`: "Install merges these entries and status matches them;
  neither reconstructs a second interpretation of the descriptor." Claude's
  install is a second, CST implementation (`claude_settings::rewrite`), and
  Cursor's install inserts `"version": 1` that status never checks.

---

## 2. One value, one owner

- **The release profile marker `"release"`.** `bundle.rs` restates it as
  `RELEASE_PROFILE` with "Spelled in `shepr-paths`, which this crate cannot
  depend on." That is not forced: the generator is `#[cfg(test)]`, and a
  dev-dependency on `shepr-paths` creates no cycle (`shepr-paths` depends only
  on core and platform). Further copies: `tests.rs` (`.env("SHEPR_BUILD_PROFILE", "release")`
  four times, `"dev"` once), `shepr-test-support/src/hook_capture.rs`
  (`.env("SHEPR_ENV", "1")`, `"SHEPR_SOCKET_PATH"`, `"SHEPR_PANE_ID"` as
  literals rather than `EnvVar::*.name()` and `SHEPR_ENV_IN_PANE`), and
  `shepr-server/src/agent_integration_contract_tests.rs`. Not diverged.
  Enforce: dev-dependency and `BuildProfile::Release`'s spelling in the
  generator; a textlint over `crates/**/*.rs` refusing `"SHEPR_[A-Z_]+"`
  string literals outside `shepr-core/src/env.rs` (asset-internal names
  excepted) would catch the test copies.
- **The API method names** `pane.report_agent_session` / `pane.report_agent`
  are restated in `bundle.rs` from `shepr-api`. Again a dev-dependency on
  `shepr-api` is cycle-free. What keeps them in step today is real: the
  server's contract test replays every captured request through the real
  handlers. Enforce: read them from `shepr-api` in the generator.
- **The 500 ms socket wait** lives in `limits::HOOK_SOCKET_WAIT` and is
  restated in prose in `lib.rs` ("waits at most 500 ms") and the `bundle.rs`
  module doc ("a 500 ms wait"), and as literals in the bundle test itself
  (`"SOCKET_WAIT_SECONDS = 0.5\n"`, `"SOCKET_WAIT_MS = 500;\n"`), so a change to
  the limit fails the test rather than being checked by it. Enforce: build the
  expected strings from `SOCKET_WAIT`; reword the prose to "the hook socket
  wait in `limits`".
- **The OpenCode TUI decoder's own timings**, not part of the generated
  envelope: the 500 ms retry delay appears as a bare literal seven times in
  `decoders/opencode_tui.js` (`Date.now() + 500` x6, `setTimeout(.., 500)`),
  plus `AbortSignal.timeout(5_000)`, `ROUTE_POLL_INTERVAL_MS = 100`,
  `SELECTION_RETRY_DELAYS_MS = [100, 400, 1_000]`. The bun tests hard-code
  their consequences (`setTimeout(resolve, 650)`, `1_600`, `700`). Enforce:
  name them once and generate them into `tui_kit.js` from `limits`, then
  extend `hook_assets_share_one_envelope` to forbid numeric delays in decoders.
- **Session start source spellings** (`"startup"`, `"select"`, `"resume"`)
  are owned by `shepr_agent::resume::AgentSessionStartSource` but spelled by
  hand in `decoders/opencode.js` (`LOCAL_START_SOURCE`), `decoders/kilo.js`
  (`SESSION_START_SOURCE`), `decoders/kimi.py` and `decoders/mastracode.py`
  (`or "startup"`), `decoders/omp.ts` (`"startup"`, `"resume"`) and
  `templates/tui_kit.js` (`SELECTION_START_SOURCE`). A misspelling would not
  fail anything: the server parses it as `Unrecognized`, which never
  replaces a session (silent degradation of resume). Not diverged today.
  Enforce: generate a `START` table into every preamble from the enum, as
  `STATES_JS` already is, and forbid the literals in decoders in
  `hook_assets_share_one_envelope`.
- **The Antigravity target has four names**: label `agy` (source `shepr:agy`,
  `mktemp` name `shepr-agy-hook`), `registry::action_label` `antigravity-cli`,
  serde id `antigravity_cli` (asset header, `bundle::integration_id`), and the
  directory `antigravity_cli/`. `install_present_integrations` logs
  `integration = "agy"` with a message saying "antigravity-cli", so one log
  line names the target two ways. Enforce: drop `action_label`; one name per
  target, from the descriptor.
- **The integration lock root** `<XDG_STATE_HOME>/shepr/integration-locks` is
  assembled in `env::resolve_config_update_lock_dir` from
  `SHARED_APP_DIR_NAME` plus a local literal, outside `shepr-paths`, which
  AGENTS.md names as the owner of the XDG layout. The integration layer rule
  does not allow `shepr-paths`, but nothing prevents adding it. Enforce:
  a `shepr-paths` accessor and a dependency-rule change.
- **The asset list is written three times**: `bundle.rs` `SPECS` (asset path,
  decoder, version), `lib.rs` (`include_str!` per asset, install name
  constants), and the server's `SHELL_ASSETS` / `BUN_ASSETS` /
  `bun_trace_name` (plus trace names in `contract_traces.toml`). What keeps
  them in step: `every_asset_with_a_decoder_is_generated` and the server's
  `assert_asset_coverage`. The `include_str!` copy is forced (it needs a
  literal); a single `macro_rules!` table could emit both the `SPECS` rows and
  the constants.
- **`assets/opencode/tui.js` is hand-written**, outside the generator, with
  its own `SHEPR_INTEGRATION_ID=opencode-tui-v2` and a
  `SHEPR_INTEGRATION_VERSION=3` that restates the TUI spec's version by hand,
  and none of the "managed by shepr" header lines every generated asset has.
  Enforce: generate it from a spec row too.
- **Hand-bumped `version` numbers in `SPECS`**: nothing checks that a changed
  asset bumped its number (currentness is exact bytes), so the number in a log
  line is unverifiable. Enforce: derive it (a content hash), or delete it
  (see 9).
- **Ten constants with the same value**: `CLAUDE_HOOK_INSTALL_NAME`,
  `CODEX_HOOK_INSTALL_NAME`, ... are all `"shepr-agent-state.sh"`.
- **The `ownsLocalLifecycle` argument scanner** is written twice
  (`decoders/opencode.js`, `decoders/kilo.js`): the `--` split, the
  `--attach` test and the `--print-logs`/`--log-level` stripping are
  identical; only the final verdict differs. The shared part belongs in
  `templates/opencode_family.js`, which both already include.
- **`HOOK_TIMEOUT` (10 s) as test literals**: `tests.rs`
  `assert_kimi_hook` (`== Some(10)`), `codex_needs_the_hooks_entry_and_the_feature_flag`
  (`"timeout": 10`), the Kimi stale-registration fixtures (`timeout = 10`),
  and `claude_settings` `install_is_a_byte_exact_noop_for_a_canonical_hook`
  (`"timeout":10`). Enforce: format from `HOOK_TIMEOUT`.

## 3. Values nobody can find, change, or trust

- **`SHEPR_OMP_IDLE_DEBOUNCE_MS` and `SHEPR_OMP_RETRY_GRACE_MS`**
  (`decoders/omp.ts`): environment knobs read inside the agent process,
  documented nowhere ("a setting that is not documented is not supported"),
  with a resolution rule of their own that contradicts `shepr_core::env`'s
  (an invalid or negative value silently falls back, where the core policy
  refuses naming the variable). `SHEPR_OMP_RETRY_GRACE_MS` is set by nothing,
  not even a test; `SHEPR_OMP_IDLE_DEBOUNCE_MS` is set only by the bun tests,
  so it is a test seam shipped in production. They are listed in
  `SHEPR_ASSET_INTERNAL_NAMES` as "tunables only the omp extension reads",
  which records them without answering who tunes them. Fix: constants
  (generated from `limits`), with the test given a seam that is not the
  production environment (the extension's install function could take
  options). Enforce: drop them from `SHEPR_ASSET_INTERNAL_NAMES`, whose test
  then fails on any asset that still spells them.
- **There is no answer to "what are the integration tunables"**:
  `limits.rs` holds three values plus one `#[cfg(test)]` one; the rest live
  as literals in JS/TS decoders (the TUI timings above, OMP's 250 ms and
  2500 ms defaults, the extension's one-retry policy) and in the agent
  descriptor table. Enforce: generate every decoder timing from `limits`.
- **`TOML_BASIC_STRING_DELIMITER_BYTES = 2`** is the two quote characters,
  a format fact, posing as a tunable in `limits.rs` (it is a `with_capacity`
  hint). It should be `limits-exempt: TOML basic string quotes` beside the
  code, or just `value.len() + 2`.
- **No clock seam in any plugin**: `plugin_kit.js`, `tui_kit.js`,
  `extension_kit.ts` and the TUI decoder call `Date.now()` and `setTimeout`
  directly, so their bun tests can only sleep (650 ms, 1.6 s, 2.5 s deadlines)
  and wait out the real delays. Enforce: a clock/timer object passed into the
  kit; the Rust crates already have a textlint for exactly this
  (`agent-clock-is-injected`), which does not reach `.js`/`.ts`.

## 4. One channel, one implementation

- **Install messages are free text with the path baked in.**
  `ArtifactRole::install_message` builds `"installed claude integration hook to
  /home/.../shepr-agent-state.sh"` and `install_present_integrations` logs it
  as `tracing::info!(integration = label, "{message}")`. The path, role and
  verb are not fields, so no log query can select "which files did shepr
  rewrite". Fix: log `role`, `path`, `integration` as fields with a fixed
  message.
- **One failure, two lines at two levels.** A status or install failure is
  logged by `logging::integration_action` at `info` with
  `outcome = Failed` and no error text, and again by
  `install_present_integrations` at `warn` with the error. The `info` line
  is noise at best and misleading at worst (a failure at info level).
- **A status line per present agent per launch** ("integration action
  finished", `action = "status"`, `info`), even when nothing is done.
- **Nothing is logged where something significant happens**: when the Codex
  feature flag is flipped (D2) or `codex_hooks` is deleted, when a shell hook
  will be inert for lack of `python3` (D4), when a JSON target's whole file is
  re-serialized (D1).
- **Operator prose for a command that no longer exists**:
  `targets::missing_agent_directory` ("claude directory not found at ...
  install claude code first", a table of per-agent names) reads like CLI
  install guidance. The launch path checks presence first, so this text is
  reachable only if the directory vanishes between the two checks (see 9).
- **The OpenCode V2 notice** "start opencode2 once and the next shepr server
  launch registers it" names a binary spelling inline in `targets.rs`.

## 5. Errors

- **Error kind travels by downcasting payloads out of `io::Error`.**
  `InstallIssue` (kind + message), `file_ops::NotRegularFile` and
  `config_file::ConfigChanged` are three payload types; `InstallError::from`
  downcasts all three. "Not a regular file" has two encodings
  (`InstallIssue` with `InstallErrorKind::NotRegularFile` from
  `resolve_target`, and the `NotRegularFile` struct from
  `read_config_bytes`), and so does "config changed" (`ConfigChanged` struct,
  and an `InstallErrorKind::ConfigChanged` arm in `InstallIssue::io_error`
  that nothing constructs). Fix: a typed error enum through the crate,
  converted to text once at the log boundary. The test
  `install_failures_keep_their_category_at_the_log_boundary` restates the
  `io::ErrorKind` mapping table rather than testing a behaviour.
- **A status error stops a repair the install could have made** (D3).
- **`LockWait::UntilFree`** in `config_file::lock_config_for_update` waits
  forever on a held lock, with nothing logged; a stuck holder silently stalls
  every later target in the detached install thread.
- **`PluginConfigEdit::write`** can fail with "OpenCode config edit is
  missing its update lock", a state the type allows (contents `Some`, lock
  `None`). Fix: `Option<(String, ConfigUpdateLock)>`.
- **Config and asset reads are unbounded** (`read_config_bytes`, and
  `registry::integration_state_for_path` / `file_matches_asset` use plain
  `fs::read` after an `is_file` check, a second read policy beside
  `read_config_bytes`'s pinned regular-file open).

## 6. Tests that prove nothing

- **`shell_hooks_reject_dev_panes_after_draining_input` cannot fail for
  Claude, Devin or Antigravity.** It feeds every shell hook the payload
  `{"session_id":"dev-session"}` with `SHEPR_BUILD_PROFILE=dev` and asserts
  no request. Claude's decoder exits on a missing `hook_event_name`, Devin's
  on `first_text("hook_event_name") not in EVENTS`, Antigravity's on a
  missing `conversationId`: those three send nothing under `release` either.
  The part of the name that says "after draining input" is not checked at all:
  `capture_hook` accepts a `BrokenPipe` on stdin. Fix: give each hook its
  known-valid payload (the table in `session_hooks_ignore_non_object_payloads_quietly`)
  and assert it reports under `release` and not under `dev`.
- **`bundled_integration_assets_report_the_descriptor_identity`** asserts
  `asset.contents.contains(agent.label())`; for labels `pi`, `omp`, `kilo`,
  `agy`, `grok` that substring is present in almost any file (`pi` is in
  `api`, `pipe`, `spawn`...). Fix: match the generated `AGENT = "<label>"`
  line.
- **`process_owned_integration_assets_do_not_report_release`** guards
  against `pane.release_agent`, a method that no longer exists anywhere in
  the repository, so it cannot fail. Same for the bun helper
  `requestHasMessage` checking for a `message` param.
- **`contract_traces.ts` `normalize` rewrites each request's `id`** to
  `<source>:<rank>` before comparing, so the id the plugin actually sent is
  never checked against the trace (the envelope's `<source>:<seq>` rule is
  only regex-checked in `bundle.rs`).
- **The bun tests use the host temp directory**: `shepr-agent-state.test.ts`
  binds sockets at `join(tmpdir(), ...)` and `shepr-tui-session.test.ts`
  uses `mkdtemp(join(tmpdir(), ...))`, i.e. `/tmp`. The repository rule
  ("never `/tmp`", `no-host-temp-dir`) is enforced only on `.rs` files, so it
  fails open by file extension.
- **Wall-clock timing in the bun tests**: negative assertions after
  `Bun.sleep(25)` ("nothing was sent") pass if the send is merely slow;
  positive ones wait out real 500 ms retry delays. Depends on the unnamed
  literals of 2 and the missing clock seam of 3.
- **Environment leaks between bun files**: `opencode/shepr-agent-state.test.ts`
  and `shepr-tui-session.test.ts` set `SHEPR_*` in `beforeEach` and never
  restore them, and three files `mock.module("node:net", ...)`, which bun
  keeps for the process; `shepr-agent-state.test.ts` needs the real
  `createServer`. Order-dependent.
- **`bundle.rs` `shell_hook_gates_follow_the_descriptor_events`** picks specs
  by index (`SPECS[2]`, `SPECS[5]`, `SPECS[3]`); reordering the table
  silently retargets the assertions.
- **`every_target_reads_current_right_after_install`** proves install implies
  Current, never that Current implies a no-op install (Cursor's `version` is
  the counterexample, 1/D9).
- **The `missing_agent_directory` message tests** (`install_*_errors_when_config_dir_missing`,
  13 of them) call `install_X` directly, bypassing the presence check the
  launch path makes, so they keep a race-only path looking load-bearing.
- **The python hook tests require host `python3`** (`require_python3`) and
  the plugin tests require host `bun`. Both are sanctioned
  (`host-program-ok`, `check_agent_asset_tests.py`) and both runtimes are the
  subject; listed for completeness.

## 7. Guards and claims that have stopped holding

- **The `#[ignore]` on `regenerate_bundled_assets` does not keep it out of
  test runs.** Its comment says "Ignored so the gate never writes into the
  tree". That holds for `brokkr check`, but `brokkr test` always passes
  `--include-ignored` (AGENTS.md and `brokkr man check brokkr-test`), so any
  `brokkr test -p shepr-integration <filter>` whose substring matches
  `bundle::regenerate_bundled_assets` (`bundle`, `bundled`, `assets`,
  `regenerate`) rewrites every committed asset from the templates, silently,
  as a side effect of a test run. A hand edit to an asset under
  investigation is reverted. Fix: make regeneration a script, or gate the
  writer on an explicit environment variable read through `shepr_core::env`.
  Checkable: yes.
- **`hook_assets_share_one_envelope` forbids transport by name**: the
  needles are `createConnection`, `settimeout` (lowercase, Python's), `AF_UNIX`,
  `Math.random`, `import random`. A decoder using `net.connect`, Node's
  `setTimeout`, or `socket.create_connection` passes. Checkable, and fails
  open on a new spelling.
- **Claims nothing enforces, false today**: the seq note (D5), the
  `IntegrationHookAction` doc, the `SHEPR_ASSET_INTERNAL_NAMES` comment, the
  Copilot/Devin/Droid comment, the misplaced Copilot shape comment, the
  "neither reconstructs a second interpretation" claim (all in D9), the
  Antigravity "every exit path" comment (D6), and `AgentIntegrationPaths`'s
  "never consults the process environment" (D8).
- **Claims nothing enforces, true today**: `opencode.js` "it never runs
  alongside this server plugin" (rests on `ownsLocalLifecycle` and OpenCode's
  launch shapes); `bootstrap.rs` "leaves at most a half-finished install ...
  every file is replaced by rename" (true, but see lateral notes: a staging
  file can be left behind).
- **Prose restating code-owned values**: `lib.rs` and the `bundle.rs` module
  doc each describe the whole envelope including the 500 ms number; two
  copies of the same paragraph that the test does not read.
- **The `pi.events.on("shepr:blocked")` contract** in `decoders/pi.ts` and
  `decoders/omp.ts` is an inbound event nothing in shepr emits and no
  document describes (its payload's `label` field is a remnant: "changed
  local prompt labels add no new information"). Either an undocumented
  feature or dead (see 9).

## 8. Policy invented per call site

- **Per-target behaviour is scattered over seven modules instead of carried
  by `IntegrationSpec`.** `targets::install` picks the artifact role by a
  `match target` (`Claude | Copilot | Devin => Settings`, `Cursor =>
  UpdatedHooks`), inserts Cursor's `version` by `target == Target::Cursor`,
  hard-codes "mastracode hooks file" for any `HooksRoot::Document` target,
  and checks OMP against Pi; `missing_agent_directory` has its own name table;
  `registration::expected_events` has `matches!(target, Copilot | Devin |
  Droid)`; `registry::action_label` special-cases Antigravity;
  `registry::agent_directory` special-cases Pi/OMP; `JsonShape::NestedClaude`
  applies the SessionStart matcher to every Claude event (latent: correct only
  while Claude registers one event). `DirectoryKey` is a second enum
  restating `IntegrationTarget`. Fix: the spec row carries role, document
  root description, extra required keys, presence directory, matcher
  source, and the "decodes every event" flag; the per-site matches go.
  Enforce: once the rows carry it, the exhaustive `spec_for` match is the
  check.
- **Two implementations of hook removal for Claude**:
  `config_edit::remove_hook_path_commands_preserving` over `serde_json::Value`
  and `claude_settings::remove_hook_path_commands` over the CST. They are kept
  in step only at runtime, by `verify_updated` refusing a mismatch (which
  protects the user's file, but turns a divergence into a permanent install
  failure instead of a test failure). D1's single CST engine removes the
  second copy.
- **Three delivery retry policies**: `plugin_kit.js` one attempt,
  `extension_kit.ts` one retry in the same queue slot, `tui_kit.js` /
  the TUI decoder retry every 500 ms indefinitely while the selection is
  current. `bundle.rs` documents the split, but the policies themselves are
  hand-written per kit.
- **Ambient dependencies**: the cwd for relative overrides (D8); the agent
  process environment for OMP tunables (3); `Date.now()` everywhere in the
  plugins (3).
- **Unbounded growth in long-running agent processes** (the owner runs
  agents for days):
  - `templates/opencode_family.js` `childSessions` gains an entry per
    subagent session and never drops one (`session.deleted` is a no-op).
  - `decoders/opencode_tui.js` `tui()`: `ctx.events` accumulates every event
    while `!ctx.hydrated`. Hydration throws "incomplete session snapshot"
    whenever `session.status` returns a status type outside
    `["busy", "retry", "idle"]`, so a new OpenCode status type means
    hydration never succeeds, `ctx.events` grows for the life of the TUI,
    and `state()` never returns idle. `ctx.deleted` also only grows.
- **A test-only shortcut production can reach**: `SHEPR_OMP_IDLE_DEBOUNCE_MS`
  (3).
- **Personal data in temp files**: every shell hook copies the agent's
  payload to `mktemp "${TMPDIR:-/tmp}/shepr-<agent>-hook.XXXXXX"`. For
  `UserPromptSubmit` (Codex, Kimi, MastraCode) that is the user's prompt
  text. `mktemp` makes it 0600 and the `0` trap removes it, but a SIGKILL
  leaves it in `/tmp`. The payload could be piped straight into python3
  instead of staged.

## 9. Code that is no longer load-bearing

- **The `Devin | Droid` arms of `expected_events`' exception**: both
  descriptors give every event an action, so only `Copilot` ever takes it.
  With the flag moved into the spec (8), the list disappears.
- **The update-in-place branch of `ensure_direct_command_hook`**: its only
  caller, `expected_events`, starts from an empty map each time and Copilot
  has one event, so the `find(..)` never matches. Likewise
  `direct_command_field()` is a function returning the constant `"bash"`.
- **`targets::grok_hook_command`**, a wrapper identical to `hook_command`
  whose doc ("uses the same POSIX shell command as the other targets") is the
  memory of when it differed; **`registry::install_operation`**, a
  pass-through to `targets::install`.
- **`missing_agent_directory`'s per-agent prose table**: reachable in
  production only if an agent directory disappears between the presence
  check and the install (see 4).
- **`SHEPR_INTEGRATION_ID`** (read by nothing), and
  **`SHEPR_INTEGRATION_VERSION` with `installed_version`,
  `parse_integration_version`, `IntegrationOutdatedReason`** and the
  `NotInstalled`/`Outdated` split: all exist only to be logged; currentness is
  exact bytes. The hand-bumped `version` in `SPECS` feeds only these.
- **`SHEPR_OMP_RETRY_GRACE_MS`**: set by nothing (3).
- **The `codex_hooks` deletion** in `build_codex_config_with_hooks` and the
  test `install_codex_only_migrates_top_level_feature_flags`: migration code
  for Codex's old flag name, which the project's rule is to drop.
- **The `pi.events` `"shepr:blocked"` listener** in Pi and OMP, unless it is a
  feature to document (7).
- **`process_owned_integration_assets_do_not_report_release`** (6).
- **`PermissionPolicy`'s two match arms both yield `0o666`**
  (`atomic_replace.rs`).
- **`case "session.deleted": break; default: break;`** in both OpenCode-family
  decoders.
- **Qoder and Qwen config-dir overrides** are captured by
  `IntegrationEnvironment::capture` (it takes every descriptor's
  `config_dir_override`) although neither has an integration target.
- **Fifteen `#[cfg(test)] install_<agent>` wrappers** in `targets.rs` and
  `registry::integration_hook_events` are one-line forwards of
  `install(paths, Target::X)` and `target.hook_events()`.

---

## Lateral notes

- **Stale registrations for an old hook path are never removed.** Removal
  matches only commands for the current `hook_path`. If `HOME`, a
  `*_CONFIG_DIR`/`*_HOME` override, or the symlink spelling of the home
  directory changes between launches, the old entries stay registered and
  keep running the old hook file, which is never updated again. The same
  happens to an agent config shared across hosts through a dotfiles symlink
  when the hosts' home paths differ: each host adds its own entry, and on the
  other host that entry runs `sh '<missing path>'`, a failing hook the agent
  may show.
- **Shutdown during install can leave a staging file** (`.shepr-<token>-<n>.tmp`)
  in the user's agent directory: the install thread is detached, and process
  exit does not run its `PreparedFile` destructor. Nothing reclaims these.
- **OMP's `retryableErrorPattern`** matches bare `500`, `429`, `502`... as
  substrings, so an error message mentioning `5000` tokens or a `1500 ms`
  timeout is classed as retryable and held as Working for the grace period.
- **Early seq stamping** (`EARLY_SEQ`) is applied to Kimi and MastraCode
  only; Codex sends `UserPromptSubmit`/`Stop`/`Interrupt` from separate
  processes stamped after interpreter start, the reorder the `EARLY_SEQ`
  comment describes.
- **Logging field names**: `logging.rs` uses the project's
  `event`/`subsystem`/`outcome` convention, but each crate hand-writes its own
  helper for it (persist, pane, ipc, api, client, server, integration); a
  shared macro would make the field set one definition.
