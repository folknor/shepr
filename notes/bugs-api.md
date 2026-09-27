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

`crates/shepr-api/src/server.rs`, `dispatch_to_app_result`. `handle_request` gives each request 60 s (`ORDINARY_REQUEST_TIMEOUT`). When that expires, the request is still sitting in `api_tx` and the app loop will run it later. The reply is `server_unavailable` / "request handling failed: timed out …", which tells the caller nothing happened. For mutating methods (`workspace.create`, `pane.split`, `pane.send_text`, `agent.start`, `pane.close`…) the action can take effect after the CLI has reported failure, and a retry does it twice. The client's own doc (client.rs, `ORDINARY_RESPONSE_TIMEOUT`) calls this "the server's more specific `server_unavailable` answer". It is actually a timeout mislabelled as unavailability. `prompt_agent` avoids exactly this for plain prompts (see its comment), but ordinary methods don't. The fix is a real `Timeout` code whose meaning is "outcome unknown", or requests that carry a deadline and that the app drops once it passes.

Related: SRV-004 (the pre-request internal-event drain can stall the app loop, making this timeout easier to hit).

## API-002 - The shutting-down guard skips every method handled on the socket thread

`server.rs` `handle_request`. Only requests that reach `handle_request` see the `server_stop` check. `agent.prompt` (including its no-wait path, which dispatches straight to the app), `agent.wait`, `events.wait`, `pane.wait_for_output`, `events.subscribe` and `server.ssh_agent.register` all bypass it. So a prompt can still be queued into a pane after `server.stop` was acknowledged, and new wait or subscribe loops start. The SSH-agent lease loop also watches only `running`, never `server_stop`.

(The server hunter reports the app-side rejection of API requests during shutdown as consistent; this entry concerns the socket-thread methods that never reach it.)

## API-007 - MAX_FRAME_SIZE is used as a field cap inside a frame capped at MAX_FRAME_SIZE

`message.rs` `TerminalFrame.bytes` and `ClientShellEndpointResponseChunk.data`, both bounded by `serialize_bounded_bytes::<MAX_FRAME_SIZE>`. A field at its stated limit can never fit, because the variant tag and length prefix push the payload over. The per-field cap should be `MAX_FRAME_SIZE` minus the envelope, or chunkers should size against a dedicated constant. As written, the cap promises something the frame cannot carry.

## API-008 - encode_frame builds the whole payload before checking the cap

`framing.rs`. The doc says oversized messages are refused "without writing anything". That holds for the socket, but the full oversized payload is still built in memory first, and `encoded_len` exists but isn't used for a pre-check. It only costs memory on the sender, but a pathological message pays the whole cost before being refused.

## API-009 - PaneSurfacePatch is a serde(skip) enum variant in a positional codec

`ServerMessage::PaneSurfacePatch` is `#[serde(skip)]` on an enum variant. Variant indices stay stable only because it is the last variant; the positional contract depends on its position.

## API-010 - EventHub poisoning restarts cursors from zero

`EventHub::push` silently drops events once its mutex is poisoned. `events_after_checked` reports that as Unavailable, but `current_sequence` returns 0, which would restart cursors from zero.

## API-012 - agent.prompt with wait and no timeout is unbounded end to end

`response_timeout` treats `agent.prompt` with `wait` but no `timeout_ms` as unbounded. That is consistent with the doc, but the app-side `submission_deadline` is then `None` too, so nothing bounds it.
