# Design hunt: server-serving (`crates/shepr-server/src/server/`)

Scope read in full (tests excepted): `mod.rs`, `client_commands.rs`,
`client_shell.rs`, `client_transport.rs`, `clients.rs`, `input_wire.rs`,
`outbox.rs`, `pane_input.rs`, `render_stream.rs`, `headless.rs` and every
non-test file under `headless/` (`api_dispatcher`, `bootstrap`,
`client_views`, `endpoint_requests`, `internal_events`, `lifecycle`,
`lifecycle/host_shutdown`, `render`, `retained_surface`, `surface_interest`,
`worker`). I followed edges into `shepr-protocol` (ids, identity, geometry,
message, command, surface_reuse), `shepr-api` (client protocol gate, stop
signal, response plumbing), `shepr-core::geometry`, `shepr-termio`'s
`HostCellSize`, `shepr-mux`'s content revision, and the `app/` and `ui/`
functions this directory calls.

## Headlines

1. **`HeadlessServer` is the god object AGENTS.md forbids for `App`.** It has
   27 fields, and ten files add `impl HeadlessServer` blocks through
   `use super::*`, each reaching into any field. Four separable machines
   (client views and geometry arbitration, render scheduling, endpoint reply
   dispatch, lifecycle) share one `&mut self`. Most of the duplicated
   decisions below exist because nothing gives those machines a home of
   their own.
2. **The PTY size rule is written twice.** `workspace_geometry_source` and
   `reapply_controlled_shell_workspace_geometry` each implement "controller if
   still viewing, else lowest outer-focused viewer, else lowest viewer". The
   rule for when a client may claim geometry is spread over six call sites.
3. **Client geometry is typed at the wire, untyped in the server, then retyped.**
   `TerminalGeometry` arrives with `GridSize`, `Option<CellPx>` and the "pixel
   mouse needs a known cell" invariant. The transport turns it into
   `u16`/`u32`/`bool` fields on `ServerEvent`, and the loop rebuilds
   `GridSize::clamped` and a zero-sentinel `HostCellSize`, then re-derives the
   pixel-mouse invariant. Pixel-mouse eligibility is decided at five sites.
4. **Two cursor deciders disagree.** The retained patch path decides the
   cursor in `retained_cursor`, and the full render decides it in
   `ui::surface_cursor`. Only the full render implements the CJK IME reveal.
   The two are kept from disagreeing on the wire by a third site:
   `render_pass_with_boundary` drops retained patches entirely when
   `reveal_hidden_cursor_for_cjk_ime` is on.
5. **Several types collapse back to primitives.** `PublicPaneId` and `BootId`
   deref to `str`, and the server passes typed pane ids to
   `App::parse_pane_id(&str)`, which parses them again. Projection revisions
   are unwrapped with `.get()` into `u64` fields. Content revisions use odd
   parity to mean "torn". `HostCellSize` uses `0x0` to mean unknown.
   Retained-render fallback reasons are a `&'static str` set.
6. **`AppState::should_quit` is dead redundancy.** Only `initiate_shutdown`
   ever sets it, and that function also sets the phase to `Stopping` and
   raises the stop signal. All nine `stop_requested(self.app.state.should_quit)`
   calls pass a value that adds nothing. The lifecycle doc that calls it "the
   in-process input path" is herdr residue.

---

## 1. Axes that should be types

### 1.1 Client surface geometry (`ServerEvent::ClientShellConnected` / `ClientShellResize`)

`ServerEvent` carries `surface_cols: u16, surface_rows: u16,
cell_width_px: u32, cell_height_px: u32, pixel_mouse: bool`. On the wire,
`EndpointClientHello.geometry` and `ClientMessage::ClientShellResize` already
carry `shepr_protocol::TerminalGeometry { grid: GridSize, cell: Option<CellPx>,
pixel_mouse }`, and that type's `TryFrom` refuses `pixel_mouse` without a
cell. Along the path:

- `handle_client_handshake` and `client_read_loop_with_endpoint_controls`
  call `geometry.width()`/`height()`, which map `None` to `0`. They feed those
  to `ProtocolCellSize::from_wire`, which re-checks `MAX_CELL_SIZE_PX` and
  re-derives `exact`. Then `client_shell_geometry_error` checks the same
  limit on the raw numbers.
- `HeadlessServer::apply_server_event` rebuilds
  `GridSize::clamped(surface_cols, surface_rows)`, which is already a
  `GridSize` on the wire. It builds `HostCellSize { width_px, height_px }`
  with `0` meaning unknown, and re-derives `pixel_mouse &&
  observed.is_known()`. It does this in both the connect arm and the resize
  arm.

**Proposed type:** one validated `ClientSurfaceGeometry { grid: GridSize,
cell: Option<CellPx>, pixel_mouse: PixelMouse }` (or `TerminalGeometry`
itself, after the size limits move into its decode). It is minted once, in
protocol decode, and stored as is on `ClientConnection`, replacing
`terminal_size`, `cell_size` and `pixel_mouse`. Code that needs a render size
asks it for a `Rect`, and code that needs spawn geometry asks it for a
`SpawnGeometry`. Today `client_geometry`, `render_full`,
`render_client_full` and `RenderTarget` each do `Rect::new(0, 0,
cols.get(), rows.get())` by hand.

### 1.2 Handshake refusal reasons are prose

