# Defects: shepr-client

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

IDs CLT-001 through CLT-006 were used by an earlier edition of this file and are not reused.

## CLT-007 - Switching to an endpoint whose config fails to decode keeps the previous endpoint's view

In `apply_active_snapshot` / `activate_endpoint_projection` (`shell/endpoints.rs`), when the newly active endpoint's config fails to decode, the error return leaves the previous endpoint's snapshot and config in place under the new active endpoint id. The screen then shows the old machine's workspaces labelled as the new one. Clear the active projection (or keep the old endpoint active) on that error so the id and the content agree.
