# Defect hunt: shepr-protocol and shepr-api

Scope: `crates/shepr-protocol` (framing, codec, preamble and build identity,
endpoint commands, surfaces, deltas, reuse, input, ids) and `crates/shepr-api`
(JSON schema, client and server transport, status, `server stop` and the
conditional `--expect-boot` stop), followed into the server
(`shepr-server/src/server/`), the client (`shepr-client/src/`), the CLI
(`src/cli/`, `src/preflight.rs`) and the root `build.rs`.

Findings are ordered by how likely they are to bite. Each names the claim it
breaks.

---

## 1. A launch-valid `ui.mouse_scroll_lines` makes the server drop the client on the first scroll

**Claim broken:** "Config is read and validated once at launch ... Any config
problem fails the launch; no fallbacks" (AGENTS.md). A config that passes
validation must work.

- `shepr-config` accepts `ui.mouse_scroll_lines` from 1 up to
  `MAX_MOUSE_SCROLL_LINES = u16::MAX` (`shepr-config/src/limits.rs`,
  `validated.rs`).
- The client copies that value into every pane-forwarded mouse event
  (`ClientPaneInputEvent::Mouse { lines: self.config.mouse_scroll_lines, .. }`
  in `shepr-client/src/shell/input/mouse.rs` and `input.rs`).
- The server's `pane_input_event_limit` (`shepr-server/src/server/client_transport.rs`)
  counts a `ScrollUp`/`ScrollDown` event as `lines.max(1)` expanded events,
  and `classify_input_event_size` returns `TooManyEvents` once the batch
  passes `MAX_INPUT_EVENT_BATCH = 4096` (`shepr-server/src/limits.rs`). The
  read loop answers `TooManyEvents` by logging "oversized targeted pane input
  batch, closing", sending `ClientDisconnected` and breaking: the connection
  is dropped.

So `mouse_scroll_lines = 5000` disconnects the client on any wheel notch over
a pane that forwards mouse, and a value like 1500 disconnects on three notches
that land in one input batch. The client never learns of
`MAX_INPUT_EVENT_BATCH` at all (it is `pub(crate)` in `shepr-server`), so no
client-side check can exist.

Fix: one limit, stated once in `shepr-protocol`, used by the config validator
(cap `mouse_scroll_lines` to a value the batch rule can always carry), by the
client batcher (split a batch before it crosses the limit, as it already does
for paste bytes) and by the server. More structurally: a scroll of N lines is
one event, not N; charging it as N against a batch cap that disconnects is
the wrong shape. Either charge scroll by event and cap `lines` separately, or
turn an over-limit batch into a notice (as pastes do), never a disconnect,
for anything a same-build client can produce.

## 2. Keys and committed text joined onto a paste message push it past `MAX_INPUT_PAYLOAD`, and the server disconnects

**Claim broken:** `MAX_INPUT_PAYLOAD` doc (`shepr-protocol/src/limits.rs`): "The
server answers an oversized paste with a rejection notice rather than a
disconnect; clients check the same limit before sending so an oversized paste
never has to cross the wire." Also `push_focused_paste` doc: "Checking here
means an oversized paste never goes out".

- `push_focused_paste` (`shepr-client/src/shell/input/input.rs`) checks the
  paste against the limit and, if the pending message plus the paste would
  exceed it, starts a new message holding only the paste.
- Every other event goes through `push_target_event`
  (`shepr-client/src/shell/input/events.rs`), which appends to the last
  message for the same pane with no size or count check.
- So a paste close to 1 MiB followed in the same input batch by a key with
  generated text (Enter after the paste, or any keystroke in the same read)
  or a `TextCommit` lands in the paste's message. The server's
  `classify_input_event_size` sees `payload_bytes > MAX_INPUT_PAYLOAD` with
  `input_bytes > 0`, returns `InputPayloadTooLarge`, and closes the connection
  ("oversized targeted pane input, closing"), not the rejection notice.
- The same unchecked append also applies to the event-count cap from
  finding 1 (for example a non-bracketed paste of more than 4096 characters
  arriving as keys in one read).

Fix: the client batcher should enforce the whole server rule (payload bytes
and expanded-event count) for every event it appends, starting a new message
when the next event would cross either limit. The server can then treat
crossing either limit as a protocol violation. Today it disconnects on a case
the same-build client produces.

## 3. After a conditional stop succeeds, a new occupant is reported as a failed stop

**Claim broken:** AGENTS.md: "The stop names the boot identity that was
observed, so a server that replaced it in the meantime is not stopped, and is
offered again as a new occupant." Also the `stop_active_server` doc.

`stop_socket_with_timeout` (`shepr-api/src/server_stop.rs`) sends the stop,
then waits until both socket paths are dead and then until the data-directory
lease is free. Those waits look at paths, not at the boot that was stopped:

