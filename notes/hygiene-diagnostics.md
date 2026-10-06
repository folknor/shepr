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

## DIAG-001 - Only persistence uses the shared structured log macro

Reported by: persistence, integrations, save-shutdown.

`shepr-platform`'s `structured_log` macro now owns `event`, `subsystem` and `outcome`
with `subsystem.operation` names, and mux persistence and `publish_file` use it. The
other crates (pane, ipc, api, client, server lifecycle, integration) still hand-write
their own helpers or skip the fields. Adopt the macro there, and add a script check
requiring `event =` in every `tracing::(warn|error|info)!` once adoption is complete.
