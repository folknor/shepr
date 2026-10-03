# Consolidations from the design hunt

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

One domain question answered independently in more than one place. The unit
of an entry is the question: every site that answers it belongs to the one
entry, with whether the sites already disagree and where the single owner
should live. Unverified: the raw reports are in the commit that precedes this
file's.

## Platform, process and files

## CON-009 - How is a private file published durably and atomically?

`persist/io.rs` `publish_private_file` (private temp, copy, fsync, rename, sync
dir, `Published::{Durable, NotDurable}`, no symlink check at the target);
`machine/ssh_metadata.rs` `store_private_json_with_directory_sync` (refuses a
symlink or non-file target, its own temp naming with `unpredictable_token` and a
sequence, dir sync failure only logged); `integration/atomic_replace.rs` with
platform's `config_file.rs` (copying owner, ACL xattrs and mode). Temp naming,
symlink policy and the meaning of a failed dir sync differ. Owner: platform
`publish_file(target, contents, Options { preserve_metadata_from,
refuse_symlink_target, durability })`; `config_file.rs` is the start of it but is
named for one consumer. (foundation)

## CON-027 - A few VT spellings still bypass the shared builders

`crates/shepr-vt/src/seq.rs` now holds one spelling for underline, colour
parameters and replies, focus, DSR, and DEC and kitty mode builders, used by vt
and the termio blitter and host side. Still open: mux `osc_rgb_response`
(`crates/shepr-mux/src/pane/terminal/helpers.rs`) keeps its own target mapping
and formatter instead of `query.reply(color, ReplyForm::St)`;
`crates/shepr-client/src/terminal_setup.rs` writes focus, paste, line-wrap and
alternate-screen modes through crossterm; and some fixed startup constants still
spell DEC modes as literals. (terminal)

## CON-051 - The detection task does not stop on the arbitrated ending

`ChildLiveness::observe` covers seven observation paths, and the server
detector-event gates refuse updates after an arbitrated ending even with the
child alive. Still open: `DetectionTask` (`crates/shepr-mux/src/pane/detection_task.rs`)
stops through child liveness or cancellation, not when the exit arbiter has
decided; wiring that needs a cancellation from the arbiter, not an arbiter lock
at every tick checkpoint. (mux-panes)

## CON-056 - Pane chrome is built with provisional content fields

Content and gutter layout are one `PaneChromeInfo::content_layout` used by spawn
sizing, UI geometry, resume geometry and retained surfaces, with one scrollbar
visibility rule. Still open: `PaneChromeInfo` is constructed with provisional
`inner_rect` and scrollbar fields until the screen mode is known, which callers
overwrite. (mux-state, server-app, server-serving)

## CON-069 - A prepared pane exit is still two arguments

Replay passes `EventOrigin::Replay(generation)` explicitly and an unprepared
death cannot remove a pane. Still open: the `PaneDied` event and its
`PreparedPaneExit` are separate arguments in `crates/shepr-server/src/app/events.rs`
rather than one bound prepared input. (server-app, server-serving)

## CON-093 - The CLI schema and its parsers are still two copies

Root command recognition reads clap's subcommands and remote stop guidance
renders the producer's argv. Still open: the clap schema in `src/cli/spec.rs`
and the typed parsers remain separate copies; the daemon invocation producer
(`--client-spawned`, `--version` in shepr-remote `local_server.rs`) and the
daemon parser are separate; `ServerAddress::stop_command` spells the local stop
argv itself. A complete invocation model needs a home below config and remote.
(edges, contracts)

## CON-109 - The client still reconstructs the server split tree

One allocation-free helper now decides which side of a split a pane is on.
Still open: the client still hashes a topology signature and sends child lists,
and the server reverse-maps them (`split_path_for_children` in shepr-core
`layout.rs`); a server-minted layout epoch would let the client send
`(workspace, path, epoch, ratio)`, which needs protocol, server API and
workspace topology changes. (client-shell, server-serving)

## CON-013 - The socket path type is thrown away after the check

`SocketPath` in `crates/shepr-core/src/socket_path.rs` owns the length check
and the platform connect uses it, but `resolve_paths_checked`
(`crates/shepr-config/src/address.rs`) validates and drops it, so `AppPaths`
and `ServerAddress` still hold a plain `PathBuf`. Carry `SocketPath` there so
the check is a type fact. (wave-3 review)

## CON-015 - What does a pane's exit mean, and does it get a checkpoint?

