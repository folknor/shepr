# Types from the design hunt

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

Domain facts that travel as primitives, and types that exist but collapse back
to a string or a number through `Deref`, `From`/`Into`, public fields,
cross-type `PartialEq` or a routinely unwrapped accessor. Includes type
aliases posing as types, sentinel values and string-typed closed sets. Where
several hunters reported one axis, the entry says so. Unverified: the raw
reports are in the commit that precedes this file's.

## Identities

## TYP-003 - WorkspacePane still derefs to PaneState with public fields

Pane public numbers are a nonzero `PanePublicNumber`, commit consumes the same
`PreparedSplit` used for launch, and `PublicPaneId::new` cannot panic. Still
open: `WorkspacePane` keeps its `Deref` to `PaneState` and public
`pane_state` and `public_number` fields, and the workspace next-number field
is public (typed). (mux-state)

## TYP-004 - App handlers still carry workspace positions as usize

The duplicated display-number fields are gone from replies and projections; the
sidebar derives positions from list order. Still open: workspace lookup APIs
and outcome structs in shepr-server (`pane_info`, `workspace_info`,
`lookup_runtime`, `PaneRemovalPlan`, `WorkspaceCreationOutcome` and others)
resolve a `WorkspaceId` to a `usize` index and re-check it with `.get(ws_idx)`;
an `AppState::workspace(&WorkspaceId) -> Option<WorkspaceRef>` would keep the
id. The distinction is documented in `crates/shepr-server/src/app/state.rs`.
(contracts, server-app)

## TYP-002 - `PaneId::raw()` and saved pane keys share `u32`

About forty production calls of `PaneId::raw()`, almost all `pane =
pane_id.raw()` in tracing fields (mux `pane/runtime.rs`, `teardown.rs`,
`child_watcher.rs`, `launch_status.rs`, `osc.rs`, `terminal.rs`,
`detection_task.rs`, mux `logging.rs` taking `pane_id: u32`, server `app/*`),
in thread names (`shepr-pty-{}`, `shepr-pane-{}-teardown`), and as snapshot
keys. The snapshot keys everything by that `u32`
(`WorkspaceSnapshot::panes: HashMap<u32, PaneSnapshot>`,
`LayoutSnapshot::Pane(u32)`, `focused`/`root_pane: Option<u32>`,
`WorkspaceHistorySnapshot::panes`, `SessionHistory::workspaces:
Vec<Vec<(u32, HistoryText)>>`, `HistoryStamp::panes`), and workspace positions
as `usize` (`type PaneKey = (usize, u32)`, the server's
`PreservedLayout::terminal_ids: HashMap<(usize, u32), TerminalId>`,
`SessionSnapshot::active: Option<usize>`, `remap_saved_index`). Restore juggles
`id_map: HashMap<u32, PaneId>`, `reverse_id_map`, `numbers: HashMap<u32,
usize>` and `public_pane_ids_by_old_raw` in one function.
`TileLayout::from_saved` documents "callers must remap restored IDs through
`PaneId::alloc` first", a comment-enforced obligation.

Proposal: `Display` and `tracing::Value` for `PaneId`; `SavedPaneKey(u32)`
(serde transparent, minted only by capture and deserialization) and
`SavedWorkspaceIndex`, with `SavedPaneRef { workspace, pane }` replacing the
tuple; `TileLayout::from_saved(SavedNode, SavedPaneKey) -> (TileLayout, Remap)`
allocating live ids itself. `raw` then becomes crate-private to `shepr-core`.
`PaneId` and its process-global allocator live in `shepr_core::layout`, and pty
imports it from there only for log lines and thread names; an `ids` module would
read better. Reported by foundation, mux-state and server-app.

## TYP-007 - `RequestId` is any string, and the client encodes structure in it

`RequestId` has `From<String>`, `From<&str>`, `Deref<str>`, `Borrow<str>` and
`PartialEq` with strings both ways; its doc says any string is a legitimate id.
The client mints four families by format: the shell ledger `client-shell:{n}`,
`endpoint::view::start_move` `client-shell-view:{serial}:on`,
`EndpointRegistry::release_unwanted_views` `client-shell-view:{serial}:off`, and
`FocusLane::request` `client-shell-focus:{view serial}:{n}`, which recovers the
serial with `view_request.split(':').nth(1).unwrap_or(view_request)`.
Distinctness holds only by prefix. `ClientShellEndpointRequest::id` is `String`,
`EndpointCommandCancellation::{unsent, possibly_sent}` are `Vec<String>`,
`drop_request`, `answer_request`, `release_highlight` and
`keep_workspace_highlight_until_snapshot` take `&str`. The ledger is documented
as the sole owner of request identities while core mints three more families.
The CLI uses the literals `"cli:detect:capture"`, `"cli:detect:explain"`,
`"cli:server:stop"`. JSON request ids use `""` for "no id"
(`request_id_from_line`; `hand_off` calls `send_busy_refusal(stream, "")`) while
`ErrorResponse::id` is `Option<String>`.

Proposal: keep `RequestId` opaque on the wire (a client-minted counter on the
TUI wire, text for JSON), and a client enum `ClientRequest::{Shell(n),
ViewOn(serial), ViewOff(serial), Focus { view, n }}` with one `to_wire` and
`from_wire`, minted by one allocator. Reported by contracts, client-core,
client-shell and edges.

## TYP-008 - Protocol counters convert freely to and from `u64`

(Also the server-side carrier of the projection revision:
`AppState::shell_projection_revision` and `ShellSessionCache.revision` in
`crates/shepr-server/src/server/headless/render.rs` are raw `u64`s mirroring it,
and `CachedShellProjection.projection_revision` and `snapshot_from_session`'s
`revision` parameter carry it raw.)

`ProjectionRevision`, `SurfaceRevision` and `ConnectionGeneration` come from
`revision.rs::counter!` with `From<u64>`, `Into<u64>`, `PartialEq<u64>` both
ways, `PartialOrd<u64>` and a public `get()`; with the cross-type comparisons a
`ProjectionRevision` compares equal to a `ConnectionGeneration`'s `get()`. Uses:

