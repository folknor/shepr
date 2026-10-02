# Hunt: client-core

Scope: `crates/shepr-client` minus `src/shell/` (endpoint management, transport and
handshake, launch and terminal setup, the loop, input, host replies, clipboard,
reconcile, timers, geometry, errors, fatal panic, `shell.rs`, `shell_runtime.rs`,
`src/tests/`). Read in full; tests skimmed only for structure. Where a finding needs
the other side of a boundary (shell, protocol, remote, core) it was followed there.

Findings are grouped by the four questions, then a section of bugs and smells. Each
finding names the sites by function and file; line numbers are deliberately left out.

---

## Headline recommendations

The scope has three big structural problems, and most of the individual findings
below are symptoms of one of them.

1. **Failure classification travels as `io::ErrorKind`.** Whether an endpoint failure
   retries or needs attention is decided by picking an `ErrorKind` at the source
   (`handshake_error`, `preamble_error`, `hello_write_error`, the `NotFound` rewrite in
   `connect_once`, `queue_full`) so that `shepr_remote::SshFailureDiagnostic::from_error`
   reads it back on the far side of a crate boundary. A client-owned typed failure
   (`EndpointFailure { cause, class: Transient | NeedsRepair, phase }`) built at the
   source would replace five encoders and one decoder, and would let `endpoint_lost`
   keep the class it currently throws away.

2. **"Which endpoint is shown" and "what is its status" each have two owners.**
   `EndpointChoice` claims it is "the only owner" of what is shown, but
   `ClientShellState::active_endpoint_id` is a second copy that core code consults
   (`endpoint_is_active`). Endpoint status lives both in `ReconnectState` (supervisor)
   and `ClientShellEndpoint::status` (shell), and the shell copy is written from seven
   sites. One `Endpoints` owner (id, connection, generation, supervisor state, status,
   cached snapshot, shown/target role) with the shell reading a projection of it would
   remove both duplications.

3. **The loop's rendering is decided at every call site.** About eighteen sites call
   `state.shell.compose(cols, rows)` and then choose `present_chrome` or
   `present_frame`; a second "is this pane content allowed" check lives inside
   `ClientState`. A per-turn dirty mark (`Chrome` or `Pane`) with one present at the end
   of `handle_event` and one at the end of `reconcile` would remove the per-site choice,
   the duplicated composes in `finish_client_shell_input`, and the dead `frames_frozen`
   checks.

A rewrite of `lib.rs` around these three owners is justified; see section 3.

---

## 1. Axes that should be types

### 1.1 Connection generation is a bare `u64` everywhere

`shepr_protocol::ConnectionGeneration` exists, but every client API that carries a
generation takes `u64`: `EndpointRegistry::{insert, insert_native, new_at, accepts,
received, mark_ready}`, `EndpointTransportFailure::generation`,
`ClientLoopEvent::{ServerMessage, ServerDisconnected}`, `EndpointSupervisorEvent`,
`EndpointSupervisors::{add_local, return_connector, record_status, disconnected}`,
`EndpointCommands::{enqueue, receive_response}`, `EndpointCommandResult::generation`,
`ViewLease::generation`, `MoveStage::Failed { generation }`, `PendingStart::failed_generation`,
`Preparing::{receive_*}`, `start_endpoint_transport`, `spawn_endpoint_reader`,
`server_reader_thread`, and on the shell side `snapshot_generation: Option<u64>`,
`endpoint_snapshot_identity`, `endpoint_snapshot_matches`,
`receive_pane_surface_from`, `apply_pane_surface_patch_from`. Internally
`EndpointConnection`, `ReconnectState` and `RequestKey` hold the newtype and convert at
every edge (`.get()`, `.into()`, `Some(generation.into())`).

A generation is minted in exactly two places (see 2.7) and only ever compared for
equality. It should be a client-owned opaque type with no `From<u64>`, minted only by
one allocator, so a test or caller cannot invent one and a revision cannot be passed
where a generation is expected (several signatures take both as adjacent `u64`s, for
example `endpoint_snapshot_matches(id, generation, boot, revision)`).

### 1.2 Projection revisions unwrapped into `u64` inside the move protocol

`ViewLease::minimum_revision: u64`, `Preparing::floor: Option<u64>`,
`ViewEvidence::snapshot_revision: Option<u64>`, the `(BootId, u64)` tuple returned by
`ClientShellState::endpoint_snapshot_identity`, and the `revision: u64` argument of
`endpoint_snapshot_matches` are all `ProjectionRevision` values that were unwrapped with
`.get()`. `ViewEvidence::coherent_surface` compares
`self.snapshot_revision == Some(surface.projection_revision.get())`. On the wire,
`EndpointReply::ClientShellSurfaceSet { projection_revision: u64 }` is raw too, so the
floor the move depends on is untyped from the moment it arrives.

### 1.3 Request ids are strings with an encoding convention

The shell ledger mints `client-shell:{n}`; `endpoint::view::start_move` mints
`client-shell-view:{serial}:on`; `EndpointRegistry::release_unwanted_views` mints
`client-shell-view:{serial}:off`; `FocusLane::request` mints
`client-shell-focus:{view serial}:{n}` by parsing the view request back with
`view_request.split(':').nth(1).unwrap_or(view_request)`. Distinctness between these
families is held only by string prefixes. `ClientShellEndpointRequest::id` is `String`,
`EndpointCommandCancellation::{unsent, possibly_sent}` are `Vec<String>` (built with
`request_id.to_string()`), and `ClientShellState::drop_request` takes `&str`.