- If any server starts at the same address after boot A exits and before the
  wait finishes (for example another `shepr` launching, or a client's
  launcher), the sockets stay live, and the call returns
  `ServerStopError::TimedOut` after `STOP_WAIT_TIMEOUT`. If the sockets
  vanish and the new server takes the lease first, the call returns
  `LeaseHeld`.
- `restart_local` in `src/preflight.rs` maps both to
  `LocalRestart::Failed(...)`, and the operator reads "could not stop the local
  server: ... did not stop within 15000ms" or "... may still be saving its
  layout", when in fact the observed server stopped and a new one is running.
  The new occupant is neither identified nor offered again. The remote path
  (`remote server stop --expect-boot` over SSH) has the same wait, so a
  remote restart ends with exit status 1 instead of a specific status.

Fix: after a successful conditional stop, wait for *that boot* to go, not for
the paths to go. For example, poll `ping` and treat "no answer" or "answers
with another boot id" as "stopped", and report the other boot as a new
occupant (a distinct error or exit status). The lease wait has the same
problem and should be skipped or reinterpreted once another boot answers.

## 4. The lease-wait documentation and the `LeaseHeld` message describe a release order the server no longer uses

**Claim broken:** doc comment on `STOP_LEASE_WAIT_TIMEOUT`
(`shepr-api/src/limits.rs`): "The server closes its sockets first and releases
the lease after the shutdown drain has saved its layout, so a stop that
returned at the sockets would let a new server start into a held lease".
`ServerStopError::LeaseHeld` renders "closed its sockets but still held
<lease> ... it may still be saving its layout".

`release_sockets_after_save` in
`shepr-server/src/server/headless/lifecycle.rs` now releases the lease
*first*, then the sockets, and the test
`the_lease_is_free_by_the_time_the_client_socket_goes` enforces that order
(from the most recent commit). A gracefully stopping server therefore never
has "sockets gone, lease held". In practice `LeaseHeld` now means another
process took the lease (see finding 3), and the message sends the operator
the wrong way. Either reword both texts to the current order, or drop the
lease wait and replace it with the boot-based wait from finding 3.

## 5. Immediate endpoint refusals and the surface-set acknowledgement can overtake held replies

**Claim broken:** `handle_client_shell_endpoint_request` doc
(`shepr-server/src/server/headless/endpoint_requests.rs`): "Commands from one
client run in arrival order on this loop, so a second command sent before the
first was answered simply runs after it, and the replies leave in the same
order."

Normal command replies go to `endpoint_replies` and are flushed after the next
render (`flush_endpoint_replies`), and that render can be held back across
loop iterations by the render cadence. `StaleBoot`, `SurfaceInactive` and the
`ClientShellSurfaceSet` acknowledgement go straight to the control lane with
`send_to_client`, without flushing the outbox first. So if command A is held
and a later B is refused or is a surface-set, B's reply reaches the client
before A's. `reject_endpoint_request_for_shutdown` flushes held replies first
for exactly this reason. The other immediate paths do not.

Impact today is limited: the client matches responses by request id and
serializes its own command lane. Still, the documented ordering does not hold,
and an activation handoff that sends `surface.set` while a lane command is in
flight sees the replies reordered. Fix: route every reply through the outbox
(or flush it before any immediate send), which leaves one ordering rule.

## 6. The client handles a shutdown notice in place of the welcome, but the server never sends one

**Claim broken:** `do_handshake` (`shepr-client/src/handshake.rs`): "A server
that is going down answers the hello with its shutdown notice. That is a
transient condition to report as such, not a malformed welcome."

`handle_client_handshake` (`shepr-server/src/server/client_transport.rs`)
either returns without writing anything when `should_quit` is set (before the
preamble, or after reading the hello), or writes the welcome and then queues
`ServerShutdown` behind it (`send_shutdown_to_unregistered_client`, and the
`ClientShellConnected` arm of the stopping loop in `headless.rs`). No path
writes `ServerShutdown` in the welcome's place. The branch is dead. What the
client actually sees from a stopping server is a bare EOF during the preamble
or welcome read (reported as `UnexpectedEof`), or a welcome followed by a
shutdown. The doc should say so, or the server should answer a hello it will
not serve with the shutdown notice so that the client branch is live.

## 7. `ServerMessage::PaneSurfacePatch` is a client-only variant inside the wire enum, and its `#[serde(skip)]` is only safe while it stays last

**Claim broken:** none today. This is a latent layout trap in a type the repo
describes as positional (codec module doc: "enum: variant index as varint").

