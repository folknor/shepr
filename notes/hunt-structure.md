# Structure from the design hunt

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

Shape and placement: crate splits, moves, god objects, boundaries that do not
follow the seams, dependency edges pointing the wrong way, state split from the
behaviour acting on it, and test layouts. Each entry carries the payoff the
hunter claimed. Unverified: the raw reports are in the commit that precedes this
file's.

## Crate boundaries and dependency edges

## STR-002 - Platform hosts policy that is not platform

- `ChildExitReason` and its checkpoint policy belong to mux (see the pane exit
  consolidation).
- The SSH bridge relay (`remote_bridge.rs`, `remote_bridge_io.rs`) is
  shepr-remote's protocol, its idle timeout defined by client heartbeats; because
  it sits in platform, `shepr-core/src/limits.rs` owns `BRIDGE_IDLE_TIMEOUT`,
  `HEARTBEAT_INTERVAL`, `SSH_ROUND_TRIP_TIMEOUT`, `SSH_ATTEMPT_SLACK` and
  `SSH_CONNECTION_ATTEMPT_BUDGET`. Its only caller is `remote/host.rs`; move it
  and the timing to remote, and core loses its only network-timing knowledge.
- `ssh_paths.rs` (OpenSSH `%C` expansion, control path naming) is SSH policy;
  the generic piece is the owned runtime directory.
- `config_file.rs` is half of agent integration's atomic replace.
- After those moves, group the flat platform modules by seam (`fs`, `ipc`,
  `process`, `host_terminal`) with re-exports per group; today client-only
  pieces (clipboard, `terminal_grid_size`, SIGWINCH watcher, OSC 52 preference,
  `begin_cli_output`), server-only pieces and filesystem and IPC primitives are
  mixed. The clipboard route is a `prefers_osc52_clipboard()` bool threaded
  through five client layers to termio, which decides `!prefers_osc52 &&
  write_clipboard(bytes)`; a `ClipboardRoute::{Osc52, Helpers(session)}` from
  platform would carry the decision.

(foundation)

## STR-003 - Move shell resolution out of core

`shepr-core/src/shell.rs`'s only caller is `shepr-config/src/validated.rs`, which
also supplies the classifier closure calling platform's `has_execute_access`.
Move `resolve_executable` and `ExecutableStatus` to config and offer
`shepr_platform::classify_executable(path)`. `is_pane_shell_process_name` and
`SHELL_NAMES` live in shepr-agent's detection but are shell knowledge, and they
are config's only reason to depend on detection; they belong with the platform
shell helpers. contracts goes further: the config to platform and agent edges
then serve only `resolve_default_shell` (reading `SHELL` and `PATH` and probing
the filesystem), which is server launch policy and could move to shepr-server
with config holding the raw string. Reported by foundation, agents and
contracts.

## STR-004 - Split value types out of the emulator crate

`shepr-protocol`, `shepr-api`, `shepr-termio` and `shepr-client` depend on
`shepr-vt` only for value types (`AbsRow`/`ScreenRow`/`ViewportRow`/`Point`,
`Selection`, `RgbColor`, `ColorScheme`, `DefaultColor`, `UnderlineStyle`,
`ModifyOtherKeysLevel`, `FocusEvent`, the width functions), which puts
`alacritty_terminal` and `vte` in the client binary's build graph, the kind of
edge AGENTS.md keeps mux and server out of the client for. Move them into a small
crate (or core modules) that vt re-exports; `KittyKeyboardFlags` moves down from
protocol so vt can return it. Then collapse the parallel wire types into the
shared ones. (terminal)

## STR-005 - Split shepr-termio by side

termio holds a child-facing half (`input/encode.rs`, mouse protocol enums,
`KeyEncodeModes`, `mouse::Position`, `TerminalKey`), a host-facing half
(`raw_input.rs`, `parse.rs`, `keybindings.rs`, `keybind_help.rs`, `lease.rs`,
`host_term/*`, `blit.rs`, `selection_render.rs`, `copy_mode.rs`,
`input/mouse.rs`), and shared values (`ScrollMetrics`, `TerminalTheme`,
`HostCellSize`, `text_width`, scrollbar geometry the server draws and the client
hit-tests). The server links keybinding help, the host input framer and the
blitter; the client links pane key encoding. Proposal: pane input encoding moves
next to the emulator and consumes one `InputModes` snapshot vt produces under one
lock (kitty flags, modifyOtherKeys, DECCKM, bracketed paste, focus reporting,
mouse protocol, alternate scroll, alternate screen, cell pixels), removing mux's
mode ladders and per-accessor locking; the host-terminal half moves into the
client or a client-only crate. Smaller: `host_term/title.rs` also writes the
clipboard (its own module); `scroll.rs` mixes a value type, shared scrollbar
geometry and server-chrome drawing. `AppState` depends on termio's
`host_term` types (`TerminalTheme`, `HostAppearance`, `HostCellSize`), host and
wire facts that pull a terminal-input crate into pure state. config's per-keypress
keybinding matching (`BindingKey`, `CanonicalKey`, `terminal_key_matches_combo`,
`ActionKeybinds::matches_*`) belongs with key input too. Reported by terminal,
server-app and contracts.

