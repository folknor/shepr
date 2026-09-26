Structural review of the client core and input: `src/client/{mod,state,errors,events,handshake,transport,input,terminal_setup,terminal_geometry,loop_config,shell_runtime,attach}.rs`, `src/raw_input.rs` (the parts that matter), and the top of `src/client/shell.rs`. I edited nothing and ran no commands. I did not read `src/input/{keybindings,lease,encode}` in depth, the `shell/` submodules, `frame_output`, `clipboard_forwarding` or `startup`.

## 1. Axes that should be types

- **Terminal geometry travels as bare tuples in five places.**
  - `(u16,u16,u32,u32,bool)`: `TerminalGeometry` alias in `terminal_geometry.rs:59`, `ClientLoopEvent::Resize(u16,u16,u32,u32,bool)` in `events.rs:7`, and the `last_size` tuple in `resize_poll_loop`.
  - `reported_size: (u16,u16)` and `reported_cell_size: (u32,u32)` in `state.rs:18-19`.
  - The `cols, rows, cell_width_px, cell_height_px, exact_cell_size` positional arguments to `do_handshake` and `run_client_loop`.
  - Cell width and height are easy to swap, and so are the grid and pixel sizes.
  - A `HostGeometry { grid: GridSize, cell: Option<ExactCellPx> | Fallback(CellPx) }` would make "exact" part of the value instead of a sibling bool. It could also absorb `input::mouse::HostGeometry`, which is another geometry type.
- **The cell size is packed into an `AtomicU64` as width<<32 | height, with 0 meaning absent** (`pack_cell_size`/`unpack_cell_size`). A tiny `AtomicCellSize` type would own this encoding and its "unreported" sentinel.
- **`ServerShutdown { reason: Option<String> }` uses the magic string `"detached"`.** It is matched in `mod.rs:302-307` (to decide the exit code) and in `errors.rs:38` (to decide the message). The protocol should carry a `ShutdownReason` enum (`Detached | Stopped | Restarting | Other(String)`).
- **`ClientError` is prose-heavy.**
  - `Protocol(FramingError::Io(io::Error::new(InvalidData, "expected endpoint welcome")))` is built in five places (handshake twice, the preamble path, the mod.rs control decode, and the transport surface decode).
  - A malformed welcome, a missing preamble, a surface decode failure and an endpoint-control failure are indistinguishable to a caller.
  - `ConnectionFailed(io::Error)` is also reused for terminal write failures (`set_mouse_capture(...).map_err(ClientError::ConnectionFailed)` in `mod.rs:756,1217,1234` and `shell_runtime.rs:85`). If the host tty fails, the user is told "failed to connect to server… Is shepr server running?". This is a real mislabel. A `HostTerminal(io::Error)` variant is needed.
- **`ClientShellKeybindingSource` comes from an env var string** (`handshake.rs:30-39`). `Some(_)` and `None` both map to `RemoteLocal`, so any value other than "server" is silently accepted. `is_remote_client_process()` reads the same env var for a different question ("am I a remote client?"). Both are ad hoc probes; one typed `ClientProcessRole` read once at startup should replace them.
- **`HandshakeResult` is a unit struct** (`handshake.rs:61`), and callers bind it and ignore it. Either return the negotiated facts (version, encoding) or return `()`.
- **Pending-request ids are classified by string prefix:** `request_id.starts_with("client-shell-surface:")` at `mod.rs:1123`. A request-id kind should be a type, or the pending activation should own the id space.
- **`EndpointControl { kind: String, data: String }` holds JSON inside the positional codec.** It is compared against `PRESENTATION_EFFECTS_READY_KIND` (`mod.rs:1244`) and `ENDPOINT_WELCOME_KIND`. AGENTS.md says wire types are shepr's own positional codec with no tagged-enum tricks. A stringly kind plus JSON payload is a second, self-describing protocol tunnelled through the first. Since there is no wire-compatibility obligation, make these real `ServerMessage` variants.
- **`surface_decoder: Option<Decoder>` is always `Some(Decoder::new(true))`** at every call site (`mod.rs:473,880`). The `Option` and the `true` flag encode nothing.
- **`mouse_scroll_lines: usize` is clamped to `u16` ad hoc** (`mod.rs:735`) in addition to wherever attach does it.
- **The stdin framer outputs `Vec<Vec<u8>>`**, and the kind of each chunk (paste, mouse report, palette reply, key) is rediscovered later by reparsing. See 2(d).

## 2. Decisions made in more than one place

