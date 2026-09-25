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
  - The only guard is `PROTOCOL_VERSION: u32 = 1` (`src/protocol/wire.rs:21`), a constant someone has to bump by hand. The repo says to change the protocol freely, so nobody will.
  - `src/build_info.rs` ("Build identity helpers") returns only `CARGO_PKG_VERSION`, which is `0.1.0` for every build.
  - The version sits inside a positional enum variant: `ClientMessage::TerminalHello` (index 0), or JSON inside `ClientMessage::EndpointControl` (index 12). The peer must already agree on the enum layout before it can read the version. If the variant order or a field differs, the server's `read_message` fails, logs `debug!("failed to read client hello")` and closes the socket. The client only sees EOF, or it misreads the Welcome.
  - Two builds can both say `1` with different layouts. The hello then decodes fine and every later frame is misaligned. The positional codec is not self-describing, so a mismatch decodes silently into garbage instead of failing.
  - After `brokkr install`, an old running server, or an old binary copied by hand to a remote host, passes every check: `server/autodetect.rs:92`, `remote/restart_policy.rs:16`, `remote/host.rs:42` and the handshake at `wire.rs:1206`. `protocol_guard` and `status`'s `restart_needed` / `server_binary_stale` report a different build as "compatible".
  - `autodetect.rs:232-236` (`saved_federation`) skips `validate_running_server_compatibility` entirely when any saved SSH machine is enabled.
  - This matters most over SSH, where the owner copies binaries between hosts by hand.
- **Fixes suggested by the hunters:**
  - Send a raw preamble before any codec payload: magic plus a build or wire fingerprint (git sha, or a build.rs hash of the wire types). Compare it byte for byte.
  - Embed a real build fingerprint (git sha plus a dirty/timestamp mark, or a hash of the wire schema) in `version()`, `ping` and the handshake. Drop the manual constant.

## WIRE-002 - Pastes over the 2 MB frame cap kill the connection

- **Claims:** `MAX_FRAME_SIZE` is a cap "in either direction". `ServerEvent::ClientPasteRejected` exists for oversized pastes.
- **Where:** `client/endpoint/writer.rs:124-149`, `client/shell/input.rs:153-164`, `server/client_transport.rs:870-890, 1002-1014`.
- **What happens:**
  - The client puts the whole `ClientPaneInputEvent::Paste(text)` into one frame. `NativeEndpointTransport::send` never checks the size against `MAX_FRAME_SIZE`; its queue allows up to 4 MB.
  - The server's `read_message` returns `Oversized` and disconnects before `pane_input_event_limit` runs.
  - So a 2-4 MB paste tears the endpoint down with no message. Above 4 MB it fails locally with "output queue is full". The graceful rejection only covers 1-2 MB.
  - The same missing sender-side cap applies to `ClientMessage::Input` on the terminal-attach path.

## WIRE-003 - Accepted geometry produces frames that can never be sent, and oversized frames freeze clients silently

Surfaced in two scopes: wire protocol, headless server.

- **Claim:** `MAX_CLIENT_SHELL_CELLS = 1_000_000` is called the "safe geometry limit" (`server/client_transport.rs:44-45`).
- **What happens:**
  - A full `PaneSurface` costs about 6-13 bytes per cell (colours are 4-byte varints), so the 2 MB cap is reached at roughly 150k-300k cells.
  - Anything bigger hits `warn!("skipping oversized frame for client"); continue` (`server/headless/render.rs:625-636`). Nothing is committed or deferred, so dedupe never matches. Every render tick re-renders and re-encodes the full frame and drops it again, while the client stays blank. The headless server hunter adds: no deferred render and no message to the client, so a client whose surface always exceeds 2 MB (very large grid with many hyperlinks or long graphemes) never gets another frame.
  - `TerminalHello` / `Resize` (terminal attach) have no upper bound at all, only `clamp_terminal_size` with minimum 1. A u16×u16 size reaches `TestBackend::new(w, h)` in `render_terminal_virtual`, which can allocate billions of cells in the server that owns every session.
  - `client_shell_geometry_error` (dimension, cell-count and pixel-size limits) applies only to the endpoint hello and `ClientShellResize`. `TerminalHello` and `Resize` pass `cols`, `rows` and `cell_*_px` straight through (`client_transport.rs:612-632,929-945`).
  - Server-side `frame_server_message` also silently drops any oversized control message (for example an OSC 52 `Clipboard` over about 1.5 MB, or a large snapshot) with only a warn.