`client_shell_geometry_error` returns `Option<&'static str>`, and the server
wraps the string in `HandshakeRefusal::InvalidSurface(String)`. The client
can only show the text and cannot tell "empty surface" from "too large"
from "cell too large". The empty branch is also unreachable, because
`GridSize` is `NonZeroU16` and a zero fails decode. The refusal should be
an enum such as `InvalidSurface(SurfaceRefusal::{TooManyCells,
DimensionTooLarge, CellTooLarge})`. Better still, the limits should live in
`TerminalGeometry`'s decode, so an oversized geometry is a decode error and
the transport never sees one.

### 1.3 Shutdown reason is prose, spelled four times

`ShutdownReason` has one variant, `Message(String)`. The literal "server is
shutting down" is built in `complete_shutdown`, in
`HeadlessServer::send_shutdown_to_unregistered_client`, and in
`client_transport::send_shutdown_to_unregistered_client`. The same text is
also built as an `ApiError` in `ShutdownLifecycle::shutdown_error` and in
the fallback of `reject_api_request_for_shutdown`. `ShutdownReason` should
be a closed enum (`Stopping`, plus whatever else exists), and the reason
should come from one constructor.

### 1.4 Endpoint rejections that a caller could branch on travel as `Rejected(String)`

- `"checkout root worker limit reached; retry later"` is a typed "busy, retry"
  condition (`EndpointError::Busy`).
- `"failed to start checkout root worker: {error}"` is a server-resource
  failure.
- `response_within` sends `"the response could not be encoded"` as
  `Rejected`, which is an internal error rather than a user refusal.
- `CheckoutRootRunner` is `Fn(PathBuf) -> Result<Option<String>, String>`,
  and `WorkerCompletion::CheckoutRoot` carries `home: Option<String>` and
  `result: Result<Option<String>, String>`. Paths travel as `String`, and the
  error is prose that ends up in `EndpointError::Rejected`.

### 1.5 `RunServerError::Io` hides distinguishable startup failures

`run_server` folds together a bad session target (`check_session_target`),
pane-launch init, a failed runtime build, a failed signal-handler install
(inside `run`) and loop failures into `RunServerError::Io`. Its two typed
outcomes are recovered by downcasting `io::Error` payloads after the fact
(`SocketBusy::from_io`, `DataDirLeaseHeld::from_io`). `DataDirLease::acquire`
and `shepr_api::start_server` should return their own typed errors, so the
"is another server running" decision is made where the failure is known. The
binary can then choose exit statuses without matching on prose.
`complete_shutdown` also turns the typed `UnexpectedPhase` into
`io::Error::other`.

### 1.6 Pane input failures

`PaneInputError::{Backpressure(&'static str), Closed(&'static str),
Other(String)}`. The `&'static str` is a kind label ("mouse input", "key
input", "paste", ...), which is a closed set, so it should be an
`InputKind` enum. `Other(String)` covers two cases. "Failed to encode mouse
wheel event" is a typed encode failure. "Non-pane input reached targeted
pane input" is only reachable because of the `RawInputEvent` detour
described in section 3.

### 1.7 Lifecycle step names

`UnexpectedPhase { step: &'static str, .. }`. The step "freezing for host
shutdown" is spelled twice, in `finish_host_shutdown_freeze` and in
`freeze_for_host_shutdown`. A `LifecycleStep` enum would name each
transition once.

### 1.8 Bare tuples standing for named facts

- `focused_panes: HashSet<(WorkspaceId, PaneId)>` and the return type of
  `panes_holding_focus`. This is the same pair as `ShellFocusTarget` and
  `ClientPaneIdentity`, giving three spellings of "a pane qualified by its
  workspace".
- `sync_terminal_title_sources -> (bool, bool)` (sidebar changed, outer title
  synced).
- `drain_internal_events_with_forwarding_up_to -> (bool, bool)`. The first
  element is never read by any caller.
- `set_client_shell_surface_active -> Option<(bool, u64)>`.
- `ClientRegistry::remove_client -> (Option<ClientConnection>, bool)`.
- `SharedSurfaces.oversized_notices: Vec<(ClientId, usize, usize)>`.
- `PaneSurfaceRenderKey = (Option<WorkspaceId>, u16, u16, u32, u32)` and the
  retained layout cache key `(usize, u16, u16)`. The latter is built from
  `workspace_id.number()`.
- `Told.mouse_capture: Option<(bool, bool)>`, commented as "(enabled,
  sgr_pixels)", and `tell_mouse_capture(enabled: bool, sgr_pixels: bool)`.
  This should be a `MouseCaptureMode` value that
  `stream_host_mouse_capture_mode` computes and the outbox compares.
- `downgrade_ineligible_pixel_mouse(.., runtime_pixels: Option<(u32, u32)>)`.
- `runtime.synchronized_output_state() -> Option<(bool, epoch)>`, where `None`
  means poisoned and the bool means active. Its callers
  (`render_pane_surface`, `workspace_surface_held`) each decode the three
  states by hand. An enum `SyncState::{Idle(epoch), Active, Poisoned}` would
  carry the meaning.

### 1.9 Boolean-flag parameters

`claim_shell_workspace_geometry(id, false)`,
`resize_shell_workspaces_sized_for(id, true)`,
`reapply_controlled_shell_workspace_geometry(true)`,
`finish_shell_workspace_geometry_change(bool, bool)` and
`apply_shell_geometry(bool)`. Every call site passes a literal for
`start_pending_agent_resumes`, and nothing at the call site says what it
means.

### 1.10 Several states encoded as independent bools and options

- `ClientShellState.outer_terminal_focus: Option<bool>` means unknown,
  focused or unfocused. Five sites compare it against `Some(true)`.
