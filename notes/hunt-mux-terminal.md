# Hunt: crates/shepr-mux/src/terminal

Scope: `TerminalState` and `terminal/state/` (upstream-tracked), `title.rs`,
`read_snapshot.rs`, followed into the server readers (`app/actions/events.rs`,
`app/events.rs`, `app/agents.rs`, `app/api/detect.rs`, `app/agent_resume.rs`,
`persist/snapshot.rs`, `persist/restore.rs`) and into the pane runtime that
feeds it (`pane/runtime.rs` detection task, `pane/process_probe.rs`).

Read-only review. No test was run; each finding says how sure it is and gives a
test sketch that would confirm it.

---

## 1. One stray different-session report freezes a live full-lifecycle agent

Confidence: high (traced by hand; no test covers the follow-up report).

The claim broken: `terminal/state/mod.rs` header - "Full lifecycle Shepr hook
integrations are hook-authoritative while live". The state of a live pi, omp,
kimi, kilo or mastracode pane stops following its own hooks.

Mechanism, in `route_full_lifecycle_hook_report` (`hooks.rs`):

- Live authority for `shepr:pi` with session S1, pi process present.
- A `pane.report_agent` for `shepr:pi` arrives carrying a different session S2
  and a seq (a child or sibling pi process that inherited `SHEPR_PANE_ID`, or
  the "unexpected session" case `pi_non_replacement_reports_preserve_full_lifecycle_authority`
  already builds). `session_anchored` is false (S2 != S1), so the report falls to
  the "pending replacement" tail. That tail runs
  `suppressed_full_lifecycle_hook_reports.entry(source).or_insert_with(..)` with
  reason `ProcessExit`, stores S2 as pending, and returns `Ignore`.
- The next report for the live session S1 now fails the accept gate
  `process_present && session_anchored && !suppressed.contains_key(source)`,
  because the entry exists. It falls to the same tail, replaces the pending
  report, and is ignored. So is every later S1 report.
- The entry is only removed by `clear_full_lifecycle_hook_suppression_for_detected_agent`,
  which runs when the detected agent changes, or from the sequenced session-start
  branch in `sessions.rs`. Neither happens while the same pi keeps running.
  Detection updates cannot reach that function either, because
  `should_ignore_detected_state_under_full_lifecycle_hook` returns early while
  the authority is live. The pane runtime has stopped scanning the screen as
  well (`may_scan_screen` returns false while `full_lifecycle_authority_active`
  is set).

Result: the sidebar shows the last accepted state (often Working) until the
agent exits or starts a new session. Opencode alone is protected, by the
`opencode_cross_talk` early `Ignore`, which returns before the suppression
entry is inserted.

Test sketch: take `live_full_lifecycle_hook_rejects_different_session_ref_for_same_source`,
then send one more `shepr:pi` report for `one.jsonl` with seq 22 and state Idle.
Expect `Some(..)` and `state == Idle`. The trace above says it returns `None` and
the state stays Working.

