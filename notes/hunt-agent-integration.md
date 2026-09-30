# Hunt: agent integrations

Scope: `crates/shepr-agent/src/integration/` (installer, registry, status
check, config editors) and `integration/assets/` (hook scripts and JS/TS
plugins), followed through `pane.report_agent` / `pane.report_agent_session`
(`crates/shepr-server/src/app/api/panes/reports.rs`) into `TerminalState`
(`crates/shepr-mux/src/terminal/state/{hooks,sessions}.rs`).

The main finding is structural. The server has a strict acceptance contract
for full-lifecycle sources (pi, omp, mastracode, opencode, kimi, kilo). Hook
authority is granted only once the source is anchored, and a fresh pane can
only be anchored by a `pane.report_agent_session` carrying a seq and a
recognized `session_start_source` (the OpenCode TUI's unsequenced `select`
report is the one exception). Until then every state report is parked as a
pending replacement and ignored. Nothing checks the assets against this
contract. The TS tests assert which requests an asset emits, and the Rust
state tests anchor sessions by calling `set_persisted_agent_session`
directly, so no test feeds an asset's real request sequence into
`TerminalState`. Findings 1 to 4 all fall through that gap.

Suggested structural fix: add one cross-boundary test harness that runs each
asset (sh via a fake socket, JS/TS via the existing bun tests) through a
scripted agent session and replays the captured JSON requests into a
`TerminalState`. Then assert on the resulting hook authority and persisted
session, not on request shapes. Pair it with a server-side rule, or an
explicit asset-side rule, for what happens to a state report that carries
no session ref (finding 3).

---

## 1. OpenCode server plugin (run / --mini) can never anchor, so its reports are all ignored

Claim broken: the asset says "V1 local run/Mini retain their server hooks"
(`assets/opencode/shepr-agent-state.js`, default export comment). The
descriptor gives OpenCode `full_lifecycle_hook_authority: true`, and
AGENTS.md says the integrations "report state and session IDs back to
shepr".

- `reportSession(sessionID)` sends `pane.report_agent_session` with a seq
  and no `session_start_source`. In
  `TerminalState::set_agent_session_ref_for_typed_start_source_at`, a
  full-lifecycle source that is not yet anchored (`!session_anchored`)
  reaches `if !Self::session_start_source_is_recognized(..) { return None; }`.
  That drops the report. It is not the unsequenced `select` path either,
  because it carries a seq and no `Select` source.
- Every `reportState(...)` from this plugin goes to
  `route_full_lifecycle_hook_report`. Nothing is anchored, so it is parked
  in `pending_replacement_report` and returns `Ignore`. A parked report is
  only promoted in `clear_full_lifecycle_hook_suppression_for_detected_agent`
  when `replacement_session_ref` is set, and only a session report sets
  that.
- There is a second problem. The `session.created` / `session.updated`
  branches read `properties.sessionID`, but these events carry the session
  under `properties.info` (see finding 2; shepr's own V1 TUI plugin reads
  `data.sessionID ?? data.info?.id` for them). If so, `session.created` sets
  `reportedRootSessionID = undefined`, and `session.updated` never produces
  a report.

Net effect: in `opencode run` or `--mini` panes the hook integration is
inert, and resume never gets an OpenCode session id from this path. It
looks as if it works only because screen detection carries the state.

Fix direction: send `session_start_source` (for example `startup`) on the
first root session, read `info.id` for created/updated, and cover this with
the harness above.

## 2. Kilo plugin reads a field its own comment says does not exist, so it never reports a session

Claim broken: `reportSession` in `assets/kilo/shepr-agent-state.js` says
"`session.created`/`session.updated` carry only the session info". The
event handler calls `reportSession(sessionID)` for exactly those events,
with `sessionID = sessionIDFromProperties(properties)`, which reads
`properties.sessionID`. The same handler reads `properties.info` a few
lines earlier to track child sessions.

- If the comment is right (and it matches OpenCode's `Session.Event.Created`
  / `Updated`, which are `{ info }`), `reportSession(undefined)` is a no-op.
  Kilo's only other session report is `session.status` with a status that
  is not in `SESSION_STATE_BY_STATUS`. OpenCode's status types (`idle`,
  `busy`, `retry`) are all mapped, so that path never fires.