- On the wire: `EndpointReply::ClientShellSurfaceSet { projection_revision:
  u64 }`; `PaneSurfacePane::content_revision: u64` and
  `ClientShellAgent::state_change_seq: u64` have no type at all.
- Server: `AppState::shell_projection_revision: u64`, `render.rs`
  `projection_revision: u64` and `CachedShellProjection.projection_revision`,
  `surface_interest.rs` returning `(bool, u64)`,
  `snapshot_from_session(revision: u64)` then `revision.into()`; plus
  location generation, `shell_session_generation`, `ShellSessionCache.revision`
  compared across structs as raw `u64`s.
- Client: every API carrying a connection generation takes `u64`
  (`EndpointRegistry`, `EndpointTransportFailure`, `ClientLoopEvent`,
  `EndpointSupervisorEvent`, `EndpointSupervisors`, `EndpointCommands`,
  `ViewLease`, `MoveStage::Failed`, `PendingStart`, `Preparing`,
  `start_endpoint_transport`, `spawn_endpoint_reader`, the shell's
  `snapshot_generation`, `endpoint_snapshot_matches(id, generation, boot,
  revision)` with two adjacent `u64`s); projection revisions are unwrapped in
  `ViewLease::minimum_revision`, `Preparing::floor`,
  `ViewEvidence::snapshot_revision` and `endpoint_snapshot_identity`.
- Connection generations are minted in two places that agree by picked numbers:
  `run_client_loop` passes the literal `1` and seeds `Some(1)` for a failed
  launch, and `EndpointSupervisors::new` starts at `2`.
- `ConnectionGeneration` lives in `shepr-protocol` but is never on the wire.

Proposal: no `From<u64>`, no cross-type comparisons, `Ord` within a type,
`ZERO` and `checked_next`, minting only through the owning allocator, the typed
value on the wire, newtypes for content revision and state change sequence, and
the connection generation owned by the client's supervisor allocator. Reported
by contracts, client-core, client-shell and server-serving.

## TYP-011 - `SshTarget` still derefs to `str`

The remote crate now uses `SshTarget::append_to`, typed target accessors and
messages, an `SshTarget` metadata target, `MachineLabel`-keyed probes and
`RemoteExecutable` equality. Still open: `SshTarget: Deref<Target = str>` and
`IntoSshTarget for &String` in `crates/shepr-config/src/machine.rs`, one
`.arg(target.as_str())` site in the machine probe, and the persisted
`StoredMetadata.target` and platform control-socket API still take text at
their boundaries. Drop the `Deref` once those go. Reported by edges and
contracts.

## Agent identity and state

## TYP-015 - The published state event splits the screen verdict again

The screen verdict is typed from manifest compilation through the mux
publisher, and the baseline sentinel is an `Option<Detection>`. Still open:
`AppEvent::StateChanged` and the server's `StateEvent::StateChanged` carry a
state plus a blocker bool, which the publisher derives from the one verdict;
carrying the typed detection across that contract (mux events and the server
app event handling in `crates/shepr-server/src/app/`) finishes it. Reported by
agents and mux-panes.

## TYP-016 - Agent state still has three mirror spellings

Protocol `AgentStatus` and API `PaneAgentState` are now aliases of the shared
presented and detection states. Still open: `ManifestState` in shepr-agent
manifests, the hook action names that overlap three state names, and the
`DetectionState` mirror in `crates/shepr-api/src/schema/detection.rs`.
(agents, contracts)

## TYP-018 - A persisted agent session can still be built around its validation

The four session copies now share one type with validating decode, and resume
plans keep their argv private. Still open: `PersistedAgentSession`
(`crates/shepr-agent/src/agent/resume.rs`) keeps three public fields, so a
direct struct literal bypasses the validating constructor. (agents)

## TYP-020 - CLI requests return untyped JSON

Detect explain is now typed from shepr-agent through the server and the CLI
printer. Still open: the shared `cli::send_request` returns
`serde_json::Value`, and each CLI command probes `response.get("error")` before
decoding its own result. Reported by agents, contracts, edges and server-app.

## Hook arbitration and detector state

## TYP-024 - Hook report outcomes collapse a dozen reasons into `None`

`transition_report` and `transition_start` return
`Option<TerminalStateMutation>`. `None` covers: built-in source naming another
agent, an identity-only integration, an invalid session ref, a replaced
session, a report after confirmed process exit, a label conflicting with the
detected agent, an owner conflict without foreground takeover, a stale or
cross-talk report, an out-of-order sequence and a full source table.
`Some(default)` means parked. The server collapses all of it into
`StateUpdate::Unchanged`, the reporter gets nothing back, and `detect explain`
cannot say why a hook was ignored. Proposal: `HookOutcome::{Applied(mutation),
Parked, Rejected(HookRejection)}` with a closed rejection enum. Inside the
hook-source machine, `HookSourceState::transition` mixes state changes with
pure queries answered through effect variants (`OrderAllowed(bool)`,
`DetectorObservationAllowed(bool)`, `Report(route)`, `Start(route)`) that
callers destructure with `let .. else { return None }`; queries should be
methods. (mux-panes)

## Pane runtime

## TYP-026 - Runtime events are an optional, nestable envelope around any `AppEvent`

`AppEvent::Runtime { pane_id, generation, event: Box<AppEvent> }` can wrap any
event, including another `Runtime` (so `admit_runtime_event` recurses) and
non-runtime payloads (`GitStatusRefreshed`, `HookStateReported`); every runtime
payload repeats `pane_id` and admission never checks it against the envelope's;
the envelope is optional (see the latent bug). `AppEvent` also mixes a server
worker completion (`GitStatusRefreshed`, produced by `app/git_refresh.rs`) and
two API-origin reports that `app/api/panes/reports.rs` wraps only for
`StateEvent::from_app_event` to unwrap; the App drops `ClipboardWrite` because
the server handles it first. Proposal: a `RuntimeEvent` enum with only what
runtimes emit (`LaunchSettled`, `Died`, `AgentProcessDetected`, `DetectorState`,
`ClipboardWrite`, `CwdReported`) and no `pane_id`, wrapped in a mandatory
`RuntimeEnvelope { pane_id, generation }` sendable only through a
`RuntimeEventSender` that owns the pair; Git completions a server-local worker
result; API reports going straight to `StateEvent`, which becomes the App's
input type. server-app and mux-panes also suggest putting the terminal id in the
envelope so admission is one lookup. Reported by mux-panes, mux-state and
server-app.