- **Suggested:** tie the server's geometry limits to what one frame can carry, or split full surfaces into multiple frames. Enforce one frame cap on both senders and chunk pastes and clipboard data.

## WIRE-004 - The JSON/base64 tunnel breaks "payloads use the positional codec"

- **Claim:** the stated contract is "payloads use the positional codec" and wire types must not use `skip_serializing_if`.
- **Where:** `protocol/endpoint.rs`, `surface_reuse.rs:15-38`, `surface_delta.rs:97-149`.
- **What happens:**
  - The shell handshake, snapshot, agent completions, health ping, presentation sync and every endpoint request/response are JSON strings inside `EndpointControl { kind: String, data: String }`. The kind strings have `.v1` suffixes.
  - `EndpointServerWelcome.error` has `#[serde(skip_serializing_if)]`.
  - Surface reuse sends `PaneSurfaceFrame` metadata as JSON.
  - Surface delta encodes positionally, then base64s it (+33%), then puts it in a `String` that is encoded positionally again.
  - On the hot render path, `surface_delta::message` does a full `encoded_len(full)` pass, then `encoded_len(delta)`, `to_vec`, base64 and `encoded_len(message)`. `frame_server_message` then encodes it once more. That is several whole-frame passes per client per render.
- **Suggested fix:** make SurfaceDelta, SurfaceReuse, Snapshot, Hello/Welcome and so on typed `ServerMessage`/`ClientMessage` variants.

## WIRE-005 - Capability and method negotiation is upstream compatibility machinery

- **Claim:** "no wire compatibility obligations; client and server are always the same build."
- **Where:** `endpoint.rs:80-94`, `client/handshake.rs:97-98,167`, `client/mod.rs:455-458`, `endpoint/supervisor.rs:277-294`.
- **What happens:**
  - The client always sends `surface_reuse: true, surface_delta: true`, and the server always advertises every capability.
  - The supervisor's check "this machine needs a server update before it can participate in multi-machine viewing" and the `handshake.encoding != SemanticFrame` check (always SemanticFrame, hard-coded at `handshake.rs:167`) are dead.
  - The two sides key on different things. The client's decoder depends on the server's advertised capabilities; the server's encoding depends on the client's hello flags. If they ever diverged, delta frames would arrive at a client with no decoder and be dropped as unknown `EndpointControl`.
- **Recommendation:** delete the negotiation, the `.v1` kind strings and the method lists.

## WIRE-006 - Underline styles are lost whenever the client composes an overlay

- **Where:** `wire.rs:1073-1075`, `client/shell/composition.rs:346,418,448,482`.
- `CellData.modifier` carries the underline style in bits 12-15 (`modifier_with_underline_style` uses `from_bits_retain`).
- `to_ratatui_buffer` uses `u16_to_modifier`, which masks those bits off with `from_bits_truncate(val & !UNDERLINE_STYLE_MASK)`.
- Composition round-trips the frame through this whenever there is a selection, copy search, copy cursor, config diagnostic, lifecycle banner, notice or overlay. Curly, dotted and dashed underlines then become plain.
- A persistent config diagnostic makes this last the whole session.
- Each round trip also rebuilds every cell's `String` plus a hyperlink HashMap, per frame.

## WIRE-007 - The handshake timeout is not a deadline

