# Hunt: restore and agent resume

Scope read in full: `crates/shepr-mux/src/persist/restore.rs`, every file of
`crates/shepr-agent/src/` (`lib.rs`, `resume.rs`, `report.rs`, `state.rs`,
`limits.rs`), `crates/shepr-server/src/app/agent_resume.rs` and
`crates/shepr-server/src/app/resume_schedule.rs`. Followed into
`persist/open.rs`, `persist/schema.rs`, `persist/capture.rs`,
`workspace/pane_tree.rs`, `workspace/geometry.rs`, `terminal/state/*.rs`,
`pane/launch_status.rs`, `pane/exit_arbiter.rs`, `pane/detect/state.rs`,
`app/pane_launch.rs`, `app/events.rs`, `app/mod.rs`, `app/host_theme.rs`, the
headless resume call sites, `shepr-config` (session settings, template, docs),
`shepr-core/src/shell_quote.rs`, `brokkr.toml` and `clippy.toml`.

Findings are not ranked. Each says where it is, what is wrong, and whether
and how the fixed version can be enforced.

---

## 1. Defects

### D1. A duplicate saved workspace ID makes every client report "some saved panes could not be restored" when nothing was lost

- `restore.rs` `plan_restore` sets `restore_damage = true` only when a saved
  workspace ID repeats (`seen_saved_ids`). That workspace is not dropped: it
  gets a fresh ID and all its panes. `SessionRestorePlan::launch` then builds
  `RestoreLoss::from_damage(dropped_workspaces, restore_damage)`, which turns
  the flag into `panes_pruned` / `RestoreLoss::Panes`.
- `RestoreLoss::Panes` is documented as "No workspace was dropped, but pane
  or layout data was pruned", and `SessionRestoreLoss::Panes` in
  `shepr-protocol/src/message.rs` renders as "The saved session was restored
  in part: some saved panes could not be restored." Nothing in restore ever
  prunes a pane: `PaneTree::plan` either admits a whole workspace or refuses
  it. The only producer of `panes_pruned` is the renamed-ID case.
- `persist/open.rs` test `restore_with_damage_backs_up_the_saved_session_before_the_first_save`
  restores both workspaces in full (asserts `len() == 2`) and then asserts the
  notice is `SessionRestoreLoss::Panes`. The test writes the false notice
  down as expected behaviour.
- The `tracing::warn!` in `open.rs` logs `restore_damage = loss.panes_pruned()`,
  a third name for the same bit.