## TYP-027 - ChildLiveness still has test-only adapters in production

`ChildLiveness` (`crates/shepr-mux/src/pane/teardown.rs`) is one locked identity
and lifecycle state, and real children derive their pid from their process
handle. Still open: a no-leader constructor used by
`crates/shepr-mux/src/pane/detection_task.rs` tests and three pid-injection
calls in `pane/runtime.rs` tests go through test-only adapters on the production
type; moving those doubles into fixtures needs an observation seam through the
runtime constructor. (mux-panes)

## TYP-029 - The dirty patch snapshot folds three reasons into `None`

`collect_dirty_patch_snapshot -> Option<TerminalDirtyPatchSnapshot>` folds a
poisoned core, an open synchronized update and a fallback (whose reason string
is logged once) into `None`; `TerminalDirtyPatch.rows` is
`Vec<(u16, Vec<CellData>)>`; the fallback reason is an `Option<&'static str>`
from a `fallback!` macro. The server's retained renderer has its own closed set
of fallback reasons as string literals in its `fallback!` and
`source_fallback!` macros (`client_missing`, `recompute_pending`,
`no_baseline`, `baseline_mismatch`, `synchronized_visible`, `runtime_missing`,
`terminal_snapshot`, `terminal_patch`, `alternate_screen_geometry`,
`hyperlink`, `invalid_patch`, `scrollbar_patch`, `synchronized_during_patch`)
stored in `retained_surface_fallbacks_reported: HashSet<&'static str>`, and
`apply_pane_surface_patch -> Result<(), &'static str>`.
`report_terminal_mutation_failure(operation: &'static str)` and
`report_dirty_patch_fallback(reason: &'static str)` are the same pattern.
`TerminalDirtyPatchSnapshot` is `pub` inside the private `runtime` module and
cannot be named outside the crate. Proposal: `Result<DirtyPatchSnapshot,
PatchUnavailable::{CorePoisoned, SynchronizedOutput, Fallback(PatchFallback)}>`
with `Vec<PatchRow { y, cells }>`, and a `FallbackReason` enum in the server
whose `terminal_snapshot` arm carries the real reason. Since mux now turns a
fallback read into no snapshot, retained-surface fallbacks that logged as
`terminal_patch` log as `terminal_snapshot`, so one label already carries two
reasons, and the `invalid_patch` case now falls back silently through
`prepare_pane_surface_patch` returning `None`. Reported by mux-panes and
server-serving.

## TYP-030 - Content and detection counters are raw `u64` with sentinels

`PaneTerminalCore::content_revision`, `detection_content_seq`,
`synchronized_output_epoch`, `history_epoch`, `default_color_generation`;
`PaneRuntime::content_seq() -> u64` (a poisoned core now reads as an odd, torn
revision, and full-draw certification is one `surface_content_revision`
function); `synchronized_output_state() -> Option<(bool, u64)>` where `None`
means poisoned and callers (`render_pane_surface`, `workspace_surface_held`)
decode the three states by hand; `TerminalDirtyPatchSnapshot::content_revision:
u64`. The parity rule is still arithmetic on a bare `u64`.
`PaneHistoryCache::revision: u64` uses `0` for "never held anything". Proposal:
newtypes per counter (`ContentRevision`, `DetectionSeq`, `SyncEpoch`,
`HistoryEpoch`, `DefaultColorGeneration`) with `bump`, `is_stable`,
`changed_since`, and `SyncState::{Idle(epoch), Active, Poisoned}`. The
bookkeeping of these counters is filed among the consolidations. Reported by
mux-panes and server-serving.

## TYP-031 - Resume-unavailable reasons are still prose

Launch status records are read by a `LaunchStatusReader` in pty, and pane start
failures are a `PaneStartFailure` that keeps real `io::Error`s until
presentation. Still open: the resume-unavailable reasons are prose strings, and
the spawn path converts the shell program to text lossily before it reaches the
failure. (foundation, mux-panes)

## TYP-032 - The raw shell setting uses an empty string for unset

`ResolvedShell` (in `crates/shepr-core/src/shell.rs`) is minted by config
validation and carried to `PtyCommand`. Still open: the raw
`TerminalConfig::default_shell` uses `""` for "use `$SHELL`" where an
`Option<String>` would say it, and `ResolvedShell`'s constructor takes a
validation callback because pty cannot depend on config. Reported by
foundation, mux-panes, contracts and server-app.

## TYP-033 - Client copy search state keeps loose counts

Pane text points are one `shepr_vt::Point<AbsRow>` across vt, mux, protocol and
the client word selection, and mux search takes a typed request. Still open:
the client copy search state in `crates/shepr-client/src/shell/state.rs` stores
total, window index and global index as separate fields of mixed widths, and
the word-selection drag takes an `(AbsRow, u16)` tuple at its mouse boundary.
(client-shell)

## TYP-034 - Tuples standing in for named pairs in the pane and workspace APIs

`PaneTerminal::dimensions()` and `PaneRuntime::terminal_dimensions() ->
Option<(u16, u16)>` are `(cols, rows)`, the test-only `current_size()` is
`(rows, cols)`, and server tests assert `Some((grown.1, grown.0))`;
`PaneGeometry::pane_size`, `sole_pane_size` and `restored_pane_size` return
`(rows, cols)` and `spawn_geometry(rows, cols, cell)` then calls
`PaneGeometry::with_cell(cols, rows, cell)`; `pixel_size() -> Option<(u32,
u32)>`; `terminal_recent_read_range -> Option<(usize, usize, u16)>`;
`PaneHistoryCache::parts()` yields `(&Arc<str>, Option<usize>, bool)`;
`osc_rgb_response(command: &str, r, g, b)` takes three bytes and builds the
command from a typed `ColorQueryTarget` as a string. Proposal: return
`GridSize`/`PaneGeometry` and named structs. Reported by mux-panes and
mux-state.

## TYP-036 - Mutation results are recovered by diffing revisions

