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

## HYGV-154 - Identities that can still be minted unchecked

`PaneId`, `TerminalId` and `WorkspaceId` can now only be built through a
validating path. The same shape remains elsewhere:

- `shepr-protocol`'s `PublicChildId::new` and its parser accept any non-empty
  workspace segment; client and server tests rely on non-canonical segments
  (`"wOLD"`, `"ws_1:p1"`, `"old-workspace"`), including
  `shepr-client`'s `activation_tests.rs` and `composition.rs`.
- `BootId` has a public `From<&str>`, and `RequestId` public `From<String>` and
  `From<&str>`.
- `shepr-mux`'s `NEXT_WORKSPACE_ID` global is untouched.
- `shepr-server/src/client_shell.rs::snapshot_from_session` re-parses id
  strings the same process produced, because the API snapshot types are
  `String`-typed.