a. **"What surface size does this client report?"** The clamp `ClientSurfaceSize{..}.clamped()` when a shell exists, raw otherwise, is decided in three places: `mod.rs:189-192`, `mod.rs:351-356` and `mod.rs:764-773`. They agree today. The owner should be one `ClientState::set_host_size(geometry)` (or a geometry type method).

b. **"How is cell size clamped to the protocol, and is pixel mouse still exact?"** The rule (`min(MAX_CELL_SIZE_PX)`, and exact only if both sides are ≤ MAX) is written independently in three places:
- `handshake.rs:109-113`
- `shell_runtime.rs:64-70` (`client_shell_resize_message`)
- `mod.rs:550-554` (`EndpointConnectOptions`)

The direct-attach `ClientMessage::Resize` at `mod.rs:791-797` does not clamp at all, so the sites already disagree. Owner: a `ProtocolCellSize::from_host(...)` constructor in `protocol`.

c. **"What mouse mode should the host be in?"** Mouse capture plus SGR-pixels is computed and applied in three places, each with its own diff against state:
- the `MouseCapture` handler (`mod.rs:1200-1222`)
- `clear_endpoint_host_effects` (`shell_runtime.rs:90-111`)
- the `Resize` handler (`mod.rs:754-763`), which also drops SGR-pixels on inexact geometry

The state is split across `state.mouse_capture_active`, `state.endpoint_*_requested`, two `*_preference` bools, and two `Arc<AtomicBool>` mirrors read by the stdin thread. On resize the loop does not recompute from `endpoint_sgr_pixels_requested`, so regaining exact geometry never re-enables SGR-pixels until the next server `MouseCapture`. That looks like a latent bug. Owner: a `HostMouseMode` struct holding the requests and preferences, with a single `desired()` and `apply()`, publishing to the atomics itself.

d. **"What is this input chunk?"** One byte chunk is classified independently by:
- `RawInputByteFramer` (framing)
- `send_unix_input_chunks` (palette and default-colour replies via `terminal_theme` string parsers, `client/input.rs:222-237`)
- `classify_unix_input` (SGR pixel mouse via `input::mouse::parse_report`)
- `parse_raw_input_bytes_sync` in the main loop, which builds a fresh framer and reparses

The shell path at `mod.rs:586` and `mod.rs:592` parses the same `data` twice back to back, which is plain waste on the per-keystroke path. Owner: the framer should emit typed `RawInputEvent`s once, on the reader thread, carrying their raw bytes for forwarding. The main loop then never reparses.

e. **"Where does a control string or bracketed paste end in host input?"** `terminal_setup.rs` has its own `PASTE_START/PASTE_END` constants and `host_control_string_end` (OSC/DCS/APC/PM/SOS with BEL/ST) for the keyboard probe. `raw_input.rs` has `BRACKETED_PASTE_START/END` and `ControlStringFamily` for the same grammar. The probe should run through `RawInputByteFramer`, or share its scanner.

f. **"Is this client federated / is Local fatal?"** `federated = endpoint_catalog.has_enabled_ssh()` is computed in `run_client_with_mode` and again in the loop. The loop value is updated on catalog change, but the startup copy had already chosen fatal vs non-fatal connect behaviour. "Local failure ends the client" is decided at:
- `mod.rs:1040`
- `mod.rs:1282-1283` (together with `endpoint::protocol_failure_is_fatal`)
- `mod.rs:1405`

Owner: one `LocalFailurePolicy` query on the registry or supervisor.

g. **"Is this the remote-client process?"** `is_remote_client_process()` is called in `run_client_with_mode`, `run_client_loop`, and `handshake_read_timeout`. The env var is also read in `errors.rs` for the reattach message. Owner: resolve once at startup into the loop config.

h. **"Should we query the host theme / cell size?"** `should_query_host_terminal_theme()` and `should_query_host_cell_size()` both return a constant `true`, yet they are consulted in several places (`terminal_setup.rs:55,69`, `mod.rs:416`). They are vestiges of removed platform branches; delete them. Also, `host_cell_size_query_required(kitty_graphics_enabled)` is passed `pixel_geometry_enabled`, so the parameter name is stale.

i. **"Which endpoint messages are accepted now?"** Acceptance is gated three times:
- `write_stream.accepts(...)`
- `endpoint::accepts_endpoint_message(...)`
- per-arm checks such as `!endpoint_active || presentation_frozen` for surfaces and a separate `presentation_frozen` check for patches

