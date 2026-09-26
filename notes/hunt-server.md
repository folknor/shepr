I reviewed the server scope by reading `src/server/mod.rs`, `headless.rs` (the struct, the run loop and the API-drain path), `clients.rs`, `api/mod.rs` and the request entry in `api/server.rs`, plus two greps across `src/`. I did not open `client_transport.rs`, `render_stream.rs`, `pane_input.rs`, `alt_screen_read.rs`, `ipc.rs`, `events.rs` or the `headless/` submodules beyond their names, so treat what follows as the high-signal findings, not a complete audit. No files were edited.

## 1. Axes that should be types

- **Ids as bare `u64` and `String`.** In `HeadlessServer` (`src/server/headless.rs`):
  - Client ids are `u64` in `clients`, `foreground_client_id`, `tab_geometry_controllers`, `terminal_attach_owners` and `sent_window_title`.
  - `tab_geometry_controllers` is `HashMap<String, u64>` keyed by tab id; `terminal_attach_owners` is `HashMap<String, u64>` keyed by "terminal id string".
  - `ClientConnectionMode::TerminalAttach { terminal_id: String }` stores a string even though `AltScreenReadSpec` in the same file already uses a typed `crate::terminal::TerminalId`.
  - `ClientShellLocation`/`ClientShellTopology` (`src/server/clients.rs`) are maps of `String` to `String` holding workspace ids and tab ids. That is exactly where a key and a value can be swapped without the compiler noticing.
  - `next_activity_stamp`/`last_activity` is another bare `u64` that can be mixed up with client ids.
  - Fix: add `ClientId`, `ActivityStamp`, `WorkspaceId`, `TabId` newtypes and use `TerminalId` everywhere. `ClientId` should be minted only by the accept path.
- **Sizes as bare tuples.** `(u16, u16)` is used for `headless_size`, `effective_size` and `ClientConnection::terminal_size`. `host_keyboard_protocol_active` is `Option<(u16, u8)>`. Nothing says which element is cols and which is rows. Use a `TermSize { cols, rows }` and a named keyboard-protocol-state struct.
- **`RenderTarget`** (`clients.rs:17`) is a five-element tuple: `(u64, (u16,u16), HostCellSize, bool, ClientConnectionMode)`, where the bool is anonymous. Make it a struct.
- **`ClientConnection` is about 25 loose fields, mostly bools and `Option<bool>` caches.**
  - The flags `shell_surface_active`, `shell_mouse_capture`, `shell_location`, `shell_snapshot`, `shell_agent_completions`, `shell_projection_revision`, `shell_endpoint_command_in_flight`, `shell_uses_endpoint_keybindings` and `shell_held_inputs` only mean anything in `ClientShell` mode.
  - `host_keyboard_protocol_active` only means something for attach mode.
  - The constructor comment admits the problem: it has to force `shell_surface_active` so the flag stays truthful for a terminal stream.
  - Fix: make the mode enum carry its state, as `ClientConnectionMode::Shell(ShellState) | TerminalPending | TerminalAttach(AttachState)`. That makes "attach client with an active shell surface" impossible to represent.
- **`writer: Option<ClientWriter>` exists only for test fixtures,** and production code carries `writer.is_none()` checks to serve them. Give tests a channel pair (or a trait/null writer) and make the field non-optional.
- **Duplicate render-flag enums.** `DeferredRender { None, Full }` and `RenderImpact { None, Full }` are two identical two-state enums, backed by a `render_pending: bool`. The run loop also keeps `needs_render`/`needs_full_render` as two bools, which allows a meaningless "full but not needed" state. Use one `RenderDemand` lattice (None < Partial < Full) with a join operation.
- **API responses travel as prose `String`s.** `ApiRequestMessage::respond_to` is `Sender<String>` (`src/api/mod.rs:67`), so every handler serialises its own JSON. Some fall back to the literal `"{}"` on failure (`headless.rs:2243`), which is not a valid response and has no id; others use hand-written JSON literals (`headless.rs:2186`).
  - Error codes are string literals scattered across about 60 sites: `"server_unavailable"` in headless, subscriptions and wait; `"internal_error"` about 10 times; `"agent_not_found"` in `app/agents.rs` and `api/wait.rs`; `"invalid_request"` in `api/server.rs` and `client_transport.rs`.
  - A client cannot branch on a code that is not an enum, and a typo mints a new code silently.
  - Fix: a `#[serde(rename_all = "snake_case")] enum ApiErrorCode`; `respond_to: Sender<Response>` with a typed `Result<ResponseResult, ApiError>`, serialised once at the socket boundary.
  - The SSH-agent path in `api/server.rs:364-383` also classifies failures by `io::ErrorKind` into codes. That should be a typed error from the registry instead.

## 2. Decisions made in more than one place