- Kilo is full-lifecycle, so with no session report none of its state
  reports is ever accepted (the same parking mechanism as finding 1). Kilo
  has `resume_support`, but `persisted_agent_session` is never set for a
  Kilo pane, so Kilo panes never resume on restore.
- The TS fixture in `assets/opencode/shepr-agent-state.test.ts` builds
  `session.updated` as `{ properties: { sessionID } }`. Its `session.created`
  child fixture uses `{ properties: { info } }`. The tests encode the
  unverified shape, so they cannot catch this.

Verify the Kilo event schema once. If it is `{ info }`, use `info.id`
(skipping children, as the code already does).

## 3. A state report without a session ref erases the resume identity, then freezes hook authority

Claim broken: resume on restore (AGENTS.md "Session restore ... and agent
resume on restore") and the asset comments that the full-lifecycle plugins
are the pane's authority.

- In `set_hook_authority_at`, an accepted report always does
  `self.persisted_agent_session = None` and stores `session_ref` from the
  report. The report is accepted when it is anchored, and
  `route_full_lifecycle_hook_report` treats an incoming `None` as anchored
  (`is_none_or`). So a session-less report sets `hook_authority.session_ref
  = None` and clears the persisted session, and the pane loses its resume
  identity. This is intended per
  `accepted_hook_report_without_session_ref_clears_previous_ref`.
- The next report finds nothing to anchor against. A session-less report
  hits `let Some(session_ref) = session_ref.clone() else { return Ignore }`.
  A session-bearing report is parked. Hook authority stays frozen at the
  state of that one session-less report until the next session report with
  a recognized start source. Kimi and MastraCode send one only on
  SessionStart; Kilo and OpenCode-run in practice never do (findings 1
  and 2).
- Assets that emit session-less state reports:
  - `session.error` in the opencode and kilo plugins calls
    `reportState("blocked", sessionID)`. OpenCode's error event has an
    optional `sessionID`, so a global error (provider or auth failure)
    produces a `blocked` report with no session. That pins the pane at
    Blocked.
  - The kimi and mastracode hooks send state reports without
    `agent_session_id` whenever the payload lacks `session_id`.
  - The pi and omp plugins (`withSessionRef`) do the same when the session
    manager has no file or id (for example a no-session run).

Fix direction: decide the contract in one place. Either assets must not
send a full-lifecycle state report without a session ref (drop it at the
source), or the server keeps the anchored ref when the incoming report has
none. The current combination is the worst of both.

## 4. Kimi (and Kilo) have no session-replacement rule, so an in-process session switch freezes the pane

Claim broken: the Kimi asset says it passes the start source through
("shepr ignores values it does not know"). The integration is meant to
report the current session and state.

- `session_report_allows_session_replacement` has arms for Claude, Codex,
  MastraCode, OpenCode, Pi, Grok, OMP and Antigravity. It has none for
  Kimi or Kilo.
- After a Kimi session change inside the same process (a new SessionStart
  with a new `session_id`):
  - The session report is refused: `replaced_hook_session.is_some() &&
    !session_replacement_allowed`.
  - Every later state report carries the new id, which differs from the
    anchored one. It is therefore not anchored and is parked.
  - Hook authority stays at the old session's last state, and resume
    targets the old session.
- The Kilo comment says this is deliberate for Kilo ("neither lets Kilo
  replace a session"). The frozen state that follows is not called out
  anywhere.

This depends on Kimi being able to switch sessions in-process (`/clear`,
`/new`, a resume picker). Verify that, then either add an arm or have the
server release authority on a refused replacement.

## 5. "Installs or updates" means updates only when a version constant is bumped

Claim broken: AGENTS.md says the server "installs or updates them at
launch".

- `integration_state_for_path` compares the `SHEPR_INTEGRATION_VERSION`
  marker in the installed file against the spec's constant, using `>=`. The
  asset bytes are never compared. If an asset is edited without bumping its
  constant (nothing enforces the bump:
  `bundled_integration_assets_match_expected_versions` only checks that the
  marker equals the constant), every host keeps the old file forever. The
  same goes for a config registration whose shape changed without a bump,
  where the status check does not look at the changed part (for example
  hook timeout values in the Codex, Devin, Droid or MastraCode entries).
- Because of `>=`, a file written by a build with a higher constant is
  "Current" for a build with a lower one. Dev and release builds share
  agent config dirs (only runtime and data dirs are per-profile), so
  whichever build bumped last owns the hook files for both.
- The assets are `include_str!` constants, so the fix is cheap: compare the
  installed bytes with the bundled bytes. Then drop the per-target version
  constants and markers entirely, which removes a whole class of
  hand-maintained numbers.

## 6. Kimi config edit can produce a TOML Kimi cannot parse, and install reports success

Claim broken: the install-order comment at the top of `targets.rs` says "A
config that cannot be edited then fails the install before anything is
written".

- `build_kimi_config_with_hooks` strips shepr's marked block and appends
  `[[hooks]]` tables as text. It never parses the input or the output. A
  user config that already defines `hooks` as an inline array (`hooks =
  [...]`) or as a `[hooks]` table gets a conflicting redefinition. The
  result is invalid TOML and Kimi refuses to start. A config that was
  already invalid is also silently accepted.
- Afterwards `kimi_hooks_registered` cannot parse the file, so the status is
  Outdated on every launch. Each launch then re-runs the `kimi --version`
  probe (up to 5 s) and rebuilds the same broken text. The text is
  unchanged, so it is not rewritten, and no error is logged.
- Compare the Claude editor, which re-parses with `verify_updated`, and the
  Codex editor, which goes through `toml_edit`. Kimi should use `toml_edit`
  too, or at least parse the result and fail when it does not parse.

## 7. Devin hook can attribute another pane's session, and runs a subprocess on every tool call

Claim broken: the report is for this pane (`SHEPR_PANE_ID`), and the
resume that follows from it should target this pane's conversation.

- `resolve_session_id` in `assets/devin/shepr-agent-state.sh` falls back to
  `devin list --format json` whenever the payload has no session id (every
  event except UserPromptSubmit and a `startup` SessionStart). It takes the
  first entry whose `working_directory` equals the project dir. With two
  Devin panes in one repository, this reports whichever session the list
  shows first. Devin is not full-lifecycle, and the first accepted session
  for a pane is kept (`conflicting_same_owner_session_ref`). So a pane can
  hold another pane's conversation, and restore resumes the wrong one, or
  it is deduplicated away against the other pane.
- Every Devin event is registered, PreToolUse and PostToolUse included, and
  each one starts python and possibly a `devin list` (2 s timeout) inside a
  synchronous hook. That is a latency cost on every tool call, for a
  session-identity-only report that changes nothing after the first one.

Register SessionStart (and maybe UserPromptSubmit) only, and drop the
cwd-matching fallback, or restrict it to a unique match.

## 8. Pi reporter does not serialize session and state reports

- OMP routes every request through `requestQueue`. Pi's `reportSession`
  calls `sendRequest` directly, while states go through `drainStateQueue`.
  On `agent_start`, `void reportSession()` (seq N) and the `working` state
  (seq N+1) go out on two concurrent connections. If N+1 lands first, N is
  dropped as a straggler (`hook_seq_superseded`).
- `sendRequest` retries with the same seq. If the first attempt timed out
  but was delivered, the retry is dropped, which is harmless. If a newer
  report landed in between, the retried report is lost, which is not.
- Impact is small today: the state report carries the session ref too. Still,
  Pi is the one JS reporter without an ordered queue. Share OMP's queue.

## 9. Seq ordering assumptions that do not hold

- The kimi and mastracode hooks stamp `date +%s%N` at hook start, and their
  comments explain that stamping after python start lets startup jitter
  reorder near-simultaneous events. The codex hook, which also sends
  `working` and `idle`, stamps `time.time_ns()` after `cat` and python
  start. If Codex ever fires hooks concurrently, the same reordering
  applies.
- `report_seq_superseded` accepts a non-increasing seq as a clock step when
  it arrives `HOOK_SEQUENCE_REANCHOR_AFTER` (5 s) or more after the last
  acceptance. The hooks' own budget is `HOOK_TIMEOUT` (10 s), and each
  python socket op may take 0.5 s (connect, send and recv each). The Devin
  list call alone is up to 2 s, and the API request waits up to
  `ORDINARY_REQUEST_TIMEOUT`. A report that is delayed but still inside its
  budget can therefore arrive more than 5 s late, be taken as a clock step
  and overwrite a newer state. A straggler and a clock step look the same
  to this rule.

## 10. Hook traps ignore SIGTERM

The `set -eu` hooks install `trap 'rm -f "$hook_input_file"' EXIT HUP INT
TERM`. A trap on HUP/INT/TERM that does not `exit` resumes the script.
Agents that kill a timed-out hook with SIGTERM therefore get a hook that
deletes its input file and then carries on into python anyway. Use `trap
'...; exit 0' HUP INT TERM`, or trap EXIT only.

## 11. Interpreter mismatch

Every sh asset has a `#!/bin/sh` shebang and is POSIX sh. Install makes it
executable (0755). `hook_command` still registers `bash '<path>'` for all
targets except Grok. The Grok comment in `targets.rs` ("a POSIX `sh` script,
so it runs under `sh` rather than the `bash` the shared command formatter
uses for the other hooks") implies the others need bash. They do not. On a
host without bash every hook fails silently, while install and status
report Current. Use one interpreter (`sh`), or invoke the executable path
directly.

## 12. Smaller contract drift

- `AgentIntegrationPaths` doc: "Install and status code ... never consults
  the process environment while it is choosing files to read or write".
  `config_update_lock_path` reads `XDG_STATE_HOME` / `HOME` live, which the
  comment on `absolute_xdg_home` concedes. Either capture them in
  `AgentIntegrationPaths` or reword the doc.
- The opencode server plugin and the kilo plugin map every `session.error`
  to `blocked`, including `MessageAbortedError` (the user pressing Esc). The
  V1 TUI plugin, reporting under the same `shepr:opencode` source, excludes
  aborts. One source, two meanings.
- The codex hook refuses a session report without `transcript_path` but
  never sends the path. The requirement gates nothing the server uses.
- `settle(false)` in the opencode and kilo `requestOnce` passes an argument
  that `settle` ignores (copied from the TUI plugin, where it matters).
- `action_label` is a second spelling of the target label used only for log
  lines ("antigravity-cli" vs "agy"). `integration_target_label`,
  `mastracode_hook_command` and `antigravity_cli_hook_command` are
  pass-through wrappers.

## Lateral findings outside the scope

- Pane id inheritance: `build_server_daemon_command`
  (`shepr-remote/src/remote/local_server.rs`) passes the caller's
  environment through, so a server started from inside another server's
  pane (the documented dev-inside-release setup) inherits that pane's
  `SHEPR_PANE_ID` and `SHEPR_ENV`. `PaneLaunchEnv` without a pane id
  "inherits whatever the server environment carries", and `SheprPaneId`
  has policy `Allowed`. Restore (`persist/restore.rs`) launches without a
  pane id when a saved public id is missing or fails to parse. Hooks in
  such a pane report under the outer server's pane id to this server's
  socket. That is usually "pane not found", and in principle it could hit
  an unrelated pane. Scrub `SHEPR_PANE_ID` (policy `Scrubbed` or
  `ServerOnly`) when no id is given.
- Structure: 14 assets each re-implement the envelope, socket, timeout and
  JSON code, and the regex test `hook_assets_share_one_envelope` exists only
  to keep them in line. Generating the sh assets from one template at build
  time would remove most of that surface, and keep install byte-comparable
  (finding 5). A single `shepr` hidden reporter subcommand would do it more
  simply, but it widens the CLI, which AGENTS.md keeps small on purpose, so
  that choice is the owner's.

## Not verified, not reported as defects

These depend on third-party agent behaviour I could not confirm from the
repo: Copilot's hook event spelling (`SessionStart` here, camelCase in
Copilot's own hooks format) and settings file name; whether Codex has an
`Interrupt` hook event; OpenCode's global plugin directory name (`plugins/`
for OpenCode vs `plugin/` for Kilo); whether Kimi switches sessions
in-process (finding 4); the exact OpenCode/Kilo `session.*` event payloads
(findings 1 to 3 assume `{ info }` for created/updated and an optional
`sessionID` for error, as OpenCode defines them).
