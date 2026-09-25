I found 16 defects in the wire, framing and byte-edge scope. I only read code (Read and LSP); nothing was built or run. The two biggest problems are structural. First, the version check cannot do what it claims. Second, most of the client-shell traffic runs through a JSON/base64 tunnel with capability negotiation, not through the positional codec.

## High severity

**1. `PROTOCOL_VERSION` cannot turn a build mismatch into a clear error** (`protocol/wire.rs:6-21`, `client/handshake.rs`, `server/client_transport.rs:589-686`)
- **Claim:** "`PROTOCOL_VERSION` only turns an accidental mismatch into a clear handshake error."
- **What happens:**
  - The version sits inside a positional enum variant: `ClientMessage::TerminalHello` (index 0), or JSON inside `ClientMessage::EndpointControl` (index 12). The peer must already agree on the enum layout before it can read the version.
  - If the variant order or a field differs, the server's `read_message` fails. It logs `debug!("failed to read client hello")` and closes the socket. The client only sees EOF, or it misreads the Welcome.
  - The constant is a hand-bumped `1`. Wire types change constantly in this young fork, so two builds can both say `1` with different layouts. The hello then decodes fine and every later frame is misaligned.
  - This matters most over SSH, where the owner copies binaries between hosts by hand.
- **Fix:** send a raw preamble before any codec payload: magic plus a build/wire fingerprint (for example git sha, or a build.rs hash of the wire types). Compare it byte for byte.

**2. Pastes over the 2 MB frame cap kill the connection; the graceful rejection only covers 1-2 MB** (`client/endpoint/writer.rs:124-149`, `client/shell/input.rs:153-164`, `server/client_transport.rs:870-890, 1002-1014`)
- **Claims:** `MAX_FRAME_SIZE` is a cap "in either direction". `ServerEvent::ClientPasteRejected` exists for oversized pastes.
- **What happens:**
  - The client puts the whole `ClientPaneInputEvent::Paste(text)` into one frame. `NativeEndpointTransport::send` never checks the size against `MAX_FRAME_SIZE`; its queue allows up to 4 MB.
  - The server's `read_message` returns `Oversized` and disconnects before `pane_input_event_limit` runs.
  - So a 2-4 MB paste tears the endpoint down with no message. Above 4 MB it fails locally with "output queue is full".
  - The same missing sender-side cap applies to `ClientMessage::Input` on the terminal-attach path.

**3. Accepted geometry produces frames that can never be sent** (`server/client_transport.rs:44-45`, `server/headless/render.rs:625-636`)
- **Claim:** `MAX_CLIENT_SHELL_CELLS = 1_000_000` is called the "safe geometry limit".
- **What happens:**
  - A full `PaneSurface` costs about 6-13 bytes per cell (colours are 4-byte varints), so the 2 MB cap is reached at roughly 150k-300k cells.
  - Anything bigger hits `warn!("skipping oversized frame for client"); continue`. Nothing is committed or deferred, so dedupe never matches. Every render tick re-renders and re-encodes the full frame and drops it again, while the client stays blank.
  - `TerminalHello` / `Resize` (terminal attach) have no upper bound at all, only `clamp_terminal_size` with minimum 1. A u16×u16 size reaches `TestBackend::new(w, h)` in `render_terminal_virtual`, which can allocate billions of cells in the server that owns every session.
  - Server-side `frame_server_message` also silently drops any oversized control message (for example an OSC 52 `Clipboard` over about 1.5 MB, or a large snapshot) with only a warn.