- **"Does this API method mutate UI state?"** `api::request_changes_ui` (`src/api/mod.rs:22`) is a hand-maintained allowlist that sits apart from the dispatch that actually performs the mutation.
  - It is consulted in two places: `api/server.rs:360` (logging) and `headless.rs:2230` (the render decision).
  - Its only guard is spot-check tests (`app/mod.rs:699-704`, `app/api/panes.rs:2427`) that cover a few variants, which is the pairwise-agreement pattern.
  - A new mutating method that is not added to the list will not repaint until something else marks the view dirty.
  - The list is missing `ClientWindowTitleSet`/`ClientWindowTitleClear`, which headless special-cases to `return true` before the list is consulted, so a third site is answering the same question.
  - `api_method_name` (`api/server.rs:571`) is a third per-method table.
  - Owner: each handler should return an outcome that carries its render impact, or `Method` should get exhaustive `fn traits(&self) -> MethodTraits { name, mutates_ui, runs_on_socket_thread }` with no `_` arm, so adding a variant forces a decision.
- **"Where is a method handled?"** Three places route methods:
  - `api/server.rs` handles some methods on the socket thread (`ServerSshAgentRegister`, and probably the wait and subscription paths).
  - `headless.rs:2213` intercepts window-title methods, `AgentPrompt` and agent-read idle checks.
  - `app/api/*` handles the rest.

  Nothing enumerates the split, so a method can be half-handled by two layers. Make it one exhaustive routing match.
- **"Server is shutting down."** The rejection is built at least twice: `headless.rs:2177` and `api/subscriptions.rs:453` / `wait.rs:875` (`server_unavailable`). There are also several "should quit" sources, all polled separately in the loop (`app.state.should_quit`, `should_quit`, `signal_quit_requested`, `shutting_down`, `host_shutdown_requested`). Worth a single `ShutdownPhase` state machine.
- **"Is this client an active shell / the foreground?"** It is answered by `is_shell_client`, `is_active_shell_client`, the `shell_surface_active` flag, `latest_shell_client` and `render_targets`, plus `foreground_client_id` and `tab_geometry_controllers`, which each record which client owns what. I did not audit whether they already disagree, but the constructor comment shows one near-miss. The state-carrying mode enum from section 1 would own it.
- **Protocol-to-domain mapping.** `update_host_theme` (`clients.rs:362-393`) maps protocol colour and appearance enums inline. If the client side maps the same enums, that is a second mapping. Put `From` impls next to the protocol types.
- **Config diagnostic variants.** `server_config_diagnostic` and `_without_keybindings` are two precomputed copies of one decision ("which diagnostics does this client see"), with the choice made at each snapshot. It is minor, but it is better as a function of `(diagnostics, KeybindingSource)`.

## 3. Structure

- **`headless.rs` is a god object.** `HeadlessServer` owns:
  - the socket listener
  - the client registry
  - the foreground policy
  - tab-geometry arbitration
  - attach ownership
  - window-title state
  - config diagnostics
  - alt-screen read queues
  - dirty flags for PTY wake sources and input modes
  - shutdown and freeze state
  - signal flags
  - the event channels

  The file runs past 2200 lines even with 8 submodules (it also holds the API dispatch interception and the agent-idle check). The `headless/` submodules split by activity (render, lifecycle, bootstrap) rather than by the state they own, so all state stays on one struct and every submodule reaches into all of it.
  - Recommended rewrite: a `ClientRegistry` (clients, id minting, foreground selection, activity stamps, geometry controllers, attach owners: everything that answers "who owns what") with its own tests; an `ApiDispatcher` (routing, deferral, alt-screen reads, typed responses); a `ShutdownController`; and a thin loop that composes them.
  - Much of the registry logic is pure and could be tested the way `AppState` is.
- **The API/app boundary is inverted.**
  - `src/api/` holds the schema and the socket server, while request handling lives in `src/app/api/` and partly in `server/headless.rs`.
  - `api::request_changes_ui` is tested from `app/`.
  - The API layer knows `crate::session::active_api_socket_path` and headless knows `api::schema` internals.
  - Make `api/` pure schema plus transport, and give request execution one home: an `api::handle(&mut App, Request) -> Outcome` that the server calls.
- **Socket-path ownership is split three ways.** `api::socket_path()` delegates to `session`, `server/socket_paths.rs` owns the client socket, and `api::SOCKET_PATH_ENV_VAR` sits in api. Both socket paths and their env overrides belong in one module.
- **Protocol concerns sit on `ClientConnection`.** `ClientConnection` in `clients.rs` also implements held-key tracking (`track_shell_input`) and theme merging. Held-input tracking is a small state machine for input semantics that belongs with `pane_input.rs`. Theme merging belongs in `terminal_theme`.
- **The tests mirror the god object.** `headless/tests/mod.rs` is at least 3400 lines of whole-server tests, because nothing smaller is constructible. Splitting the state out would let most of them become unit tests.

## Other things noticed

- **Possible bug:** `headless.rs:2243` sends `"{}"` as the API response if serialisation fails, so the caller gets no id and no error. Also, `respond_to.send` results are ignored everywhere, which is fine, but nothing records that the caller disconnected.
- `client_shell_boot_id` is a `pid-nanos` string built inline. It would be clearer as a typed `BootId` minted in one place.
- `CLIENT_ACCEPT_POLL_INTERVAL` polls the listener every 250ms because it is not wired into `tokio::select!`. That is a steady idle wakeup; a tokio `UnixListener` (or an `AsyncFd` on the listener fd) would remove it.