The pane scroll and clear methods return `SurfaceChange` and the API handlers
use it, but the server's headless input path still compares scroll metrics
around each input batch, and its input helper discards the individual
results. Aggregating `SurfaceChange` through the batch helper removes the
comparison. The app-level form of the same pattern is filed among the
consolidations. (mux-panes)

## TYP-037 - History cache edges

`PaneHistorySource(pub(crate) Arc<PaneTerminal>)` is built by reaching into the
tuple field; `PaneHistorySource::refresh -> bool` and
`read_primary_history_inner -> Option<()>` fold "alternate screen active" and
"core unreadable" together. Fine today because both mean "keep the previous
cache", but `HistoryUnavailable::{AlternateScreen, CorePoisoned}` costs nothing.
`next_restored_revision` sets the top bit of a `u64` while `PaneHistoryCache`'s
counter "never reaches the top bit": two files partition one integer space;
`PaneStamp = Option<u64>`. Proposal: `HistoryRevision::{Live(u64),
Restored(u64)}`. Reported by mux-panes and mux-state.

## Workspace, persistence and Git

## TYP-041 - The Git status cache key is a bare path, and read errors are prose

The cache entry is now `Miss` or `Hit` with an `AheadBehindState`. The cache key
is still a `PathBuf` that means the canonical checkout root for a repo, the raw
resolved cwd for a non-repo and a placeholder seeded by
`Workspace::mark_identity_undiscovered`; a `GitStatusKey::{Checkout, Outside}`
minted by discovery needs the workspace refresh input to carry it, not just a
path (the boundary is commented in `app/git_refresh.rs`). `GitReadError`'s
payloads are prose (`arguments: args.join(" ")`, `message: error.to_string()`);
`FileRead` should carry a `FileReadReason` enum. (mux-state)

## TYP-045 - Cwds are `PathBuf`s right after `UsableCwd` exists

Save-time process validation now returns `UsableCwd`; event-loop observations
deliberately stay unstat'ed paths (a stat could block the loop; commented at
the code). Saved cwds (`PaneSnapshot::cwd`, `WorkspaceSnapshot::identity_cwd`)
are still `PathBuf` with restore checking `is_absolute()` by hand. On the wire and in the server, paths are
`String`: `WorkspaceCreateSource::Cwd`, `WorkspaceCheckoutRootParams::cwd`
(validated later by `api::cwd::launch_cwd`), `EndpointReply::WorkspaceCheckoutRoot
{ root, home }`, `ClientShellWorkspace::new_workspace_cwd` (where `""` means
none, through `map_or_default`), `ClientShellPane::{cwd, foreground_cwd}` (built
with lossy `display().to_string()`), `SessionRestoreNotice::backup_dir`,
`CheckoutRootRunner: Fn(PathBuf) -> Result<Option<String>, String>`,
`WorkerCompletion::CheckoutRoot { home: Option<String> }`, and
`prepare_workspace_checkout_root -> (PathBuf, Option<String>)` where a non-UTF-8
home silently becomes "no home". Proposal: an `AbsolutePath` checked at
deserialization for saved and wire cwds, `UsableCwd` (or an `ObservedCwd`) as the
return of runtime cwd reads, and a `RemotePath` newtype for paths on the
server's host that the client never opens. Reported by mux-state, contracts,
server-app and server-serving.

## Geometry and coordinates

## TYP-048 - Cell size and host geometry are decomposed into primitives with `0` for unknown

`HostGeometry` has `cols()`, `rows()`, `cell_width()`, `cell_height()` (both `0`
when the cell is unknown) and public `pane` and `exact` fields, so a literal or
`geometry.exact = true` bypasses "exact only with a cell". In the client it is
pulled apart and rebuilt in a loop: `set_host_size` rebuilds it from
`ClientHostSize` plus the old cell fields; `run_until_exit` feeds the parts into
`ProtocolCellSize::from_host` then builds a new one; `handle_event` destructures
`Resize(geometry)` into `handle_resize(cols, rows, cell_width, cell_height,
exact)`; `bounded_cell_geometry` returns `(u32, u32, bool)` decomposed from the
`ProtocolCellSize` it just built; `ioctl_cell_size`, `AtomicCellSize::load` (which
packs `width << 32 | height` with `0` as not reported), `last_cell_size`,
`reported_cell_size_from_events` and `cell_size_fallback` use `(u32, u32)`, and
`ioctl_terminal_geometry` `(u16, u16, u32, u32)`; `platform::terminal_grid_size()`
returns `(u16, u16)` though core has `GridSize`.

Server and termio use `HostCellSize { pub width_px, pub height_px }` with
`Default` (zeros) meaning unknown, `is_known()` re-validating through
`CellPx::new` and `or_default()` normalising invalid sizes to zero; the framer
validates into a `CellPx` then destructures back to `(u32, u32)` for
`RawInputEvent::HostCellSizeReport`. `SpawnGeometry::cell_size: HostCellSize`
uses zero for "never reported"; `ui::resize_pane_infos` passes the raw fields to
`PaneGeometry::new` while spawn sizing goes through `SpawnGeometry::cell_px()`,
so one value is converted two ways on two paths that size the same PTY.
`PaneGeometry`, `ProtocolCellSize` (public `cell` and `exact`) and
`TerminalGeometry` (public `grid`, `cell`, `pixel_mouse`, whose invariant is
enforced by `new` and deserialization only) all expose zero accessors;
`client_transport.rs` rebuilds `ProtocolCellSize::from_wire(hello.geometry.width(),
..)` from those zeros. `Terminal::width_px()/height_px()` return 0 when unknown,
`PaneSurfacePane.pixel_width/pixel_height` carry 0 on the wire (and
`retained_surface.rs` builds zeros directly), and the client tests `> 0`.
`HostPixelExtent` has public `width_px`/`height_px` on a `Copy` type, so the
`> 0` invariant `new` checks can be undone. `ClientMouseGeometry { cols, rows,
width_px, height_px }` is another shape of the same fact.
`ServerEvent::ClientShellConnected`/`ClientShellResize` carry `surface_cols,
surface_rows, cell_width_px, cell_height_px, pixel_mouse` as primitives although
the wire already has a validated `TerminalGeometry`; `apply_server_event`
rebuilds `GridSize::clamped`, a zero-sentinel `HostCellSize` and the pixel-mouse
invariant in both the connect and resize arms, and `client_geometry`,
`render_full`, `render_client_full` and `RenderTarget` each build `Rect::new(0, 0,
cols.get(), rows.get())` by hand. `GridSize { pub cols: NonZeroU16, pub rows }`
leads to `.cols.get()` everywhere.