**4. The JSON/base64 tunnel breaks "payloads use the positional codec"** (`protocol/endpoint.rs`, `surface_reuse.rs:15-38`, `surface_delta.rs:97-149`)
- **Claim:** the stated contract is "payloads use the positional codec" and wire types must not use `skip_serializing_if`.
- **What happens:**
  - The shell handshake, snapshot, agent completions, health ping, presentation sync and every endpoint request/response are JSON strings inside `EndpointControl { kind: String, data: String }`. The kind strings have `.v1` suffixes.
  - `EndpointServerWelcome.error` has `#[serde(skip_serializing_if)]`.
  - Surface reuse sends `PaneSurfaceFrame` metadata as JSON.
  - Surface delta encodes positionally, then base64s it (+33%), then puts it in a `String` that is encoded positionally again.
  - On the hot render path, `surface_delta::message` does a full `encoded_len(full)` pass, then `encoded_len(delta)`, `to_vec`, base64 and `encoded_len(message)`. `frame_server_message` then encodes it once more. That is several whole-frame passes per client per render.
- **Fix:** make SurfaceDelta, SurfaceReuse, Snapshot, Hello/Welcome and so on typed `ServerMessage`/`ClientMessage` variants.

**5. Capability and method negotiation is upstream compatibility machinery** (`endpoint.rs:80-94`, `client/handshake.rs:97-98,167`, `client/mod.rs:455-458`, `endpoint/supervisor.rs:277-294`)
- **Claim:** "no wire compatibility obligations; client and server are always the same build."
- **What happens:**
  - The client always sends `surface_reuse: true, surface_delta: true`, and the server always advertises every capability.
  - The supervisor's check "this machine needs a server update before it can participate in multi-machine viewing" and the `handshake.encoding != SemanticFrame` check (always SemanticFrame, hard-coded at `handshake.rs:167`) are dead.
  - The two sides key on different things. The client's decoder depends on the server's advertised capabilities; the server's encoding depends on the client's hello flags. If they ever diverged, delta frames would arrive at a client with no decoder and be dropped as unknown `EndpointControl`.
- **Recommendation:** delete the negotiation, the `.v1` kind strings and the method lists.

## Medium

**6. Underline styles are lost whenever the client composes an overlay** (`wire.rs:1073-1075`, `client/shell/composition.rs:346,418,448,482`)
- `CellData.modifier` carries the underline style in bits 12-15 (`modifier_with_underline_style` uses `from_bits_retain`).
- `to_ratatui_buffer` uses `u16_to_modifier`, which masks those bits off with `from_bits_truncate(val & !UNDERLINE_STYLE_MASK)`.
- Composition round-trips the frame through this whenever there is a selection, copy search, copy cursor, config diagnostic, lifecycle banner, notice or overlay. Curly, dotted and dashed underlines then become plain.
- A persistent config diagnostic makes this last the whole session.
- Each round trip also rebuilds every cell's `String` plus a hyperlink HashMap, per frame.

**7. The handshake timeout is not a deadline** (`client_transport.rs:36-40,581-589`, `client/handshake.rs:119-129`)
- **Claim:** the comment says it guarantees "the connection is closed within the 5-second deadline".
- **What happens:** `set_recv_timeout` is `SO_RCVTIMEO`, which resets on every read. A peer trickling one byte every 3 s holds the handshake thread, and its buffer of up to 2 MB, indefinitely. The client-side 5 s/60 s timeouts behave the same way.

**8. Linux-only violation, and wire tests that pass for the wrong reason** (`wire.rs:135,284-320`, `input/model.rs:34-60`)
- **Claim:** "Linux only. No `#[cfg(windows)]` or `cfg!` branches for other platforms."
- **What happens:**
  - `ClientPaneInputEvent::Key` carries `windows_record` and `physical_key_id`.
  - `to_raw_input_event` uses `cfg!(any(windows, test))`, and `KeyIdentity`/`KeySource` gain variants under `cfg(any(windows, test))`.
  - So test builds run code that production Linux never compiles. `client_shell_pane_input_roundtrips_semantic_and_windows_keys` and the dead-key tests exercise dead paths.
  - On Linux these wire fields are always None.