serde_derive gives a skipped variant different indices on the two sides.
Serialization uses each variant's declared position. Deserialization numbers
only the variants that are not skipped (`deserialized_fields.iter().enumerate()`
in `serde_derive/src/de/identifier.rs`). `PaneSurfacePatch` is the last
`ServerMessage` variant, so nothing shifts. A variant added after it would
encode at index N+1 and decode as index N, a silent cross-variant decode
within a single build. The comment on the variant ("Keep it skipped so
framing it fails") does not warn about this.

Related dead code: `surface_reuse::Decoder::decode` has a second-stage
`ServerMessage::PaneSurfacePatch` arm that validates and applies a patch
against the baseline. A wire message can never produce that variant (it is
skipped), and the decoder's own patch path returns early with `return
Ok(ServerMessage::PaneSurfacePatch(patch))`, so the arm is unreachable.

Recommendation (structural): split the enum. Use `ServerMessage` for exactly
what crosses the wire, and a client-side `DecodedServerMessage` (or have the
decoder return its own enum) that adds `PaneSurfacePatch`. That removes the
skip, the unreachable arm, and the `write_message` failure test that only
exists to guard the skip.

## 8. The frame reader accepts frames the documented format never produces

**Claim broken:** framing module doc (`shepr-protocol/src/framing.rs`): "A
larger one is cut into full `MAX_FRAME_SIZE` frames with the top bit set, then
one final frame without it". Also `MAX_CLIENT_MESSAGE_SIZE` doc: "Largest
message a server accepts from a client: one frame."

- `read_frames` accepts a continued frame of any length up to the cap,
  including zero. A peer can send an endless run of `0x80000000` prefixes, and
  the reader loops forever reading 4-byte headers with no progress and no
  error. There is no memory growth, but the reader thread spins on input the
  encoder can never emit.
- The server reads client messages with `read_message_limited(...,
  MAX_CLIENT_MESSAGE_SIZE)`, which sets `max_frame = min(MAX_FRAME_SIZE,
  max_message)` but still follows continuation bits. So a client message split
  over several frames that total no more than 2 MiB is accepted, contrary to
  "one frame". The same applies to `read_handshake_message`.

Fix: reject `continued && claimed_len != MAX_FRAME_SIZE` in `read_frames` (a
non-final frame must be full), and give the client-message and handshake
readers a single-frame reader that treats the continuation bit as a protocol
error.

## 9. A server whose paths are not UTF-8 launches fine, then fails every handshake with a bare EOF

**Claim broken:** "Any config problem fails the launch; no fallbacks." Also the
welcome contract ("each connection's handshake welcome carries the server's
config").

The welcome carries `ValidatedConfig`, whose wire form includes `AppPaths`
(`shepr-config/src/io.rs`, `#[derive(Serialize)]` over `PathBuf` fields,
`current_dir` and `home_dir` included). serde serializes a `PathBuf` with
`to_str()` and errors on non-UTF-8. `handle_client_handshake` maps that
encode error to `io::Error` and returns it. The client gets EOF and reports a
transient connection failure, then retries forever. The same happens for
anything else in the config that validates but does not encode.