- Broken claim: the `SessionRestoreLoss` doc ("Every variant loses
  something") and the rendered operator text.
- Fix: give the renamed-ID case its own variant (or make it a backup-only
  condition with no notice), and drop `panes_pruned` until something prunes.
  Enforceable by a type: `RestoreLoss` should be built from the events that
  happened (dropped workspaces, renamed IDs), not from a bool named for
  something else.

### D2. An unconfirmed resume launch loses the saved agent session, with nothing logged

- `app/pane_launch.rs`, `LaunchOutcome::Unconfirmed` for
  `LaunchKind::AgentResume`: abandons the resume with
  `ResumeUnavailableReason::ShellLaunchUnconfirmed`. The comment claims "The
  saved identity is untouched, so the next restore still resumes the
  session."
- `pane/launch_status.rs` says of `Unconfirmed`: "The pane's death follows and
  is an ordinary one." An ordinary death goes through
  `App::apply_pane_removal`, which removes the pane, and its persisted agent
  session goes with it. The next save writes a layout without that pane, so
  the next restore resumes nothing. The checkpoint (when `needs_checkpoint`
  holds) only delays this: removal and a later save still follow.
- The `restore_error` that the abandonment records is never drawn, because
  `ui/panes.rs` draws the failure text only for a pane with no runtime, and
  this pane keeps its runtime until the death removes it. Neither
  `handle_pane_launch_settled` nor `launch_status.rs` logs anything for
  `Unconfirmed` (the coordinator warns only for `Failed`).
- The result: the agent pane vanishes on restore, its session is gone from
  the saved layout, and no log line or UI notice says so.
- Fix: decide what an unconfirmed resume means for the pane (keep it as a
  placeholder carrying the session, the way a `Failed` launch is kept), and log
  it. A test that settles an `AgentResume` launch as `Unconfirmed`, delivers
  the death, saves and checks the saved `agent_session` would enforce it.

### D3. A resume command whose send fails leaves a bare shell and an invisible error

- `pane_launch.rs` `send_resume_command`, `Some(Err(_))`: warns, then
  `abandon_agent_resume(.., CommandSendFailed)`. The pane has a live runtime,
  so the recorded `restore_error` is never rendered (`ui/panes.rs` only draws
  it when there is no runtime). The operator sees a plain shell where an agent
  was, with nothing in the pane saying the resume failed.
- `terminal/state/detection.rs` `abandon_agent_resume` doc says "no runtime
  ever existed here, so that detection is only the seed". That is false for
  this caller and for the `Unconfirmed` one (D2): both have a runtime.
- Fix: either render a resume failure as a notice that does not need a
  runtimeless pane, or route these two cases to a different record than
  `restore_error`. Testable: assert the surface for a pane whose resume send
  failed carries the failure text.

### D4. A saved agent session that fails to decode is dropped from disk with no backup

- `persist/schema.rs` `deserialize_agent_session` turns an invalid saved
  session into `None` with a warn. Restore never learns that saved data was
  discarded, so `restore_loss` stays `None`, `open.rs` sets
  `SessionBackupPolicy::NoBackupNeeded`, and the first save overwrites the only
  copy of the session reference.
- Broken claim: `RestoredSession::restore_loss` ("What saved data restore
  discarded ... The caller preserves the source session file whenever this
  value is present") and the backup policy `plan_workspace`'s doc describes
  ("The workspace is not lost on disk"). A resumable session ID is exactly
  the saved data the owner cares about after a multi-day agent run.
- The warn names no workspace or pane (see 4.3).
- Fix: report a discarded session as restore damage (backup before first
  save). Testable: a session file with one invalid `agent_session`, open, save,
  assert a backup exists.

### D5. Restore silently repairs damaged focus, root and zoom, contrary to its own stated policy

- `restore.rs` `plan_workspace` doc: a saved-file defect "drops this one
  workspace, rather than refusing the whole session ... or repairing it (which
  silently rewrites a corrupt file)".
- `workspace/pane_tree.rs` `PaneTree::plan` does repair: a saved `focused` or
  `root_pane` that names no leaf falls back to the first leaf, and a saved
  `zoomed: true` on a one-pane workspace or with a missing focus is dropped.
  None of these sets `restore_damage`, so no backup is taken and nothing is
  logged; the first save rewrites the file. A shepr save never writes a focus
  that names no leaf, so these are corrupt-file cases by the doc's own
  definition.
- Fix: return the repairs from `plan` and count them as damage (backup and
  log), or refuse the workspace as the doc says. Testable as D4.

### D6. The resume command is quoted for POSIX shells, but config accepts non-POSIX shells

- `agent_resume.rs` `start_pending_agent_resume` types
  `plan.to_shell_command()` plus `\r` into the pane's shell.
  `to_shell_command` is `shepr_core::shell_quote::join_argv`, documented as
  "POSIX shell word quoting".
- `shepr-platform/src/executable.rs` `SHELL_NAMES` (what
  `default_shell`/`SHELL` validation admits) includes `fish`, `csh`, `tcsh`,
  `elvish`, `xonsh` and `nu`. The `'a'\''b'` concatenation `quote_always`
  produces is not valid in nu, and csh expands `!` inside single quotes.
  Plain words (`is_plain_word`) pass through unquoted, so typical UUID session
  IDs are fine; a Pi or Omp session path with a space or quote is not.
- Broken claim: the comment "Quote the planner's validated command before
  typing it into the shell", given that the shell can be any accepted one.
- The only end-to-end test of the typed command
  (`pending_agent_resume_waits_for_live_host_theme_before_launch`) runs
  host `/bin/sh` (see 6.4), so nothing covers another shell.
- Fix options: refuse resume (or the shell) where the quoting does not apply,
  quote per shell family, or launch the resumed agent through a path that does
  not depend on the interactive shell's grammar. Enforceable by a test per
  accepted shell family, or by a type pairing a `ResolvedShell` with its
  quoting.

### D7. `AGENT_ABSENCE_STARTUP_HOLD`'s documentation says restored panes hold, and they do not

- `shepr-mux/src/limits.rs`: "A restored pane holds absence for the same
  interval as agent resume".
- `pane/detect/state.rs` `DetectorState::new`: `LaunchKind::Fresh |
  LaunchKind::Restored => None`; only `LaunchKind::AgentResume` holds.
- The private alias `AGENT_RESUME_DETECTION_HOLD` exists only to feed this
  one constant (see 9).
- Not mechanically checkable as prose; fix the wording and fold the alias.

### D8. Several durable comments in scope state things that are false today

Each is a claim nothing checks:

- `restore.rs` `restored_terminal` doc: "cwd, label and launch argv: always
  kept." `PaneSnapshot` has no launch argv (it has cwd, public number, label,
  agent session).
- `terminal/state/mod.rs` `PaneStartFailure::ResumeUnavailable` doc: "(no
  command to run, the pane gone from under the attempt)". A plan always has a
  command (`AgentResumePlan::with_argv` refuses an empty program), and the
  reasons that do occur (`ShellLaunchUnconfirmed`, `CommandSendFailed`) are
  not named.
