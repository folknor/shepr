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

The large entries have implementation specs in `notes/spec-*.md`, named in each
entry; two large entries have none yet and say so.

## Spec coordination

The five specs were written in parallel with agreed boundaries. Where they
touch each other:

- `notes/spec-shell-render.md` and `notes/spec-shell-requests.md` land in one
  interleaved order (requests 1; render 1 to 5, then 7; requests 2 and 3;
  render 6 and 8; requests 4 and 5; render 9), recorded under "Combined order
  with D" in the render spec and in section 6a of the requests spec, which
  also adjust their assumptions of each other to it.
- The render spec's reset drops requests while every feature's state still
  exists; both specs keep that order, but with tickets the requests spec's
  rollbacks no longer depend on it.
- `notes/spec-data-model.md` landing 3 and `notes/spec-app-loop.md` landing 2
  both change `Workspace::display_name` to return `&str`; whichever lands
  second drops it.
- The app-loop spec stops the per-pass resume scan with a retired latch on
  `ResumeSchedule` (resume plans only come from restore); a pending-resume count
  on `AppState` from the data-model spec would be the alternative.
- The app-loop spec's crate sealing forces `pub` to `pub(crate)` on `AppState`
  items through `unreachable_pub`; that is its only edit to the data-model
  spec's declarations. It keeps the window title template and hostname on
  `HeadlessServer`, out of `AppState`.
- `notes/spec-pixel-geometry.md` edits files the app-loop spec owns
  (`ClientViewKey` fields, the `ShellConnected`, `ShellResize` and
  `ShellPaneInput` arms, the render key and `SurfaceBoundary::render`), and
  makes `SpawnGeometry`'s cell an `Option<CellPx>`, where the data-model spec
  moves `SpawnGeometry` into `shepr_mux::workspace` and renames mux's
  `PaneGeometry` to `WorkspaceChrome`.

## Crate boundaries and dependency edges

## STR-002 - Platform hosts policy that is not platform

- `ssh_paths.rs` (OpenSSH `%C` expansion, control path naming) is SSH policy;
  the generic piece is the owned runtime directory.
- The clipboard route is a `prefers_osc52_clipboard()` bool threaded through
  five client layers to termio, which decides `!prefers_osc52 &&
  write_clipboard(bytes)`; a `ClipboardRoute::{Osc52, Helpers(session)}` from
  platform would carry the decision.

(foundation)

Platform stays a flat module layout, so the clipboard route is not part of a
regrouping by seam; it stands or falls on its own.

## Terminal emulation

## STR-017 - `Terminal` is a god struct whose handler borrows twelve fields

`Terminal` (`crates/shepr-vt/src/lib.rs`) has its fields across five concerns: emulator plus parser, history
accounting (`rows`, `history_lines`, `max_scrollback`, `keyboard_depth`), host
colours, the effects outbox (seven queues) and damage tracking. `with_handler`
destructures twelve into `CoreHandler` field by field, which is why the handler
copies `Terminal` methods instead of calling them. Regroup into `Emulator`,
`HistoryCapacity`, `HostDefaults`, `Effects` and `Damage`, with the handler
borrowing the parts it needs. `modes::ModeSpec` stores one fact twice (`get:
Getter::Extra(extra)` and `extra: Option<ExtraMode>`), and struct-literal rows
could set them differently; derive `extra` from `get`. Large; no spec
written (lower priority: vt is stable and rarely changed). (terminal)

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

## Workspace and persistence

## STR-024 - `Workspace` is a bag of public fields, and its pane tree is kept consistent at runtime