## STR-006 - shepr-agent is three crates

Identity (`agent/mod.rs`, `agent/resume.rs` types: tiny and pure, needed by
config, the client sidebar, mux and server), detection (regex engine over screen
text plus a `/proc` prober, server-only), and integration (a config-file editor
with `toml_edit`, `jsonc-parser`, flock locks, atomic replace and 16 bundled
assets, server-only and launch-time only). Because they share a crate the client
binary's graph includes the editor and the engine, and config depends on all of
it for one enum and one shell-name predicate. Suggested: `shepr-agent` (identity,
descriptor, report origin, session ref, state), `shepr-detect` (manifests, rule
engine, process identification over platform `/proc` readers), and
`shepr-integration` (installer and assets), the descriptor staying the single
table. `/proc` plumbing (`detect/proc_tree.rs`: stat parsing, task and children
walking, cmdline, cwd readlink, budgets and the `FOREGROUND_*` limits) moves to
platform by AGENTS.md's own rule; mux then stops reaching through
`shepr_agent::detect::` for it. `AgentSessionRefKind` lives in core for consumers
that left; it can move into `resume.rs` or become `AgentSessionRef::is_id()`.
(agents)

## STR-010 - shepr-config does four unrelated jobs

1. TOML model, loading, unknown-key detection, validation.
2. Process runtime layout: `AppPaths` (XDG, `SHEPR_STARTUP_CWD`),
   `BuildProfile` and the marker policy, `ServerAddress` and the socket override
   rule, `DATA_DIR_LEASE_FILE_NAME`, `operator_entrypoint`, and operator guidance
   prose (`ServerAddress::build_mismatch_guidance`). `shepr-api` depends on config
   only for `AppPaths`; remote, the client and the CLI use it for layout, not
   settings.
3. Keybinding runtime matching, run per keypress in the client.
4. A theme library (eighteen palettes) plus `sanitize_window_title_text`, render
   sanitizing used by the server.