- `resume_schedule.rs` `AttemptOutcome::Abandoned` doc: "(PTY could not be
  opened, missing launch env)". The code abandons on a `launch_pane` error and
  on the unreachable pane-gone branch (9.4); "missing launch env" names nothing
  in the code.
- `pane_launch.rs` Unconfirmed comment (D2).
- `detection.rs` `abandon_agent_resume` doc (D3).
- `app/events.rs` `decide_pane_exit`: "since history capture leaves an
  unreadable terminal's cached history as it was". History capture was removed
  with `pane_history` (lateral L1).

---

## 2. One value, one owner

### 2.1 The default resume spacing is spelled in five places

`startup_per_agent_delay_ms = 100`:

- `shepr-config/src/limits.rs` `DEFAULT_STARTUP_PER_AGENT_DELAY` (the owner);
- `shepr-config/src/model.rs` doc comment "Default: 100 ms";
- `shepr-config/src/default-server.toml` `# startup_per_agent_delay_ms = 100`
  (checked against the default by
  `server_default_template_documented_values_match_defaults`);
- `docs/config.md` `[session]` table, `100` (unchecked);
- `app/agent_resume.rs` test `pending_agent_resume_launches_hidden_panes_with_current_terminal_area`:
  `let barrier = now + Duration::from_millis(100)`, coupled to the default with
  no reference to it.

They agree today. Fix: the test reads `config.session().startup_per_agent_delay`;
the model doc drops the number; a test can compare `docs/config.md`'s
`[session]` table defaults to `ServerConfig::default()` the way the template
test does. The same docs table repeats `resume_agents_on_restore = true`
unchecked.

### 2.2 The agent is stored twice in every saved session identity

`resume.rs` `PersistedAgentSession { source, agent, session_ref }`. The
source names exactly one agent (`AgentSource::agent()`), and
`is_valid_identity` exists to check that the second copy agrees with the
first. The saved file carries both `"source"` and `"agent"`, so the session
file can hold a disagreement that decoding then refuses. `ReportOrigin::owns`
compares both again. Fix: drop the `agent` field and derive it from the
source; the disagreement becomes unrepresentable. No migration is owed.

The live report path has the same doubling on the wire: a hook sends a
`source` and an agent label, and `ReportOrigin::parse` exists largely to
refuse `MismatchedAgent`. The label carries no information the source does
not.

### 2.3 Integration source strings restate the label

`lib.rs` `IntegrationTarget::source()` hand-spells `"shepr:pi"`, ...,
`"shepr:agy"`; every one is `"shepr:" + target.label()`. Nothing checks that
relationship (`official_sources_round_trip_with_nonempty_names` checks only
round trips). Enforceable by a test asserting
`source() == format!("shepr:{}", label())` for every target.

### 2.4 `AgentState`'s spelling is written twice

`state.rs` derives `#[serde(rename_all = "snake_case")]` and also hand-writes
`Display` with the same four strings ("The snake_case spelling serde uses").
They agree. Enforceable by a test comparing `to_string()` with
`serde_json::to_string`, or by making `Display` delegate to the serde form.

### 2.5 `AgentSessionStartSource::ALL` restates the enum