`Workspace` (`crates/shepr-mux/src/workspace.rs`, with the split and commit
logic in `workspace/pane_tree.rs`) exposes `id`, `custom_name`, `identity_cwd`
and `next_public_pane_number` as `pub` and `root_pane`, `layout`, `panes` and
`zoomed` as `pub(crate)`, which persist capture (`persist/snapshot.rs`) reads
directly and restore fills through `Workspace::from_restored`; restore also
re-implements the public-number rule that `valid_panes` checks. Its Git
identity is private. Nothing outside mux touches the tree fields, but the
server writes `custom_name` directly beside `set_custom_name`, and only tests
write `identity_cwd` (leaving the cached fallback label stale). `layout:
TileLayout`, `panes: HashMap<PaneId, WorkspacePane>` and `root_pane` must name
the same panes, and `has_consistent_panes` (a `Vec` and a `HashSet`) re-proves
it in `valid_panes` on restore and in `focus_pane`, `swap_panes`,
`resize_pane`, `set_split_ratio_at`, `commit_prepared_split` and
`detach_pane`. The split prepares on a cloned `TileLayout` (`prepare_split`
returns a `PreparedSplit` carrying the new id, terminal state, spawn geometry,
public id and the cloned layout), launches, then `commit_new_pane` and
`commit_prepared_split` re-diff the two layouts to verify "this layout plus
exactly one pane", each checking the public number again; this exists because
the layout is edited apart from the records. The split also has two focus
knobs (`prepare_split`'s `focus_new_pane` and `commit_new_pane`'s `focus`).
Core's `Node` is a public enum, but `TileLayout` hides its root and
`from_saved` validates ids and focus, so only the layout-versus-records
agreement is unenforced. `SplitBranch` lives in core's `geometry.rs` beside
`Rect` but is a layout path element, and split addresses are
`Vec<SplitBranch>` compared with `==`; `TileLayout::resize_pane` swaps focus,
calls `resize_focused`, and swaps back. Mux's `workspace::PaneGeometry` (chrome
inputs) shares its name with core's `PaneGeometry` (PTY size). Proposal:
`Workspace { id, name, tree: PaneTree, git: GitIdentity }` where `PaneTree`
owns `Node`, focus, zoom and the records together, `prepare_split` returns a
token (new id, reserved number, spawn size, no cloned layout) consumed by
`commit`, capture and restore go through `to_snapshot`/`from_snapshot` using
only read methods, and a `SplitPath` type owns path-addressed ratio access.
`display_name()` would return `&str`. TYP-003 closes with it only if the tree
makes the record fields private and drops `WorkspacePane`'s `Deref`; TYP-034
and STR-026 are independent. Spec: `notes/spec-data-model.md` landings 1 and 3
(one private `PaneRecord` replacing `WorkspacePane` and `PaneState`, a
`TreePlan` validating restore before it builds, the split token without a
layout, a single focus behaviour, panes saved by public number inside their
layout leaf), closing BUG-061's consistency half, TYP-003 and TYP-002's
snapshot half. Reported by mux-state and foundation.

## STR-026 - Pane chrome geometry is view code living in mux

`workspace/geometry.rs` is ratatui-based border and gap logic (`Block`,
`Borders`, ratatui `Rect`) consumed by the server's UI and surface code, with two
hand-written `Rect` converters; mux itself needs only spawn sizes. Proposal: pure
chrome math on core's `Rect` with a small border bitset, in core, and the ratatui
adapter in the server; mux loses its ratatui dependency except what `pane/`
needs. Not in any spec, but `notes/spec-data-model.md` renames mux's
`PaneGeometry` to `WorkspaceChrome` and moves `SpawnGeometry` into
`shepr_mux::workspace`, which this would build on. (mux-state)

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

