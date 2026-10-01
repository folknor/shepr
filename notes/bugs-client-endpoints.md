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

## CEND-001 - "Session not fully restored" notices are dropped, mis-keyed and overwritten

Surfaced by the client endpoints hunt (the gate) and the client shell hunt (the
slot, the boot key and the reset).

Claims broken: the server sends `ClientShellError { SessionRestoreIncomplete }`
to every client right after the snapshot ("Every client is told, not only the
first: whoever looks at this server is looking at a session with holes",
`shepr-server/src/server/headless.rs`), and the client shell's
`receive_server_notice` says this kind "arrives on connect from whichever
machine restored, so it names that machine".

The gate. `PresentationGate::decide` (`endpoint/message_policy.rs`) applies
`ClientShellError` only through the final `_ if self.endpoint_active => Apply`
arm, and `endpoint_active` in `ClientLoop::handle_server_message` requires the
endpoint to be the registry's active one with `surface_active`. Every supervisor
connection (all configured machines, and Local after any reconnect) handshakes
with `surface_active = false` and is inserted with `insert_native(.., false, ..)`,
so when the notice arrives the endpoint is never active and the notice is
dropped. Only the very first Local connection at launch (handshaken with
`surface_active = true`) can show it. So after a remote host reboots and
restores with holes, or after the Local server restarts under a client with
machines configured, the user is never told. The only test is at shell level
(`shell/tests/presentation_regressions.rs`), which bypasses the gate.

The shell (`receive_server_notice`, `push_endpoint_notice` in
`shell/navigation/actions.rs`, and `reset_endpoint_projection` in
`shell/state.rs`):

- The server sends the notice "After the snapshot, so the client keys the notice
  to this boot". The client keys it with `self.snapshot`'s boot, the active
  endpoint's boot even when the notice came from another machine.
- There is one `visible_endpoint_notice` slot. At startup with several partially
  restored machines, each notice replaces the previous one, and all but the last
  are never seen.
- `reset_endpoint_projection` (every endpoint switch and every boot change)
  clears `visible_endpoint_notice`, so switching to the machine the notice is
  about erases it.

Fix direction (endpoints hunter): classify `ClientShellError` by kind in the
gate; restore notices are endpoint metadata (like `EndpointSnapshot`) and should
apply from any accepted connection, while the other kinds answer actions on the
active endpoint and can keep the current rule. Gating should be by what the
message is about, not its envelope. Better still, make the restore state a field
of `ClientShellSnapshot` rather than a one-shot message, so it cannot be lost to
ordering or gating and is re-delivered on every reconnect.

## CEND-004 - The disconnect notice text does not read as the documented sentence

Claim broken: `handoff_interrupted_notice`'s doc says `notice` is "the same
predicate the active-endpoint path shows after the label ("connection was lost;
reconnecting", "was removed or re-pointed"), so both read as one sentence about
the named machine", and its test asserts those strings.

The only production caller (`ClientLoop::handle_timer`) passes
`format!("{}; reconnecting", failure.message)`, where `failure.message` is a raw
`io::Error` string. From a reader that is `EndpointFramingError`, which embeds
the storage key, so the user sees "build endpoint ssh:build: server closed
connection; reconnecting", "Local endpoint local: server closed connection;
reconnecting", or "Local Broken pipe (os error 32); reconnecting". "was removed or
re-pointed" names a removed feature: machines are fixed at launch.

Fix direction: map failure kinds to fixed predicates for the UI (keep the raw
error for the log and the machine diagnostic) and drop the removed/re-pointed
wording.

## CEND-008 - Picking Local in Attention promises a reconnect that will not happen

`begin_endpoint_activation` defers a Local pick with "Local is reconnecting;
selection will resume when it is ready" whatever Local's state is, including
Attention (build mismatch, permission problem), where it will not become ready
without outside action.

## CEND-012 - The `Connected` repaint shows nothing new

`ClientLoop::handle_endpoint_supervisor` `Connected` composes and presents a
frame before changing any shell state (status is set Online only when the
snapshot arrives), so the "machine list" repaint it comments on shows nothing
new.

## CEND-014 - Structural: "which endpoint" is held in six places

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

The handoff protocol (source-off first, six phases, rollback through target-off
and source-on, successor intents, effects fence) exists to keep at most one
server-side surface on and pane input ordered. If surface activation were
idempotent per connection and the client simply chose which connection's frames
to draw and where to send input, with the server told only "viewing" vs "not
viewing" for its foreground and PTY size rules, the rollback paths and their
failure modes would disappear. The hunter suggests weighing this as a rewrite rather than
another round of patches to `activation.rs`.
