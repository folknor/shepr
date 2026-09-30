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
