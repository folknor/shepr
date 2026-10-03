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

## CON-005 - When is a cell size exact, how big may it be, and how small may a host grid be?

"Pixel coordinates are exact only with a known cell" is decided in
`HostGeometry::new` (core), `TerminalGeometry::new` and its `TryFrom` (protocol)
and `ProtocolCellSize::from_host`, which adds a `MAX_CELL_SIZE_PX` bound core
does not know: a 100000 px cell is exact to core and not to protocol. The cell
size limit is three policies: `client_shell_geometry_error` refuses,
`ProtocolCellSize::from_wire` nulls, `ProtocolCellSize::from_host` clamps. The
minimum host grid is decided by `GridSize::clamped` (one cell),
`HostGeometry` through `PaneGeometry` (4 by 2) and `ClientHostSize::new` through
`ClientSurfaceSize::clamped`; two disagree (filed as a bug). The grid upper
budget is filed separately. Owner: core `HostGeometry` with its own grid rule and
a `CellPx` that owns the pixel bound. Reported by foundation and server-serving.

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

## CON-018 - What does a child's environment contain, and under which policy?

`ChildEnv` documents "every name shepr writes into a child in one place", but
mux writes `SHEPR_ENV`, `SHEPR_SOCKET_PATH`, `SHEPR_BUILD_PROFILE`,
`SHEPR_PANE_ID` and `TERM_PROGRAM` through `EnvVar`, pty writes `PWD` and
removes `OLDPWD` by literal, `PtyCommand::env` accepts any `AsRef<OsStr>`, and
`SHELL` is set in `interactive_shell` and overwritten in `launch_spec`. mux
`pane/launch.rs` decides a pane policy per `EnvVar` (`pane_env_policy`) and per
`ChildEnv` (`pane_child_env_policy`); `SHELL` and `PATH` are in both, held in
step by the pairwise test `a_name_in_both_vocabularies_has_one_pane_policy`.
Which agent owns which variable (`ClaudeConfigDir`, `CodexHome`, ..., and session
markers stripped from panes) is a fact about the agent, split from the
descriptor and from `integration/env.rs`, which maps variables to directories by
hand. Owner: one registry where each name appears once with its pane policy as a
property, `PtyCommand` taking registered names for everything shepr sets, and the
descriptor naming its `config_dir_override` and `session_markers` so the
stripping and integration-path lists derive from the agents. Reported by
foundation, mux-panes and agents.

## CON-019 - Where is the server log, and where are the XDG directories?

`data_dir.join(SERVER_LOG_FILE)` is composed in `bootstrap.rs` (twice),
`remote/local_server.rs` and implicitly in `logging::help_log_paths_summary`;
owner `AppPaths::server_log()`. `XDG_CONFIG_HOME` is read by
`shepr-config/src/io.rs` and again per call by mux `git/config.rs`
`git_user_config_paths_at` (errors swallowed). The integration config lock
directory (`integration/env.rs` `resolve_config_update_lock_dir`) recomputes the
XDG state home and hard-codes `"shepr"` independently of config's
`SHARED_APP_DIR_NAME`, and `devin_dir`, `opencode_dir`, `kilo_dir` and
`opencode_state_dir` each re-decide "XDG var or `~/.config`/`~/.local/state`".
Owner: shared `xdg_config_home()`/`xdg_state_home()` in core or platform.
Reported by foundation and agents.

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

## CON-029 - What character does a key produce?

`copy_mode_command_char` (also used by `keybind_help_text_char`), encode's
`text_char_for_key`/`shifted_text_char`/`is_shifted_ascii_punctuation`,
`TerminalKey::with_text_commit` (uppercase means Shift), parse's
`parse_legacy_key_sequence`, keybindings' `generated_character_key`, and
`BindingKey::canonical_key`. Two pairwise tests keep some in step. The context
policies legitimately differ (copy mode applies the US shift table to letters
too), but "Shift plus this base key gives which char" should be one
`TerminalKey::produced_char()` with each context's policy on top. (terminal)

## CON-030 - How wide is text?

