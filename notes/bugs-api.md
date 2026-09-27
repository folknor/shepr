# Defects: shepr-protocol, shepr-api

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Hunter coverage: shepr-protocol `codec.rs`, `framing.rs`, `limits.rs`, `preamble.rs`, `message.rs`, `lib.rs`; shepr-api `lib.rs`, `server.rs`, `client.rs`, `schema.rs`, `event_hub.rs`, and the first ~420 lines of `wait.rs`. Not read: `subscriptions.rs`, `error.rs`, the rest of `wait.rs`, the schema submodules, and the protocol's surface/frame/endpoint modules. The hunter found the decode side of the codec sound (canonical varints, bounded lengths, item and depth caps, trailing bytes rejected).

## API-009 - PaneSurfacePatch is a serde(skip) enum variant in a positional codec

`ServerMessage::PaneSurfacePatch` is `#[serde(skip)]` on an enum variant. Variant indices stay stable only because it is the last variant; the positional contract depends on its position.