Proposal: `Option<CellPx>` end to end with the pixel bound in `CellPx`
construction; `HostGeometry` with private fields and `CellKnowledge::{Unknown,
Estimated(CellPx), Exact(CellPx)}` (or storing an already bounded
`ProtocolCellSize`); private fields on `HostPixelExtent`, `ProtocolCellSize` and
`TerminalGeometry`; `Option<PixelExtent>` from `PaneGeometry` to the wire; one
validated `ClientSurfaceGeometry` minted in protocol decode and stored on the
connection; `GridSize` accessors returning `u16`. Reported by foundation,
terminal, contracts, client-core, server-serving and server-app.

## TYP-049 - Row spaces beyond the three typed ones travel as integers

`ViewportRow`, `ScreenRow` and `AbsRow` exist, but `Terminal::cursor_y() -> u16`
is a line of the live screen (mux adds `viewport_start + cursor_y` by hand in
`terminal_recent_read_range`); `TerminalScrollbar.offset` is a `ScreenRow` typed
`usize` (mux iterates and wraps each value); `RowView::y() -> u16` is a viewport
row wrapped back up before `viewport_hyperlink_uri`; `CursorViewport { x, y }` is
a `Point<ViewportRow>` in all but name. `AbsRow(pub u64)` has a public field,
`From<u64>`, `saturating_add/sub(u64)`, and `.0` arithmetic across mux and the
client; `ScreenRow(pub usize)` and `ViewportRow(pub u16)` are built from raw
loop counters (`ViewportRow(row - pane.y)`, `ViewportRow(cursor.y - inner.y)` in
the client). `Selection::ordered_cells()` returns `((AbsRow, u16), (AbsRow,
u16))` so callers can drop `Point`, and `pub pane_id` is compared field-wise.
In the client shell, surface-local coordinates (from `PaneSurfacePane`,
`PaneSurfaceSplit`, patch rows, the cursor) and screen coordinates (hits, frame
cells) are both `Rect`/`(u16, u16)`, and the
`layout.pane_surface.x.saturating_add(..)` translation is written in `compose`
(twice), `apply_tagged_pane_surface_patch` and `compose_pane_surface`. Proposal:
`LiveRow` or a `ScreenRow` from the cursor accessor, typed scrollbar fields,
`RowView::y() -> ViewportRow`, private `AbsRow` with `checked_offset_from` and
an `AbsRange`, row iterators and `Rect::viewport_row_at`, `Selection::range()`
and `belongs_to`, and a `SurfaceRect`/`ScreenRect` split with one
`SurfaceOrigin::to_screen`. Reported by terminal and client-shell.

## TYP-051 - Scrollback bytes and history lines are both `usize`

