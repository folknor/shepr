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

Decided: move the bridge relay, its watchdog and the SSH attempt timing into
remote, keeping the heartbeat cadence and bridge expiry related through one
shared connection-health constant with the assertion in remote. The
suspend-aware `CLOCK_BOOTTIME` clock stays in platform. `ChildExitReason`
stays in platform (shepr-detect ownership consumes it, and AGENTS.md assigns
exit classification there); platform stays flat; `config_file.rs`'s
ownership, permission and xattr primitives stay in platform. Last of the crate
waves.

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

Decided: the workspace allocator is owned by `AppState` and passed to restore;
terminal ids are allocated with terminal creation and the boot id with server
lifetime construction; protocol keeps canonical construction and parsing. A
surface component above protocol owns delta planning, the decoder baseline,
ratatui conversion, wide-glyph normalization and, from STR-046, frame
composition. `FramingError` must stop embedding `SurfaceDecodeError`, or the
extraction cycles. Shared validated grid types that move below protocol must not
import protocol envelopes; the compositor takes styles as arguments.

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

Stale in part: checkout-root handling already uses `discover_checkout_root`,
and retention and read-error dedup already sit in mux's private
`GitStatusCache`. Decided: `shepr-git` below mux owns its key, result and error
vocabulary, discovery, runner, refresh algorithm and cache (mux re-exports what
workspaces need), on a long-lived worker that takes targets and refresh intent
and returns results, importing no `Workspace` or server event. Pin explicit
invalidation and clearing (today `mark_due` and `clear`), completion after a
failed or panicking refresh, and coherent cache commits (a caught panic must not
leave a half-mutated persistent cache). AGENTS.md's Git ownership sentence
changes with it.

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

## Client core

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
`pub(super)`/`pub(crate)` mean nothing there either; the `remote/` directory
name itself reflects that older layout and goes with the fix. Reported by
client-shell, client-core and edges.

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

Decided: frame composition (wide-glyph repair, clipping, hyperlink remapping)
moves into STR-011's surface component; `TextEditor` to host-side input;
preferences with chrome state, keeping typed errors until presentation.
Declined: unifying the word definitions. Double-click deliberately prefers URLs
and quoted paths while mux word motion distinguishes word and big-word, and
that distinction stays explicit (CON-038 covers saying so at the code).

## STR-039 - The client move protocol has no owner, and ClientLoop is open

`lib.rs` is now launch, loop and dispatch modules with no production glob
imports. Still open: the endpoint move protocol is spread across
`endpoint/choice.rs`, `preparing.rs`, `view.rs`, `registry.rs`, `reconcile.rs`
and `dispatch.rs` with no one owner, and every `ClientLoop` field is
`pub(crate)` so `reconcile.rs` and `dispatch.rs` can reach in;
`ClientLoop::new` takes ten positional arguments. (The presenter staying in
`ClientState` was declined with its reason at the code: host-mode writes and
pane patches share one blit baseline.) (client-core)

## Edges

## STR-047 - Preflight wording still sits with the restart engine

`src/main.rs` dispatches all six launch variants in one match with explicit TUI
modes. Still open: `src/preflight.rs` mixes operator wording with the local
restart engine, `src/autodetect.rs` stays a separate module rather than part of
the TUI launch path, and daemon error classification stays in the daemon
`main`. (edges)

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
