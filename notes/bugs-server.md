# Defects: shepr-server

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

IDs SRV-001 through SRV-018 were used by an earlier edition of this file and are not reused.

## SRV-019 - A client shell request can move the session's default focus without a render

`handle_client_shell_api_request` ignores the return value of `set_default_shell_target_from_client`, which mutates app state through `switch_workspace_tab`. Shell projections are unaffected (per-client locations override the default), but the session's default focus can change without the loop asking for a render, so anything that reads the default target sees it late. Honour the return value.