pty `ReaderExit::{ShutdownRequested, Closed, IoFailed, Panicked}` (severity by
derive order), platform `ChildExitReason` with `requires_session_checkpoint`,
mux `reader_exit_callback` mapping one onto the other; "does this exit get a
checkpoint" is `requires_session_checkpoint` plus a second clause in
`shepr-server/src/app/events.rs` `pane_exit_needs_checkpoint` ("and the core is
not broken"), plus the method again in mux `transition_pane_exit` and
`CheckpointCandidate::qualifies`. AGENTS.md says checkpoint policy lives in
shepr-server; the method lives in platform. Owner: one `PaneEnding` in mux next
to the exit arbiter, carrying the reason and whether the core is intact, with a
single `needs_checkpoint()`; platform keeps only `ExitKind::{Exited(code),
Signalled(sig)}`. (foundation)

## Terminal emulation and input

## CON-022 - Does a mouse report use pixels or cells, and is pixel mouse eligible?

Client `mouse.rs` sends pixels when `hit.sgr_pixel_mouse && hit.pixel_width >
0`; server `pane_input::downgrade_ineligible_pixel_mouse` downgrades unless the
client geometry equals the runtime grid and pixel extent;
`apply_client_pane_input_event` maps `Pixels` only if
`runtime.sgr_pixel_mouse_enabled()`; mux `encode_mouse_event` maps cells to
pixels through `cell_pitch` under 1016 and back otherwise; the server's connect
and resize arms decide `pixel_mouse && observed.is_known()`; the
`ClientShellPaneInput` arm uses `client.pixel_mouse &&
outbox.told_sgr_pixels()` (the outbox's dedupe memo as an authority);
`stream_host_mouse_capture_mode` uses `client.pixel_mouse &&
runtime.sgr_pixel_mouse_enabled()`; protocol's `TerminalGeometry` and
`ProtocolCellSize` decide again. They agree only because each re-checks the same
mode bit, each under its own lock. Owner: pixel mode as one value on the connection,
recomputed when its inputs change, and one function next to the mouse protocol
type that takes the pane's pixel extent and the report. Reported by terminal and
server-serving.

## CON-023 - How big is a pane in pixels?

vt `PaneGeometry::text_area_px()` (drives `CSI 14 t`, the 2048 report and
`width_px()/height_px()`); server `client_shell.rs` `inner_rect.width *
HostCellSize.width_px` with `(0, 0)` as unknown, published as
`PaneSurfacePane.pixel_width/height`; mux `cell_pitch` (`width_px() / cols`, at
least 1). They agree only while the pane's cell equals the client's and the grid
equals `inner_rect`; with several clients of different cell sizes, the server
publishes per-client extents while the child was told the geometry-source
client's. Owner: `PaneGeometry`, with the wire carrying `Option<PixelExtent>`
from it. (terminal)

## CON-035 - Where does an OSC end?

vte decides; `scan.rs::Scanner` mirrors vte's framing to find working directory,
progress and oversized OSCs; mux `osc.rs::OscStreamCollector` mirrors it a third
time for the opt-in debug log (and says so). If the collector drifts, the debug
log shows sequences the terminal did not see. Owner: the scanner, emitting an
`OscBody` event when the debug log is on. Reported by terminal and mux-panes.

## CON-037 - Synchronized output: two notions, one draw gate

DECRQM ?2026 answers from `ExtraModes.synchronized_update` (replay order) and
`mode_get(SynchronizedOutput)` from the parser deadline. This is documented and
correct; the terminal hunter asks only that the two be named apart
(`sync_update_in_replay`, `sync_update_buffering`) so nobody unifies them.
Separately, `PaneTerminal::render_into` and `collect_dirty_patch_snapshot`
return early while mode 2026 is set, and the server checks
`synchronized_output_active()` or `synchronized_output_state()` before calling
them (`ui/surface.rs`, `retained_surface.rs`, `client_shell.rs`): a deliberate
double check today because `render_into` returning `()` cannot say whether it
drew. Owner: a typed draw result (`Drawn | Deferred | Unreadable`) read once under
one lock. Reported by terminal and mux-panes.

## CON-038 - What is a displayable title, and what is a word?

Title: vt cuts at `MAX_TITLE_BYTES` (bytes), mux `sanitize_agent_osc_string`
filters controls and caps at `AGENT_OSC_MAX_CHARS` (chars), termio
`write_window_title` strips controls again for the host; config's
`sanitize_window_title_text` is render-time sanitizing used by the server. Each
has a reason, but nothing owns the rule. Word: mux `text_class`
(`COPY_MODE_WORD_SEPARATORS`) for copy-mode motions, client
`word_bounds::is_word_separator` (including CJK punctuation) for double-click,
and `TextEditor::word_boundary`. The first two apply to the same pane text and
differ; whether that is intended is written nowhere. Reported by terminal,
client-shell and contracts.

## Agents and hooks

## CON-049 - Hook asset contracts are spelled in every asset

Each of the 16 assets spells the environment gate (`SHEPR_BUILD_PROFILE =
release`, `SHEPR_ENV = 1`, `SHEPR_SOCKET_PATH`, `SHEPR_PANE_ID`), the method and
param names, the action vocabulary (`IntegrationHookAction::as_str`), the
`<source>:<seq>` id, the 500 ms socket wait, and the descriptor's source and
label. Several re-check the event-to-action map: Codex's `expected_events` is
`CODEX_HOOK_EVENTS` in Python, Devin hard-codes `("SessionStart",
"UserPromptSubmit")` (the descriptor says "session action" and the asset says
"only these two events"), Claude and Cursor check their one event. Pairwise tests
hold them (`hook_assets_share_one_envelope`,
`bundled_integration_assets_report_the_descriptor_identity`, the bun traces). Kilo
and the OpenCode plugin duplicate `SESSION_STATE_BY_STATUS`,
`CHILD_EVENT_STATES`, `sessionIDFromProperties`, `stateFromSessionStatus` and the
transport (and have already drifted: the cycle guard filed as a bug); Pi and OMP
duplicate about 150 lines of transport, sequence and queue code. Owner: a
generated preamble per language carrying the envelope, gate, identity and event
map from the descriptor, with the agent-specific decoder appended. The module
comment argues against templating because decoders differ; that holds for the
decoders only. (agents)

## Pane runtime and workspace

## CON-054 - Pane counter bookkeeping is partly outside the one rule

`backend.rs` now records every mutation through one `CoreMutation` and
`record_mutation` rule (`pane/terminal.rs`) that decides the content,
detection, sync and history counters. Still open: the detection increment
helpers in `crates/shepr-mux/src/pane/agent_detection.rs` (free functions on
`&mut u64`) sit outside that rule, and `PaneTerminalCore`'s fields are still
`pub`/`pub(super)`, so `DetectionTask::tick` and the osc tests read
`core.detection_content_seq` directly. The counter types are filed among the
types. Reported by mux-panes, terminal and server-serving.

## CON-059 - How does a layout tree collapse, and which panes are adjacent?

Pruning and pane-id collection now live on core's `Node`, and core's
`find_in_direction` uses one directional helper. Still open: mux
`workspace/geometry.rs` decides adjacency (`ranges_overlap`, `pane_to_right`,
`pane_below`, `u16` saturating) separately from core's helper (`u32` ends), and
the two differ at the right or bottom edge of a `u16::MAX` area. (mux-state)

## CON-061 - Which pane maps to which terminal and runtime?

`AppState` now keeps a `PaneId -> TerminalId` index with `terminal_of` and
`runtime_of`, used by six paths. Still open: `find_pane` in
`crates/shepr-server/src/app/ids.rs` re-walks workspaces, and state assembled
directly (outside the creation, restore and removal paths that maintain the
index) still falls back to a scan, so the index is a second record kept in step
by hand. Reported by server-app and mux-panes.

## Persistence and session saves

## App and server loop

## CON-071 - Geometry claims are decided by scattered handlers

The PTY size rule is now one function in
`crates/shepr-server/src/server/headless/client_views.rs`, geometry is settled
before the render plan (rendering no longer resizes PTYs), and alternate-screen
mode is a per-workspace record rather than read from the delivered baseline.
Still open: who claims geometry is decided in several places (focus gain, pane
interaction, connect, activation, navigating commands, and a four-branch policy
in `handle_client_shell_command` keyed on `claims_shell_geometry` and
`changes_topology`) with controller storage in the client registry; one owner
with `claim(client, ClaimReason)` would hold them. Who views a workspace is also
answered twice: settlement (`settle_workspace_geometry_before_plan`,
`apply_all_workspace_geometry`) uses `location.focused_workspace_id()` while
render uses `shell_target_for_client`, which also checks the workspace still
exists. (server-serving, wave-2 review)

## CON-076 - Does a projection need recomputing?

`render_plan` uses `!settled || projected_location_generation !=
location.generation() || snapshot.is_none()`; `render_client_full` also compares
`session_generation != shell_session_generation`. The plan relies on every bump
of the session generation also calling `mark_view_changed`, which holds at both
bump sites today with nothing tying them. Owner: one
`ClientShellState::projection_due(&SessionGeneration)`. Projection identity
resolution is also repeated (`snapshot_from_session` resolves the focused
workspace twice, `shell_target_for_client` applies the same filter,
`focus_target_for_surface`, `shell_focused_runtime` and `visible_pane_runtimes`
each go id to index to workspace to runtime, `send_pane_focus` uses a linear
`position`): a `ViewedWorkspace { index, workspace }` resolved once per client
per pass. (server-serving)

## Wire, handshake and surfaces

## CON-080 - Is this update a patch against unchanged topology, and may it apply?

`SurfaceTopology`, `SurfaceBaseline::admits`, named revision transitions and one
patch application are now shared by the server and both decoder paths. Still
open: the client's `endpoint/choice/preparing.rs` keeps a second full baseline
and applies patches to it instead of reading the connection decoder's baseline
(`Decoder::current_surface`); the boundary is commented there. Reported by
contracts and server-serving.

## Config and keybindings

## Edges

## Client