`resume.rs`: `parse` searches `ALL`, and the round-trip test iterates `ALL`,
so a variant left out of `ALL` would never parse and no test would notice.
Enforceable at compile time with the exhaustive-match trick
`lib.rs` already uses for `AGENTS` (a `const` block whose `match` lists every
variant and asserts the array length).

### 2.6 The restored-pane spawn size rule exists twice

- `restore.rs` `restored_pane_size`: zoomed pane gets its zoomed size, every
  other pane its tiled size, fallback `sole_pane_size`.
- `workspace/geometry.rs` `WorkspaceChrome::resume_panes` (used by
  `agent_resume.rs`): the same rule written as a list walk.

They agree today (both go through `visible_panes` and
`into_content(pane_scrollbars, false)`). Fix: restore asks `resume_panes` (or
one `spawn_sizes(layout, zoomed)`) for every pane. Enforceable by deleting one.

### 2.7 The resume candidate rule exists twice

`agent_resume.rs` `has_pending_agent_resume_candidates` and
`pending_agent_resume_candidates` walk the same rule separately (the doc says
"Same rules as"). A test (`candidate_probe_agrees_with_the_collected_candidates`)
keeps them in step for the cases it builds. Fix: one iterator of candidates;
the probe is `.next().is_some()`.

### 2.8 The backup directory name is spelled outside its owner

`persist/files.rs` `BACKUP_DIRECTORY_NAME = "session-backups"` is the owner.
`persist/open.rs`'s warn message hardcodes "session-backups" in its text
(the actual `backup_dir` is in scope and could be logged as a field), and
`app/session.rs` test spells `data_dir.join("session-backups")` where
`shepr_mux::persist::session_backup_directory` exists. Fixable; a textlint on
the literal outside `files.rs` would enforce it.

### 2.9 The `pane` log field means two different identifiers

`restore.rs` logs `pane = %launch.public_id, pane_id = %launch.pane_id`;
`agent_resume.rs`, `pane_launch.rs` and `launch_status.rs` log
`pane = %pane_id` (the internal `PaneId`). A log query on `pane` matches two
kinds of value. Enforceable by a textlint keyed on `pane = %` followed by a
`pane_id` binding, or by giving the identifiers distinct field names.

### 2.10 Positional expectations zipped against the agent table

`lib.rs` test `integration_classes_preserve_authority_for_every_agent` zips a
23-entry positional `expected` array with `AGENTS`. `zip` stops at the shorter
side, so an appended agent is silently untested. Add an `assert_eq!` on the
lengths, or key the expectations by `Agent`.

---

## 3. Values nobody can find, change, or trust

### 3.1 The resume tunables are scattered across three crates with no index

What decides when and how a resume happens:

- spacing: `[session] startup_per_agent_delay_ms` (config);
- theme wait: `PENDING_AGENT_RESUME_THEME_WAIT` (750 ms, `shepr-server/src/limits.rs`);
- detector absence hold: `AGENT_ABSENCE_STARTUP_HOLD` (30 s, `shepr-mux/src/limits.rs`);
- unconfirmed-settlement timeout: `LAUNCH_SETTLE_AFTER_PANE_END` (`shepr-mux/src/limits.rs`).

Each is named and documented in its crate's limits module, which the
`numeric-consts-live-in-limits` rule enforces, but nothing tells a reader that
these four together are the resume timeline. A short section in `reference/`
(or a module doc in `resume_schedule.rs`) naming all four would answer it;
not mechanically checkable beyond `check_cited_paths.py` keeping the names
real.

### 3.2 The setting's name does not say what it does

`startup_per_agent_delay_ms` spaces agent resumes only (fresh restored shells
all launch at once in `SessionRestorePlan::launch`). The template calls it
"Milliseconds between automatic agent restores", `docs/config.md` "Pause
between automatic agent resumes", the model doc "Time between automatic agent
restores". Three wordings, a name that says neither. Renaming is free here (no
compatibility owed).

### 3.3 The theme wait is fixed for every `App`

`ResumeSchedule::new` takes the wait as a parameter, which is good, but
`App::open` always passes the constant. Tests through `App` work around it by
handing synthetic instants (`now + PENDING_AGENT_RESUME_THEME_WAIT`), which is
fine; noting only that the knob has an injection point at the schedule and
none at the app.

---

## 4. One channel, one implementation

### 4.1 Resumes that did not happen are mostly silent