The fix direction: while a process is present and the source's authority is
anchored to a different session, a mismatching report is cross-talk. Ignore it
(the way opencode's is ignored) and do not open a replacement generation.
A replacement generation should only open on process-exit evidence or on a
sequenced session-start report.

## 2. A suspended agent (Ctrl-Z) is treated as an exited one and loses its resume session for good

Confidence: high on the TerminalState side. The pane side is traced through
`foreground_shell_agent_action`, which returns `ReportProcessExit` whenever the
pane shell is back in the foreground, whatever the reason.

The claims broken: "Session restore ... and agent resume on restore" (AGENTS.md
Scope) and the hook-authority claim in finding 1.

When the agent is suspended the shell becomes the foreground job, so the
detector publishes `StateChanged { process_exited: true }`.
`set_detected_state_with_screen_signals_at` (`detection.rs`) then:

- sets `persisted_agent_session = None` for that agent,
- clears the hook authority,
- inserts a `ProcessExit` suppression holding the session ref.

On `fg` the probe reports `ReportReplacementProcess` and `AgentProcessDetected`
arrives. `clear_full_lifecycle_hook_suppression_for_detected_agent` keeps the
`ProcessExit` entry, because it has no `replacement_session_ref`. The agent is
the same process on the same session, and it sends no new SessionStart:

- Claude: resume is gone. Claude's hook only ever sends SessionStart, so the
  pane has no persisted session until `/clear` or a restart. A server restart
  brings back a bare shell.
- Codex: comes back on its next turn report, which carries the session id.
- pi, omp, kimi, kilo, mastracode: every report for the old session falls to the
  pending tail described in finding 1 (nothing is anchored any more) and is
  ignored. Hook authority does not come back, and neither does the resumable
  session. omp and mastracode have `screen_manifest: false`, so no screen
  fallback exists either. They show the process-exit fallback (Idle) for the
  rest of the process's life.

TerminalState cannot tell a suspend from an exit, because `process_exited` is a
bool. Either the probe has to tell them apart (the job's processes in state `T`
in `/proc/<pid>/stat`), or TerminalState needs a third outcome ("suspended":
drop live authority but keep the session and the generation) so it stops
treating the event as irreversible.

## 3. Idle and Unknown flips reorder the sidebar although both present as Idle

Confidence: high.

The claim broken: "Unknown presents as Idle" (AGENTS.md). The comment on
`record_agent_state_change_seq` says the seq exists "so endpoint agent sorting
can observe transitions".

`record_agent_state_change_seq` (`app/actions/events.rs`) compares raw
`AgentState`, so a change between Idle and Unknown bumps
`last_agent_state_change_seq`. The client uses that seq twice: as the secondary
sort key under Priority sort (`agent_sidebar.rs`), and as the change trigger for
recency (`shell/endpoints.rs`). A screen whose rules stop matching for a moment
(`manifest.rs` returns `Unknown` when no rule fires) moves the row to the top
with no visible change of state. So does the process re-detection
(`set_detected_agent_process_at` resets the fallback to Unknown). Compare
`presentation_state()` instead of the raw state.

## 4. `detect explain` credits the screen for a state that a hook decided

Confidence: high.

The claim broken: AGENTS.md - "`shepr detect explain <pane>` says which rule
decided its state".

`handle_detect_explain` (`app/api/detect.rs`) special-cases only
`full_lifecycle_hook_authority_active()`. For any other effective hook authority
it evaluates the screen rules and returns them as the explanation, although
`terminal.state` comes from `hook_authority.state` (`recompute_effective_state`)
unless a visible blocker overrides it. That covers shepr's own Codex turn hooks,
where `shepr:codex` is not full-lifecycle and not reserved, and any custom
source. With Codex the output routinely contradicts the sidebar: the hook says
Working, the screen rule says Idle. The explain should name the hook source when
`hook_authority` is effective and no visible blocker applies.

## 5. Custom hook reports are silently refused whenever the pane has a session identity

Confidence: high (traced).

The claim broken: `warn_unrecognized_hook_identity` says "Custom reports remain
usable".

In `set_hook_authority_at`, `current_session_owner_conflicts` returns true for
any custom source once `current_session_identity_for_persistence()` is `Some`.
`AgentSource::parse(custom)` is `Custom`, which never equals the stored official
source. Takeover then needs `session_ref`, and `session_ref_from_report` always
returns `None` for a custom source, so the report is dropped. Any Claude, Codex
or other official session recorded on the pane therefore disables every custom
state report for that pane. An unparseable custom label (for example
`"myagent"`) hits the same wall, through the `parse_canonical_label` failure
branch, which returns `true`. Either the owner check should apply only to reports
that carry a session identity, or the warning's promise should be dropped.

## 6. Mutating paths that return `None` leave the caller unaware

Confidence: medium. The paths are real; the reachable impact is small today.

`update_terminal_state` treats `None` as "nothing changed": no dirty mark, no
state-change seq. Several paths mutate and then return `None`:

- `set_agent_session_ref_for_typed_start_source_at` (`sessions.rs`) can clear
  `hook_authority` (the Codex replacement branch, `replaced_hook_session`, the
  foreground takeover) and remember stale sessions, and only then run
  `PersistedAgentSession::from_report(..)?`. If `from_report` fails, the
  effective state has changed but the caller hears `None`. Today the API only
  builds refs through `session_ref_from_report`, which respects the agent's
  policy, so this is latent. It relies on an invariant held in a different crate.
- Both entry points record the report's seq (`accept_hook_report_at`) before
  the known-agent, owner and conflicting-session rejections. A rejected report
  still advances the source's ordering. Harmless for well-behaved hooks, but it
  is a partial mutation on a rejected input.
- `abandon_agent_resume` discards its own mutation (`let _ =`). The label
  disappears without `record_agent_state_change_seq`. Given finding 3, skipping
  the bump happens to be right here.

Structural fix: make each entry point validate first and mutate second, so a
`None` means untouched.

## 7. The "full-lifecycle only" invariant on the suppression maps is not held

Confidence: high. Low impact today.

The field docs on `suppressed_full_lifecycle_hook_reports` and
`stale_full_lifecycle_hook_sessions` say only full-lifecycle source and label
pairs can enter them. The process-exit branch of
`set_detected_state_with_screen_signals_at` builds `official_session` with
`is_official_agent_source`, which is any official pair, and inserts a
`ProcessExit` suppression for Claude, Codex, Cursor and so on. Then
`set_hook_authority_at` (the `session_ref.is_some()` removal) moves those
entries into the stale-session map. The `ProcessExit` entry is never cleared for
those agents: `clear_full_lifecycle_hook_suppression_for_detected_agent` keeps
it, having no replacement. Meanwhile it gates `detected_state_observed_before_release_suppression`
and occupies a protected sequence slot. Either filter with
`full_lifecycle_hook_authority` or fix the docs. The first matches intent.

## 8. `RestoreFailure` promises API presentation that does not exist

Confidence: high.

`RestoreFailure`'s doc: "The pane surface and the API both present it". Only
`ui/panes.rs` reads `restore_error`. No API schema field carries it, and nothing
in `shepr-api` mentions restore. The `Display` impl has no caller, since
`restore.rs` logs it with `?reason`, which is Debug. Either expose it (pane info
and `status`) or correct the doc and delete `Display`.

## 9. Dead or misleading data on the hook path

Confidence: high. Smells, but each one states something untrue.

- `HookAuthority.message` is accepted from `pane.report_agent`, stored and
  cloned into pending reports, and never read by anything.
- `HookAuthority` derives `Serialize` and `Deserialize`, with a comment about
  "Decoding needs a fresh local observation time", but it is never serialized or
  decoded anywhere. The derives and the `#[serde(skip, default = "Instant::now")]`
  are dead, and the comment describes a path that does not exist.
- `EffectiveStateChange` computes and allocates two labels and two known agents
  on every mutation. The only reader (`record_agent_state_change_seq`) uses
  `previous_state` and `state`. `unchanged_effective_state_change()` builds a
  full struct whose only use is to have `previous_state == state`, so the seq
  code returns immediately, just to make `update_terminal_state` return
  `Released`. A `bool` would carry the same thing.
- `TerminalReadSnapshot` and `truncated` (`read_snapshot.rs`) plus the four
  `recent_*_snapshot` methods have no production reader apart from history
  persistence, which ignores `truncated`. They are leftovers of pane reads, which
  the project removed on purpose. (Pane scope; flagged because the type lives
  here.)

## 10. Every internal event forces a full shell projection and walks every pane

Confidence: high. A performance defect against "Hot paths multiply".

`App::handle_internal_event_with_render_demand` (`app/events.rs`) throws away
the `StateUpdate` that `handle_app_event` returns. For every event it then:

- calls `sync_full_lifecycle_authority_detection_pauses()`, which iterates every
  pane of every workspace;
- bumps `shell_projection_revision`;
- returns `RenderDemand::Full`.

This includes `StateChanged` events that TerminalState ignored (for example
detection under full-lifecycle authority) and `TerminalCwdReported` with an
unchanged cwd. Detection publishes run per pane per tick, so the cost is panes x
panes of atomic stores, plus a full projection rebuild and fan-out to every
client, whether or not anything changed. Use the returned `StateUpdate` (and
the mutation's flags) to decide. Sync the authority flag only for the terminal
that changed.

## 11. Two definitions of "the session to persist"

Confidence: medium (smell with a divergence risk).

`TerminalState::current_session_identity_for_persistence` (`hooks.rs`) decides
`session_ref_changed`, and so when the session is marked dirty. It uses
`from_report`: `AgentSource::parse` plus the `accepted_for` check.
`persist/snapshot.rs` `capture_workspace` decides what is written, and
re-implements the same logic from the public fields with `AgentSource::from_pair`
and no `accepted_for` check. They agree today only because `session_ref` is
built solely for official pairs with a policy-valid kind. Make the method public
and have the snapshot call it. Then the value that marks a save dirty is the
same value the save writes.

## 12. Clock mixing in the arbitration (note, not a proven defect)

Hook reports are stamped with the loop's per-iteration `clock_now`, sampled
before the drain that handles them. Detection events carry the runtime's tick
`now`, sampled before its `/proc` probe and screen read. Every "newer than"
decision (`newer_custom_authority`, `hook_authority_not_newer_than`,
`fallback_not_older_than_hook`, `detected_state_observed_before_release_suppression`)
compares the two. The windows are milliseconds wide, and the headless loop
drains queued internal events before API requests, which removes the worst
ordering. It is still an implicit contract, and nothing documents it.

The hook seq re-anchor rule (`report_seq_superseded`) is documented as
intended. The seqs are wall-clock nanoseconds (`time.time_ns()` in every shipped
hook), so the server could judge a straggler against its own wall clock instead
of "5 s since the last acceptance". Today a report delayed more than 5 s behind
a newer one is taken as a clock step and applied.

---

## Structural recommendation

`terminal/state/` is about 1,500 lines of arbitration spread over four maps
(`hook_report_sequences` and `hook_report_accepted_at`, the suppression map, the
stale-session map, plus `recent_agent_process_exit`). Their invariants live in
prose on the fields, and findings 1, 2, 6 and 7 all break those invariants. It
is also string-typed at the boundary. The API already canonicalizes labels, yet
every report re-parses `source` and `agent_label` many times
(`parse_agent_label` allocates through `normalized_agent_lookup_name`), and
agent-specific rules appear as literal pairs (`("shepr:codex", "codex")`,
`("shepr:opencode", "opencode")`, `("shepr:mastracode", "mastracode")`,
`("shepr:grok", "grok")`).

The payoff case for a rewrite:

- Parse once at the API edge into `enum ReportSource { Official(Agent), Custom { source, label } }`.
  Move the per-agent quirks into `AgentDescriptor` flags next to
  `full_lifecycle_hook_authority`.
- Model each official source as one explicit per-source generation state
  machine (`Live { session }`, `Suspended { session }`, `Exited { session, pending }`,
  `Cleared { session }`) instead of four maps whose keys must agree. That gives
  "same process, different session" (finding 1) and "suspended, not exited"
  (finding 2) a state of their own.
- Make every entry point validate first and mutate second (finding 6).

The cost is upstream tracking. `terminal/state/` is on the upstream-watch list,
and a rewrite turns future herdr fixes into manual re-derivations instead of
ports. That trade is the owner's to make. Findings 1 through 5 can be fixed in
place without it.
