# Defects: client endpoints

Filed from the defect hunt over `crates/shepr-client/src/` outside `shell/`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## CEND-014 - Structural: "which endpoint" is held in six places

Outside the resolution loop: the owner resolves this directly. Do not assign it
or related bugs to fixers.

The registry's `active`, `Presentation` (`Owned` / `Handoff` / `Unavailable`),
the selection tracker's `selected` / `attempt` / `failed`,
`ClientState::deferred_local`, `ClientLoop::scheduled_activation`, and
`PendingEndpointActivation::successor`. `ClientLoop::run` re-derives agreement
every turn (`settle`, then `automatic_activation`), and
`begin_endpoint_activation`, `complete_endpoint_activation`,
`rollback_endpoint_activation` and `handle_endpoint_disconnect` each patch a
subset. A rollback that tore down a healthy target connection (since fixed)
and the rollback-leaves-surface-on finding (in
`notes/bugs-rejected-candidates.md`) are consequences. One owner for the endpoint
choice (a single enum covering selected, deferred, handing off from/to,
failed-on-generation) would remove most of these interactions.

## CEND-015 - Structural: the handoff protocol treats a server-side surface as an exclusive lease

Outside the resolution loop: the owner resolves this directly. Do not assign it
or related bugs to fixers.

The handoff protocol (source-off first, six phases, rollback through target-off
and source-on, successor intents, effects fence) exists to keep at most one
server-side surface on and pane input ordered. If surface activation were
idempotent per connection and the client simply chose which connection's frames
to draw and where to send input, with the server told only "viewing" vs "not
viewing" for its foreground and PTY size rules, the rollback paths and their
failure modes would disappear. The hunter suggests weighing this as a rewrite rather than
another round of patches to `activation.rs`.
