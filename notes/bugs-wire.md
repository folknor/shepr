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

## WIRE-001 - Build identity is checked late, and not everywhere

Surfaced in four scopes: wire protocol, headless server, CLI/config, platform.

- `build.rs` now hashes `Cargo.toml`, `Cargo.lock`, `build.rs` and every file under `src/` into `crate::build_info::BUILD_ID`; `PROTOCOL_VERSION` and `version()` derive from it, so every existing compatibility check compares real builds. What remains:
  - **Raw preamble.** The version still rides inside a positional message (`ClientMessage::TerminalHello`, or JSON inside `EndpointControl`), so a peer whose layout for that message differs sees EOF, not a mismatch error. Write and check magic plus `BUILD_ID` before any codec payload, in the client connect path (`src/client/handshake.rs`), the server accept path (`src/server/client_transport.rs`) and possibly `src/remote/`.
  - **Skipped check.** `src/server/autodetect.rs` (`saved_federation`) skips `validate_running_server_compatibility` entirely when any saved SSH machine is enabled.
  - **Wording.** `src/cli/protocol_guard.rs` calls a mismatch "newer" or "older" by comparing numbers, which is meaningless for fingerprint-derived versions. It should say "different build" and give the restart guidance either way.

## WIRE-003 - Accepted geometry produces frames that can never be sent, and oversized frames freeze clients silently

Surfaced in two scopes: wire protocol, headless server.

- **Claim:** `MAX_CLIENT_SHELL_CELLS = 1_000_000` is called the "safe geometry limit" (`server/client_transport.rs`).
- **What happens:**
  - A full `PaneSurface` costs about 6-13 bytes per cell (colours are 4-byte varints), so the 2 MB cap is reached at roughly 150k-300k cells.
  - Anything bigger hits `warn!("skipping oversized frame for client"); continue` (`server/headless/render.rs`). Nothing is committed or deferred, so dedupe never matches. Every render tick re-renders and re-encodes the full frame and drops it again, while the client stays blank. A client whose surface always exceeds 2 MB (very large grid with many hyperlinks or long graphemes) never gets another frame.
  - `TerminalHello` / `Resize` (terminal attach) have no upper bound at all, only `clamp_terminal_size` with minimum 1. A u16×u16 size reaches `TestBackend::new(w, h)` in `render_terminal_virtual`, which can allocate billions of cells in the server that owns every session.
  - `client_shell_geometry_error` (dimension, cell-count and pixel-size limits) applies only to the endpoint hello and `ClientShellResize`. `TerminalHello` and `Resize` pass `cols`, `rows` and `cell_*_px` straight through.
  - Server-side `frame_server_message` also silently drops any oversized control message (for example an OSC 52 `Clipboard` over about 1.5 MB, or a large snapshot) with only a warn.
  - Client side: `write_message` refuses oversized frames locally, but any `EndpointTransport::send` error is recorded as an endpoint failure by `registry.rs`, so a non-paste oversized message still ends the endpoint (with a clear local error).
- **Suggested:** tie the server's geometry limits to what one frame can carry, or split full surfaces into multiple frames. Chunk clipboard data.

## WIRE-004 - The JSON/base64 tunnel breaks "payloads use the positional codec"

- **Claim:** the stated contract is "payloads use the positional codec" and wire types must not use `skip_serializing_if`.
- **Where:** `protocol/endpoint.rs`, `surface_reuse.rs`, `surface_delta.rs`.
- **What happens:**
  - The shell handshake, snapshot, agent completions, health ping, presentation sync and every endpoint request/response are JSON strings inside `EndpointControl { kind: String, data: String }`. The kind strings have `.v1` suffixes.
  - `EndpointServerWelcome.error` has `#[serde(skip_serializing_if)]`.
  - Surface reuse sends `PaneSurfaceFrame` metadata as JSON.
  - Surface delta encodes positionally, then base64s it (+33%), then puts it in a `String` that is encoded positionally again.
  - On the hot render path, `surface_delta::message` does a full `encoded_len(full)` pass, then `encoded_len(delta)`, `to_vec`, base64 and `encoded_len(message)`. `frame_server_message` then encodes it once more. That is several whole-frame passes per client per render.
  - Composition also rebuilds every cell's `String` plus a hyperlink HashMap per frame whenever it round-trips a frame through a ratatui buffer (`client/shell/composition.rs`).
- **Suggested fix:** make SurfaceDelta, SurfaceReuse, Snapshot, Hello/Welcome and so on typed `ServerMessage`/`ClientMessage` variants.

## WIRE-005 - Capability and method negotiation is upstream compatibility machinery

- **Claim:** "no wire compatibility obligations; client and server are always the same build."
- **Where:** `endpoint.rs`, `client/handshake.rs`, `client/mod.rs`, `endpoint/supervisor.rs`.
- **What happens:**
  - The client always sends `surface_reuse: true, surface_delta: true`, and the server always advertises every capability.
  - The supervisor's check "this machine needs a server update before it can participate in multi-machine viewing" and the `handshake.encoding != SemanticFrame` check (always SemanticFrame, hard-coded in `handshake.rs`) are dead.
  - The two sides key on different things. The client's decoder depends on the server's advertised capabilities; the server's encoding depends on the client's hello flags. If they ever diverged, delta frames would arrive at a client with no decoder and be dropped as unknown `EndpointControl`.
- **Recommendation:** delete the negotiation, the `.v1` kind strings and the method lists.

## WIRE-013 - Socket permission windows and no peer credential check

- `bind_local_listener` is followed by `restrict_socket_permissions` (`headless.rs`). The socket exists with umask-derived permissions between the two calls. `bind_private_local_listener` (SSH bridge sockets) chmods to 0600 itself, but also after the bind.
- There is no `SO_PEERCRED` check, so the file mode is the only access control.

## WIRE-017 - Direct attach and the shell client disagree on large pastes

- The shell client rejects a paste over `MAX_INPUT_PAYLOAD` (1 MiB) locally with a notice (`push_focused_paste`). Direct terminal attach (`src/client/attach.rs`) splits large input into frames under the cap and sends it all. One limit should apply to both.