`Terminal::new(cols, rows, max_scrollback: usize)` takes a byte budget;
`Terminal.history_lines`, `CoreHandler.history_limit` and
`RowOrigin::note_pushed(.., history_limit)` hold line counts; `with_handler`
passes both side by side, and `scrollback_lines(bytes, columns)` converts.
`AdvancedConfig::scrollback_limit_bytes: usize` is threaded raw through about
twenty mux signatures beside `u16` cols and rows, and the documented policy ("0
disables; any non-zero keeps at least 1000 lines") is re-derived where it lands.
`Terminal::new(cols: u16, rows: u16, ..)` clamps raw numbers while `resize`
takes `PaneGeometry`. Proposal: `ScrollbackBudget(bytes)` owning the policy with
`lines_at(columns) -> HistoryLines`, and construction from `PaneGeometry`.
Reported by terminal and contracts.

## TYP-052 - Mouse coordinates have no unit or base in their type

`encode_mouse_event(kind, x: u32, y: u32, ..)` takes "1-based cells, or pixels
for SGR-pixels"; mux pairs pixels with `MouseProtocolEncoding::SgrPixels` and
adds the `+1` itself for four combinations. `RawInputEvent::Mouse(crossterm
MouseEvent)` holds pixels minus one under host mode 1016, which the framer
cannot say; `classify_unix_input` decides afterwards by checking whether the raw
bytes start with `ESC [ <` and reading an `AtomicBool`, then adds 1, and
`HostPixelExtent::cell` subtracts it again. termio's `mouse::Position` and
protocol's `ClientMousePosition` are near copies. Mouse modifiers round-trip
through `u8` (`apply_scroll(.., modifiers: u8)` gets `bits()` and
`from_bits_truncate`). Proposal: `MouseReportPosition::{Cell(CellPos),
Pixel(PixelPos)}` with an explicit 1-based newtype at the encoder, and the framer
told the host mouse mode so it emits typed positions. Reported by terminal and
client-core.

## TYP-053 - The Unix socket stream, bound socket and socket path are untyped

`pub type LocalStream = UnixStream` and `LocalListener = UnixListener`:
`connect_trusted_local_stream` (peer uid checked) and `connect_local_stream`
(not checked) return the same type, so nothing stops a client path from writing
to an unverified stream (`wake_listener` and `probe` use the untrusted one
deliberately). `bind_private_socket` and `bind_single_use_private_socket` return
`(LocalListener, SocketStartupLock, SocketFileIdentity)`, and
`remove_socket_file_if_owned(path, identity)` takes them separately, so consumers
rebundle it (`ServerHandle`, `TeardownResource::Socket { path, identity }`,
`BridgeSocketStartupCleanup`). Proposal: `TrustedServerStream` from the trusted
connect only; an `AdmittedPeer` from an accept helper that ran the credential
check; `BoundSocket { listener, lock, path, identity }` with
`remove_if_still_ours(self)`; a `SocketPath` constructed with the length check
(see the socket path consolidation). `SocketStartupLock` also reports its
outcome (`"busy"`, `"acquired"`, `"released"`) by hand in three places.
(foundation)

## Terminal values

## TYP-056 - Progress reports and OSC evidence are bytes and empty strings

`ProgressReport(pub Vec<u8>)` holds the ConEmu `4;state;percent` payload; mux
stores it as `latest_progress: String` (empty means none) and detection matches
text (`"4;3;"`). `AgentOscStateTracker::latest_title()`/`latest_progress()`
return `""` for none, and `AgentDetectionInputs { osc_title: String,
osc_progress: String }` and the detector's `screen.map_or("", ..)` carry the
empty string as "no evidence". `OscDebugEvent::command: String` is one of `"0"`,
`"2"`, `"9"`, `"21337"`. Proposal: `Progress { state: ProgressState, percent:
Option<u8> }` parsed once by the scanner and `Option<&str>` evidence through to
the matcher. Reported by terminal and mux-panes.

## TYP-060 - Durations and poll timeouts are `i32` milliseconds with `-1` for forever

`termio/limits.rs` mixes `*_TIMEOUT_MS: i32` with `PASTE_STALL_TIMEOUT:
Duration`; `held_input_flush_timeout_ms() -> i32` leaks the poll unit into the
framer API; `child_io::poll_fd_readable`, `poll_fd` and `fd::poll_pty_and_wake`
take `i32` with `-1` meaning forever, passed through from the client
(`idle_flush_timeout_ms`, `Deadline::remaining_millis_i32`);
`poll_read_ready -> Option<bool>` is consumed as `!= Some(false)`.
`SessionConfig::startup_per_agent_delay_ms: u32` is converted to `Duration` in
the server. Proposal: `Duration` throughout (and a `Wait::{Forever, Until,
Now}`), converted at the `poll` call; validation produces the `Duration`.
Reported by terminal, foundation, client-core and contracts.

## Wire, API and config

## TYP-061 - Endpoint failures a client could branch on are `Rejected(String)`

`command::EndpointError::Rejected(String)` is the only failure for anything the
app refuses: "workspace not found", "pane not found", "split children not
found", "ratio must be finite", "split pane belongs to another workspace", "the
pane is on the alternate screen", "copy search query is too large", "cwd must be
an absolute path", "the pane could not be split", "the new pane is unavailable";
the server loop adds "checkout root worker limit reached; retry later" (a busy
condition), "failed to start checkout root worker" (a resource failure) and
`response_within`'s "the response could not be encoded" (internal). The client
shell keys notice identity on the prose (`format!("{method}:{message}")`), and
every `Work` completion treats a reply of the wrong variant with
`set_endpoint_error("endpoint returned an unexpected ... result")` (five
sites). The JSON API reports an unparseable pane id as `pane_not_found`, so a
syntax error and a missing pane are indistinguishable to a hook. Proposal:
`WorkspaceGone(WorkspaceId)`, `PaneGone(PublicPaneId)`, `SplitGone`,
`InvalidArgument(..)`, `Busy`, `Internal`, with `Rejected(String)` only for
user-facing messages, and typed replies per command (an associated reply type)
so the ledger's continuation cannot receive the wrong variant. The `32f70f2`
move typed the loop's errors but left the app's as one variant. Reported by
contracts, server-app, server-serving and client-shell.

## TYP-064 - Wire grid cells: the wide-glyph tail is a sentinel and `FrameData` has no invariant

A `FrameGrid` view with private fields now owns the shape, budget and
hyperlink validation, reused by protocol and composition code. Still open:
`FrameData` itself keeps public mutable fields (so a frame is valid only once
checked, not by construction), a wide tail is still "`symbol` empty and
`grid_width == One`" (`pane_row.rs::is_tail`), and `CellData::skip: bool` is a
ratatui diff hint on the wire. Proposal: `GridCellWidth::{Grapheme, One,
WideLead, WideTail}` and `try_from` deserialization into the validated grid.
Reported by contracts and client-shell.

## TYP-069 - Smaller config axes

- `ThemeConfig::name: Option<String>` is canonicalized by
  `canonical_theme_name` and again by `Palette::from_name`; a `ThemeName` enum
  generated by `define_builtin_themes!` makes the unreachable "no built-in
  palette" branch unrepresentable.
- `ValidatedClientUiConfig::mouse_scroll_lines: NonZeroU16` while the raw model
  has `Option<NonZeroUsize>` and `DEFAULT_MOUSE_SCROLL_LINES: usize`.
- `SidebarTokenRule::hide: Option<bool>` where only `Some(true)` matters, and
  `style_for_value -> Option<SidebarTokenStyle>` using `None` for hidden: a
  `TokenRendering::{Hidden, Styled}`.
- `AgentSidebarToken::Styled { token: Box<Self>, .. }` permits nesting the parser
  never produces; a `SidebarTokenSpec<T> { token, style, rules }` removes the box
  and the recursion in `allows_rules` and `parts`.
- `AppSettings::cjk_ime_agents: Vec<ConfigAgent>` where empty means every agent:
  `AgentFilter::{Any, Only(..)}`.
- `ClientShellWorkspace::git_ahead_behind: Option<(usize, usize)>` reaches
  `SpaceTokenContext` as a tuple although mux has the counts as a struct.

Reported by contracts, server-app and client-shell.

## TYP-070 - Keybindings: a tuple alias, labels as data, help groups as strings

`KeyCombo = (KeyCode, KeyModifiers)` is a type alias exposed through
`LiveKeybindConfig::prefix`, `BindingTrigger`, `format_key_combo`,
`normalize_key_combo` and `terminal_key_matches_combo`, while the identity type
`CanonicalKey` is private; callers outside config normalize tuples themselves.
`ResolvedBinding::label` and `IndexedKeybind::label` are derived from the trigger
and re-parsed (`prefix_rhs_label` strips `"prefix+"`, termio `indexed_label`
reconstructs ranges by `strip_suffix` on digits). Help groups `"global"`,
`"workspaces"`, `"panes"`, `"navigation"` are literals in `keybinding_table!` and
again in `keybind_help_groups`, looked up by `position(|(name, _)| *name ==
group)`; insert-after and alias merging find rows by comparing label strings;
entries are `(String, Cow<str>)` tuples. A typo drops an entry into a group that
is never shown. Proposal: a public `CanonicalKey` with `matches`, labels from
`Display` on the trigger, a range binding kept as one `IndexedRange` value, and a
`HelpGroup` enum column with `HelpRow { keys, label }`. Reported by contracts and
terminal.

## TYP-071 - API log outcomes are strings

`ApiLogOutcome { Ok, Timeout, Error }` is converted to `&'static str`, then
`server::finish_api_response` adds `"client_disconnected"` as a bare literal and
`logging::api_request_completed` picks the level with `outcome != "ok"`; the
logging functions take `(name, mutates_ui, routine)` unpacked instead of
`MethodTraits`. Similar: `logging::startup(role: &'static str)`,
`HostWriteFailure::observe(write: &'static str, ..)`, mux persist events with
hand-written `event = "persist.snapshot"` literals at a dozen sites,
`logging::init_file_logging(dir, file_name: &str)` with two constants (a
`LogFile::{Server, Client}` would also own its path). Reported by contracts,
client-core, mux-state and foundation.

## Remote and launch

## TYP-073 - Some launch and serve outcomes are still grouped

`LaunchError` (eleven variants) and `RunServerError` (nine cases) carry launch
outcomes typed through autodetect and the CLI. Still open: the Local endpoint
status in the client is not seeded from `LaunchError`; low-level launch IO is
one group; and signal-install, loop and shutdown failures are grouped in
`RunServerError::Serve(io::Error)`, where `complete_shutdown` still erases
`UnexpectedPhase`. (edges, server-serving)

## TYP-074 - One platform policy error still rides in io::Error

Bind, lease-acquisition and SSH-runtime errors are typed and their downcasts
gone. Still open: `crates/shepr-platform/src/private_file.rs` embeds
`PrivateDirectoryPolicyError` in an `io::Error` and recovers it in
`PrivateDir::is_policy_refusal`. (foundation)

## TYP-075 - The launch builders still pass shell text as `&str`

`PosixScript` and `AccountShellCommand`
(`crates/shepr-remote/src/remote/shell_command.rs`) are taken by the SSH
methods and the bridge entry point, but the command builders in
`crates/shepr-remote/src/remote/launch.rs` still produce `&str`/`String` that
`sh_output_within` adapts internally. Have the builders return the typed
values. (edges)

## Client

## TYP-078 - Typed move and session failures end as prose

`Preparing::rejection: Option<String>` (from `EndpointError::to_string()` and
literals such as "surface activation returned an invalid acknowledgement"),
`FocusLane::receive -> Result<(), String>`, `view::commit_move -> Result<_,
String>`; `reconcile` wraps each in another `format!`.
`EndpointTransportFailure { kind: io::ErrorKind, message: String }` is rebuilt
into an `io::Error` for `ClientError::ConnectionLost` and into a diagnostic in
`endpoint_lost`, losing the source chain. `ClientExit { message: Option<String> }`
and `ClientRunError::Launch(io::Error)` flatten a session outcome already
classified in `run_launched_client`. Proposal: `MoveFailure::{Rejected, BadAck,
FocusMismatch, LostPair, ProjectionUnavailable, TimedOut, TargetLost}` (with the
deadline failure one more variant), and typed launch and exit outcomes.
(client-core)

## TYP-080 - Endpoint-qualified addresses are spelled six ways in the shell

`ClientEndpointFocusTarget::{Workspace, Pane}` (no endpoint),
`ClientNavigatorTarget::{Machine, Workspace, Pane}` (endpoint in each variant),
`WorkspaceNavigationTarget { endpoint_id, workspace_id, boot_id, generation }`,
`AggregateAgentTarget { endpoint_id, pane_id }`, `pending_agent_reveal:
Option<(ClientEndpointId, PublicPaneId)>`, `ShellHitMap::endpoint_agents:
Vec<(Rect, ClientEndpointId, PublicPaneId)>` beside `agents: Vec<(Rect,
PublicPaneId)>`, `WorkspaceHit` and `ClientWorkspacePress`. Proposal: `Location
{ endpoint, target: Target::{Machine, Workspace, Pane} }` and a `PinnedLocation`
adding the snapshot identity. (client-shell)

## Bool parameters, tuples and sentinels

## TYP-085 - Bool parameters and bool return pairs

Each is a two-valued domain fact passed positionally:
`ipc::acquire_flock_lock(path, blocking: bool)`;
`PtyIoInbox::push_terminal_response -> (bool, bool)`;
`ChildIo::owns_child_process() -> bool`;
`restore_host_keyboard_protocol(writer, modify_other_keys_active, kitty_entry_active)`
only ever called as `(true, false)` or `(false, true)`;
`copy_mode_page_lines(height, half_page: bool)`;
`write_clipboard_bytes(bytes, prefers_osc52_clipboard: bool, w)`;
`RepeatPlan::Reprocess { tracked: bool }`;
`do_handshake(.., mouse_capture: bool, surface_active: bool, ..)` and
`do_handshake_for_link`; `stdin_reader_loop`'s three bools plus two
`Arc<AtomicBool>` mirrors (a `HostInputProbe`); `HostModes::apply_mouse(writer,
exact_geometry, reassert)`, `HostModes::new(bool, bool)`,
`HostMouseMode::new(bool, bool)`, `set_pane_keyboard_report_all(writer, enabled,
shell_requests_report_all)`; `EndpointRegistry::insert(.., viewed: bool, ..)`;
`claim_shell_workspace_geometry(id, false)`,
`resize_shell_workspaces_sized_for(id, true)`,
`reapply_controlled_shell_workspace_geometry(true)`,
`finish_shell_workspace_geometry_change(bool, bool)`, `apply_shell_geometry(bool)`
with literals for `start_pending_agent_resumes`;
`handle_internal_event_inner(ev, prepared_checkpoint: Option<bool>)` collapsing
`PreparedPaneExit`; `ResumeSchedule::observe(now, has_pending_plans: bool,
eligible: bool)`, `wakeup` and `is_due` (where `(false, true)` is meaningless);
`CheckpointTicket::host: bool`, `HostShutdownCheckpoint::take_result() ->
Option<bool>`, `HostShutdownFreeze::persist_session: bool`,
`frozen_session_policy() -> Option<bool>`; `App::create_default_workspace ->
bool` folding three outcomes; `Workspace::split_pane(.., shell_config, true,
&spawn)` with twelve positional arguments and `commit_new_pane(..,
public_number, true)`; `publish_private_file(.., replace: bool)`;
`snapshot_history_decision(.., replacement: Option<&SessionSnapshot>, ..)` where
`None` means "after the write"; `preserve_existing_in -> io::Result<bool>`;
`RestoredPaneStart::Running { duplicate_agent_session: bool }`;
`PendingHistory::resolve_for_save(.., allow_unchanged: bool)`;
`apply_pane_chrome(.., pane_gaps: bool, pane_outer_borders: bool)` and
`PaneGeometry`'s three chrome bools; `commit_new_pane -> Option<()>`.
Reported by foundation, terminal, client-core, server-serving, server-app and
mux-state.

## TYP-086 - Bare tuples and paired options standing for named facts in the server

- `focused_panes: HashSet<(WorkspaceId, PaneId)>` and `panes_holding_focus`,
  beside `ShellFocusTarget` and `ClientPaneIdentity`: three spellings of a pane
  qualified by its workspace.
- `sync_terminal_title_sources -> (bool, bool)`,
  `set_client_shell_surface_active -> Option<(bool, u64)>`,
  `ClientRegistry::remove_client -> (Option<ClientConnection>, bool)`,
  `SharedSurfaces.oversized_notices: Vec<(ClientId, usize, usize)>`,
  `PaneSurfaceRenderKey = (Option<WorkspaceId>, u16, u16, u32, u32)`, the
  retained layout cache key `(usize, u16, u16)`.
- `Told.mouse_capture: Option<(bool, bool)>` and `tell_mouse_capture(enabled,
  sgr_pixels)`: a `MouseCaptureMode`.
- `downgrade_ineligible_pixel_mouse(.., runtime_pixels: Option<(u32, u32)>)`.
- `ClientShellState.outer_terminal_focus: Option<bool>` compared against
  `Some(true)` at five sites.
- `host_terminal_appearance: Option<HostAppearance>` plus
  `host_terminal_appearance_explicit: bool`, on both `AppState` and the client
  shell state in `server/`, with `set_host_appearance` keeping the pair coherent.
- `HeldReply { ready: Option<Vec<u8>>, refusal: Option<Vec<u8>> }` with two
  legal states of four.
- `ClientRenderState { last_surface, recompute_pending, debt }`, where
  `surface_debt()` and `takes_patches()` each re-derive a combined state and
  `refuse()` sets two at once.
- `PreparedRender::Semantic { message, committed_surface: Option<Box<..>> }`,
  whose `None` case `commit_sent_frame` recovers by matching `message`, with a
  logged branch for the impossible case.
- `ShutdownLifecycle { phase, freeze: Option<HostShutdownFreeze> }`, with
  `freeze` meaningful only in some phases and re-checked by
  `frozen_session_policy` and `frozen_warning_generation`.
- `GitRefreshScheduler`'s `git_refresh_in_flight`, `due_after_in_flight`,
  `git_identity_refresh_requested` and `last_git_remote_status_refresh = now -
  the refresh interval` as the due-now sentinel.
- `App::create_default_workspace`'s retry kept as two `Option`s.
- `PaneSurfacePatch.surface_revision` and `PaneSurfaceFrame.surface_revision`
  set to `SurfaceRevision::new(0)` by the producer and overwritten by
  `ClientRenderState`: a draft type without the field.
- `ViewEpoch::ZERO` means both "this client alone is stale" and "never
  settled".
- `completion_backlog` uses `UnboundedSender::strong_count()` as a semaphore.
- `PaneInputError::{Backpressure(&'static str), Closed(&'static str),
  Other(String)}`, where the label is a closed `InputKind` set.
- `App::hostname: String` from `hostname().unwrap_or_default()`.

Reported by server-serving and server-app.

## TYP-087 - Parallel closed sets and identical structs hand-mapped across crates

Mirror enums converted by hand in handlers: `SplitDirection -> Direction`,
`PaneDirection -> NavDirection`, `PaneWordMotion -> TerminalWordMotion`,
`PaneCopySearchDirection -> TerminalSearchDirection`, `PaneParagraphMotion ->
i8`; core `Direction` versus protocol `PaneSurfaceSplitDirection`. Three rect
types (ratatui `Rect` in `AppState`, `SpawnGeometry` and `PaneChromeInfo`; core
`Rect`; protocol `SurfaceRect`) with `layout_rect`/`ratatui_rect` free functions
in mux and manual copies in `retained_surface`; core `Rect` has public fields
whose `new` clamps the far edges to fit `u16`, skipped by the literals in mux.
The wire's `ClientHostColor`, `ClientHostAppearance`,
`ClientHostDefaultColorKind`, `ClientMousePosition`, `ClientMouseGeometry`,
`ClientPaneInputEvent::Key` and `PaneSurfaceScrollMetrics` sit beside vt and
termio's `RgbColor`, `ColorScheme`, `DefaultColor`, `Position`,
`HostPixelExtent`, `TerminalKey` and `ScrollMetrics`, joined by
`theme_conversion.rs`. Two clock samples: `AppClock { now, wall_now }` and
`HookClockSample { monotonic, wall }`, built from one another; the client shell
also has `ClientShellState.now` beside a `now` parameter on many methods.
Reported by server-app, foundation, terminal, mux-state and client-shell.

## TYP-088 - Labels are normalized three ways

User labels from the API go through one server helper,
`normalized_user_label` in `crates/shepr-server/src/app/api_helpers.rs`
(trim, empty clears). Labels restored from a saved session reach the pane
through `set_manual_label` in shepr-mux without it, so `pane_border_title`
still trims at render as the only guard for those. A `Label` (trimmed,
non-empty) minted once, held as `Option<Label>` by the stores and by the saved
schema, would cover restore too and let the render trim go.
`normalize_reported_agent_label` stays separate: it also canonicalizes agent
names. (server-app)