- `host_terminal_appearance: Option<HostAppearance>` together with
  `host_terminal_appearance_explicit: bool` is really
  `Option<(HostAppearance, Provenance::{Inferred, Explicit})>`.
  `set_host_appearance` exists only to keep the pair coherent.
- `HeldReply { ready: Option<Vec<u8>>, refusal: Option<Vec<u8>> }` has two
  legal states, `Pending { refusal }` and `Ready(bytes)`. The type allows
  four.
- `ClientRenderState { last_surface, recompute_pending, debt }`:
  `surface_debt()` and `takes_patches()` each re-derive a combined state
  from three fields, and `refuse()` sets two of them at once.
- `PreparedRender::Semantic { message, committed_surface:
  Option<Box<PaneSurfaceFrame>> }`. The `None` case is recovered in
  `commit_sent_frame` by pattern-matching `message` for `PaneSurface`, with
  a logged "did not contain a surface baseline" branch for the impossible
  case. A `Full(PaneSurfaceFrame)` / `Delta { message, next_baseline }`
  split would make that branch unrepresentable.
- `ShutdownLifecycle { phase, freeze: Option<HostShutdownFreeze> }`.
  `freeze` is only meaningful in `Frozen`, and in `Stopping` after a freeze.
  `frozen_session_policy` and `frozen_warning_generation` each re-check the
  phase. The phase enum should carry its payload, for example
  `Frozen(HostShutdownFreeze)` and `Stopping { frozen: Option<..> }`.
  `HostShutdownFreeze.persist_session: bool` also mirrors `AppPolicy`, and
  `restored_policy` maps it back.

### 1.11 Generations and revisions as raw `u64`

All of these are compared across structs:

- `ClientShellLocation::generation() -> u64`, copied into
  `projected_location_generation: u64` and
  `CachedShellProjection.location_generation`.
- `shell_session_generation: u64`, compared with
  `ClientShellState.session_generation: u64`.
- `ShellSessionCache.revision: u64`, compared with
  `AppState::shell_projection_revision: u64`.
- `PendingCheckpointedPaneExit.checkpoint_generation: u64` and
  `replaying_checkpointed_pane_exit: Option<u64>`.
- `HostShutdownFreeze.generation: Option<u64>`, `warning_generation() ->
  u64`, `release_delay_lock(generation: u64)`.

Each counter should have its own newtype, so that a location generation
cannot be compared against a session generation.

---

## 2. Decisions made in more than one place

### 2.1 The PTY size rule's fallback (which viewer sizes a workspace)

**Question:** when a workspace's remembered controller no longer views it,
which client's geometry sizes it?

**Sites:**

- `client_views::workspace_geometry_source`: controller if viewing, else
  lowest-id outer-focused viewer, else lowest-id viewer.
- `client_views::reapply_controlled_shell_workspace_geometry`: same rule,
  re-implemented over a sorted `viewers` list (`find` focused, else
  `viewers[0]`). It then writes the answer back with
  `set_geometry_controller`.

**Agreement:** the two agree today. The source function keeps its own
fallback because "navigation can make it stale before any geometry
settlement runs", so both copies are live.

**Related fragments of the same policy:**

- `surface_interest::set_client_shell_surface_active` computes
  `focused_viewer_already_owns_workspace` (another outer-focused viewer
  blocks the claim).
- `ClientRegistry::claim_geometry` and `claim_unowned_geometry` check
  `is_active_shell_client`, but `set_geometry_controller` does not.
- `handle_client_shell_command` has a four-branch claim policy keyed on
  `traits.claims_shell_geometry` and `changes_topology`, including a
  `let _ = self.clients.claim_geometry(..)` whose result is ignored.
- The triggers that claim geometry are focus gain (`ClientShellFocus`), pane
  interaction (`ClientShellPaneInput`), connect (unowned only), activation
  (unless a focused viewer exists), and navigating or acting commands.

**Owner:** a `GeometryArbiter` that owns `geometry_controllers` and the
selection rule. Its single entry point would be
`claim(client, ClaimReason)`, plus one `settle(&views) -> Vec<(WorkspaceId,
GeometrySource)>`. `ClientRegistry` currently holds the map, while the rule
lives on `HeadlessServer`.

### 2.2 Which clients count (presenting, active or attached)

**Question:** does this connection take part (present a surface, size panes,
receive titles, make a pane visible)?

**Sites, with the predicate each uses:**

- `presents_surface` (active and attached): used by
  `workspace_geometry_source` and `automatic_creation_source`.
- Inline `is_active_shell_client() && outbox.is_attached()`:
  `ClientRegistry::app_client_count`, `pane_viewers`,
  `reapply_controlled_shell_workspace_geometry`,
  `sync_immediate_pty_sources`, `any_shell_surface_contains_pane`.
- Active only: `latest_shell_client` (and so `create_automatic_workspace`'s
  gate and `shell_cwd_refresh_deadline`), `promote_to_foreground`,
  `claim_geometry`, `window_title_clients`, `panes_holding_focus`,
  `stream_shell_keyboard_mode`, and `stream_host_mouse_capture_mode` (which
  reads `surface_active` directly).
- Attached only: `render_targets`. `render_plan` and `render_full` then
  filter by active again.

**Disagreement:** `attached` is false only for test fixtures (see 4.6), so
production answers agree. The predicates are still chosen independently at
about fifteen sites, and the choice between them is accidental. For example,
`create_automatic_workspace` gates on active-only but picks its source with
active-and-attached.

**Owner:** one `ClientRegistry::presenting()` iterator, and no `attached`
state at all.

### 2.3 Which panes a surface of workspace W shows