`io.rs` alone holds the build profile, path resolution, the loader pipeline, BOM
repair and unknown-key reporting. The validated types keep raw config:
`ValidatedClientConfig` stores the whole raw `ClientConfig` only to answer
`machines()`, and `ValidatedServerConfig` hands out `session()`, `advanced()` and
`experimental()` unvalidated. The loader machinery (`LoadedConfig<C, R>`, trait
`ConfigResolution`, the resolution structs, `from_values`/`from_loaded`/
`from_resolution`, two `resolve_*_config` wrappers taking unused arguments, a
test-only `parse_document` faking `/bin/sh`) is generic over two roles differing
only in their validate step. `AppSettings` copies a dozen fields out of
`ValidatedServerConfig`. Proposal: a small layout crate (or platform module) for
job 2, matching moved to termio, config left as "parse and validate the two
files" with one `validate(raw, &LaunchContext) -> Result<Validated,
Vec<ConfigDiagnostic>>` per role and validated types owning validated values
only. Every caller of `build_mismatch_guidance` and `attach_command` passes
`operator_entrypoint()` (the parameter exists for tests); a default method with a
test seam would stop five call sites restating the choice. Reported by contracts
and server-app.

## STR-011 - shepr-protocol mixes the wire with policy and state

Wire types, codec, framing and preamble are its job. Allocators are not:
`TerminalId::alloc` (a process-global counter and clock stamp) and
`BootId::for_this_process` mint identities in the wire crate; mux and server own
that policy. The surface delta planner (`surface_delta::message`, including the
"six bytes per cell" heuristic) is server-side policy and the decoder
(`surface_reuse::Decoder`) client-side state; `ratatui_conversion.rs` and
`pane_row.rs` (wide-glyph normalization) are rendering rules. Proposal: protocol
keeps types, codec, framing and preamble; a `shepr-surface` crate (or a clearly
separate module) owns grid validation, the baseline, planning and decoding;
allocators move up. Workspace ids are similarly allocated from a process-global
`NEXT_WORKSPACE_NUMBER` while uniqueness is owned by `AppState::workspaces`, and
restore must call `reserve_workspace_ids` before any allocation (an ordering
enforced by a comment); an allocator owned by the app state and passed to restore
makes it structural. Reported by contracts and mux-state.

## STR-012 - shepr-api holds CLI workflows

Besides the schema, client and listener, it holds `server_stop.rs` (about 1100
lines of stop orchestration), `status.rs::read_server_presence_at`,
`daemon_exit.rs` and `guidance.rs` (one-variant operator prose in a different
voice from config's guidance). The stop and presence flows are launcher logic used
by the CLI and remote. `listener.rs` reaches back into `server.rs` for
`handle_connection`, `reject_busy_connection` and `send_busy_refusal`; the JSON
connection service could be its own module beside `client_protocol.rs`, leaving
the listener a pure classifier and dispatcher. (contracts)

## STR-013 - shepr-remote is three crates

1. The SSH client side: machine connectors, discovery, the bridge's local half,
   SSH process plumbing, startup preflight, the metadata cache.
2. The remote-host side of the bridge (`host.rs`, `run_remote_client_bridge`).
3. The local server launcher (`local_server.rs`, 860 lines: probing, the launch
   lock, spawning `shepr-server`, the boot log, the sibling-server identity
   behind `status client`, the different-build policy), with no SSH at all. It
   lives here because part 2 calls it, and is used by the TUI launch, the local
   restart offer, `status client` and the bridge host.

Proposal: a launch crate below remote holding `local_server`, sibling status,
`run_remote_client_bridge` (which is "ensure a local server and relay stdio"),
the restart-offer engine, `DaemonExit`, the server invocation grammar and
`--version` parsing; remote becomes SSH only. `shepr-server` links all of remote
for one function, `interactive_shell_command`; with quoting in core the edge and
the `shepr-server-layer` allowance go, so the daemon stops linking the bridge,
discovery, preflight and the launcher. The CLI grammar constants (`PROGRAM_NAME`,
`REMOTE_INSTALL_NAME`, `COMMAND_*`, `FLAG_*`, `option_name_from_flag`) are
exported from `remote/args.rs` and the binary's clap spec imports them from the
SSH crate. Reported by edges and server-app.

## STR-014 - The endpoint failure vocabulary lives in the SSH crate

`EndpointFailure` and its single disposition table now exist
(`crates/shepr-remote/src/failure.rs`), separate from the OpenSSH classifier,
but the client uses them for every endpoint, Local included, so its attention
policy still comes from the SSH crate. They belong in a crate both sides
already use (protocol's endpoint module, or a launch crate). The client may not
depend on `shepr-api` but reaches it transitively through remote. Reported by
edges and client-core.

## STR-015 - Odd edges into the server and client

- `crossterm` is in `app/state.rs` only for a test helper.
- `shepr-test-fixtures` sits below mux, so `shepr-server/src/test_support.rs`
  defines fixture traits for mux types (`PaneRuntimeFixture`,
  `WorkspaceFixture`, `TerminalStateFixture`); other crates above mux that need
  them will duplicate them.
- `shepr-client`'s `shell` depends on `shepr-agent` only for `status_priority`
  and `parse_agent_label`; typing the wire's agent and status removes the edge.
- `HostGeometry` (core) is built on `PaneGeometry`; it belongs to the client (or
  termio), built on `GridSize::clamped` and `ProtocolCellSize`.

Reported by server-app, client-shell and client-core.

## STR-016 - Lock poison policy lives in the emulator crate

`shepr-vt/src/locks.rs` (`lock_auxiliary`, `try_lock_auxiliary`,
`recover_auxiliary_poison`, `lock_terminal_core`, `terminal_core_is_poisoned`) is
general poison policy; every mutex in mux (arbiter, cwd state, teardown tracker,
deferred order, sync timer, `render_signal.rs`) uses it.
`lock_terminal_core<T>` accepts any mutex, so "this is the terminal core" is a
naming convention. Move the auxiliary policy to core or platform, and make the
core a `TerminalCore(Mutex<..>)` newtype whose only lock returns
`Result<Guard, TerminalCorePoisoned>`. Reported by terminal and mux-panes.

## Terminal emulation

## STR-017 - `Terminal` is a god struct whose handler borrows twelve fields

`Terminal` has 25 fields across five concerns: emulator plus parser, history
accounting (`rows`, `history_lines`, `max_scrollback`, `keyboard_depth`), host
colours, the effects outbox (seven queues) and damage tracking. `with_handler`
destructures twelve into `CoreHandler` field by field, which is why the handler
copies `Terminal` methods instead of calling them. Regroup into `Emulator`,
`HistoryCapacity`, `HostDefaults`, `Effects` and `Damage`, with the handler
borrowing the parts it needs. `modes::ModeSpec` stores one fact twice (`get:
Getter::Extra(extra)` and `extra: Option<ExtraMode>`), and struct-literal rows
could set them differently; derive `extra` from `get`. (terminal)

## STR-018 - Dirty-row state lives in vt, its clearing policy in mux

`RenderState` owns `Dirty` and per-row `Cell<bool>` bits; mux
`terminal_collect_dirty_patch` clears rows through shared references
(`RowView::clear_dirty`, interior mutability), computes `rows_left` and calls
`set_dirty`. Any caller can `set_dirty(Clean)` without clearing rows. Move it
into `RenderState::take_dirty_rows(max_rows) -> DirtyRows` that commits on drop
or an explicit `commit()`, and remove `set_dirty` and `clear_dirty` from the
public API. (terminal)

## Pane runtime and agent ownership

## STR-022 - Dependency direction inside `pane/`

- `pane/terminal/backend.rs` constructs `runtime::TerminalDirtyPatchSnapshot`
  and returns `crate::pane::WheelRouting`, both declared in `runtime.rs`: the
  terminal model depends on the runtime layer that owns it.
- `PaneTerminal` takes `&ChildLiveness` and scans `/proc` through `osc.rs`
  (`current_transient_default_color_owner`,
  `should_restore_host_terminal_theme`); the process questions belong to the
  runtime and detection side, and the terminal should only offer
  `note_default_color_owner(generation, Pgid)` and
  `drop_default_color_overrides_if(owner)`.
- `PaneTerminal::render_queued: Arc<AtomicBool>` is render-scheduler state
  stored on the terminal; it belongs with `RenderSignal`'s per-pane state.
- `osc.rs` does four jobs: OSC 7 parsing (hostname matching, percent decoding),
  the OSC framing state machine for the debug log, the agent title and progress
  evidence tracker, and the host-theme restore policy for OSC 10/11 (which scans
  `/proc`). Split into `osc7.rs`, `osc_debug.rs`, `agent_osc.rs`, with the theme
  logic moving per the second bullet.

(mux-panes)

## STR-023 - `process_probe.rs` holds the whole detector

Named for the `/proc` probe, it contains `DetectorState`, the tick protocol, the
screen cache, the probe scheduler, agent presence counting and the publish
helpers, while `agent_detection.rs` holds a few pure decisions the detector
calls. Suggested: `detect/state.rs`, `detect/schedule.rs`, `detect/probe.rs`,
`detect/publish.rs`, with `detection_task.rs` as the async shell and the runtime
tests exercising `foreground_shell_agent_action` (through a `#[cfg(test)]`
wrapper) moving next to the detector. `DetectorState::tick` is a coroutine
flattened into fields: it takes `TickObservation::{Begin, Probe, Screen}`, keeps
per-tick scratch (`tick_schedule`, `tick_agent_changed`, `tick_group_changed`)
between calls, asks for more input through `TickOutput { probe: bool, screen:
bool }`, and relies on every step re-running the screen path with idempotent
gates. A typestate (`begin(obs) -> Tick::{Done, NeedsProbe(ProbeTick),
NeedsScreen(ScreenTick)}` with `resume`) keeps scratch from leaking and stops the
idempotence invariant being load-bearing; one per-tick context replaces most of
`ProcessProbeRequest`, `ProcessProbeScheduleInput`, `ProcessProbeCompletion`,
`ScreenScanGate`, `ScreenReadRequest`, `ScreenPublishContext`,
`DetectionScreenReadInput` and `ScreenDetectionPublishInput`. (mux-panes)

