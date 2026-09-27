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

## API-001 - A timed-out API request still runs, but the caller is told the server is unavailable

`crates/shepr-api/src/server.rs`, `dispatch_to_app_result`. `handle_request` gives each request 60 s (`ORDINARY_REQUEST_TIMEOUT`). When that expires, the request is still sitting in `api_tx` and the app loop will run it later. The reply is `server_unavailable` / "request handling failed: timed out ...", which tells the caller nothing happened. For mutating methods (`workspace.create`, `pane.split`, `pane.send_text`, `agent.start`, `pane.close`...) the action can take effect after the CLI has reported failure, and a retry does it twice. The client's own doc (client.rs, `ORDINARY_RESPONSE_TIMEOUT`) calls this "the server's more specific `server_unavailable` answer". It is actually a timeout mislabelled as unavailability. `prompt_agent` avoids exactly this for plain prompts (see its comment), but ordinary methods don't. The fix is a real `Timeout` code whose meaning is "outcome unknown", or requests that carry a deadline and that the app drops once it passes.

## API-009 - PaneSurfacePatch is a serde(skip) enum variant in a positional codec

`ServerMessage::PaneSurfacePatch` is `#[serde(skip)]` on an enum variant. Variant indices stay stable only because it is the last variant; the positional contract depends on its position.

## API-012 - agent.prompt with wait and no timeout is unbounded end to end

`response_timeout` treats `agent.prompt` with `wait` but no `timeout_ms` as unbounded. That is consistent with the doc, but the app-side `submission_deadline` is then `None` too, so nothing bounds it.

## API-013 - An open events.subscribe stream outlives the start of shutdown

`stream_subscriptions` (`server.rs`) checks only `running`, not `server_stop`. New subscriptions are refused once shutdown starts, but a stream that was already open keeps running until the API handle drops.