**Sites:**

- `sync_immediate_pty_sources`: zoomed means the focused pane, else
  `layout().pane_ids()`.
- `visible_pane_runtimes`: same rule, written again.
- `any_shell_surface_contains_pane` and `shell_client_views_pane`:
  `workspace.shows_pane`.
- `retained_pane_layout`: `pane_geometry_in(area).visible_panes(layout,
  zoomed)`.
- `ui::compute_surface_for`: the authoritative layout.

**Agreement:** the sites agree because `shows_pane` and the zoom branch were
written to match.

**Owner:** `Workspace::visible_pane_ids()`, used everywhere.
`Workspace::shows_pane` already exists in mux; the other two sites should
call a sibling of it.

### 2.4 Cursor shown for a pane

**Question:** what cursor state does a client see?

**Sites:**

- `ui::surface_cursor`, the full render. It covers synchronized output,
  scrollback hiding, and the CJK reveal with its agent filter and shape.
- `retained_surface::retained_cursor`, the patch path. It covers
  synchronized output and scrollback hiding only.

**Disagreement:** the two already disagree on the CJK reveal. The
disagreement is hidden by a third site:
`render_pass_with_boundary` sends every patch candidate to the full step
when `reveal_hidden_cursor_for_cjk_ime` is set. With that experimental
setting on, every client loses retained rendering because one function
lacks the rule.

**Owner:** one cursor function over `(AppState, runtime, inner_rect,
pane_id)` that both paths call.

### 2.5 Scrollbar gutter and visibility

**Sites:**

- `ui::panes::stable_scrollbar_gutter` derives the gutter as the inner
  rect's last column when the content rect differs from `pane_inner`, and
  shows it when `should_show_scrollbar`.
- `retained_surface::resolve_retained_panes` re-derives
  `reserved_scrollbar_gutter` with the same arithmetic.
- `retained_scrollbar_patch` decides visibility as
  `max_offset_from_bottom > 0 && pane_scrollbars && !alternate_screen`
  instead of calling `should_show_scrollbar`.

**Owner:** a `scrollbar_gutter(pane_inner, scrollbars, alt) -> Option<Rect>`
next to `shepr_mux::workspace::terminal_content_rect`. The ui comment already
says that function exists "so the two cannot drift".

### 2.6 Wire pane metadata (`PaneSurfacePane`)

**Sites:**

- `client_shell::render_pane_surface` builds content revision (with the
  parity trick), mouse flags, alternate screen, scroll metrics with
  `as u64` casts, and pixel size.
- `retained_surface::render_patches` rebuilds the same metadata from the
  `TerminalDirtyPatch` snapshot, with its own copy of the
  `PaneSurfaceScrollMetrics` conversion and without the parity rule.

**Owner:** `impl From<&PaneRuntimeSnapshot> for PaneSurfacePane` (or a
builder), used by both.

### 2.7 Patch admission against a baseline

**Question:** may this patch apply to this baseline?

**Sites:**

- `ClientRenderState::prepare_pane_surface_patch`: `Baseline::accepts`,
  plus `patch.projection_revision != last.projection_revision`, plus
  `validate_patch_rows`, plus pane membership.
- `render_stream::apply_pane_surface_patch`: the same four checks again,
  plus a grid size check.
- `shepr_protocol::surface_reuse::Decoder::decode` on the client:
  `accepts`, a grid size check, and its own projection-equality
  condition.

The server-side repeat is described as a defensive re-check, which is
legitimate. However, each site composes the rule out of the same pieces,
and `Baseline::accepts` takes five positional arguments including two pairs
of same-typed revisions that can be swapped. So the rule is not owned
anywhere.

**Owner:** `Baseline::admits(&PaneSurfacePatch) -> Result<(),
PatchRefusal>` in protocol, called by all three.

### 2.8 Pixel-mouse eligibility

**Sites:**

- Connect and resize arms: `pixel_mouse && observed.is_known()`.
- `ClientShellPaneInput` arm: `client.pixel_mouse &&
  outbox.told_sgr_pixels()`. This uses the outbox's dedupe memo as an
  authority.
- `downgrade_ineligible_pixel_mouse`: geometry match and bounds.
- `apply_client_pane_input_event`: `runtime.sgr_pixel_mouse_enabled()`,
  else a cell position.
- `stream_host_mouse_capture_mode`: `client.pixel_mouse &&
  runtime.sgr_pixel_mouse_enabled()`.
- Upstream in protocol: `TerminalGeometry::new` and `try_from`,
  `ProtocolCellSize::from_wire`, and `from_host`.

**Owner:** pixel mode should be one value on the connection, recomputed when
its inputs change. Pane input should take that value, not re-derive it.

### 2.9 Input batch admission

`client_transport::pane_input_event_limit` and
`classify_input_event_size` sum `expanded_event_count` and `text_bytes`
against `MAX_INPUT_EVENT_BATCH` and `MAX_INPUT_PAYLOAD`. The client's
`shell/input/events.rs` batching and `shell/input/input.rs` paste pre-check
sum the same quantities against the same constants. The comment "Clients
pre-check pastes with the same `text_bytes` accounting" admits the pairing.

**Owner:** `shepr_protocol::InputBatchCharge` with `add(&event)` and
`fits()`, plus a classification of paste overflow versus input overflow.

### 2.10 Cell size limit

**Sites:**

- `client_shell_geometry_error` refuses oversize cells.
- `ProtocolCellSize::from_wire` nulls oversize cells.
- `ProtocolCellSize::from_host` clamps oversize cells.