Fix: encode the welcome config once at server startup (it is immutable for the
server's life) and fail the launch if that fails. That also saves
re-serializing the whole config on every accept.

## 10. The handshake bounds reads but not the welcome write

**Claim (weak):** `handle_client_handshake` bounds the preamble and hello by one
deadline (`HANDSHAKE_TIMEOUT`). The welcome that follows is written with a
blocking `write_message` on a stream that has no send timeout. The welcome is
the whole serialized config, far bigger than a socket buffer can hold for a
large config. A local peer that sends a valid preamble and hello and then
never reads pins that handshake thread indefinitely. The API socket sets
`STREAM_WRITE_TIMEOUT` for the same reason. The client socket's handshake
does not.

## 11. The build identity may not be recomputed when `CARGO_PROFILE_*` environment overrides change

**Claim broken (if confirmed):** root `build.rs` module doc: the identity
covers "every `CARGO_PROFILE_*` override set through the environment ... Any
change to an input yields a new identity".

The script declares `rerun-if-changed` for the tree and
`rerun-if-env-changed` only for `OPTIONAL_PROFILE_VARS`. Once any rerun
directive is emitted, Cargo reruns the script only on those triggers. The
comment argues that the variables Cargo derives itself live in a
per-profile, per-target output directory. That holds for `PROFILE` and
`TARGET`, but a `CARGO_PROFILE_RELEASE_LTO` or `..._CODEGEN_UNITS` set in the
environment neither changes the output directory nor appears in a rerun
directive. The script hashes those variables when it runs, but may not run
again when they change, which leaves a stale `BUILD_ID` on a binary built
differently. I did not verify Cargo's exact fingerprinting of the build-script
run unit. Worth one experiment: build, change a `CARGO_PROFILE_RELEASE_*`
environment variable, build, compare `shepr --version`. If confirmed, emit
`rerun-if-env-changed` for each `CARGO_PROFILE_*` and `CARGO_CFG_*` name the
script saw (it enumerates them anyway). Overrides that appear for the first
time remain unobservable, and the doc should say so.

---

## Structural and lateral observations

- **The cross-build JSON contract is not fixed the way the preamble is.** The
  preamble has a fixed layout so that "any two builds can read each other's
  preamble". The restart flow, however, also relies on JSON: `ping`'s
  `Pong { version, build_id, boot_id }` and `server.stop`'s
  `expected_boot_id` param, exchanged between *different* builds. The
  conditional stop is only safe if the other build understands the
  parameter. `ServerStopParams` is a plain serde struct, and serde ignores
  unknown fields, so a build that did not know `expected_boot_id` (renamed,
  removed) would stop unconditionally. That is precisely the "stop the
  replacement" outcome the flag exists to prevent. Every build today has the
  field, so this is latent. A sturdier design is a separate method name for
  the conditional stop (an unaware server answers `invalid_request` and stops
  nothing), plus a frozen test fixture for `ping` and `server.stop` as the
  one cross-build JSON surface.
- **Two spellings of a split path on the wire.**
  `LayoutSetSplitRatioParams.path` is `Vec<bool>`, while
  `PaneSurfaceSplit.path` (what the client read the split from) is
  `Vec<SplitBranch>`. The server translates `bool` to `SplitBranch` in
  `handle_layout_set_split_ratio`. Using `SplitBranch` in both removes a
  translation and a convention ("`true` descends into the second branch")
  that lives only in a comment.
- **`#[serde(default)]` on `PaneInfo.scroll`** (`command.rs`) has no meaning
  on a positional type (every field is always present) and suggests that
  missing fields are tolerated. Drop it.
- **`EndpointCommandTraits` for copy motion and search**
  (`pane.copy_motion`, `pane.copy_search`) set `claims_shell_geometry: true`
  with `mutates_ui: false`. `handle_client_shell_command` therefore runs
  `claim_shell_workspace_geometry` / `resize_shell_workspaces_sized_for` for a
  read-only copy-mode step, which can resize PTYs as a side effect of moving a
  copy cursor. If that is intended (copy mode as an interaction that claims the
  workspace), a sentence in the trait doc would help. Otherwise it should be
  `false`.
- **The client-status doc refers to an older client.** `ClientStatusJson.server`
  says "`None` from a client that predates the field". Remote discovery does
  read other builds' JSON, so the case can exist, but it contradicts "no
  compatibility with ... any older shepr" and the other docs that remove such
  phrasing. The whole status JSON is part of the cross-build surface from the
  first bullet and should be treated as such, or not relied on across builds.
- **`request_value_until` returns `Ok(())` without sending** when the deadline
  has already passed (`send_stop_request`'s first check). The following wait
  then reports `TimedOut` for a stop that was never sent. This is unreachable
  with a 15 s budget. Returning a timeout error there would be clearer.

## Checked and found sound

- Codec: canonical varint (overlong and overflow rejected at the u64 and u128
  boundaries), zigzag, bool and option tag validation, length prefixes checked
  against the remaining input before allocation, collection cap, depth
  accounting (struct, seq, option, enum and struct-variant each nest), trailing
  bytes rejected, and `skip_field` rejected on serialize.
- Framing encoder: exact frame boundaries for payloads that are 0, 1, exactly
  N×`MAX_FRAME_SIZE` and N×`MAX_FRAME_SIZE`+1 bytes. Nothing is retained past
  the cap. The reader never allocates more than one frame ahead of the bytes
  that arrived.
- Preamble: fixed layout, the unidentifiable build matches nothing (itself
  included), and a codec frame fails the magic check.
- Ids (`WorkspaceId`, `PublicPaneId`, `TerminalId`, `BootId`): parse and
  re-encode round trip, which refuses every non-canonical spelling, and
  deserialization goes through the same parse.
- Surface deltas and reuse: `apply_rows` validates every span before writing.
  The decoder validates rows and hyperlink indices before touching the
  baseline. The retained-patch path on the server falls back when a dirty
  row has a hyperlink, so patch cells never reference a link table the client
  lacks. Reuse is only emitted for identical frames, so dimensions match. The
  render lane is cleared together with `request_repaint`, so a discarded
  frame never leaves the server's baseline ahead of the client's.
- Endpoint responses are measured as a whole envelope before sending, so only
  responses past `MAX_MESSAGE_SIZE` become `ResponseTooLarge`. Copy search is
  capped at `MAX_RETURNED_MATCHES`, which keeps it under the codec
  collection cap.
- API server: peer-uid check, connection cap with a busy refuser that never
  blocks accept, bounded request line, write timeout, and `server.stop`
  answered on the socket thread with the boot comparison done by the server
  itself, in the same request.
- Every build comparison outside tests goes through `is_this_build`.
