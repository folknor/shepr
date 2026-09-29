# Hygiene findings: values and their owners

This file collects the findings from the nine-scope hygiene hunt that answer the
hunt's first two questions: (1) values spelled at more than one site instead of
being defined once and read - environment variables and their resolution rules,
tunable constants and thresholds, ports, endpoints and addresses, filesystem
paths and roots, timeouts and limits, exit codes and status strings; and (2)
values nobody can find, change or trust - a knob defined once but where nobody
tuning the system would look, a value with no injection point, configuration read
at the moment of use rather than validated once at startup. Entries gather every
site and every hunter that reported the same value. It is a working document
assembled from nine independent readings, none of them verified by running the
build, so individual entries may be wrong; a later fix pass is expected to find
phantoms here.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

---

## HYGV-154 - Workspace ids still travel as strings in places

Pane, tab, terminal, workspace and boot ids now validate on construction.
Remaining string-typed workspace ids:

- The JSON API schema (`WorkspaceInfo`, `TabInfo`, `PaneInfo`, `AgentInfo`,
  `SessionSnapshot` in `shepr-api`) carries ids as `String`, so
  `shepr-server/src/client_shell.rs::snapshot_from_session` parses them back.
  Typing those fields keeps the JSON text but is an API surface choice;
  `shepr-api/src/schema/tests.rs` uses non-canonical `"w_1"` there.
- `shepr-client/src/shell/endpoints.rs`: `ClientEndpointFocusTarget::Workspace`
  holds a `String` while `Pane` is typed; `activation_tests.rs` builds it with
  `"old"` and `"new"`.
- `shepr-mux/src/workspace.rs`: `PaneRemoval.workspace_id` and
  `TabRemovalPlan.workspace_id` are `String` (the server fills the latter with
  `workspace.id.to_string()` and compares it back), and `App::public_workspace_id`,
  `public_tab_id` and `public_pane_id` return `String`.
- `shepr-mux/src/persist/restore.rs::restored_workspace_id` loops only because
  saved ids are reserved after restore; reserving them first removes the loop.
- The mux test `generated_workspace_ids_are_short_base32_handles` asserts
  `len <= 3`, which depends on how many ids other tests in the binary took from
  the global counter.