**Disagreement:** these are three different policies for one limit. In the
transport, the refusal runs first, so the nulling branch is dead.

### 2.11 Does a host theme change owe clients a recompute?

**Sites:**

- The `ClientShellHostTheme` arm calls `request_recompute()` on every
  client when `sync_host_theme_from_foreground` reports a change.
- `promote_client_to_foreground` and `promote_latest_remaining_client`
  discard `sync_host_theme_from_foreground`'s result.
- The connect arm marks the view changed but requests no recompute.

**Disagreement:** these sites already disagree. A theme that changes because
the foreground changed gets an epoch bump only, while a theme report gets
epoch plus recompute. Either the recompute is needed in all paths, or it is
needed in none.

**Owner:** `set_foreground` returns a `ForegroundChange { foreground: bool,
theme: bool }`, and one function maps it to what clients owe.

### 2.12 Does a command change the shell projection?

**Sites:**

- The app returns `effects.shell_projection_changed`, and `app/api.rs`
  reconciles it against the revision counter
  (`effects.shell_projection_changed && !projection_revision_changed`).
- `handle_client_shell_command` special-cases `EndpointCommand::PaneScroll`
  by command variant. It takes `projection_before`, compares
  `shell_projection_revision` afterwards, and ORs in the flag.
- `internal_events::handle_internal_event_with_forwarding` compares
  `shell_projection_revision` before and after at two return points.

**Problem:** "changed" is both declared as an effect and inferred from a
counter diff, at three places. Separately, whether a change is viewer-local
(scroll) or shared is decided in the server by matching the command variant.
The app outcome should carry `Invalidate::PaneViewers(pane)` as an effect.

### 2.13 Does a projection need recomputing?

**Sites:**

- `render_plan`: `!settled || projected_location_generation !=
  location.generation() || snapshot.is_none()`.
- `render_client_full`: `session_generation != shell_session_generation ||
  projected_location_generation != ... || snapshot.is_none()`.