- **Claim:** the comment says it guarantees "the connection is closed within the 5-second deadline".
- **Where:** `client_transport.rs:36-40,581-589`, `client/handshake.rs:119-129`.
- **What happens:** `set_recv_timeout` is `SO_RCVTIMEO`, which resets on every read. A peer trickling one byte every 3 s holds the handshake thread, and its buffer of up to 2 MB, indefinitely. The client-side 5 s/60 s timeouts behave the same way.

## WIRE-008 - The `ipc` socket-close probe consumes data

Surfaced in two scopes: wire protocol, JSON API.

- **Claim:** the name `local_stream_peer_closed`.
- **Where:** `ipc.rs:122-141`.
- **What happens:**
  - `probe_stream_closed` returns `Ok(true)` ("closed") when a byte is readable, and it eats that byte.
  - It also forces the stream back to blocking whatever mode it was in before.
  - Its only caller is the API subscription loop (`api/server.rs:693`), where it currently reads as "any input means stop". The name and behaviour disagree, and any reuse on a framed stream would desync the framing.
  - The JSON API hunter adds: a client that sends a trailing blank line gets its wait or subscription dropped silently, with no response.

## WIRE-009 - Production `expect`/indexing panics in the wire and render stream code

- **Claim:** "No `unwrap()` in production code."
- **Where:** `render_stream.rs:256` (`"prepared patch baseline"`), `render_stream.rs:296` (`"planned patch pane"`), `render_stream.rs:289` (unchecked slice index), `wire.rs:627` (`"cell within bounds"`), `surface_delta.rs:37` (`apply_rows` slice), `render_stream.rs:435,440`.

## WIRE-010 - Dead wire fields and dead code

Surfaced in two scopes: wire protocol, endpoints/SSH.

- `TerminalFrame.seq/width/height/full`: the client only writes `frame.bytes` (`client/mod.rs:969-973`).
- The `ClientWriterQueueState.ordered` lane is only popped and cleared, never pushed (`client_transport.rs:244`).
- The WouldBlock arm in `server_reader_thread` (`client/transport.rs:89`) is unreachable because `EndpointReader` absorbs WouldBlock (loops internally).
- `ipc::bind_private_local_listener` is identical to `bind_local_listener` despite its "private" doc.

## WIRE-011 - `write_message_rejects_oversized_payload` tests nothing

- **Where:** `wire.rs:2299`.
- It only checks that `Detach` encodes.
- `write_message`'s own Oversized check is at `u32::MAX`, not `MAX_FRAME_SIZE`.

## WIRE-012 - The hand-written surface-delta decoder duplicates the struct layouts

- **Where:** `surface_delta/decode.rs:87-136`.
- It hard-codes the field order of `PaneSurfaceFrame`, `FrameData` and `PaneSurfaceSplit`. Adding or reordering a field in `wire.rs` silently breaks it. Only the layout test guards it.
- `surface_reuse::Decoder` stores a `PaneSurface` baseline without checking `cells.len() == w*h`. It also silently drops the baseline on a mismatched `PaneSurfacePatch` and passes the patch through.

## WIRE-013 - Server socket permission window and no peer credential check

- `bind_local_listener` is followed by `restrict_socket_permissions` (`headless.rs:204-205`). The socket exists with umask-derived permissions between the two calls.
- There is no `SO_PEERCRED` check, so the file mode is the only access control.

## WIRE-014 - Stale docs in `render_ansi.rs`

- The module doc lists OSC 52 emission, which the module does not do.
- It describes a Windows-skips IME repeat, but `repeat_ime_anchor_after_sync()` always returns `true`.

## WIRE-015 - Every `Vec<u8>` on the wire decodes one byte at a time

- Every `Vec<u8>` on the wire (`Input.data`, `TerminalFrame.bytes`, response chunks) decodes one byte at a time through serde's SeqAccess because there is no bytes newtype. That is up to 2M visitor calls per frame.
- Suggested: a bytes newtype for the `Vec<u8>` fields.