## Workspace, Git and persistence

## STR-024 - `Workspace` is a bag of public fields, and its pane tree is kept consistent at runtime

`Workspace` exposes `id`, `custom_name`, `identity_cwd`, the six Git caches and
`next_public_pane_number` as `pub` and the tree fields as `pub(crate)` so persist
can read and fill them; the server writes Git fields directly; there is no place
its invariants are enforced except `valid_panes` on restore. `layout:
TileLayout`, `panes: HashMap<PaneId, WorkspacePane>` and `root_pane` must name the
same panes; `has_consistent_panes` re-proves it (a `Vec` and a `HashSet`) in
`focus_pane`, `swap_panes`, `resize_focused_pane`, `resize_pane`,
`set_split_ratio_at`, `commit_prepared_split` and `detach_pane`, on every drag
event. The split two-phase dance (prepare on a cloned `TileLayout`, then
`commit_new_pane` re-diffs the two layouts to verify "this layout plus exactly one
pane") exists because the layout is edited apart from the records. Core's `Node`
is fully public, so an invalid tree is constructible anywhere; `SplitBranch` lives
in `geometry.rs` but is a layout path element, and split addresses are
`Vec<SplitBranch>` compared with `==`; `TileLayout::resize_pane` swaps focus,
calls `resize_focused`, and swaps back. Proposal: `Workspace { id, name, tree:
PaneTree, git: GitIdentity }` where `PaneTree` owns `Node`, focus, zoom and the
records together, `prepare_split` returns a `PreparedSplit` token (new id,
reserved number, spawn size) consumed by `commit`, capture and restore go through
`to_snapshot`/`from_snapshot` using only read methods, and a `SplitPath` type
owns path-addressed ratio access. `display_name()` would return `&str`.
Reported by mux-state and foundation.

## STR-026 - Pane chrome geometry is view code living in mux

`workspace/geometry.rs` is ratatui-based border and gap logic (`Block`,
`Borders`, ratatui `Rect`) consumed by the server's UI and surface code, with two
hand-written `Rect` converters; mux itself needs only spawn sizes. Proposal: pure
chrome math on core's `Rect` with a small border bitset, in core, and the ratatui
adapter in the server; mux loses its ratatui dependency except what `pane/`
needs. (mux-state)

## STR-027 - Git status is one subsystem split across a crate boundary through its cache

mux `git/` has the runner, discovery, config dependency tracking and status; the
server's `app/git_refresh.rs` has the scheduler, the cache map, deduplication by
key, pruning, read-error dedup, the worker body and panic containment; the server's
`app/api/checkout_root.rs` runs its own `git rev-parse`;
`AppState::apply_workspace_git_statuses` writes the workspace's Git fields. The
cache travels through `AppEvent::GitStatusRefreshed` and back, and its fields are
public because the server reads them. Proposal: a `shepr-git` crate (runner,
discovery, config, status, and a `GitStatusCache` owning `refresh(targets)` and
retention), with the cache on a long-lived worker so it never crosses the event
channel; mux keeps only the identity value types and
`Workspace::apply_git_status(result) -> bool`; the app keeps only scheduling.
Reported by mux-state and server-app.

## STR-028 - Persistence orchestration lives in the server, and `persist/` files do several jobs each

`App::with_paths` sequences load, history gating, restore, loss and protection
decisions, logging, the empty-workspace fallback and persister spawn;
`session.rs` builds the preserved-layout map and decides Clear versus Save (see
the session-open consolidation). Within mux, `snapshot.rs` holds the on-disk
schema, capture from live state (reading runtimes) and the history carry engine;
`io.rs` holds path layout, the regular-file policy, atomic publish, session file
read and write, and a hand-written history JSON serializer with its fair-share
trim; `writer.rs` is half save orchestration and half the recovery-copy subsystem.
Proposal: `persist/schema.rs`, `capture.rs`, `history/{carry.rs,
serialize.rs}`, `files.rs` (paths, publish, `SessionPath`), `recovery.rs`, and a
`writer.rs` holding only the save sequence. There are also two `SessionSnapshot`
types (`app::api::session::SessionSnapshot`, the projection input, and
`shepr_mux::persist::SessionSnapshot`, the saved file). Reported by mux-state and
server-app.

## STR-029 - Constants and logging are crate-wide grab-bags

mux `limits.rs` mixes Git timeouts, detection cadences, history chunking,
copy-mode word separators, OSC bounds, snapshot cadence and pane teardown signals
(`FIRST_WORKSPACE_NUMBER` restates `WorkspaceId`'s zero rule); mux `logging.rs` is
half pane (taking `pane_id: u32`) and half persist, while `writer.rs` emits its
own events. server `limits.rs` mostly serves `server/` with app-only constants
mixed in. client `limits.rs` puts transport and endpoint timing next to shell
presentation constants, all `pub(super)` at the crate root. Each constant belongs
beside the policy it parameterises. Reported by mux-state, server-app and
client-core.

## Server app

## STR-030 - `App` is an open bag the server loop reaches into

Almost every field is `pub(crate)` or `pub`. The loop writes `app.policy`, calls
`app.session_saver.freeze_session_saves()`, sets `app.state.should_quit`, drains
`app.event_rx`, reads `app.clock`, `app.terminal_runtimes`, `app.last_render_at`,
drains `app.runtimes_replaced_panes`. Render cadence (`last_render_at`,
`last_presentation_at`, `can_render_now`, `can_present_now`,
`record_render_attempt`, `next_headless_loop_deadline_with_git_refresh`) is loop
state living on `App`. Suggested split: `Session` (`AppState` plus mutators
returning outcomes), `Runtimes` (registry, spawn handles, teardown tracker, render
signals), `Persistence` (`SessionSaver` with its own policy), and schedulers the
loop owns (git refresh, resume, default-workspace retry, render cadence), with
coordinating methods returning effects instead of poking signals. The window
title is a pure function of state, template and hostname, yet
`window_title_template` is set after construction by
`configure_validated_window_title` and `hostname` is a field; it fits
`AppSettings` plus a ui function. Reported by server-app and server-serving.

## STR-031 - `AppState` is "pure data" without enforced invariants

Public fields (`terminals`, `workspaces`, `bookmark`, `session_dirty`,
`should_quit`, `host_*`, `next_agent_state_change_seq`); the bookmark doc says
every write goes through two setters while `bookmark` is `pub` (a `Bookmark` type
with private fields would enforce it). Terminals live in a global
`HashMap<TerminalId, TerminalState>` beside the panes that attach them; the
invariants (every pane has a terminal, none is shared) exist only in
`assert_invariants_for_test`, and `remove_unattached_terminal_ids` scans every
pane per terminal to defend against sharing that never happens. Owning
`TerminalState` from `WorkspacePane` (or a store keyed by pane) removes the scan,
`ensure_test_terminals` and the "pane attached to a missing terminal" state.
`AppState::workspace_geometry` keyed by `WorkspaceId::number()` is per-workspace
session data that belongs on the workspace (deleting
`retain_live_workspace_geometry` and `has_workspace_without_area`'s set
reconstruction). (server-app)

## STR-032 - The ui seam versus the patch renderer

`ui` is the pure render of one workspace for one client, but the server's
`retained_surface` patch path reimplements layout validation, scrollbar
visibility, gutter placement and cursor computation, importing only
`render_pane_scrollbar_buffer` and `pane_is_scrolled_back`. The real seam is "how
a pane looks" (ui) versus "which bytes go to which client" (server): move the
per-pane presentation decisions into ui as a `PaneSurface` description both the
full render and the patch diff consume. `resize_surface`/`PaneResizer` (whose doc
admits it is not a barrier) is geometry application, not rendering. On the server
side, `resolve_retained_panes` recomputes layout and checks committed rects, and
the alternate-screen closure in `render_full` re-checks identity consistency with
its own copy; both belong on a `CommittedBaseline { surface, identities }` owned by
`ClientRenderState` (today `surface_pane_identities` sits on `ClientConnection`
and is cleared separately in `request_repaint`). Reported by server-app and
server-serving.

## Server serving

## STR-036 - Pane input takes a detour through `RawInputEvent`

`pane_input::apply_client_pane_input_event` handles `Mouse` and `TextCommit`
first, then converts the rest through `input_wire::WirePaneInput`
(`ClientPaneInputEvent -> RawInputEvent`, the eight-variant host-terminal input
superset) only to get `Key` and `Paste` back. That detour is why
`Other("non-pane input reached targeted pane input")` exists and why `input_wire`
maps `TextCommit` to `Unsupported` and has an unused `Mouse` arm. A direct
`ClientPaneInputEvent::Key -> TerminalKey` mapping removes the trait, the file and
the unreachable error. `apply_scroll` round-trips modifiers through `u8`, and
`lines.max(1)` is applied by both caller and callee. (server-serving)

## STR-038 - `client_shell.rs` is misnamed and does two jobs

`snapshot_from_session` is the shell projection and belongs with
`ShellSessionCache` in `render.rs`; `render_pane_surface` and `split_hit_rect`
are the surface build, and the split hit-rect geometry (borders and gaps) is
layout policy that arguably belongs next to the border rules that draw splits.
(server-serving)

## Client core

## STR-039 - `lib.rs` is the loop, the launch, the finalization and the dispatcher

About 1300 production lines: `run_launched_client` (connect, handshake, panic
installation, terminal setup, runtime, finalization, exit classification),
`run_client_loop` (twelve arguments behind a clippy `expect`), `ClientLoop::new`
(ten arguments) and `handle_server_message` (about 320 lines). `ClientLoop`
handlers destructure `self` in every method because the move's state (`choice`)
lives in `ClientState` while the registry and command lanes live in
`ClientLoop`. Suggested: `launch.rs` (ordered launch phases returning one
`Launched` value, plus finalization; `ClientLoopConfig` goes, it is created with
placeholder fields overwritten after terminal setup); an `endpoints` owner
(registry, supervisors, command lanes, choice, view serial and request-id
allocator, implementing the move protocol now spread across `choice.rs`,
`preparing.rs`, `view.rs`, `registry.rs`, `reconcile.rs` and `lib.rs`);
`dispatch.rs` with the presentation gate as its first step; and a presenter owned
by `ClientState`. `shell_runtime.rs` is not the runtime of the shell but the
loop's glue (request cancellation, input routing, action dispatch, the waiting
notice, `view_geometry`, `resize_views`, keyboard sync, host effect clearing, the
disconnect notice table, snapshot installation, outcome finishing); it,
`transport.rs`, `reconcile.rs`, `events.rs` and `clipboard_forwarding.rs` all
`use super::*` and share `lib.rs`'s namespace. (client-core)

## STR-040 - One connection's I/O is split across two modules and assembled two ways

The writer (`endpoint/writer.rs`, `NativeEndpointTransport`) and the reader
(`transport.rs` at the crate root, `server_reader_thread`) live apart; the reader
is spawned from the writer's internals (`stop_handle()`, `read_activity()`); the
surface decoder is created at two sites; launch assembles a connection with
`start_endpoint_transport` while the supervisor's `establish` builds the writer on
the attempt thread and `EndpointSupervisorEvent::Connected` carries the raw reader
stream back for `handle_endpoint_supervisor` to spawn on the loop thread, with its
own failure branch duplicating the `Status` arm. An
`EndpointConnectionIo::start(stream, lifetime, endpoint, generation)` built inside
the attempt gives one assembly path and one place for the reader-spawn failure.
(client-core)

## STR-041 - `ClientError` serves three roles, and launch validation runs after the terminal is taken

`ClientError` is the launch connect error, the handshake outcome and the loop's
exit reason; `handshake_error` must handle `HostTerminal`, `EndpointSetup` and
`Panicked`, which a handshake can never produce, and `run_launched_client`
classifies the loop's exit by variant. Split into `HandshakeError` (with a
`class()`), `LoopExit` and launch errors. `EndpointSupervisors::new` returns
`connector.launch_fatal_setup_error()` inside `run_client_loop`, after raw mode,
the alternate screen and the stdin and resize threads are up, so a launch-fatal
configuration problem is found after the terminal was taken; it belongs with the
other launch checks before `setup_terminal`. (client-core)