**9. The `ipc` socket-close probe consumes data** (`ipc.rs:122-141`)
- **Claim:** the name `local_stream_peer_closed`.
- **What happens:**
  - `probe_stream_closed` returns `Ok(true)` ("closed") when a byte is readable, and it eats that byte.
  - It also forces the stream back to blocking whatever mode it was in before.
  - Its only caller is the API subscription loop (`api/server.rs:693`), where it currently reads as "any input means stop". But the name and behaviour disagree, and any reuse on a framed stream would desync the framing.

**10. The TerminalHello path skips validation the shell path has** (`client_transport.rs:612-632,929-945`)
- `client_shell_geometry_error` (dimension, cell-count and pixel-size limits) applies only to the endpoint hello and `ClientShellResize`.
- `TerminalHello` and `Resize` pass `cols`, `rows` and `cell_*_px` straight through. This is the same gap as in finding 3.

## Low

**11. Production `expect`/indexing panics**
- **Claim:** "No `unwrap()` in production code."
- **Where:** `render_stream.rs:256` (`"prepared patch baseline"`), `render_stream.rs:296` (`"planned patch pane"`), `render_stream.rs:289` (unchecked slice index), `wire.rs:627` (`"cell within bounds"`), `surface_delta.rs:37` (`apply_rows` slice), `render_stream.rs:435,440`.

**12. Dead wire fields and dead code**
- `TerminalFrame.seq/width/height/full`: the client only writes `frame.bytes` (`client/mod.rs:969-973`).
- The `ClientWriterQueueState.ordered` lane is only popped and cleared, never pushed (`client_transport.rs:244`).
- The WouldBlock arm in `server_reader_thread` (`client/transport.rs:89`) is unreachable because `EndpointReader` absorbs WouldBlock.
- `ipc::bind_private_local_listener` is identical to `bind_local_listener` despite its "private" doc.

**13. `write_message_rejects_oversized_payload` tests nothing** (`wire.rs:2299`)
- It only checks that `Detach` encodes.
- `write_message`'s own Oversized check is at `u32::MAX`, not `MAX_FRAME_SIZE`.

**14. The hand-written surface-delta decoder duplicates the struct layouts** (`surface_delta/decode.rs:87-136`)
- It hard-codes the field order of `PaneSurfaceFrame`, `FrameData` and `PaneSurfaceSplit`.
- Adding or reordering a field in `wire.rs` silently breaks it. Only the layout test guards it.
- `surface_reuse::Decoder` stores a `PaneSurface` baseline without checking `cells.len() == w*h`. It also silently drops the baseline on a mismatched `PaneSurfacePatch` and passes the patch through.

**15. The socket permission window**
- `bind_local_listener` is followed by `restrict_socket_permissions` (`headless.rs:204-205`). The socket exists with umask-derived permissions between the two calls.
- There is no `SO_PEERCRED` check, so the file mode is the only access control.

**16. Stale docs in `render_ansi.rs`**
- The module doc lists OSC 52 emission, which the module does not do.
- It describes a Windows-skips IME repeat, but `repeat_ime_anchor_after_sync()` always returns `true`.

## Performance traps
- Every `Vec<u8>` on the wire (`Input.data`, `TerminalFrame.bytes`, response chunks) decodes one byte at a time through serde's SeqAccess because there is no bytes newtype. That is up to 2M visitor calls per frame.
- The API server reads request lines one byte per syscall (`api/server.rs:529-560`).

## Codec itself (`codec.rs`)
I found no defects here. Varint overflow, overlong and zigzag handling, length-versus-input bounds, depth limits, trailing-byte rejection and skip-field rejection all look correct. The Option and enum layouts match the module doc.

## Recommended rewrite
1. Put a raw magic plus build-fingerprint preamble on the connection.
2. Make every message a typed variant: no `EndpointControl`, JSON or base64 layers, and no capability or method negotiation.
3. Enforce one frame cap on both senders, and chunk pastes and clipboard data.
4. Tie the server's geometry limits to what one frame can carry. Otherwise, split full surfaces into multiple frames.
5. Use a bytes newtype for the `Vec<u8>` fields.