The freeze rule for pane surfaces vs patches vs presentation effects is spread across `mod.rs:960-1006` and the prose in `state.rs:144-175`. Owner: a single `PresentationGate` returning Apply/Drop/Buffer per message.

## 3. Structure

- **`mod.rs` is a ~1000-line `run_client_loop` with one giant `match`.**
  - All loop state lives as locals: `write_stream`, `pending_activation`, `scheduled_activation`, `endpoint_commands`, `supervisors`, `selection`, `catalog_watch`, `federated`, `next_surface_serial`, and the three atomics.
  - `shell_runtime.rs` functions therefore take 7-10 `&mut` parameters each (`begin_endpoint_activation` takes 10).
  - The recommendation is a rewrite into a `ClientLoop` struct that owns all of this, with one method per event (`on_stdin`, `on_resize`, `on_server_message`, `on_timer`), and a separate `SessionMode` enum `{ Shell(ShellSession), DirectAttach(AttachSession) }`.
  - Today "shell vs direct attach" is `state.shell.is_some()` / `state.attach_escape.is_some()`, re-checked in about 15 branches, while both Options coexist in `ClientState`. The two modes share only transport and terminal output; the type should say so.
  - `ClientState` also mixes presentation (blit encoder, freeze, repaint), host-terminal modes (mouse, keyboard, title) and mode-specific data. Split these into `Presenter`, `HostModes` and the session type.
- **`run_client_with_mode` takes `attach_request: Option<(String,bool)>` and `attach_escape: Option<AttachEscapeState>`.** It then discards the passed `AttachEscapeState` (`attach_escape.map(|_| AttachEscapeState::from_config(..))`), so the parameter is really a bool. A `ClientMode::{Shell, Attach{terminal_id, takeover}}` enum would replace both.
- **Startup config is split three ways:** `ClientLoopConfig` duplicates fields that are then copied into `ClientState` (mouse_scroll_lines, redraw_on_focus_gained, pixel_geometry_enabled, mouse_capture_active), and positional arguments carry the rest. One immutable `ClientSettings` plus mutable state is enough.
- **`client/input.rs` has a stale module doc** ("the server handles semantic parsing… avoid duplicating parsing logic in the client"). The client shell parses input fully (`shell.handle_raw_events`). The initial-host-input block (lines 82-131) duplicates the loop's flush and held-escape logic nearly line for line. Also, `stdin_reader_loop` is a pointless wrapper around `unix_stdin_reader_loop`, a leftover from platform removal.
- **Thread spawning is inconsistent:** `transport::start_endpoint_transport` spawns a named reader thread, while `mod.rs:882` spawns the same `server_reader_thread` unnamed with the arguments duplicated. The reader also collapses EOF, decode errors and IO errors into `ServerDisconnected`, losing the cause the loop later needs for its "reconnecting" message.
- **Dependency edges:**
  - `client/shell.rs` imports `crate::app::state::Palette` and `ClientShell*` protocol projection types. The client depends on the server's app state module, so `Palette` should live in a shared theme module.
  - `errors.rs` depends on `crate::server::socket_paths` and `crate::session`. Error `Display` reads env vars and the socket path at format time, which makes messages nondeterministic in tests. Build the message at construction time instead.
- **`raw_input.rs` is crate-root but mixes concerns:** the host-reply tracking (awaited colour and cell-size replies, appearance-on-focus) is client-host-terminal state living inside a generic byte framer. Move it to `src/input/` next to `parse`/`mouse`, and split host-reply accounting into a client-side wrapper.
- **`client/shell/` is a full second UI** (sidebar, copy mode, menus, workspace navigation) with 25+ submodules under the client. Whether it duplicates `app/` rendering and copy-mode logic deserves its own scope. The deep `shell::ClientShell*` re-export via `pub(crate) use state::*` makes the boundary porous.
- **Tests:** `client/input.rs`'s test `stdin_input_event_carries_raw_bytes` tests enum construction only. The loop is tested via `mod tests` against `ClientState::test_new()`, whose presentation writes to `io::sink()` under `#[cfg(test)]` inside production code (`state.rs:250-253`). A `Presenter<W: Write>` would remove that cfg.

## Other flags

- The `set_mouse_capture` else-arm (`terminal_setup.rs:343-346`) is a no-op `match`.
- `remember_direct_notice` uses `Vec::remove(0)`; a `VecDeque` fits.
- `mod.rs:292` prints `direct_notices` to stderr even on clean exit. That is presumably intended, but notices collected before a successful detach also print.
- `refresh_host_mouse_capture` is re-emitted on every Resize event, even when nothing changed.
