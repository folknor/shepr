# Defects: wire protocol, JSON API and configuration

Filed from the defect hunt over `crates/shepr-protocol`, `crates/shepr-api` and
`crates/shepr-config`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## WIRE-005 - Immediate endpoint refusals and the surface-set acknowledgement can overtake held replies

Scopes: protocol-api and server-serving-ui (both found it independently; the
serving hunter also cites `flush_endpoint_replies` "in the order the commands
ran", and offers dropping the ordering claim as the alternative if the client
matches replies only by request id).

**Claim broken.** `handle_client_shell_endpoint_request` doc
(`shepr-server/src/server/headless/endpoint_requests.rs`): "Commands from one
client run in arrival order on this loop, so a second command sent before the
first was answered simply runs after it, and the replies leave in the same
order."

Normal replies go to `endpoint_replies` and are flushed after the next render
(`flush_endpoint_replies`), which the render cadence can hold back across loop
iterations. `StaleBoot`, `SurfaceInactive` and the `ClientShellSurfaceSet`
acknowledgement go straight to the control lane with `send_to_client`, without
flushing the outbox. If command A is held and a later B is refused or is a
surface-set, B's reply reaches the client first.
`reject_endpoint_request_for_shutdown` flushes held replies first for exactly
this reason; the other immediate paths do not.

Limited impact today: the client matches responses by request id and serializes
its command lane. But an activation handoff that sends `surface.set` while a lane
command is in flight sees the replies reordered. Fix: route every reply through
the outbox (or flush before any immediate send).

## WIRE-006 - The client handles a shutdown notice in place of the welcome, but the server never sends one

Scope: protocol-api.

**Claim broken.** `do_handshake` (`shepr-client/src/handshake.rs`): "A server that
is going down answers the hello with its shutdown notice. That is a transient
condition to report as such, not a malformed welcome."

`handle_client_handshake` (`shepr-server/src/server/client_transport.rs`) either
returns without writing anything when `should_quit` is set (before the preamble,
or after reading the hello), or writes the welcome and then queues
`ServerShutdown` behind it (`send_shutdown_to_unregistered_client`, and the
`ClientShellConnected` arm of the stopping loop in `headless.rs`). No path writes
`ServerShutdown` in the welcome's place, so the branch is dead. A stopping server
gives a bare EOF during the preamble or welcome read (reported as
`UnexpectedEof`), or a welcome followed by a shutdown. Fix the doc, or have the
server answer a hello it will not serve with the shutdown notice.

## WIRE-007 - `ServerMessage::PaneSurfacePatch` is a client-only variant inside the wire enum, safe only while it stays last

Scope: protocol-api. Latent; no claim broken today.

