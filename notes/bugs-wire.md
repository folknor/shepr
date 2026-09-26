# Wire protocol and framing defects

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
```

## WIRE-003 - A full surface must fit in one frame

- Geometry is bounded to what one frame can carry (`MAX_SURFACE_CELLS = MAX_FRAME_SIZE / 16`, `MAX_SURFACE_DIMENSION`, `MAX_CELL_SIZE_PX` in `src/protocol/wire.rs`) on both client and server. Cells with long graphemes or many hyperlinks can still exceed the 16-byte budget (reported once to shell clients). Splitting a full surface across frames, and chunking OSC 52 clipboard data, would remove the limit.

## WIRE-004 - The JSON/base64 tunnel breaks "payloads use the positional codec"

- **Claim:** the stated contract is "payloads use the positional codec" and wire types must not use `skip_serializing_if`.
- **Where:** `protocol/endpoint.rs`, `surface_reuse.rs`, `surface_delta.rs`.
- **What happens:**
  - The shell handshake, snapshot, agent completions, health ping, presentation sync and every endpoint request/response are JSON strings inside `EndpointControl { kind: String, data: String }`. The kind strings have `.v1` suffixes.
  - Surface reuse sends `PaneSurfaceFrame` metadata as JSON.
  - Surface delta encodes positionally, then base64s it (+33%), then puts it in a `String` that is encoded positionally again.
  - On the hot render path, `surface_delta::message` does a full `encoded_len(full)` pass, then `encoded_len(delta)`, `to_vec`, base64 and `encoded_len(message)`. `frame_server_message` then encodes it once more. That is several whole-frame passes per client per render.
  - Composition also rebuilds every cell's `String` plus a hyperlink HashMap per frame whenever it round-trips a frame through a ratatui buffer (`client/shell/composition.rs`).
- **Suggested fix:** make SurfaceDelta, SurfaceReuse, Snapshot, Hello/Welcome and so on typed `ServerMessage`/`ClientMessage` variants, and drop the `.v1` kind strings.

## WIRE-013 - Other sockets still have a permission window, and no peer credential check

- The headless server socket is owner-only from the moment it's reachable (`bind_owner_only_listener`). The JSON API socket (`src/api`) and `ipc::bind_private_local_listener` (SSH bridge sockets) still bind then chmod. Move the staging approach into `ipc.rs` and use it for all three.
- There is no `SO_PEERCRED` check, so the file mode is the only access control.

## WIRE-017 - Direct attach and the shell client disagree on large pastes

- The shell client rejects a paste over `MAX_INPUT_PAYLOAD` (1 MiB) locally with a notice (`push_focused_paste`). Direct terminal attach (`src/client/attach.rs`) splits large input into frames under the cap and sends it all. One limit should apply to both.
