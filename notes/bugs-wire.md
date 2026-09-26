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

## WIRE-001 - Build identity is not checked, so a mismatched build decodes silently

Surfaced in four scopes: wire protocol, headless server, CLI/config, platform.

- **Claim:** `protocol/wire.rs:6` says "Client and server are always the same build", and that `PROTOCOL_VERSION` "only turns an accidental mismatch into a clear handshake error". AGENTS.md says "no frozen fixtures".
- **What happens:**
  - The only guard is `PROTOCOL_VERSION: u32 = 1` (`src/protocol/wire.rs`), a constant someone has to bump by hand. The repo says to change the protocol freely, so nobody will.
  - `src/build_info.rs` ("Build identity helpers") returns only `CARGO_PKG_VERSION`, which is `0.1.0` for every build.
  - The version sits inside a positional enum variant: `ClientMessage::TerminalHello` (index 0), or JSON inside `ClientMessage::EndpointControl` (index 12). The peer must already agree on the enum layout before it can read the version. If the variant order or a field differs, the server's `read_message` fails, logs `debug!("failed to read client hello")` and closes the socket. The client only sees EOF, or it misreads the Welcome.
  - Two builds can both say `1` with different layouts. The hello then decodes fine and every later frame is misaligned. The positional codec is not self-describing, so a mismatch decodes silently into garbage instead of failing.
  - After `brokkr install`, an old running server, or an old binary copied by hand to a remote host, passes every check: `server/autodetect.rs:92`, `remote/restart_policy.rs:16`, `remote/host.rs:42` and the handshake in `wire.rs`. `protocol_guard` and `status`'s `restart_needed` / `server_binary_stale` report a different build as "compatible".
  - `autodetect.rs:232-236` (`saved_federation`) skips `validate_running_server_compatibility` entirely when any saved SSH machine is enabled.
  - This matters most over SSH, where the owner copies binaries between hosts by hand.
- **Fixes suggested by the hunters:**
  - Send a raw preamble before any codec payload: magic plus a build or wire fingerprint (git sha, or a build.rs hash of the wire types). Compare it byte for byte.
  - Embed a real build fingerprint (git sha plus a dirty/timestamp mark, or a hash of the wire schema) in `version()`, `ping` and the handshake. Drop the manual constant.

## WIRE-003 - Accepted geometry produces frames that can never be sent, and oversized frames freeze clients silently

Surfaced in two scopes: wire protocol, headless server.

- **Claim:** `MAX_CLIENT_SHELL_CELLS = 1_000_000` is called the "safe geometry limit" (`server/client_transport.rs`).
- **What happens:**
  - A full `PaneSurface` costs about 6-13 bytes per cell (colours are 4-byte varints), so the 2 MB cap is reached at roughly 150k-300k cells.
  - Anything bigger hits `warn!("skipping oversized frame for client"); continue` (`server/headless/render.rs`). Nothing is committed or deferred, so dedupe never matches. Every render tick re-renders and re-encodes the full frame and drops it again, while the client stays blank. No deferred render and no message to the client, so a client whose surface always exceeds 2 MB (very large grid with many hyperlinks or long graphemes) never gets another frame.
  - `TerminalHello` / `Resize` (terminal attach) have no upper bound at all, only `clamp_terminal_size` with minimum 1. A u16×u16 size reaches `TestBackend::new(w, h)` in `render_terminal_virtual`, which can allocate billions of cells in the server that owns every session.
  - `client_shell_geometry_error` (dimension, cell-count and pixel-size limits) applies only to the endpoint hello and `ClientShellResize`. `TerminalHello` and `Resize` pass `cols`, `rows` and `cell_*_px` straight through.
  - Server-side `frame_server_message` also silently drops any oversized control message (for example an OSC 52 `Clipboard` over about 1.5 MB, or a large snapshot) with only a warn.
  - Client side: `write_message` now refuses oversized frames locally, but any `EndpointTransport::send` error is recorded as an endpoint failure by `registry.rs`, so a non-paste oversized message still ends the endpoint (with a clear local error now).
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
- **Suggested fix:** make SurfaceDelta, SurfaceReuse, Snapshot, Hello/Welcome and so on typed `ServerMessage`/`ClientMessage` variants.