Model it as a client enum (`ClientRequest::{Shell(u64), ViewOn(u64), ViewOff(u64),
Focus { view: u64, n: u64 }}`) with one `to_wire()` and one `from_wire()`; the focus lane
then holds the view serial instead of reparsing it, and responses can be routed by
variant rather than by asking each consumer "is this mine".

### 1.4 Typed failures that end as prose

- `Preparing::rejection: Option<String>` (from `EndpointError::to_string()`, "surface
  activation returned an invalid acknowledgement", "endpoint focus returned an invalid
  acknowledgement", "unexpected focus response"), `FocusLane::receive -> Result<(),
  String>`, `view::commit_move -> Result<_, String>` ("endpoint move lost its coherent
  snapshot/surface pair", "endpoint projection is unavailable"). `reconcile` then wraps
  each into another `format!`. A `MoveFailure` enum (`Rejected(EndpointError)`,
  `BadAck`, `FocusMismatch`, `LostPair`, `ProjectionUnavailable`, `TimedOut`,
  `TargetLost(DisconnectKind)`) would let notices and logs branch, and make the
  deadline failure in `reconcile` one more variant rather than a separate closure.
- `EndpointTransportFailure { kind: io::ErrorKind, message: String }` is reconstructed
  into an `io::Error` for `ClientError::ConnectionLost` and into
  `SshFailureDiagnostic::from_message` in `endpoint_lost`, losing the source chain both
  times.
- `ClientExit { message: Option<String> }` and `ClientRunError::Launch(io::Error)`: the
  session outcome (`ServerShutdown`, `ConnectionLost`, panic, clean detach) is decided in
  `run_launched_client` and then flattened to text, and launch failures (settings env
  error, terminal setup, local connect, handshake) are all `io::Error`. The binary
  cannot branch on any of them today (it only prints), but the classification already
  exists and is discarded.

### 1.5 Bool-parameter axes

- `do_handshake(stream, geometry, mouse_capture: bool, surface_active: bool, deadline)`
  and `do_handshake_for_link(.., mouse_capture, surface_active, link_kind, ..)`.
  Launch passes `true` for `surface_active`, the supervisor `false`; nothing at the call
  site says which bool is which.
- `stdin_reader_loop` takes three independent `bool`s (`host_color_query_sent`,
  `host_cell_size_query_sent`, `host_escape_disambiguation_active`) plus two
  `Arc<AtomicBool>` mirrors; a `HostInputProbe` struct produced by terminal setup would
  carry them.
- `HostModes::apply_mouse(writer, exact_geometry: bool, reassert: bool)`,
  `HostModes::new(shell_preference: bool, initially_active: bool)`,
  `HostMouseMode::new(bool, bool)`, `set_pane_keyboard_report_all(writer, enabled,
  shell_requests_report_all)`.
- `EndpointRegistry::insert(.., viewed: bool, ..)`.
- Return-value bools: `finish_client_shell_input -> Result<bool, _>` (true means
  exit), `cancel_endpoint_commands -> bool` (repaint), `present_surface_patch ->
  io::Result<bool>` (presented), `AtomicCellSize::store -> bool` (changed).

### 1.6 `HostMouseMode` encodes a three-state source as `Option` plus `bool`

`endpoint_request: Option<EndpointMouseRequest>` plus `use_preference: bool` encode
"initial (keep whatever is active)", "use the shell preference" and "use the endpoint's
request". `desired()` returns a `(bool, bool)` tuple. An enum
`MouseSource::{Initial, Preference, Endpoint { enabled, sgr_pixels }}` and a
`MouseModeRequest { enabled, sgr_pixels }` return type would make the invalid pairing
(`Some(request)` with `use_preference: true`) unrepresentable.

### 1.7 Launch state of Local spread over Options and bools

`run_launched_client` and `run_client_loop` track Local's launch outcome with
`initial_stream: Option<LocalStream>`, `initial: Option<LocalStream>`,
`initial_local_failure: Option<SshFailureDiagnostic>`, `local_unavailable: bool`,
`connected_generation: Option<u64>`, `seeded_failure: Option<ClientEndpointStatus>` and
`add_local(.., generation: Option<u64>, ..)` (where `Some` means "already connected,
schedule nothing"). This is one enum: `LocalAtLaunch::{Attached(transport, Generation),
Absent, Failed(EndpointFailure)}`.

### 1.8 Shell outcome as a bag of bools plus raw wire messages

`ClientShellInput` is `detach`, `repaint`, `full_redraw`, `resize`,
`query_host_appearance`, `query_host_theme` (bools) plus `requests: Vec<ClientMessage>`
and `actions: Vec<ClientShellAction>`. `finish_client_shell_input` reinterprets the
`ClientMessage`s by variant: `ClientShellHostTheme` is recorded and sent to every viewed
connection, `ClientShellResize` triggers `resize_views` (and throws the carried geometry
away), and the `_` arm sends to the shown endpoint. Resize can be requested two ways
(the `resize` bool and a `ClientShellResize` request); the second is dead (the shell
never emits it). The routing rule (viewed vs shown) is a decision about message kinds
made by a wildcard in core. A typed effect list (`ShellEffect::{PaneInput(..),
HostFocus(bool), HostTheme(update), Resize, Detach, Repaint { full }, QueryHost(..),
Endpoint(..), Activate(..), Clipboard(..)}`) with the routing attached to the variant
would remove both problems.

### 1.9 Clipboard payload is base64 text inside a binary codec

`ServerMessage::Clipboard { data: String }` carries base64; the client decodes it in
`decode_clipboard_payload` and an invalid payload becomes `InvalidData` at runtime. The
wire codec is binary and positional, so `Vec<u8>` would make the malformed case
unconstructible and drop an encode/decode pair on both ends.

### 1.10 Smaller axes

- Cell pixel sizes as `(u32, u32)` tuples (`ioctl_cell_size`, `AtomicCellSize::load`,
  `current_terminal_geometry_with`'s `last_cell_size`, `reported_cell_size_from_events`,
  `cell_size_fallback`) and the grid-plus-cell as `(u16, u16, u32, u32)`
  (`ioctl_terminal_geometry`) while `shepr_core::geometry::CellPx` exists.
- `bounded_cell_geometry` returns `(u32, u32, bool)` decomposed from the typed
  `ProtocolCellSize` it just built.
- Poll results: `poll_read_ready -> Option<bool>` (error / ready / not ready), consumed
  as `!= Some(false)`; `pending_mode: Option<bool>` in the stdin reader (latched SGR
  pixel mode for a pending sequence).
- Timeouts as `i32` milliseconds (`idle_flush_timeout_ms`, the termio constants,
  `Deadline::remaining_millis_i32`) rather than `Duration` converted at the poll call.
- Terminal restore mask as `u8` constants (`RESTORE_*`) and `HostRestoreAction<W> =
  (Option<u8>, fn ..)`: a `bitflags` type or an enum list.
- `ClientLoop::next_view_serial: u64` passed around as `&mut u64` to `view::start_move`,
  `view::release_unwanted` and `EndpointRegistry::release_unwanted_views`.
- `ClientEndpointId::storage_key() -> String` is used only for log fields and notice
  keys (`session_restore_incomplete:{key}`, `{key}:{boot}`), never storage; it allocates
  per log line. It should be `Display` (log) plus a typed notice key.
- `HostWriteFailure::observe(write: &'static str, ..)` and `logging::startup(role:
  &'static str)` name a closed set with strings.

---

## 2. Decisions made in more than one place

### 2.1 Is this endpoint failure transient or does it need repair?

Sites answering it:

- `endpoint::supervisor::handshake_error`: maps each `ClientError` variant to an
  `ErrorKind` chosen for how `SshFailureDiagnostic::from_error` will read it (its own
  comment: "must stay out of InvalidData, which the attention classifier treats as a
  compatibility problem").
- `handshake::preamble_error`: rewrites `PreambleError::UnexpectedEof` and `Io` into
  `FramingError` so they classify as transient.
- `handshake::hello_write_error`: keeps the socket error kind "because the endpoint
  supervisor decides between retrying and asking for attention by that kind".
- `supervisor::connect_once`: rewrites Local `NotFound` to `ConnectionRefused` with a
  new message.
- `endpoint::writer::queue_full`: `ConnectionAborted`.
- `transport::framing_error_to_io`: `InvalidData` for every non-IO framing error.
- `errors::endpoint_setup_failure`: picks `from_error`, `from_local_setup_error` or
  `from_message` by `ClientError` variant.
- `shepr_remote::SshFailureDiagnostic::from_error` and `is_ssh_link_error_kind`: the
  decoder, in another crate.
- `ClientEndpointStatus::after_failure`: the status from the diagnostic.
- The supervisor's join-error arm: `Reconnecting` set directly.
- `reconcile::endpoint_lost`: always `Reconnecting` (via `supervisors.disconnected` and
  `mark_endpoint_disconnected`) and a `from_message` diagnostic, whatever the kind.
- `shell_runtime::endpoint_disconnect_notice`: a third reading of `ErrorKind`, for text.

They already disagree. The same `InvalidData` (a codec failure) is Attention during the
handshake and Reconnecting once the connection is live (`endpoint_lost` ignores the
kind); a server shutdown during the handshake becomes `ConnectionAborted` (retry), while
a shutdown after it becomes `write_stream.fail(.., ConnectionAborted, reason)` and loses
the typed `ShutdownReason`. A queue-full backpressure failure, a local condition, is
reported to the user as "connection was lost". Owner: a client-owned
`EndpointFailure` type produced at each source with its class fixed there, rendered by
one function, with `SshFailureDiagnostic` demoted to the payload of the SSH-process
cause.

### 2.2 Which endpoint is shown?

- `EndpointChoice::shown()` (documented as "the only owner of that fact: nothing else
  records a selection").
- `ClientShellState::active_endpoint_id`, read by core through `endpoint_is_active` in
  `handle_endpoint_supervisor` (attention notice), the `Connected` failure branch,
  `handle_server_message` (answer vs drop a completed command), `handle_timer` (expired
  commands), and inside the shell by `mark_endpoint_disconnected`.

They are kept in step only by `view::commit_move` calling `activate_endpoint_projection`.
They disagree whenever nothing is shown: after `Lost::Shown`, `choice.shown()` is `None`
while the shell still names the lost endpoint as active. Owner: the choice; the shell
should receive the shown id (or `None`) as input to composition, not keep its own.

### 2.3 What is this endpoint's status?

The supervisor's `ReconnectState` (in flight, next attempt, online since) is one answer;
the shell's `ClientShellEndpoint::status` is another, written from:
`run_client_loop` (initial Connecting or `after_failure`), `handle_endpoint_supervisor`
`Status` arm, its `Connected` arm (twice: `record_status(Online)` and
`set_endpoint_status(Online)`), the `Connected` reader-spawn failure branch,
`install_client_shell_snapshot` (Online on every snapshot, any role), `commit_move`
(Online), and `mark_endpoint_disconnected` (Reconnecting). The shell's own default for
Local (`local_endpoint()`) is Online, which `run_client_loop` then overrides when Local
is absent. `waiting_notice` reads the shell copy. Owner: the supervisor (or a merged
endpoint table), with the shell reading.

### 2.4 Will a pick be abandoned, and is the target ready?

`shell_runtime::dispatch_client_shell_actions` predicts what `view::start_move` will do
on the next reconcile, so it can choose a notice:

- `abandoned = connection.is_none() && !endpoint_id.is_local() && choice.shown().is_some()`
  mirrors `start_move`'s `!pending.to.is_local() && pending.from.is_some()` abandon rule.
- `metadata_ready = shell.endpoint_snapshot_identity(id, generation).is_some()` mirrors
  `start_move`'s Waiting check.

Nothing ties the two. Owner: `EndpointChoice::select` (or a `start_move` dry run) should
return the outcome, and the notice should be chosen from it.

### 2.5 What geometry does an endpoint render?

- `handshake::do_handshake_for_link` builds `TerminalGeometry` from
  `HandshakeGeometry { host, surface_size }` after its own `bounded_cell_geometry`.
- `shell_runtime::view_geometry` builds the same `TerminalGeometry` from
  `state.reported_geometry` and `shell.surface_size`, with its own
  `bounded_cell_geometry`.
- `ClientLoop::run_until_exit` builds the `HandshakeGeometry` for `spawn_due` through
  `ProtocolCellSize::from_host` and a fresh `HostGeometry::new`.
- At launch, `ClientShellConfig::initial_surface_size` derives the surface size from
  preferences and config, duplicating the sidebar derivation in
  `ClientShellState::new_at` (same `preferences.sidebar_collapsed.unwrap_or(..)` and
  `clamp_width` code), because the shell state does not exist yet when the first
  handshake runs.

Owner: `view_geometry` (one `TerminalGeometry` producer), passed to the handshake as is;
build the shell state before the launch handshake so `initial_surface_size` can go.

### 2.6 Is the host's pixel geometry exact, and what is it?

- `initial_terminal_geometry` reads the ioctl once and sets `exact`.
- `host_cell_size_query_required` reads the ioctl again to decide whether to query.
- `resize_poll_loop` reads it every poll.
- The stdin reader reads it again per chunk via `HostPixelExtent::current()` (in
  `consume_input_bytes`) and uses that, not the resize poller's value, to map SGR pixel
  mouse reports into cells.
- `AtomicCellSize` holds host-reported cell size shared with the resize thread.

Two reads made moments apart can disagree (exact geometry recorded but the query sent,
or the reverse), and the server's cell size (from the poller) and the shell's
pixel-to-cell mapping (from stdin's read) can differ across a resize. Owner: one
host-geometry source (the poller) publishing a snapshot the stdin thread reads; the
launch decision should be `!geometry.exact`.

### 2.7 Who mints connection generations?

`run_client_loop` passes the literal `1` to `start_endpoint_transport` and
`EndpointRegistry::new_at`, and seeds `Some(1)` for a failed launch;
`EndpointSupervisors::new` starts `next_generation` at `2`. The two agree because
someone picked the numbers. Owner: the supervisor's allocator, used by the launch path
too.

### 2.8 Who mints request ids?

The shell `Ledger` is documented as "the sole owner of issued request identities", but
core mints three more families (see 1.3). Owner: one client request-id allocator.

### 2.9 May pane content be presented now?

- `endpoint::PresentationGate::decide` drops pane frames that are not from the shown
  endpoint.
- `ClientState::present_frame` and `present_surface_patch` re-check
  `choice.frames_frozen()`.
- `ClientState::present_chrome` deliberately skips that check, justified by a comment
  that the pane cells are always coherent.

Every `present_frame` and `present_surface_patch` call site is reached only for the
shown role or right after a commit, so the frozen check never fires; the decision is
made by the gate and re-made, unreachably, by the presenter, while each of about eighteen
call sites picks chrome or frame by hand. See headline 3.

### 2.10 Is this inbound message evidence for the move?

`PresentationGate` returns `Buffer` for surfaces, patches and move responses, but
`Apply` for `EndpointSnapshot` regardless of role; `handle_server_message`'s snapshot arm
then separately checks `role == Target` to feed `Preparing::receive_snapshot`. The gate
also takes `move_response: bool`, which `handle_server_message` computes by asking
`preparing().accepts_response(..)`. Owner: the gate should take the choice and return
`Apply`, `Buffer`, `ApplyAndBuffer` or `Drop` for every kind; new `ServerMessage` variants
currently fall into the gate's `_` arm as shown-only without anyone deciding.

### 2.11 Attaching Local: launch path and supervisor path

The launch connects, handshakes and builds the transport itself
(`run_launched_client`, `start_endpoint_transport`), and the supervisor does the same
for every later attempt (`connect_once`, `establish`, then `spawn_endpoint_reader` on
the loop thread). Differences that are decisions, not code:

- An absent socket is `None` (silent, Connecting) at launch, and a rewritten
  "start its server to reconnect" `ConnectionRefused` in the supervisor.
- Local's build-mismatch guidance is computed twice
  (`paths.server_address().build_mismatch_guidance(..)` in `run_launched_client` and in
  `EndpointSupervisors::add_local`), while the `ConnectTarget::Local` doc claims it is
  "resolved once".
- Launch handshakes with `surface_active: true` and no deadline (Local read timeout);
  supervisor attempts use `false` and the attempt budget.
- Launch shows Local by fiat (`EndpointChoice::showing(Local)`, `viewed = true`) with no
  coherent pair, bypassing `commit_move`'s checks; reconnects go through a move.
- Launch sends `ClientShellFocus { focused: true }` unconditionally; a commit sends
  `host_focus_baseline()`.
- `ends_client_for(&Local)` is evaluated three times in launch, each choosing between a
  fatal and a degraded path.

Owner: one attach routine, used synchronously at launch, with `LocalFailurePolicy`
applied once to its typed outcome.

### 2.12 What is the current host palette?

- `input::send_unix_input_chunks` batches palette replies into one `StdinInput` up to
  `PALETTE_COLOR_COUNT`, flushing early on any other event or idle timeout.
- The shell's `push_host_theme_update` merges consecutive `PaletteColors` into one
  request up to the same count.
- `ClientState::record_host_theme_update` keeps at most one `PaletteColors` update,
  replacing the previous one wholesale.

A palette reply split by an idle flush becomes two partial `PaletteColors` updates, and
the record keeps only the second; `turn_on` then replays a partial palette to every
newly viewed endpoint. See bug B2. Owner: a host-theme model merged by palette index.

### 2.13 Is a host terminal write failure fatal?

Mouse mode writes (`handle_resize`, the `MouseCapture` arm, `clear_endpoint_host_effects`)
and keyboard report-all writes (`ClientShellKeyboardReportAll` arm,
`sync_client_shell_keyboard_report_all`) map failure to `ClientError::HostTerminal` and
end the client. Frame and patch writes, window titles, clipboard writes and host
queries log once and carry on. Same terminal, same failure, two policies chosen per
call site. Owner: `ClientState` (or `HostModes`) with a single policy.

### 2.14 Local versus machine policy

Each of these asks "is this Local or a machine" separately: heartbeat
(`EndpointRegistry::crosses_ssh`), handshake read timeout (`HandshakeLinkKind` derived
from `EndpointLink` in `establish`), attempt-counter reset on Online
(`record_status`'s `endpoint_id.is_local()`), abandon when unconnected
(`start_move` and `dispatch_client_shell_actions`), fatal on loss
(`LocalFailurePolicy::ends_client_for`), mismatch guidance (`ConnectTarget::Local` only).
That is three enums (`ClientEndpointId`, `EndpointLink`, `HandshakeLinkKind`) plus
`ConnectTarget` and `AttemptTarget` for one axis. Owner: an `EndpointPolicy` derived
once from the id.

### 2.15 Endpoint notice wording

`"{label}: {message}"` is assembled in `handle_endpoint_supervisor`'s `Status` arm, its
`Connected` failure branch, `run_client_loop` (with the literal `"Local"` rather than
`endpoint_label`), `reconcile::fail_move` callers, `endpoint_lost`, and
`waiting_notice`. A typed notice (`EndpointNotice { endpoint, kind }`) rendered once
would also make the label source single.

### 2.16 Smaller duplicates

- Mouse mode set up twice: `setup_terminal` builds `HostModes::new(false, mouse_capture)`
  (placeholder preference `false`), then `run_client_loop` replaces it with
  `configure_mouse_mode(HostMouseMode::new(pref, pref))`, swapping the `Arc` mirrors.
- Clipboard writes: `forward_clipboard` (server clipboard) and the `ClipboardWrite` arm
  of `dispatch_client_shell_actions` each call `write_clipboard_bytes` with the
  setting and their own logging.
- `surface sized for this client` is checked in `Preparing::receive_surface` and again
  in `ViewEvidence::coherent_surface` (a deliberate re-check, listed only because both
  compare `frame.width == size.cols && frame.height == size.rows` by hand; a
  `PaneSurfaceFrame::is_sized_for(size)` would make it one answer).

---

## 3. Structure

### 3.1 `lib.rs` is the loop, the launch, the finalization and the dispatcher

About 1300 production lines: `run_launched_client` (connect, handshake, panic
installation, terminal setup, runtime, finalization, exit classification),
`run_client_loop` (state construction, thread spawns, initial transport, supervisors,
initial notice; twelve arguments behind a clippy `expect`), `ClientLoop::new` (ten
arguments), and `handle_server_message` (about 320 lines, one arm per wire variant).
`ClientLoop` handlers destructure `self` into pieces (`let Self { state, write_stream,
endpoint_commands, .. } = self;`) in every method because the move's state (`choice`)
lives in `ClientState` while the registry and command lanes live in `ClientLoop`, and
the move's I/O needs both.

Suggested shape:

- `launch.rs`: the ordered launch phases (validate, probe terminal geometry, attach
  Local, take the terminal, start helpers) returning one `Launched` value, and the
  finalization; `run_client` stays as the entry point. `ClientLoopConfig` disappears: it
  is created with placeholder fields (`host_escape_disambiguation_active: false`,
  `initial_host_input: Vec::new()`) that are overwritten after terminal setup.
- `endpoints` (a new owner): registry, supervisors, command lanes, choice, view serial
  and request-id allocator in one struct whose methods implement the move. Today the
  move protocol is spread across `choice.rs` (pure state), `preparing.rs` (pure
  evidence), `view.rs` (I/O), `registry.rs` (`release_unwanted_views` mints off ids),
  `reconcile.rs` (orchestration and notices) and `lib.rs` (routing evidence into
  `Preparing`).
- `dispatch.rs`: inbound server messages, with the gate as its first step.
- A presenter owned by `ClientState` (headline 3).

### 3.2 `shell_runtime.rs` is a grab bag under a misleading name

It holds request cancellation, input routing (`input_endpoint`), shell action dispatch,
the waiting notice, `view_geometry`, `resize_views`, keyboard sync, host effect clearing,
the disconnect notice table, snapshot installation and shell outcome finishing. None of
it is "the runtime of the shell module"; it is the loop's glue between shell, endpoints
and host. It and `transport.rs`, `reconcile.rs`, `events.rs`, `clipboard_forwarding.rs`
all `use super::*`, so they are textually separate but share `lib.rs`'s namespace.
`install_client_shell_snapshot` returns `Result` but never fails.

### 3.3 One connection's I/O is split across two modules

The writer (`endpoint/writer.rs`, `NativeEndpointTransport`) and the reader
(`transport.rs` at the crate root, `server_reader_thread`) of one connection live in
different places; the reader is spawned from the writer's internals
(`stop_handle()`, `read_activity()`); the surface decoder is created at two sites; and a
connection is assembled two ways (launch: `start_endpoint_transport`; supervisor:
`establish` builds the writer on the attempt thread, `EndpointSupervisorEvent::Connected`
carries the raw reader stream back, and `handle_endpoint_supervisor` spawns the reader
on the loop thread with its own failure branch duplicating the `Status` arm). An
`EndpointConnectionIo::start(stream, lifetime, endpoint, generation) -> (writer handle,
reader thread)` built inside the attempt would give one assembly path and one place for
the reader-spawn failure.

Registry insertion also has two paths (`insert` without reader activity, used for
launch Local via `new_at`, and `insert_native` with it); `received` exists only for
transports without reader stamps, which in production means none that are health
tracked.

### 3.4 Types owned by the wrong crate

- `ConnectionGeneration` is in `shepr-protocol` (`revision.rs`) but is never on the
  wire; only `shepr-client` uses it. It also inherits the protocol counters' escape
  hatches (1.1, 4.1).
- `SshFailureDiagnostic` (shepr-remote) is the client's universal endpoint diagnostic,
  including for the Local socket, which never involves SSH. The client's attention
  policy therefore depends on `shepr-remote`'s private `ErrorKind` table.
- `HostGeometry` (shepr-core) is built on `PaneGeometry`, so a host terminal size is
  clamped to the pane minimum (bug B1). It belongs to the client (or termio) and should
  be built on `GridSize::clamped` and `ProtocolCellSize`.
- `host_replies.rs` contains no client code beyond a `Deref`/`DerefMut` newtype over
  `RawInputFramer<HostReplies>`, and its test exercises termio's `HostReplies`. The
  newtype buys nothing; the test belongs in termio.

### 3.5 Public surface shaped by a lower crate's test

`shepr-server` dev-depends on `shepr-client` for one test
(`shepr-server/src/server/netside_tests.rs`), which drives `EndpointChoice`,
`EndpointRegistry`, `view::start_move`, `HostBaseline` and `ClientShellState` against
two in-process servers. That is why `pub mod endpoint`, `pub use view::{..}`, `pub use
shell::{ClientShellConfig, ClientShellState}`, `EndpointRegistry::new_at`, `insert`,
`viewed` and the choice methods are `pub`; the binary uses only `run_client`,
`ClientExit` and `ClientRunError`. The test is a cross-crate integration test and would
sit better in a dedicated integration-test workspace member that depends on both,
letting the client's surface shrink to the three items the binary uses.

### 3.6 Launch validation after the terminal is taken

`EndpointSupervisors::new` returns `connector.launch_fatal_setup_error()` as
`ClientError::EndpointSetup`, but it runs inside `run_client_loop`, after raw mode, the
alternate screen and the stdin and resize threads are up. A launch-fatal configuration
problem is therefore found after the terminal has been taken over. It belongs with the
other launch checks in `run_launched_client`, before `setup_terminal`.

### 3.7 `ClientError` serves three roles

It is the launch connect error, the handshake outcome and the loop's exit reason.
`handshake_error` must handle `HostTerminal`, `EndpointSetup` and `Panicked`, which a
handshake can never produce ("this only keeps the match exhaustive"), and
`run_launched_client` classifies the loop's exit by matching variants
(`ServerShutdown` is a clean exit; `ConnectionLost` is clean only if terminal restore
also failed). Split into `HandshakeError` (typed refusal, preamble, IO, protocol,
shutdown; with a `class()`), `LoopExit` (detach, quit, terminal gone, server shutdown,
Local lost, host terminal write failed, panic) and launch errors.

### 3.8 `limits.rs` mixes owners

Transport and endpoint timing sit next to shell presentation constants (sidebar,
navigator overlay, context menu, selection autoscroll, copy queue, diagnostic text
size). Every constant is `pub(super)` at the crate root so the shell can reach it. Split
by owner so each module's limits live beside it.

### 3.9 `shell.rs`'s directories are cosmetic

`shell.rs` maps `shell/input/`, `shell/navigation/`, `shell/overlays/`,
`shell/presentation/` and `shell/sidebar/` files into one flat module namespace with
`#[path]` attributes and `use x::*` globs. The folder layout suggests seams the module
tree does not have: every file sees every other file's `pub(super)` items. Either make
the directories real modules with narrow interfaces or drop the directories. (Detail is
the shell hunter's; the root file is in this scope.)

### 3.10 Test layout follows imports, not the design

`src/tests/mod.rs` holds tests for `terminal_geometry`, `terminal_setup`, `errors` and
`clipboard_forwarding`, which forces `lib.rs` to carry `#[cfg(test)] use` re-exports of
those modules' private functions. The tests belong in each module's own `tests` block.
`src/tests/endpoint_choice.rs` builds a full `ClientLoop` fixture that `endpoint/view.rs`'s
unit tests reach into via `crate::tests::endpoint_choice`, so a module's unit tests
depend on a crate-level integration fixture.

---

## 4. Types that resolve to primitives

### 4.1 The protocol counters

`ProjectionRevision`, `SurfaceRevision` and `ConnectionGeneration` (`counter!` in
`shepr-protocol/src/revision.rs`) implement `From<u64>`, `From<Self> for u64`,
`PartialEq<u64>` both ways, `PartialOrd<u64>`, and a public `get()`. The client uses all
of them: `.get()` into `u64` fields (1.1, 1.2), `generation.into()` and
`Some(generation.into())` back, `snapshot.revision < self.lease.minimum_revision`
(newtype vs `u64`), `snapshot.revision == revision` in `endpoint_snapshot_matches`,
`connection.generation == generation` in the registry. With the cross-type comparisons a
`ProjectionRevision` compares equal to a `ConnectionGeneration`'s `get()` with no
complaint. Offer instead: no `From<u64>` (construct from a typed source or `ZERO` and
`checked_next`), no cross-type `PartialEq`/`PartialOrd`, `Ord` within the type, and
a `ProjectionRevision` on the wire in `EndpointReply::ClientShellSurfaceSet`.

### 4.2 `RequestId`

`From<String>`, `From<&str>`, `Deref<Target = str>`, `Borrow<str>`,
`PartialEq<str | &str | String>`, and `String: PartialEq<RequestId>`. Its doc says any
string is a legitimate id, on purpose; the client nonetheless relies on structure inside
it (the focus lane's `split(':').nth(1)`). Escape hatches used in scope:
`RequestId::from(id)` in `EndpointCommands::send_next`, `.to_string()` into
`EndpointCommandCancellation`, `drop_request(&request_id, ..)` via `Deref`, the
`format!(..).into()` minting in `view.rs`, `registry.rs`, `focus_lane.rs`. Offer: keep
`RequestId` opaque on the wire, and add the client-side enum of 1.3 as the only thing
that converts to and from it.

### 4.3 `SshFailureDiagnostic`

`Deref<Target = str>` plus `from_message(impl Into<String>)` (class `Other`). In scope,
`from_message` is the escape hatch that discards a known class: `reconcile::endpoint_lost`
rebuilds a diagnostic from `failure.message`, the supervisor's join-error arm and
`endpoint_setup_failure`'s catch-all do the same. Offer: a constructor per known cause,
and no `Deref` (callers use `Display`).

### 4.4 Geometry values with public fields and conventional clamps

- `ClientSurfaceSize { pub cols, pub rows }` with a `clamped()` method that callers must
  remember; `ClientHostSize` exists only to call it and is immediately decomposed back
  into `cols`/`rows` (`set_host_size`, `run_launched_client`).
- `HostGeometry { pub pane, pub exact }`: `exact` can be set true without a cell size by
  a struct literal, defeating the constructor's `exact && pane.cell().is_some()`.
- `shepr_protocol::TerminalGeometry { pub grid, pub cell, pub pixel_mouse }`: the
  invariant `pixel_mouse` implies `cell` is enforced by `new` and by deserialization but
  not by construction from fields.
- `ProtocolCellSize { pub cell, pub exact }`.
- In scope the escape is decomposition: `handle_resize` and `run_client_loop` take a
  `HostGeometry` apart into five primitives, pass them through `bounded_cell_geometry`,
  and rebuild it; `ClientLoopEvent::Resize(geometry)` is unpacked into
  `handle_resize(cols, rows, w, h, exact)`. Offer: private fields; `HostGeometry` stores
  a `ProtocolCellSize` (already bounded) so bounding happens once at ingestion; a
  `ClientSurfaceSize` that can only be built clamped.

### 4.5 Sentinels and pseudo-types

- `AtomicCellSize` packs `width << 32 | height` with `0` meaning "not reported"; the
  `CellPx::new` check on unpack makes it sound, but the API returns `(u32, u32)`.
- `EndpointReadActivity` packs nanoseconds and a "snapshot seen" bit into one
  `AtomicU64`, `0` meaning no frame; sound and documented, but `observed()` returns
  `(Option<Instant>, bool)`.
- `ClientLoop::wait_for_next_event` returns `ClientLoopEvent::Timer` as the sentinel for
  "the panic latch fired" and for "the event channel closed"
  (`ev.unwrap_or(ClientLoopEvent::Timer)`).
- `ClientShellEndpoint::snapshot_generation: Option<u64>` where `None` means "test
  only", and `endpoint_snapshot_matches` accepts it with `is_none_or`, so a production
  check carries a test-only branch.
- `HostInputFramer` is a newtype with `Deref` and `DerefMut` to its only field.
- `ClientPresentationLogContext` (shell, used by `ClientState::write_frame`) holds
  `endpoint: String`, `boot_id: Option<String>`, `projection_revision: Option<u64>`,
  `surface_revision: Option<u64>`: typed values stringified and unwrapped for logging.
- `ClientShellState::endpoint_label(&self, id)` ignores `self` and forwards to
  `display_label()`, suggesting a configurable label that does not exist.

---

## 5. Bugs, smells and surprises

B1. **The host terminal size is clamped to the pane minimum.** `HostGeometry::new`
builds a `PaneGeometry`, whose constructor clamps the grid to `PANE_MIN_COLS` x
`PANE_MIN_ROWS` (4 x 2). `ClientState::set_host_size` first clamps with
`ClientSurfaceSize::clamped` (minimum 1) and then stores through `HostGeometry::new`, so
`reported_geometry.cols()` is never below 4. A host terminal narrower than 4 columns or
shorter than 2 rows gets frames composed for a larger grid. The test
`client_host_size_clamps_the_grid_to_one_surface` asserts 1 column for `ClientHostSize`,
a value the stored geometry can never hold. Low impact, but it shows `HostGeometry` is
built on the wrong type.

B2. **A split palette reply is replayed partially.** `ClientState::record_host_theme_update`
replaces any earlier `PaletteColors` update with the newest one. If the host's palette
replies straddle the stdin reader's idle flush (a slow terminal, or a read boundary),
the shell emits two partial `PaletteColors` updates and only the second is recorded;
`view::turn_on` then sends that partial palette to every endpoint viewed later. The
merge should be by palette index. Needs a slow host to trigger.

B3. **A machine may be labelled `Local`.** `MachineLabel::parse` and the duplicate-label
validation accept `Local`; `ClientEndpointId::display_label` returns `"Local"` for both,
so every notice ("Local: ...", "Local is not ready"), the sidebar and the navigator
cannot tell them apart. Either reserve the label or display Local differently.

B4. **`endpoint_lost` throws away the failure class.** It builds the machine diagnostic
with `SshFailureDiagnostic::from_message(failure.message)`, so the badge diagnostic after
a live connection drops is always class `Other`, whatever the transport reported
(`TimedOut` from the heartbeat, `InvalidData` from a decode or patch rejection).

B5. **Dead decisions.** `ClientState::frames_frozen` checks in `present_frame` and
`present_surface_patch` cannot fire at any call site (2.9). The
`ClientMessage::ClientShellResize` arm in `finish_client_shell_input` is never reached
(the shell does not emit it). `ServerMessage::SurfaceUpdate` and `EndpointWelcome` reach
the loop only as protocol violations; the decoded message type should not be able to
hold them (`DecodedServerMessage::Wire(ServerMessage)` admits every wire variant).

B6. **Doubled compose on input.** `handle_stdin_input`, the response arm of
`handle_server_message` and `handle_timer` compose a frame when `outcome.repaint`, then
`finish_client_shell_input` composes again and discards the first whenever
`dispatch_client_shell_actions` reports a repaint. A per-turn dirty mark would compose
once.

B7. **Exit flush depends on how the loop ended.** In `run_until_exit`, an `Exit` with
`should_quit` set breaks out and flushes the output writer; an `Exit` from detach or a
lost terminal returns `Ok(())` without the flush. The registry's drop handles endpoint
flushing either way, but the host writer flush differs by path for no stated reason.

B8. **Exit classification by coincidence.** `run_launched_client` treats
`ConnectionLost` as a clean exit only when terminal restoration also failed
(`connection_lost_during_terminal_hangup`). Two independent failures are read as one
cause (a terminal hangup) by inference.

B9. **Possible ledger leak.** `reconcile` skips a queued transport failure when a
connection of another generation is already present, and then nothing disconnects the
old command lane; `handle_timer` drops expired commands whose generation is no longer
accepted, without telling the shell. If that skip is ever reached with a command in
flight, the shell's ledger entry for it is never answered or dropped. The current event
ordering seems to make the skip unreachable; if so, the check and the filter are dead
and should go, and if not, the lane should be disconnected.

B10. **An SGR pixel mouse event with no geometry is dropped entirely.**
`classify_unix_input` returns `None` (`let geometry = geometry?;`) for a pixel mouse
report when no pixel extent has been read yet, discarding the click rather than
falling back to cell coordinates or holding it.

B11. **Pixel coordinates cross a 1-based convention twice.** `classify_unix_input` adds
1 to the parser's 0-based column and row to build `HostPixels`, and
`HostPixelExtent::cell` subtracts 1 again. The convention is shared by agreement
between two crates.

B12. **Every loop turn recomputes geometry and connect options.** `run_until_exit`
builds `EndpointConnectOptions` (including `shell.surface_size`, a layout computation)
before every wait, and `reconcile` computes `view_geometry` and `surface_size` again,
even when no attempt is due and no move exists. Cheap today, but it runs per pane patch
event; computing them only when a supervisor attempt is due or a move is pending is
free.

B13. **`ClientLoopTimer` duplicates the deadline source.** The loop recomputes the
earliest deadline from every source each turn, converts it to a delay, and hands it to
`ClientLoopTimer::deadline`, which keeps the minimum of that and any earlier deadline
not yet fired. The retained earlier deadline can belong to work that no longer exists
(a committed move), causing a spurious wake. Arming `sleep_until(next_timer_deadline)`
directly would remove the type.

B14. **Machine label notices use a literal.** `run_client_loop` presents
`format!("Local: {failure}")` rather than going through `endpoint_label`, so Local's
label is written twice (here and in `ClientEndpointId::display_label`).
