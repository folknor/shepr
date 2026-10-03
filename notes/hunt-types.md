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

## TYP-007 - TUI request ids still travel as text

Every client request id is minted by one `RequestId::allocate()`, focus-id
parsing is gone, and absent and empty JSON ids are distinct. Still open: TUI
request ids serialize as text, and `RequestId` keeps its string constructors,
`Deref`, `Borrow` and string comparisons. (contracts, client-core, client-shell)

## TYP-008 - Connection generations and two wire counters are still raw

The protocol counters no longer convert into or compare with `u64`, and the
server and client projection carriers are typed; the supervisor owns the
initial connection generation. Still open: `new(u64)`, `get()` and `From<u64>`
remain on the counters; dozens of production connection-generation
declarations in shepr-client, `SurfaceGeneration` and the snapshot-generation
returns are raw `u64`; and the wire fields `content_revision` and
`state_change_seq` are `u64`. (contracts, client-core, client-shell,
server-serving)

## TYP-024 - The server collapses typed hook outcomes again

`HookOutcome::{Applied, Parked, Rejected}` with typed rejection reasons now
comes out of the ownership transitions (`crates/shepr-agent/src/ownership/mod.rs`),
with outcome-preserving entry points in mux `terminal/state/hooks.rs`. Still
open: server admission does not consume `report_hook_outcome_at` and
`report_session_start_outcome_at`, so rejection and parking still collapse to
an unchanged state update; API responses and detect explain say nothing about
an ignored report. (mux-panes)

## TYP-026 - AppEvent still carries both transport and reducer roles

Runtimes send a pane-free `RuntimeEvent` through an envelope that cannot be
nested or mismatched. Still open: `AppEvent` still exposes the bare runtime
reducer variants and carries the Git completions and API-origin reports, and
the server rebuilds envelopes between admissions; `App::handle_state_event`
exists for `app/api/panes/reports.rs` to send `StateEvent` directly. (mux-panes,
mux-state, server-app)

## TYP-031 - The launch failure carries the shell program as lossy text

Resume-unavailable reasons are a typed enum. Still open: `pane/runtime/spawn.rs`
converts the shell program path to text and `LaunchStatus.program` in
`crates/shepr-mux/src/pane/launch_status.rs` stores a `String` and rebuilds a
`PathBuf`, so a non-UTF-8 shell path is reported lossily. (foundation,
mux-panes)

## TYP-048 - Some cell and pixel extents still use zero for unknown

Core and protocol geometry fields are private with typed accessors, client
resize and cell reports carry geometry types, and the server events carry
`HostGeometry`. Still open: termio `HostCellSize` (`host_term/cell_size.rs`)
and the server connection storage (`clients.rs`) keep zero-valued axes;
`ClientMouseGeometry`, the pane-surface pixel fields (protocol `input.rs` and
surfaces) and the vt mouse adapters keep primitive extents; a raw `CellPx` is
publicly constructible for refusal diagnostics. (foundation, terminal,
contracts, client-core, server-serving, server-app)

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

## TYP-016 - Agent state still has three mirror spellings

Protocol `AgentStatus` and API `PaneAgentState` are now aliases of the shared
presented and detection states. Still open: `ManifestState` in shepr-agent
manifests, the hook action names that overlap three state names, and the
`DetectionState` mirror in `crates/shepr-api/src/schema/detection.rs`.
(agents, contracts)

## TYP-020 - CLI requests return untyped JSON

Detect explain is now typed from shepr-agent through the server and the CLI
printer. Still open: the shared `cli::send_request` returns
`serde_json::Value`, and each CLI command probes `response.get("error")` before
decoding its own result. Reported by agents, contracts, edges and server-app.

## Hook arbitration and detector state

## Pane runtime

## TYP-027 - ChildLiveness still has test-only adapters in production

`ChildLiveness` (`crates/shepr-mux/src/pane/teardown.rs`) is one locked identity
and lifecycle state, and real children derive their pid from their process
handle. Still open: a no-leader constructor used by
`crates/shepr-mux/src/pane/detection_task.rs` tests and three pid-injection
calls in `pane/runtime.rs` tests go through test-only adapters on the production
type; moving those doubles into fixtures needs an observation seam through the
runtime constructor. (mux-panes)

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