vt `unicode_codepoint_width`/`unicode_text_width` (grid widths with the
voiced-mark override); termio `blit::text_width` (and aliases `symbol_width`,
`cell_width`: grapheme width plus one per U+FF9E/U+FF9F, hard-coding the
codepoints instead of using vt's predicate); mux `terminal_buffer_symbol_into`
(measures `symbol.width()` against `CellWide` with vt's predicates); client
`word_bounds` (re-measures row text with `unicode_codepoint_width`). The grid and
grapheme rules differ on purpose, but the voiced-mark rule is written twice and
the predicates three times. Cell width from `CellWide` is mapped four times in
`helpers.rs` (`terminal_blank_symbol_for_width`, `terminal_grid_width`, the
`expected_width` match in the render path and its test twin) and a fifth in
`text.rs` `TextBufferBuilder::push_cell`. Owner: one width module exposing both
rules by name, and `CellWide::columns()`/`grid_width()`. Reported by terminal and
mux-panes.

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

## CON-069 - Is a pane exit checkpointed before removal?

The two `prepare_pane_removal_by_id` calls serve different moments (before the
hold and after the checkpoint) and are documented as such, and the runtime
envelope requeue is one helper. Still open: the server's
`replaying_checkpointed_pane_exit` field passes a parameter through `self`
(`handle_scheduled_tasks_headless` sets it, `handle_internal_event_with_forwarding`
`take()`s it, early returns clear it by hand), and the direct `AppEvent`
fallback survives for callers outside the prepare-and-hold path. Owner: `PaneDied`
applicable only as a `(event, PreparedPaneExit)` input, and an explicit
`Origin::Replay(generation)` argument, which needs the pending queue and
scheduler in `server/headless.rs` to carry it. Reported by server-app and
server-serving.

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

## CON-093 - Command lines: producers and parsers are separate copies

`shepr`: `RemoteCliCommand::args` (shepr-remote) produces argv; the clap builder
in `src/cli/spec.rs` plus typed parsers (`status::parse`, `server::parse`,
`detect::parse`) consume it. Shared `COMMAND_*`/`FLAG_*` constants keep spelling
in step, but shape is held only by the pairwise test
`generated_remote_cli_arguments_parse_with_the_cli_spec`; the spec and the typed
parsers are two copies held by `every_cli_spec_root_has_typed_parser` and
`every_cli_spec_leaf_parses_to_a_typed_command`; `root_exit_flags_before_subcommand`
is a fourth copy of the subcommand list with `"detect"` as a bare literal (as in
`CliCommand::from_matches`); the binary's clap spec imports its names from the
SSH crate. `shepr-server`: `local_server.rs` produces `--client-spawned` and
`--version` (a literal) and `shepr-daemon` parses both with its own constant; no
test ties them. The `server stop` command is spelled three times:
`RemoteCliCommand::ServerStop`, `src/preflight.rs::remote_stop_command`
(hand-formatted `"ssh {} {} server stop --expect-boot {}"`) and
`ServerAddress::stop_command`. A rename would move the spec, the producer and the
round-trip test together while the two printed copies keep telling the operator
the old spelling. Which commands need application paths is decided in
`cli::run` and re-checked downstream. Owner: one `SheprInvocation` and one
`ServerInvocation` enum, each with `argv()` and `parse()`, next to `daemon_exit`
and `server_stop` (or clap derive); printed commands render the same value.
Reported by edges and contracts.

## Client

## CON-108 - What does compose draw over pane cells?

`fast_path_blocker` (`surface_patch.rs`) predicts compose's layers (mode bar,
overlay, endpoint error, notice card, selection, copy-mode owner, unknown pane
hits, surface overflow) as `&'static str` reasons that production throws away;
`render_mode_bar` decides the bar is drawn when `mode != Terminal ||
endpoint_error.is_some()`; compose also draws a lifecycle banner and hides the
cursor when the active endpoint is not Online, which the blocker does not check
(covered only because compose clears `hits.panes`; a cursor-only patch still
takes the fast path). The patch path also rebuilds `PaneHit` fields by hand
(scrollbar rect offset, scroll metric conversion, mouse and pixel flags). Owner:
compose records what it occluded in a `LastComposition` that the fast path
consults, and one `PaneHit::from_wire(pane, origin, clip)`. Mouse capture off
disables hits by clearing them in `render_shell` and in `compose` plus a check in
the right-click arm. (client-shell)

## CON-109 - Which side of a split is a pane on?

The client's `split_child_panes` (`rect.x < split.pos` means first) and
`pane_surface_topology_signature` (`rect.x >= split.pos` means second; outside
means neither, hashed with FNV) answer it separately, both reconstructing the
server's layout tree, which the server reverse-maps again in
`split_path_for_children`; `pane_split_topology_matches_hit` and
`pane_split_target_is_current` layer further checks on the hash. Owner: the
server. `PaneSurfaceSplit` already carries `path`; adding a server-minted layout
epoch (or the child lists) lets the client send `(workspace, path, epoch,
ratio)` and drop the hash and the rect classification. The split hit-rect
geometry in the server (`client_shell.rs::split_hit_rect`) is layout policy that
could sit with the border rules. Reported by client-shell and server-serving.