serde_derive numbers a skipped variant differently on the two sides:
serialization uses each variant's declared position, deserialization numbers only
non-skipped variants (`deserialized_fields.iter().enumerate()` in
`serde_derive/src/de/identifier.rs`). `PaneSurfacePatch` is the last
`ServerMessage` variant, so nothing shifts; a variant added after it would encode
at index N+1 and decode as index N, a silent cross-variant decode within one
build. The comment ("Keep it skipped so framing it fails") does not warn about
this, in a type the codec module doc describes as positional ("enum: variant
index as varint").

Related dead code: `surface_reuse::Decoder::decode` has a second-stage
`ServerMessage::PaneSurfacePatch` arm that validates and applies a patch. No wire
message can produce that variant, and the decoder's own patch path returns early
with `return Ok(ServerMessage::PaneSurfacePatch(patch))`, so the arm is
unreachable.

**Recommendation (structural).** Split the enum: `ServerMessage` for exactly what
crosses the wire, and a client-side `DecodedServerMessage` (or a decoder-owned
enum) adding `PaneSurfacePatch`. That removes the skip, the unreachable arm, and
the `write_message` failure test that guards the skip.

## WIRE-023 - The build identity may not be recomputed when `CARGO_PROFILE_*` environment overrides change

Scope: protocol-api. Unverified; the hunter proposes one experiment.

**Claim broken (if confirmed).** Root `build.rs` module doc: the identity covers
"every `CARGO_PROFILE_*` override set through the environment ... Any change to
an input yields a new identity".

The script declares `rerun-if-changed` for the tree and `rerun-if-env-changed`
only for `OPTIONAL_PROFILE_VARS`. Once any rerun directive is emitted, Cargo
reruns the script only on those triggers. The comment argues that the variables
Cargo derives itself live in a per-profile, per-target output directory, which
holds for `PROFILE` and `TARGET`; but a `CARGO_PROFILE_RELEASE_LTO` or
`..._CODEGEN_UNITS` set in the environment neither changes the output directory
nor appears in a rerun directive. The script hashes those variables when it runs
but may not run again when they change, leaving a stale `BUILD_ID` on a binary
built differently.

Experiment: build, change a `CARGO_PROFILE_RELEASE_*` environment variable,
build, compare `shepr --version`. If confirmed, emit `rerun-if-env-changed` for
each `CARGO_PROFILE_*` and `CARGO_CFG_*` name the script saw (it enumerates them
anyway). Overrides appearing for the first time remain unobservable, and the doc
should say so.

## WIRE-024 - Smaller protocol and API observations

Scope: protocol-api (structural and lateral).

- **Two spellings of a split path on the wire.** `LayoutSetSplitRatioParams.path`
  is `Vec<bool>`, while `PaneSurfaceSplit.path` (what the client read the split
  from) is `Vec<SplitBranch>`; the server translates in
  `handle_layout_set_split_ratio`. `SplitBranch` in both removes a translation
  and a convention ("`true` descends into the second branch") that lives only in
  a comment.
- **`#[serde(default)]` on `PaneInfo.scroll`** (`command.rs`) has no meaning on a
  positional type (every field is always present) and suggests missing fields are
  tolerated. Drop it.
- **Copy motion and search claim shell geometry.** `EndpointCommandTraits` for
  `pane.copy_motion` and `pane.copy_search` set `claims_shell_geometry: true`
  with `mutates_ui: false`, so `handle_client_shell_command` runs
  `claim_shell_workspace_geometry` / `resize_shell_workspaces_sized_for` for a
  read-only copy-mode step, which can resize PTYs as a side effect of moving a
  copy cursor. If intended (copy mode claims the workspace), say so in the trait
  doc; otherwise set it `false`.
- **The client-status doc refers to an older client.** `ClientStatusJson.server`
  says "`None` from a client that predates the field". Remote discovery does read
  other builds' JSON, so the case can exist, but it contradicts "no compatibility
  with ... any older shepr". The status JSON is part of the cross-build surface in
  PLAT-031 and should be treated as such, or not relied on across builds.
- **`request_value_until` returns `Ok(())` without sending** when the deadline has
  already passed (`send_stop_request`'s first check); the following wait then
  reports `TimedOut` for a stop never sent. Unreachable with a 15 s budget;
  returning a timeout error there would be clearer.

## WIRE-026 - Config leftovers after config became launch-only

Scope: config (lateral, from the change that stopped config crossing hosts).

- **The default-template test no longer catches an undocumented key.** The old
  test serialized `Config::default()` and failed when any default leaf was
  missing from the commented `default.toml`. With `Serialize` gone from the
  model, the new `default_template_documented_values_match_defaults` only checks
  that documented values parse and equal the defaults, and it skips `[theme]`,
  `[theme.custom]`, `rows_by_agent` and machines. A new config key with no
  template entry now passes. Getting the coverage back needs a way to list
  `Config`'s fields without production `Serialize`: test-only
  `cfg_attr(test, derive(Serialize))` across the model types, or a hand-kept key
  list checked against the template. Decide whether the coverage is worth that.
- **`PathProvenance` and `ConfigSource` have no production reader.**
  `AppPaths::provenance()` is read only by shepr-config's own tests; nothing
  outside the crate reads either type, so their `Display` is unused too. Delete
  them, or keep them only if a diagnostic or `status` output is meant to show
  where paths came from.
- `parse_document` in `validated.rs` carries `#[cfg(test)]` on both its impl
  block and itself; the inner one is redundant.