mux `limits.rs` mixes detection cadences, history chunking,
copy-mode word separators, OSC bounds, snapshot cadence and pane teardown signals
(`FIRST_WORKSPACE_NUMBER` restates `WorkspaceId`'s zero rule); mux `logging.rs` is
half pane (taking `pane_id: u32`) and half persist, while `writer.rs` emits its
own events. server `limits.rs` mostly serves `server/` with app-only constants
mixed in. client `limits.rs` puts transport and endpoint timing next to shell
presentation constants, all `pub(super)` at the crate root. Each constant belongs
beside the policy it parameterises. Reported by mux-state, server-app and
client-core.

## Server app

## STR-030 - `App` still exposes its internals to the server loop

`SessionSaver` (with its own `SavePolicy`), `ResumeSchedule` and
`GitRefreshScheduler` (over the `shepr-git` worker) are extracted. Still open:
`App` is `pub` in a `pub mod app` with `pub state`, `pub event_tx` and `pub
render_notify` and most other fields `pub(crate)`, though the daemon links only
`run_server`. `server/headless*` reads `app.state`, `terminal_runtimes`,
`event_rx`, `policy`, `session_saver`, `render_dirty`, `render_notify`,
`clock`, `paths` and `restore_notice`, and drains `runtimes_replaced_panes`
itself; `git_refresh` and its flag fields are `pub(crate)` only for tests, and
`resume_schedule`, `persist_pane_history`, `last_render_at` and
`last_presentation_at` are never used outside `app/`. `event_tx` is read only by
tests once construction has cloned it. Render cadence (`last_render_at`,
`last_presentation_at`, `can_render_now`, `can_present_now`,
`record_render_attempt`) and the deadline fold
`next_headless_loop_deadline_with_git_refresh` live on `App` in
`app/runtime.rs`, though only the loop uses them. `AppPolicy` and
`persist_pane_history` sit on `App` beside the saver that decides persistence.
The default-workspace retry (`default_workspace_retry_at`,
`default_workspace_retry_failures`) is a hand-rolled scheduler of
`ResumeSchedule`'s shape (and is never reset, BUG-069). Host-theme state is
split between `App.live_host_theme_reported` and `AppState.host_terminal_*`.
`AppState::clock_now` beside `App.clock` is effectively dead: `set_clock`
writes both, but only one test reads it. The window
title is a pure function of state, template and hostname, yet
`window_title_template` is set after construction by
`configure_validated_window_title` and `hostname` is a field. Suggested:
`App` and its fields private with a narrow surface for the loop; cadence, the
deadline fold and the default-workspace retry in loop-owned schedulers beside
resume and git; the saver owning its policy; `AppSettings` plus a ui function
for the title. Spec: `notes/spec-app-loop.md` (eleven landings: private `App`
opened as `App::open -> (App, AppOutputs)`, a loop-owned `schedule.rs` with one
deadline fold, effects instead of signals, `AppPolicy` deleted, a pure
`ui::render_window_title`, the headless tests split per subject), also closing
BUG-057, BUG-060's loop clauses, BUG-069, BUG-077, CON-076 and CON-113.
Reported by server-app and server-serving.

## STR-031 - `AppState` invariants are checked only by `assert_invariants_for_test`

Public fields (`terminals`, `workspaces`, `bookmark`, `session_dirty`,
`next_agent_state_change_seq`, `host_terminal_*`). The `bookmark` doc says
every write goes through `set_bookmark` and `set_bookmark_index`, but the field
is `pub` and tests assign it; a `Bookmark` type with private fields would
enforce the doc. Terminals live in a global `HashMap<TerminalId,
TerminalState>` beside the panes that attach them, with a second derived index
`pane_terminal_ids` that restore, split, focus and removal must keep in step.
Every cross-structure invariant (the index equals the live panes, every pane's
terminal exists, none is shared, pane and workspace ids are unique, the
bookmark position equals its index) exists only in
`AppState::assert_invariants_for_test` and
`Workspace::assert_invariants_for_test`. `remove_unattached_terminal_ids`
rescans every pane of every workspace per removal to defend against sharing
that never happens. Owning `TerminalState` from `WorkspacePane` (or a store
keyed by pane) removes the scan, the index, `ensure_test_terminals` and the
"pane attached to a missing terminal" state. `workspace_geometry` (keyed by
`WorkspaceId`) is per-workspace session data that belongs on the workspace,
which deletes `retain_live_workspace_geometry`. The `AppState` doc ("pure
data", mutation state belongs to `App`) no longer describes a struct holding
derived indices and the drained `lifecycle_authority_dirty` set. Spec:
`notes/spec-data-model.md` landings 2, 4 and 5 (a `WorkspaceSet` owning the
workspace id allocator and bookmark, records owning their terminals, spawn
geometry per workspace, `WorkspaceId`-keyed APIs closing TYP-004 and CON-061,
and `TerminalId` retired, since it maps one to one onto `PaneId` for a pane's
whole life and is never on the wire or saved). (server-app)

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
`Notices`, `ChromeLayout` and `TransientError` own their fields. `Endpoints`
(`shell/endpoints.rs`) owns the entry list and the `EndpointChoice` whose
`presented()` is the active id. Still open: `ClientShellState` keeps dozens of
fields, mutated from most of the shell's `impl ClientShellState` files.
`collapsed_endpoints`, `snapshot`, `active_snapshot_generation` and
`active_boot_key` stay on the shell, and the per-endpoint snapshot cache
(`cache_endpoint_snapshot_at_generation`) and `apply_active_snapshot` each
carry their own "older revision" check. `Presentation` is still loose:
`last_composed_size`, `last_composed_at`, `last_composition` and `hits` sit
beside `PaneSurfaces` (`ClientState::reported_geometry` is a different fact,
the drawn frame's size; the defect is the unpaired early return writing
`last_composed_size`).
`Mode` and `CopySession` (`mode`, `copy_mode`, `copy_pipeline`,
`mouse_selection`, `navigate_workspace_id`) were deferred because their
transitions (`apply_active_snapshot`, `reset_endpoint_projection`,
`presented_surface_changed`) coordinate several domains; a split needs those
transitions reshaped first, not fields wrapped. `ClientCopyModeState` is
built field by field in one production site and four test sites. Spec for the
client half: `notes/spec-shell-render.md` landings 3 to 6 (`Presentation`,
`ModeState`, `CopySession` with one constructor, and `ActiveProjection` with
one older-revision rule). `shepr-remote`'s
`lib.rs` does the same flattening: a `#[path = "remote/x.rs"] mod x;`
declaration per module (plus nested `#[path]` for `relay_watchdog` and the
`*_tests.rs` files), `use bridge::*` and friends, `use super::*` at the top of
the bridge, ssh, launch, server_lifecycle and discovery modules, `impl
RemoteExecutable` methods in `launch.rs` and `shell_command.rs` (whose two are
plain aliases of `command` and `bridge_command`) for a type defined in
`machine/executable.rs`, and `SSH_OWN_FAILURE_EXIT_CODE` in `bridge.rs`
reached through globs, so `pub(super)` means "crate root" there; `lib.rs`
itself also holds `SshFailureDiagnostic` and the exit types every glob module
pulls from. The `remote/` directory name reflects that layout and goes with
the fix. The shepr-remote half is large; no spec written. Reported by
client-shell, client-core and edges.

## STR-043 - The shell's render mutates state

`ClientShellState::compose` (`shell/presentation/composition.rs`) takes `&mut
self` and, besides assigning `hits` wholesale, writes
`reveal_navigation_workspace` (on a size change), `last_composed_size` (even
on the unpaired-surface early `None` return), `last_composition`,
`last_composed_at`, `mouse_selection.repaint_deadline`, `notices` (through
`drawn`) and the Help overlay's `scroll` (clamped to `hits.help_max_scroll`
after drawing). `render::ShellRenderState` also hands the sidebar renderers
`&mut workspace_scroll`, `&mut agent_scroll` and both reveal flags:
`endpoint_sidebar::render_collapsed` and `render_expanded` consume the flags
with `mem::take` (the collapsed one even with an empty body) and clamp and
reveal the scroll while drawing, and `agent_sidebar::render_agent_list` clamps
`agent_scroll`. The navigator's effective scroll is computed in
`render_navigator_overlay` and never stored, while `scroll_navigator_to`
clamps differently; input handlers read `hits.help_max_scroll` and the scroll
metrics render produced. `activate_endpoint_projection` saves and restores
`agent_scroll` around `apply_active_snapshot` to work around the shared
mutable scroll. The server keeps render pure; the client should too: a layout
and scroll resolution pass produces a view model (rows, scroll starts, hit
rects), then drawing is `&self`. `endpoint_command_for_action` is a translator
that also scrolls the agent panel and calls `reveal_workspace`; that side effect
belongs to the caller. The bugs this causes are filed as BUG-070. Spec:
`notes/spec-shell-render.md` landings 1 to 3 (a pure `resolve_sidebar`, an
`OverlayView` per overlay, and `compose` split into pure `resolve_frame` and
`draw_frame` with `commit_frame` the only writer). (client-shell)

## STR-044 - Overlays should be one module each

The overlay code sits in `shell/overlays/`, but each overlay is still spread
across `state.rs` (types), `overlays/overlay_input.rs` (an `if matches!` chain
per overlay in `route_overlay_key`), `input/mouse.rs` (a block per overlay plus
`overlay_primary`/`overlay_clear` dispatch), `overlays/mod.rs` (render),
`context_menu.rs`/`global_menu.rs` (items and actions) and `ShellHitMap` (flat
`help_*`, `navigator_*`, `overlay_primary/clear/cancel`, `*_menu_rows` fields,
with `Rect::default()` meaning absent); `OverlayRender` is a product of every
overlay's hit rects copied field by field into `ShellHitMap` in `compose`, and
`render_client_overlay` returns `None` for the two menus, which `compose`
renders through their own functions first. Proposal: per-overlay modules owning state, `render -> (painted,
Hits)`, `on_key`, `on_mouse`, with the overlay enum holding its own hits so stale
or foreign hit fields cannot exist; `ClientRenameOverlay.title` derived from its
target. Spec: `notes/spec-shell-render.md` landing 7. (client-shell)

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
`dropped` takes a context without `submit`. `abandon_copy_operation` and
`ScrollLanes::retain_panes` already rely on a feature-local id mismatch to make
a still-ledgered request stale. `ClientCopyModeState::operation_generation`,
carried in `Work::CopySearch`, is a per-feature token already: it decides
whether a search result applies at all, not only the deferred
copy-after-search, and it restarts at 0 per copy session (a latent collision
masked by the pipeline reset). Outside the shell, the endpoint layer
(`endpoint/choice/focus_lane.rs`, `endpoint/view.rs`,
`endpoint/choice/preparing.rs`, `endpoint/commands.rs`, and a fire-and-forget
view-off id in `endpoint/registry.rs`) keeps its own request ids outside the
ledger. No stale answer is applied anywhere today; the five checks are
correct. Spec: `notes/spec-shell-requests.md` landings 2 and 3 (a `Ticket`
from one shell-wide counter carried by each `Work` variant, a `Rollback`
context without `submit`, the `opened_since` guard deleted; the endpoint
layer's ids stay out of the ledger as wire correlation). (client-shell)

## STR-039 - The client move protocol has no owner, and ClientLoop is open

`lib.rs` is now launch, loop and dispatch modules with no production glob
imports. Still open: the endpoint move protocol is spread across
`endpoint/choice.rs`, `endpoint/choice/preparing.rs`,
`endpoint/choice/focus_lane.rs`, `endpoint/view.rs`, `endpoint/registry.rs`,
`endpoint/commands.rs` and the crate-root `reconcile.rs` and `dispatch.rs`
with no one owner, and every `ClientLoop` field is `pub(crate)` so
`reconcile.rs` and `dispatch.rs` can reach in;
`ClientLoop::new` takes ten positional arguments. (The presenter staying in
`ClientState` was declined with its reason at the code: host-mode writes and
pane patches share one blit baseline.) Spec: `notes/spec-shell-requests.md`
landings 4 and 5 (an `EndpointHub` in `endpoint/hub.rs` as the only code that
moves the endpoint choice, the shell no longer committing a move inside
`activate_endpoint_projection`, `ClientLoop` closed with a five-part `new`).
(client-core)

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
