# Hygiene: diagnostics

Logging and operator-facing text (one channel, one implementation; levels,
identifiers, silence where something happened) and errors (swallowed, stripped of
context, or aborting where a refusal was owed). Filed from the nine-scope hunt;
each entry names the hunts that reported it and says how the fixed form could be
enforced.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

---

## DIAG-001 - The `event` / `subsystem` / `outcome` field convention is hand-written per crate and unevenly applied

Reported by: persistence, integrations, save-shutdown.

Each crate writes its own helper for the project's structured fields (persist,
pane, ipc, api, client, server, integration), and many lines skip them:

- `persist/capture.rs`: two `tracing::error!`s ("workspace focus or root has no pane
  record; not saved", "workspace layout and pane records disagree") have
  `workspace` but no `event` / `subsystem`.
- `persist/open.rs`: the partial-restore warn has `dropped_workspaces` and
  `restore_damage` but no `event`, `subsystem` or `path`; the `log_restore` info
  right after has all three.
- `writer.rs` states event literals "are the emitted log schema, so the names stay
  visible at the event site"; `recovery.rs` routes `persist.snapshot` /
  `persist.backup` through `RecoveryKind::event()` in two helpers and spells
  `"persist.snapshot"` inline three more times; `persist.restore` is spelled in
  `files.rs` and `open.rs`. No list of persistence events exists.

Fix: one shared macro or helper owning the field set, and one convention for event
names. Enforceable by a script check (multi-line, so not a single-line textlint)
requiring `event =` in every `tracing::(warn|error|info)!` under the persistence
and lifecycle modules.

## DIAG-004 - Resume outcomes have no channel; a resume that did not happen is mostly silent

Reported by: restore-resume.

Resume counts, suppressed duplicates and dispatches are now logged and reported.
Remaining: after the command is typed, success is never confirmed. When the absence
hold expires in an `AgentResume` pane without the agent appearing, nothing is logged;
the detector has no pane or session identity to log (a boundary comment in mux
`pane/detect/publish.rs` marks it), so the caller has to carry the session reference
and public pane id to that point.