What the operator learns today:

- resume disabled by config: nothing (expected, but there is no info line);
- a duplicate session suppressed (`pane_restore_startup`): no log, no notice;
  the second pane starts as a plain shell and loses its session;
- an invalid saved session dropped at decode: a warn with no workspace or pane
  (D4);
- an unconfirmed resume launch: nothing at all, and the pane vanishes (D2);
- a resume command whose send failed: a warn, and an error the UI never draws
  (D3);
- a resume that launched: nothing. There is no info line saying "resumed
  codex session X in pane w1-3", so when an agent does not come back there is
  no log to compare against;
- the `persist.restore` summary line (`open.rs` `log_restore`) reports a
  workspace count and an outcome, but not how many resumes were planned or
  suppressed.

The `SessionRestoreNotice` channel exists and reaches every client, but it
covers only layout loss. Resume outcomes have no channel. Not mechanically
enforceable; the fix is a resume summary (planned, suppressed as duplicate,
abandoned with reason) logged once and, for failures, carried in the restore
notice.

### 4.2 The placeholder text cannot be acted on

`terminal/state/mod.rs` `PaneStartFailure::guidance`: "Could not resume the
saved agent. Restart this session." and "Pane directory is unavailable.
Restore the directory and restart this session." "This session" names
nothing the operator can restart (there is a server restart, `shepr stop` then
`shepr`). The resume failure text names neither the agent nor the session
reference nor the command, although the plan (and `plan.to_shell_command()`)
was in hand when the resume was abandoned, so the operator cannot resume by
hand. The operator text is assembled here, not by the code that owns operator
guidance (`shepr-launch` owns "the operator text naming the commands that
reach a server").

### 4.3 Lines missing the identifiers needed to act

- `schema.rs` "ignoring invalid saved agent session": no workspace, pane or
  agent.
- `restore.rs` `restored_terminal`, "preserving unavailable restored pane":
  cwd and reason, no pane.
- `agent_resume.rs` "failed to start shell for deferred agent resume": pane
  (internal id) and agent, no workspace, public id or session.

### 4.4 One event, two lines, two levels

A restored shell whose launch fails before forking logs `error!` "failed to
restore pane" in `SessionRestorePlan::launch`, then `warn!` "preserving
unavailable restored pane" from `restored_terminal` for the same pane. The
equivalent failure for a resume launch is a single `warn!`, and a child-reported
launch failure (`launch_status.rs`) is a `warn!`. Pick one level for "a pane
could not start" and log it once.

### 4.5 The restore damage warn's fields and text

`open.rs`: `restore_damage = loss.panes_pruned()` (see D1) and a message that
names the directory by its literal name instead of the path (2.8).

---

## 5. Errors

### 5.1 Unconfirmed resume settlement swallowed (D2)

No log, no visible error, data lost.

### 5.2 Silent no-ops in the resume command path

`pane_launch.rs` `send_resume_command`'s `None => {}` arm (no runtime) leaves
the terminal in `AgentResumeState::Launching { command: None }` forever. That
state is `is_pending()`, so `has_pending_agent_resumes()` stays true, the
schedule never retires, and every loop iteration walks every pane. Admission
(`admit_event` requires the runtime's current generation) makes the arm
unreachable today, which is why it should not exist: replace it with an
`error!` and an abandonment, or restructure so the runtime is passed in.

The same shape: if `take_agent_resume_command` returns `None` for an
`AgentResume` settlement (the state is not `Launching`), nothing is logged and
a `Planned` plan with a live runtime is stuck pending (`candidate()` requires
no runtime).

### 5.3 `resume::plan` returns `Option` for a total function

`PersistedAgentSession::new` already requires `session_ref.accepted_for(agent)`,
which requires `resume_support`, and descriptor executables are nonempty, so
`plan` cannot fail for a value of that type. A `None` from it in
`restore_plan_for_snapshot` would silently start a plain shell with no log.
Make it `PersistedAgentSession::resume_plan(&self) -> AgentResumePlan`; the
type then carries the guarantee.

### 5.4 Abandonment on a pane that is gone

`App::abandon_agent_resume` goes through `update_terminal_state`, which does
nothing for a missing pane. The `PaneGone` reason is therefore recorded on
nothing (see 9.4).

---

## 6. Tests that prove nothing

### 6.1 A test asserts the false restore notice

`persist/open.rs` `restore_with_damage_backs_up_the_saved_session_before_the_first_save`
asserts `SessionRestoreLoss::Panes` for a restore that lost no panes (D1).

### 6.2 Two restore tests exercise a test-only reimplementation

`restore.rs` `take_restore_plan_for_snapshot` is a `#[cfg(test)]` copy of the
duplicate rule (`filter(|plan| set.insert(key))`), not the production
`pane_restore_startup`. `restore_plan_selection_suppresses_duplicates` and
`restore_does_not_rehydrate_duplicate_agent_session_metadata` test that copy;
the latter's last assertion, `restored_terminal_agent_session(Some(&s), true).is_none()`,
is the function's first `if`. The production rule is covered by
`pane_restore_startup_resumes_a_session_once_and_starts_duplicates_as_shells`
and `complete_restore_plan_defers_one_resume_and_plans_duplicate_as_shell`.
Delete the copy and its two tests.

### 6.3 Tests that cannot fail

- `restore_rehydrates_agent_session_metadata`: `restored_terminal_agent_session`
  re-validates an already validated `PersistedAgentSession` through its own
  constructor (9.3), so the assertions compare a value with itself.
- `complete_restore_planning_needs_no_runtime_or_directory_access`: planning a
  missing directory succeeds whether or not planning stats it, so the test
  cannot observe the directory access its name rules out. Only a seam (an
  injected filesystem, or a path that hangs) could make it observable;
  otherwise rename it to what it checks.
- `resume.rs` `ids_are_data_not_shell_text` asserts the argv vector, not the
  shell text that is typed; the quoting it is named for is never checked
  there.

### 6.4 Resume tests run the host's `/bin/sh`, around the fixture rule

`shepr-test-fixtures/src/config.rs` sets `FIXTURE_SHELL = "/bin/sh"` for every
default test config. The `agent_resume.rs` tests that do not call
`set_test_shell` (`pending_agent_resume_waits_for_live_host_theme_before_launch`,
`pending_agent_resume_can_launch_after_theme_wait_expires`, the hidden, zoom-hidden,
background and first-resize tests, and `failed_deferred_restore_...` with
`missing_shell == false`) spawn the host's `/bin/sh`, and the marker test
depends on it interpreting the typed command. The `no-borrowed-process-stand-ins`
textlint matches only a literal inside `PaneShellConfig::new(...)`, so the
constant slips past it, and no `host-program-ok` marker records the
exception. For the marker test the host shell is arguably the subject; then it
should say so, and the others should use the fixture `idle_shell`. A textlint
on `"/bin/sh"` string literals in test-fixture crates would close the gap.

### 6.5 Test identities name a configuration they do not use

`test_support.rs` `test_codex_plan(identity, argv)` keeps only the text after
the last NUL and always builds a Codex session. Callers pass
`"shepr:codex\0codex\0Id\0probe-session"`, a leftover spelling of an older
NUL-joined resume key. A caller writing `"shepr:pi\0pi\0Path\0..."` would still
get a Codex plan. Take a session id only.

### 6.6 Smaller ones

- The 100 ms coupling in `pending_agent_resume_launches_hidden_panes_with_current_terminal_area` (2.1).
- `restore.rs` `failed_cold_restore_preserves_panes_and_saved_directories`
  writes `/tmp/shepr-restore-test-a` and `-b` into its JSON and overwrites
  them immediately; the literals mean nothing and read as a `/tmp` use.
- The positional zip in `integration_classes_preserve_authority_for_every_agent` (2.10).

---

## 7. Guards and claims that have stopped holding

- `ResumeSchedule.retired`: "plans are minted only by session restore, before
  the first pass. Once set, nothing scans." Nothing enforces this.
  `TerminalState::plan_agent_resume` is a `pub` production method whose only
  callers are tests; a production caller after retirement would plan a resume
  that never runs. Checkable: delete `plan_agent_resume` (restore uses
  `with_pending_agent_resume_plan`) and give tests a seam, or a textlint
  refusing `plan_agent_resume(` outside tests.
- `AgentSessionStartSource::ALL` (2.5): checkable at compile time, unchecked
  today.
- `AGENT_ABSENCE_STARTUP_HOLD` doc (D7): false today.
- `plan_workspace` doc's "rather than repairing it" (D5): false today.
- `RestoreLoss::Panes` / `SessionRestoreLoss::Panes` doc (D1): false today.
- Stale comments listed in D8: false today.
- `docs/config.md` defaults table (2.1): restates values the template test
  already checks for the template; unchecked for the docs.
- `TARGETS_HAVE_INTEGRATIONS`'s "`Grok` is the last `IntegrationTarget`
  variant": fails closed (a new last variant breaks the count assert), so it
  holds; it could compare against an exhaustive match the way `AGENTS` does,
  so the comment is not needed.

---

## 8. Policy invented per call site

### 8.1 What a damaged saved value does depends on where it is

- repeated pane numbers: workspace dropped, backup, notice;
- repeated workspace ID: renamed, backup, notice saying panes were lost (D1);
- focus, root or zoom naming no leaf: silently repaired, no backup (D5);
- invalid agent session: silently dropped, no backup (D4);
- anything the schema refuses: whole file refused, backup, notice.

One rule (every discard or repair of saved data backs up the file and is
reported, naming what) would replace five. Enforceable by having the planner
return a damage list that the open path must consume.

### 8.2 Test-only shortcuts reachable from production

- `AgentResumePlan::for_command(session, program, args)`: public in
  `shepr-agent`, used only by `shepr-server`'s `test_support`. It lets any
  production caller build a plan that types an arbitrary command into a
  restored shell, in a project that deliberately keeps no way to drive panes.
- `TerminalState::plan_agent_resume` (7).
- `PersistedAgentSession::from_report` and `AgentSource::from_pair`: every
  caller is a test (shepr-agent, shepr-detect and shepr-server tests).
  `AgentResumePlan::args()` likewise.

`shepr-test-fixtures` exists for exactly this; its layering rule does not
yet allow `shepr-agent`, and adding it is a one-line change. The existing
`check_dead_test_helpers.py` catches the inverse (test helpers nothing calls)
but not production `pub` items only tests call; extending it to report those
would enforce this.

### 8.3 Agent label acceptance differs by path

`ReportOrigin::parse` trims, lowercases and accepts aliases
(`" Claude "`, `"claude-code"`), while `PersistedAgentSession::from_report`,
`AgentSource::from_pair` and the saved `Agent` deserializer accept only the
canonical label. Given 2.2, the label could be dropped from reports
altogether, which removes the question.

### 8.4 The two resume entry points treat a consumed pass differently

`server/headless.rs` (loop) calls `start_pending_agent_resumes` and syncs pane
focus; `server/headless/client_views.rs` `finish_shell_workspace_geometry_change`
also requests a recompute from every client. `start_pending_agent_resumes`
already marks the shell projection dirty. Either the recompute is needed on
both paths or on neither; a single post-pass helper would decide it once.

### 8.5 Two writers of `AgentResumeState::Planned`

`TerminalState::with_pending_agent_resume_plan` (builder, restore) and
`TerminalState::plan_agent_resume` (setter, tests only). One is enough.

---

## 9. Code that is no longer load-bearing

### 9.1 Production API only tests call

`AgentSource::from_pair`, `PersistedAgentSession::from_report`,
`AgentResumePlan::for_command`, `AgentResumePlan::args`,
`TerminalState::plan_agent_resume` (8.2). Evidence: every call site found by
search sits under `#[cfg(test)]` or in a test-support file.

### 9.2 The `PaneAgentSessionSnapshot` alias and the re-validation around it

`schema.rs` `pub type PaneAgentSessionSnapshot = shepr_agent::resume::PersistedAgentSession;`.
`restore.rs` `persisted_agent_session_from_snapshot` rebuilds a
`PersistedAgentSession` from the fields of a `PersistedAgentSession` through
the validating constructor, which cannot fail for a decoded value;
`restored_terminal_agent_session` and `restore_plan_for_snapshot` wrap it with
`Option` plumbing that is never `None` for a present session. The alias is
the remnant of a once-separate snapshot type. Fold to `.cloned()`.

### 9.3 `resume::plan`'s `Option` (5.3)

### 9.4 The pane-gone abandonment path

`agent_resume.rs` `start_pending_agent_resume`'s `let Some(public_id) = ...
else` branch, `App::abandon_resume`, and `ResumeUnavailableReason::PaneGone`
("the pane no longer exists"). Candidates are collected from the same state
moments before in the same pass, with no mutation in between, so the branch
cannot run; if it did, the reason would be recorded on a pane that does not
exist (5.4). Delete all three.

### 9.5 Small leftovers

- `AGENT_RESUME_DETECTION_HOLD`, a private alias whose only reader is
  `AGENT_ABSENCE_STARTUP_HOLD` (D7).
- `agent_resume.rs` `derived_pending_agent_resume_pane_infos`, a one-line free
  function with one caller; `resume_candidate` returns a `&TerminalState` both
  callers discard (`Some((_, plan, cwd))`).
- `restore.rs` `AgentRestoreState` / `PaneRestoreStartup` /
  `RestorePlanContext` thread one bool (`resume_agents_on_restore`) through
  four layers; `Option<&mut HashSet<..>>` (none when disabled) says the same.
- `terminal/state/sessions.rs` holds only a test seam
  (`seed_hook_authority_for_test`); the file name suggests session logic.
- `ResumeOutcome::replaced_runtimes`: a candidate has no runtime by
  definition, so the launch installs one rather than replacing it.
- `restore_error` is the field for every start failure, fresh launches
  included (`PaneStartFailure`'s own doc: "whether newly opened or restored").
- `RestoreLoss::Panes` and `panes_pruned` have no honest producer (D1).
- The NUL-joined identity strings in resume tests (6.5).
- `start_pending_agent_resumes` marks the session dirty when a pass only
  launched (the plan stays until settlement, and settlement marks it dirty
  again), costing an extra save per resumed agent.

---

## Lateral findings

### L1. The checkpoint's "core intact" gate may be a pane-history leftover

`app/events.rs` `decide_pane_exit` justifies gating the exit checkpoint on an
intact terminal core with "a core that broke ... has nothing new to give a
checkpoint ... since history capture leaves an unreadable terminal's cached
history as it was". `PaneEnding::needs_checkpoint` (`pane/exit_arbiter.rs`)
returns false whenever the core is broken. With `pane_history` removed, a
checkpoint saves layout, cwd (read from `/proc`) and the agent identity (from
`AgentOwnership`), none of which come from the terminal core. If that holds,
the gate now only does harm: a pane whose reader panicked and is then
signalled at logout is removed without the checkpoint that would have kept
its agent session for resume. Needs the owner's confirmation of intent;
AGENTS.md also describes `PaneEnding` as carrying "whether its terminal core
is intact".

### L2. Server config docs still describe removed border settings

After `0810c21e` removed `pane_borders` and `pane_outer_borders`:

- `default-server.toml` header: "whether panes have borders, gaps and
  scrollbars";
- `docs/config.md` line for `server.toml`: "pane borders, gaps and scrollbars
  and their colours". Colours are the client's, per AGENTS.md;
- AGENTS.md: "whether panes have borders, gaps and scrollbars".

### L3. Resume success is never confirmed

After the command is typed (`send_resume_command` `Ok`), the plan is cleared
and the only trace of the resume is the seeded idle agent, which the detector
withdraws after `AGENT_ABSENCE_STARTUP_HOLD` (30 s) if no agent appears. A
shell rc that discards typeahead, an agent that rejects the session, or a
missing executable all end with the agent gone from the sidebar and nothing
logged or shown beyond what the shell printed. Together with 4.1 this is the
main reason a lost resume is hard to diagnose. A cheap improvement: when the
absence hold expires without the agent appearing in an `AgentResume` pane, log
it with the session reference and the pane's public id.

### L4. Duplicate detection keys on the exact reference kind

`AgentResumeKey` is the whole `PersistedAgentSession`, so for Pi and Omp
(`SessionRefPolicy::IdOrPath`) one session saved as an id in one pane and as a
path in another is not detected as a duplicate, and both panes resume it. Rare,
since a report prefers the path when both are present.

### L5. Per-iteration walks while resumes pend

`start_pending_agent_resumes` runs on every loop iteration and calls
`has_pending_agent_resumes()` up to three times, each walking every pane
record, and `has_pending_agent_resume_candidates` builds a `Vec<PaneContent>`
per workspace with a pending pane. It stops once the schedule retires, so the
cost is bounded to the startup window; worth one walk per pass if 2.7 is
fixed.