## Client shell

## STR-042 - `ClientShellState` still owns the endpoint, presentation, mode and copy state

The shell has a real module tree with no path attributes or parent globs, and
`Notices`, `ChromeLayout` and `TransientError` own their fields. Still open:
`ClientShellState` keeps about forty fields mutated from some nineteen files.
`Endpoints` (list, active id, collapsed set; `active_snapshot_generation`
mirrors `snapshot_generation`, each with its own "older revision" check),
`Presentation` (surfaces, hits, the last composition, terminal size; the shell's
`last_composed_size` and `lib.rs`'s `reported_geometry` both hold it), `Mode` and
`CopySession` were deferred because their transitions coordinate several
domains; a split needs those transitions reshaped first, not fields wrapped.
Tests still build `ClientCopyModeState` field by field. `shepr-remote`'s `lib.rs` does the same flattening (eleven `#[path =
"remote/x.rs"] mod x;` declarations, `use bridge::*` and friends, most modules
beginning `use super::*`, `impl RemoteExecutable` methods in `launch.rs` for a
type defined in `machine/executable.rs`, `SSH_OWN_FAILURE_EXIT_CODE` and
`failed_before_remote_result` in `bridge.rs` reached through globs), so
`pub(super)`/`pub(crate)` mean nothing there either. Reported by client-shell,
client-core and edges.

## STR-043 - The shell's render mutates state

`render::ShellRenderState` hands render `&mut workspace_scroll`, `&mut
agent_scroll`, `&mut reveal_focused_workspace` and `&mut
reveal_navigation_workspace`; the sidebars clamp and reveal while drawing. Help's
scroll is clamped after compose from a hit-map value; the navigator's effective
scroll is computed in its renderer and never stored while `scroll_navigator_to`
computes another. The server keeps render pure; the client should too: a layout
and scroll resolution pass produces a view model (rows, scroll starts, hit
rects), then drawing is `&self`. `endpoint_command_for_action` is a translator
that also scrolls the agent panel and calls `reveal_workspace`; that side effect
belongs to the caller. (client-shell)

## STR-044 - Overlays should be one module each

The overlay files now sit in a real `shell/overlays/` module, but each overlay
is still spread across `state.rs` (types), `overlay_input.rs` (an `if
matches!` chain per overlay), `mouse.rs` (an arm per overlay), `overlays.rs`
(render), `context_menu.rs`/`global_menu.rs` (items and actions) and
`ShellHitMap` (flat `help_*`, `navigator_*`, `overlay_primary/clear/cancel`,
`*_menu_rows` fields, with `Rect::default()` meaning absent); `OverlayRender` is a
product of every overlay's hit rects copied field by field into `ShellHitMap` in
`compose`. Proposal: per-overlay modules owning state, `render -> (painted,
Hits)`, `on_key`, `on_mouse`, with the overlay enum holding its own hits so stale
or foreign hit fields cannot exist; `ClientRenameOverlay.title` derived from its
target. (client-shell)

## STR-045 - Requests: one typed continuation per command

The ledger's `Work` enum with `answered`/`dropped` dispatch is good (precedent
`3ea7fa4`), but feature-local `RequestId` copies (`CopyPipeline::awaiting`,
`ScrollFlight::request`, `ClientWordSelection::pending`,
`PendingWorkspaceHighlight::request_id`,
`ClientRenameTarget::NewWorkspace::label_lookup`) each re-answer staleness beside
the ledger that claims sole ownership of request identity. If `Work` carried a
per-feature token (a session generation the feature bumps on reset), features
would check the token, and the `opened_since` orphan guard in `dropped_entry`
(which exists because "the types cannot rule that out") could become a type rule:
`dropped` takes a context without `submit`. (client-shell)

## STR-046 - Client-shell code that belongs in another crate

- `wire_cells.rs` and `compose_pane_surface.rs` are frame composition over
  `FrameData`; with its "length must equal width * height" invariant they belong
  where a validated `FrameData` lives.
- `word_bounds.rs` is pure pane-text logic; it belongs with mux's word logic or in
  termio, where the two word definitions should be reconciled.
- `TextEditor` is a generic line editor that fits termio beside `TerminalKey`.
- `preferences.rs` is persistence (atomic write, probe) under `overlays/`; it
  belongs with chrome state, and `store` returns `Result<(), String>` while
  `probe_writable` returns a formatted `io::Error`, the string then shown as the
  endpoint error banner.

(client-shell)

## Edges

## STR-047 - The `src/` launcher's dispatch is split across partial matches

`main.rs::launch_with_args` dispatches in four steps (an `if help/version`, an
`if let Some(cli_command)`, an `if matches!(.. ClientBridge)`, then a `match` with
an unreachable arm); with help and version in `Launch` it is one exhaustive
match. `src/preflight.rs` mixes operator wording (which belongs in the binary)
with the local restart engine (which does not). Proposed layout: `main.rs` parses
and matches once; `launch/{tui.rs, client.rs, bridge.rs}` with the TUI path
absorbing `autodetect.rs`; `cli/{status.rs, server.rs, detect.rs}`; `notices.rs`
for all preflight and launch wording. `shepr-daemon`'s `main` re-decides what
`shepr-api` defines: `report_server_error` and `config_error` map to raw exit
constants by hand; `RunServerError::exit_class() -> DaemonExit` beside the error
and `ExitCode::from(class.code())` remove the `exit_with(i32)`/`u8::try_from`
handling. (edges)

## STR-048 - Preflight and the connectors resolve each machine twice

`MachineSshPreflight` keeps one `MachineProbe` per machine and verifies the remote
executable at startup; `MachineSshConnector::new` builds a fresh `MachineProbe`,
and only the disk cache passes between them, so every connector re-verifies the
cached hint on its first connect: one more SSH round trip per machine right after
preflight verified it. `RemoteSsh::new` (a fresh temporary config directory) is
rebuilt on every preflight check and every stop, and the SSH runtime directory is
validated twice per `RemoteSsh` (`SshControlDir::runtime` and
`write_managed_ssh_config`, which re-derives `runtime_dir` from `app_paths` while
taking a `control_dir` for the socket, so the two can disagree). Proposal: pass
the verified per-machine state from preflight to the connectors
(`MachineSshPreflight::into_connectors()` or one object used by both phases).
(edges)

## Tests

## STR-049 - Test layouts mirror accretion

- `src/tests/mod.rs` in the client holds tests for `terminal_geometry`,
  `terminal_setup`, `errors` and `clipboard_forwarding`, forcing `lib.rs` to carry
  `#[cfg(test)] use` re-exports of their private functions;
  `src/tests/endpoint_choice.rs` builds a full `ClientLoop` fixture that
  `endpoint/view.rs`'s unit tests reach into.
- The client shell's `shell/tests/` groups by feature (`copy.rs` 3040 lines,
  `endpoints.rs` 2549 lines) while unit tests also sit in production files and
  some copy-mode tests live in `input/input.rs`.
- The server's `headless/tests/mod.rs` holds over a hundred tests across
  shutdown, titles, endpoints, projections, retained patches, geometry, input,
  theme, clipboard and host shutdown, because a whole `HeadlessServer` was long
  the only test seam; `ShutdownLifecycle` and `EndpointWorkers` can now be
  tested alone.
- Once components exist, tests should sit with the component they exercise and
  drive its API rather than writing `pub(super)` fields.

Reported by client-core, client-shell and server-serving.