## WIRE-005 - Capability and method negotiation is upstream compatibility machinery

- **Claim:** "no wire compatibility obligations; client and server are always the same build."
- **Where:** `endpoint.rs:80-94`, `client/handshake.rs`, `client/mod.rs`, `endpoint/supervisor.rs`.
- **What happens:**
  - The client always sends `surface_reuse: true, surface_delta: true`, and the server always advertises every capability.
  - The supervisor's check "this machine needs a server update before it can participate in multi-machine viewing" and the `handshake.encoding != SemanticFrame` check (always SemanticFrame, hard-coded in `handshake.rs`) are dead.
  - The two sides key on different things. The client's decoder depends on the server's advertised capabilities; the server's encoding depends on the client's hello flags. If they ever diverged, delta frames would arrive at a client with no decoder and be dropped as unknown `EndpointControl`.
- **Recommendation:** delete the negotiation, the `.v1` kind strings and the method lists.

## WIRE-006 - Underline styles are lost whenever the client composes an overlay

- **Where:** `wire.rs` (`u16_to_modifier`), `client/shell/composition.rs`.
- `CellData.modifier` carries the underline style in bits 12-15 (`modifier_with_underline_style` uses `from_bits_retain`).
- `to_ratatui_buffer` uses `u16_to_modifier`, which masks those bits off with `from_bits_truncate(val & !UNDERLINE_STYLE_MASK)`.
- Composition round-trips the frame through this whenever there is a selection, copy search, copy cursor, config diagnostic, lifecycle banner, notice or overlay. Curly, dotted and dashed underlines then become plain.
- A persistent config diagnostic makes this last the whole session.
- Each round trip also rebuilds every cell's `String` plus a hyperlink HashMap, per frame.

## WIRE-008 - The `ipc` socket-close probe consumes data

Surfaced in two scopes: wire protocol, JSON API.

- **Claim:** the name `local_stream_peer_closed`.
- **Where:** `ipc.rs` (`probe_stream_closed`).
- **What happens:**
  - `probe_stream_closed` returns `Ok(true)` ("closed") when a byte is readable, and it eats that byte.
  - It also forces the stream back to blocking whatever mode it was in before.
  - Its only caller is the API subscription loop (`api/server.rs`), where it currently reads as "any input means stop". The name and behaviour disagree, and any reuse on a framed stream would desync the framing.
  - A client that sends a trailing blank line gets its wait or subscription dropped silently, with no response.

## WIRE-012 - The hand-written surface-delta decoder duplicates the struct layouts

- **Where:** `surface_delta/decode.rs`.
- It hard-codes the field order of `PaneSurfaceFrame`, `FrameData` and `PaneSurfaceSplit`. Adding or reordering a field in `wire.rs` silently breaks it. Only the layout test guards it.
- `surface_reuse::Decoder` stores a `PaneSurface` baseline without checking `cells.len() == w*h`. It also silently drops the baseline on a mismatched `PaneSurfacePatch` and passes the patch through.

## WIRE-013 - Socket permission windows and no peer credential check

- `bind_local_listener` is followed by `restrict_socket_permissions` (`headless.rs`). The socket exists with umask-derived permissions between the two calls. `bind_private_local_listener` (SSH bridge sockets) now chmods to 0600 itself, but also after the bind.
- There is no `SO_PEERCRED` check, so the file mode is the only access control.

## WIRE-014 - Stale docs in `render_ansi.rs`

- The module doc lists OSC 52 emission, which the module does not do.
- It describes a Windows-skips IME repeat, but `repeat_ime_anchor_after_sync()` always returns `true`.