The plan does not check the session generation. It relies on every bump of
`shell_session_generation` also calling `mark_view_changed`. That holds at
both bump sites today (`refresh_stale_shell_session_cache` runs inside a
pass, and the timer path's caller marks the view changed), but nothing ties
them together. One `ClientShellState::projection_due(&SessionGeneration)`
should answer both.

### 2.14 What happens when a client leaves

**Sites:**

- `reap_closed_clients`: `remove_client`, then if not `Stopping`,
  `reapply_controlled_shell_workspace_geometry(true)` and
  `mark_view_changed`.
- `remove_client_and_resize_if_needed`: `remove_client`, then if not
  `Stopping`, `reapply(true)`. The caller's arm then marks the view
  changed.
- `set_client_shell_surface_active(false)`: removes the controllers, promotes
  the latest remaining client, and reapplies geometry. This is a third
  "client stops presenting" path, which does not go through `remove_client`.

**Owner:** a single `ClientRegistry` operation that returns a typed
departure outcome, so that one place applies its consequences.

### 2.15 Visible-state change bookkeeping (`immediate_pty_sources_dirty`, `host_input_modes_dirty`)

**Question:** did this change move what some client views?

**Sites:** the flags are set by hand at about ten sites: connect, resize,
`remove_client`, `create_automatic_workspace`, pane death, endpoint
navigation and reconcile, surface activation (computed outside
`set_client_shell_surface_active` from a `surface_active` captured before
it, although the function itself knows `changed`), plus `run`.

The doc admits that a missed mark only delays a repaint. That is exactly the
"answer kept in step by hand" pattern.

**Owner:** derive a cheap view key per client (location, active, workspace
zoom and membership generation), and recompute when the key changes.

### 2.16 When the PTY size rule is (re)applied

**Triggers:**

- Client events (`claim_*`, `resize_shell_workspaces_sized_for`).
- Command completion.
- Departures.
- Pane death.
- API topology change (`handle_api_request_with_shutdown_check` compares
  `workspace_order()` before and after).
- Inside the render pass. `render_full` re-applies a workspace's geometry
  when its source's baseline shows an alternate-screen flip, through a
  forty-line inline closure. `render_pass_with_boundary` applies headless
  geometry for workspaces without an area.

**Problem:** the alternate-screen trigger is the one that conflicts with
"Render is pure". It mutates PTYs from within the render pass, keyed on the
sent baseline instead of on runtime state.

**Owner:** a geometry settlement step that runs before the render plan,
driven by a "geometry inputs changed" signal. Alternate-screen transitions
are a PTY event that the mux can report.

### 2.17 Shutdown checks

**Sites:** `stop_requested(should_quit)` is evaluated nine times per loop
path. `handle_api_request_with_shutdown_check_inner` and
`handle_client_shell_command` each do "if `stop_requested`, then
`initiate_shutdown`, then if `Stopping`, reject".

`ClientShellSurfaceSet` and `WorkspaceCheckoutRoot` bypass the second check,
because they are dispatched before `handle_client_shell_command`. They are
safe only because `run` never dispatches server events once a stop is
requested. That makes the check in `handle_client_shell_command` a
re-check for some commands and the only check for none.

**Owner:** one gate at the dispatch entry.

### 2.18 Handshake classification

**Question:** how is a peer's preamble and hello answered?

**Sites:**

- `shepr_api::server::client_protocol::refuse_client`: reads the preamble
  and hello first, then writes the preamble and refusal in one write.
- `client_transport::handle_client_handshake`: reads the preamble, writes
  the preamble at once, then reads the hello.

Both decide what `DifferentBuild` and `NotShepr` mean independently, and
they order their writes differently.

**Owner:** one handshake function in `shepr-api` (or protocol). It would
return `Hello(EndpointClientHello)` or a classified refusal, and the server
handler would receive an already-validated hello.

### 2.19 Unencodable message closes the connection

`ClientOutbox::frame` and `ControlSender::send` each implement "encode,
on failure warn and close". `frame_server_message` is a one-line alias of
`encode_message`, kept so `SurfaceBoundary` has a function pointer to
replace.

---

## 3. Structure

### 3.1 Break up `HeadlessServer`

Separable machines in the 27 fields:

- **ClientViews / registry.** `clients`, `focused_panes`, the geometry
  controllers, the foreground client, and the theme sync. The decisions in
  `client_views.rs` are pure functions of (registry, workspace order,
  workspace focus). They do not need `App` beyond read-only workspace
  topology. `ClientRegistry` already claims to be "pure client identity and
  ownership state ... testable without a PTY", but the rules live on
  `HeadlessServer`.
- **RenderScheduler.** `view_epoch`, `headless_settled`,
  `shell_session_cache`, `shell_session_generation`, the retained fallback
  reason and set, `immediate_pty_sources_dirty`, `host_input_modes_dirty`.
  It also needs the render cadence (`last_render_at`,
  `last_presentation_at`, `can_render_now`, `can_present_now`,
  `record_render_attempt`, `next_headless_loop_deadline_with_git_refresh`),
  which lives on `App` even though only the serving loop uses it. That is
  state split from the behaviour acting on it.
- **EndpointDispatcher.** `worker_tx`, `worker_rx`,
  `checkout_root_runner`, and the reply tickets. `endpoint_requests.rs`
  and `worker.rs` belong together with `client_commands.rs`.
- **Lifecycle driver.** `lifecycle`, `host_shutdown_monitor`,
  `shutdown_unregistered_clients`, `shutdown_flushes`,
  `pending_checkpointed_pane_exits`, `replaying_checkpointed_pane_exit`.

With `App` passed explicitly instead of reached through `self`, each part
becomes testable alone. The 6464-line `headless/tests/mod.rs` (105 tests
covering shutdown, titles, endpoints, projections, retained patches,
geometry, input, theme, clipboard and host shutdown) exists because the
only test seam is a whole `HeadlessServer`. The sibling test files cover
only four topics, so the test layout mirrors accretion rather than design.

### 3.2 `replaying_checkpointed_pane_exit` is a parameter passed through a field

`handle_scheduled_tasks_headless` sets the field, then calls
`handle_internal_event_with_forwarding`, which `take()`s it. Three early
returns must clear it by hand. It should be an explicit argument, for
example `handle_internal_event(ev, Origin::Replay(generation))`. The
re-wrapping of `AppEvent::Runtime` around the event for re-queueing is also
written twice in the `PaneDied` arm.

### 3.3 Pane input takes a detour through `RawInputEvent`

`pane_input::apply_client_pane_input_event` handles `Mouse` and `TextCommit`
first. It then converts the rest through `input_wire::WirePaneInput`
(`ClientPaneInputEvent -> RawInputEvent`, the host-terminal input superset
with eight variants) only to get `Key` and `Paste` back out. This detour is
why `Other("non-pane input reached targeted pane input")` exists, and why
`input_wire` maps `TextCommit` to `Unsupported` and has a `Mouse` arm that is
never used. A direct `ClientPaneInputEvent::Key -> TerminalKey` mapping in
`pane_input` removes the trait, the file, and the unreachable error.
`apply_scroll` also round-trips modifiers through `u8` (`modifiers.bits()`,
then `from_bits_truncate`), and `lines.max(1)` is applied both by the
caller and by the callee.

### 3.4 `ServerEvent` mixes three channels

`ServerEvent` carries client transport events, plus `ClientWriterDrained`
(a per-frame wake from writer threads) and `HostShutdownWake` (the logind
monitor). Neither wake carries data: the loop already has `outbox_wake: Notify`,
which the queue raises on close and room. The writer thread does a reliable
`blocking_send(ClientWriterDrained)` on the 64-slot channel after every
render frame. With several clients at 60 fps, these sends compete with input
events for the channel and for `SERVER_EVENT_DRAIN_LIMIT`, and can block the
writer thread on a full channel. Replacing both wakes with `Notify` (the
existing `outbox_wake`, plus one for host shutdown) would shrink
`ServerEvent` to client facts and remove the matching filter in
`handle_server_event`.

### 3.5 `ClientConnection` and `ClientShellState` expose writable invariants

All fields are `pub(crate)`. `surface_active` is written directly in
`apply_server_event` and in `set_client_shell_surface_active`. The registry's
invariants (the foreground client is an active shell; inactive clients hold
no geometry control) are maintained by the server at those write sites, not
by the registry. `ClientShellLocation.focused_workspace_id` is
`pub(crate)`, so any write bypasses the generation it exists to move (only
tests write it today). `ClientShellLocation.index` and `generation` are
already private, so only one field leaks. Fix: make
`registry.set_surface_active(id, bool) -> ActivationChange` and the location
field private.

### 3.6 Production types shaped by test fixtures

- `ClientOutbox.attached: bool` is "false only for `detached()` fixtures".
  Every presenting predicate in production checks it (see 2.2). A test
  double should not be a production state. The fixtures should use
  `test_pair()` or `test_buffered`.
- `client_read_loop_with_endpoint_controls(.., endpoint_control_writer:
  Option<&ControlSender>)` is always `Some` in production.
- `HeadlessServer::new(.., api_server: Option<ServerHandle>, ..)` is `None`
  only in tests.
- `SurfaceBoundary` holds function pointers for encode and render, so a test
  can inject an oversize encode failure.
- `ClientRegistry::get/get_mut/contains_key/Index` are generic over
  `K: Copy + Into<ClientId>` only so tests can write `clients.get(&1)`
  (through cfg(test) `From<u64>` and `From<i32>` impls).

### 3.7 Constructor side effect

`HeadlessServer::new` opens the API socket's client gate, which makes the
server publicly reachable from inside a constructor. Tests rely on this
ordering. An explicit `server.open_client_protocol()` step in `run_server`
would make the documented startup order (lease, bind, restore, then open)
visible at the place that documents it.

### 3.8 `client_shell.rs` is misnamed and does two unrelated jobs

`snapshot_from_session` is the shell projection, and it belongs with
`ShellSessionCache` in `render.rs`. `render_pane_surface` and
`split_hit_rect` are the surface build. The split hit-rect geometry
(borders and gaps) is layout policy, and arguably belongs in
`ui`/`shepr-mux` next to the border rules that draw the splits.

### 3.9 Projection identity resolution is repeated

`snapshot_from_session` resolves `location.focused_workspace_id` twice
(once to filter, once to index). `shell_target_for_client` applies the same
filter. `focus_target_for_surface`, `shell_focused_runtime` and
`visible_pane_runtimes` each go from `WorkspaceId` to index to workspace to
runtime by hand. `send_pane_focus` finds the index by a linear `position`
instead of `workspace_index`. A `ViewedWorkspace<'a> { index, workspace }`
resolved once per client per pass would remove five near-copies.

### 3.10 The retained renderer re-validates what the full step already knows

`resolve_retained_panes` recomputes layout and checks each committed rect.
The alternate-screen closure in `render_full` re-checks the identities'
consistency (same length, single workspace) with its own copy of those
checks. Both should be methods on a `CommittedBaseline { surface,
identities }` type owned by `ClientRenderState`. Today
`surface_pane_identities` sits on `ClientConnection`, beside the render
state it is "aligned with", and is cleared separately in `request_repaint`.

### 3.11 Naming drift for one concept

The `Arc<ServerStopSignal>` is called `stop_requested`, `stop_request`,
`stop_signal` and `should_quit` across `headless.rs`, `lifecycle.rs` and
`client_transport.rs`. `should_quit` is also the name of the dead
`AppState` flag.

---

## 4. Types that resolve to primitives

### 4.1 `PublicPaneId: Deref<Target = str>` is used to re-parse typed ids

`HeadlessServer::release_client_shell_inputs`, the `ClientShellPaneInput`
arm, and `handle_client_shell_command` (for `PaneScroll`) call
`self.app.parse_pane_id(&pane_id)` with a typed `PublicPaneId`. Through
`Deref` this becomes `&str`, and `parse_pane_id` then parses it back into a
`PublicPaneId`. `App::resolve_pane_id` exists to avoid exactly this (its doc
says "so a typed id is never spelled and parsed again"). The `Deref` and the
`PartialEq<str>`/`<&str>`/`<String>` impls should go. Callers would use
`as_str()` where they mean text, and the compiler would point at every
re-parse.

### 4.2 `BootId: Deref<Target = str>`

`surface_reuse::Baseline::new(boot_id: &str, ..)` and `accepts(boot_id:
&str, ..)` take the boot id as text. `client_transport` bounds
`boot_id.len()` against `MAX_ENDPOINT_BOOT_ID_BYTES`. That check is dead,
because a `BootId` only decodes from its canonical `<pid>-<nanos>` form,
which is far below 128 bytes. `RequestId` is the one that needs the bound,
and the bound belongs in its decode, not in the transport loop.

### 4.3 `ProjectionRevision` is unwrapped and rewrapped

`shell.projection_revision.get()` is passed to `snapshot_from_session(..,
revision: u64, ..)`, which converts back with `revision.into()`. It is
stored as `CachedShellProjection.projection_revision: u64` and compared
with `.get()`. `EndpointReply::ClientShellSurfaceSet { projection_revision:
u64 }` puts the raw value on the wire. The protocol type should be used
end to end.

### 4.4 Content revision: parity sentinel and zero sentinel

`render_pane_surface` sends `after | 1` when the pane's content moved during
the draw, or when the revision was already odd. Mux only ever advances by
two (`wrapping_add(2)`), so odd means "torn, treat as changed". The retained
path sends `snapshot.content_revision` without this rule. A pane without a
runtime gets `0`, and `PaneRuntime::content_seq` maps a poisoned core to
`0`. The type should be `ContentRevision::{Stable(n), Torn}`, or at least a
newtype whose constructor owns the parity rule.

### 4.5 `HostCellSize` with `0x0` meaning unknown

`HostCellSize { pub width_px: u32, pub height_px: u32 }` has
`Default = 0x0 = unknown`, `is_known()`, and `or_default()`, which maps any
invalid size back to `0x0`. `shepr_core::geometry::CellPx` already models a
known size as `NonZeroU32`. The server converts into `HostCellSize`, and
then:

- `render_pane_surface` writes `pixel_width: 0` when unknown;
- `pane_surface_render_key` normalises with `or_default`;
- `ui::resize_pane_infos` passes `width_px` and `height_px` raw into
  `PaneGeometry::new`.

The server should store `Option<CellPx>` instead.

### 4.6 `ClientId` and `ActivityStamp` minted from integers in tests

These are test-only escape hatches (`From<u64>`, `From<i32>`,
`PartialEq<u64>`, `PartialEq<i32>`), but they shape the production
signatures (see 3.6).

### 4.7 String-typed enum: retained fallback reasons

`retained_surface_fallback_reason: Option<&'static str>` and
`retained_surface_fallbacks_reported: HashSet<&'static str>` are filled from
string literals inside the `fallback!` and `source_fallback!` macros:

- `client_missing`
- `recompute_pending`
- `no_baseline`
- `baseline_mismatch`
- `synchronized_visible`
- `runtime_missing`
- `terminal_snapshot`
- `terminal_patch`
- `alternate_screen_geometry`
- `hyperlink`
- `invalid_patch`
- `scrollbar_patch`
- `synchronized_during_patch`

That is a closed set of reasons, and a `FallbackReason` enum would make the
set checkable. `apply_pane_surface_patch -> Result<(), &'static str>` is
another prose-typed outcome.

### 4.8 Sentinels standing in for "not yet known"

- `PaneSurfacePatch.surface_revision` is set to `SurfaceRevision::new(0)` by
  `render_patches`, and `PaneSurfaceFrame.surface_revision` to `new(0)` by
  `render_client_full`. In both cases `ClientRenderState` overwrites the
  value. The producer cannot know the revision, so the producer-side type
  should not have the field: a `PaneSurfaceDraft` without a revision, which
  the render state stamps.
- `ViewEpoch::ZERO` is "never current". It is used both for "this client
  alone is stale" (`invalidate`) and for "never settled". That is fine as a
  value, but an `Option<ViewEpoch>` or a `Settled::{Never, At(e)}` would say
  it.
- `HostShutdownMonitor` generation `0` means "no warning yet".
  `frozen_warning_generation() != Some(generation)` compares
  `Option<u64>`s across that sentinel.

### 4.9 `WorkspaceId::number()` used as a map key

`retained_pane_layout` keys its cache by `workspace_id.number()` rather than
by `WorkspaceId`. That is harmless today, but it is the one place the id is
collapsed back to its integer.

---

## Lateral findings (bugs, smells, optimizations)

- **Dead `AppState::should_quit`.** See headline 6. The `app_quit` parameter
  of `ShutdownLifecycle::stop_requested` should go, along with the field.
- **The fallback `ApiError` in `reject_api_request_for_shutdown` is dead.**
  Every caller runs after `initiate_shutdown`, so `shutdown_error()` is
  always `Some`.
- **`sync_host_shutdown_freeze(&mut self, _now: Instant)` ignores its
  argument.** It also runs at the top of every loop iteration, again in
  `handle_scheduled_tasks_headless`, and once per internal event (in
  `handle_internal_event_with_forwarding`, which includes every PTY runtime
  event). The call is cheap, but per-event placement in a hot path is worth
  questioning. Answering a pending warning once before each drain batch
  gives the same guarantee.
- **`render_plan` runs on every loop wake, sometimes twice.** Each run
  allocates and sorts `render_targets`. For clients with surface debt it
  calls `surface_deliverable`, which locks the terminal cores of every
  visible pane (`synchronized_output_state`). AGENTS.md calls this a
  multiplying hot path. A cached `held` per workspace per epoch, or a
  mux-side "synchronized output ended" signal, would avoid the core locks.
- **`completion_backlog` uses `UnboundedSender::strong_count()` as a
  semaphore.** An explicit counter, or a bounded channel with
  `try_reserve`, would say what it means.
- **`ClientRenderState::commit_sent_frame` (patch arm) keeps the patch's
  `surface_revision` even when it drops the baseline.** That is correct (the
  revision was sent) but subtle, and deserves a comment or a type.
- **`client_shell_boot_id` is stored per server but comes from a
  process-global `OnceLock`.** Two `HeadlessServer`s in one process (the
  netside test runs two) share a boot id, so `StaleBoot` cannot tell them
  apart. This is harmless in production.
- **`ClientPasteRejected { size, max }` carries `max`,** which is always
  `MAX_INPUT_PAYLOAD`.
- **The host palette bound `colors.len() > 256` in the read loop is a
  magic number duplicating the decoder's bound.** The transport test shows
  the decoder already refuses 257 entries, so this check is unreachable.
- **`Delivery` results are often discarded.** Examples are
  `send_to_all_clients`, `tell_*` inside the `stream_*` functions, and
  `complete_reply`. Where `Closed` only means "the reap will handle it",
  consider not returning anything; where it matters, use `#[must_use]`.
- **`handle_server_event`'s `may_move_focus` list classifies `ServerEvent`
  variants by effect.** Several arms already call `sync_pane_focus` through
  `remove_client`, `create_automatic_workspace` and the geometry claims.
  Since the sync is level-based this is safe, but the list is one more
  classification to keep in step with the enum.

## Suggested order

1. Remove `should_quit`, delete the dead checks (boot id length, palette
   bound, empty surface, fallback `ApiError`), and drop
   `PublicPaneId`/`BootId` `Deref` (which surfaces every re-parse).
2. Introduce a typed client geometry and pixel mode stored on the
   connection, and move the size limits into protocol decode.
3. Extract `GeometryArbiter` (controllers, rule, claim reasons) and a
   presenting-clients view into `ClientRegistry`, and make `surface_active`
   private behind a registry operation.
4. Unify the cursor, scrollbar gutter, visible-panes and pane metadata
   builders between the full and retained paths. After that, drop the CJK
   gate in `render_pass_with_boundary`.
5. Split `HeadlessServer` into the four machines in 3.1, move the render
   cadence off `App`, and re-home the test suite per machine.
