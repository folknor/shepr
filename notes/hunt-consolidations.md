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

## CON-051 - The detection task does not stop on the arbitrated ending

`ChildLiveness::observe` covers seven observation paths, and the server
detector-event gates refuse updates after an arbitrated ending even with the
child alive. Still open: `DetectionTask` (`crates/shepr-mux/src/pane/detection_task.rs`)
stops through child liveness or cancellation, not when the exit arbiter has
decided; wiring that needs a cancellation from the arbiter, not an arbiter lock
at every tick checkpoint. (mux-panes)

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
daemon parser are separate; `ServerAddress`'s stop guidance (shepr-paths
`guidance.rs`) spells the local stop argv itself. A complete invocation model needs a home below config and remote.
(edges, contracts)

## CON-109 - The client still reconstructs the server split tree

One allocation-free helper now decides which side of a split a pane is on.
Still open: the client still hashes a topology signature and sends child lists,
and the server reverse-maps them (`split_path_for_children` in shepr-core
`layout.rs`); a server-minted layout epoch would let the client send
`(workspace, path, epoch, ratio)`, which needs protocol, server API and
workspace topology changes. It also carries the half of BUG-061 the pane tree
does not close: the drag resolves its path with `split_path_for_children` per
event. `notes/spec-data-model.md` landing 1 adds the `SplitPath` type this
would address by. (client-shell, server-serving)

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

## CON-022 - Is pixel mouse eligible, and who decides?

The connection carries one `pixel_mouse` bool (`TerminalGeometry::exact()`,
recomputed on connect and on `ShellResize`). Still open, the remaining
deciders: the client's `effective_sgr_pixel_mouse` (`terminal_setup.rs`) and
`pane_mouse_position` (hit extent above zero); the server's `ShellPaneInput`
arm adds `client.outbox.told_sgr_pixels()`, the outbox dedupe memo, as an
authority (and `forget_presentation()` clears it, downgrading until the next
`tell_mouse_capture`); `stream_host_mouse_capture_mode` ANDs
`client.pixel_mouse` with the focused runtime's mode and ignores the pane's
extent; `pane_input::downgrade_ineligible_pixel_mouse` is the real gate (grid
and `PanePixelSize` equal, coordinates in range); mux `encode_mouse_event`
maps cells to pixels through its own `cell_pitch`. The pane mode bit is also
mirrored three ways (`PaneTerminal::sgr_pixel_mouse_enabled`, the `PaneRead`
field copied into `PaneSurfacePane.sgr_pixel_mouse`, and
`InputModes::sgr_pixel_mouse_enabled`). The result is safe but wasteful: a
client whose cell differs from the geometry-source client's is told to capture
pixels, sends them, and is always downgraded to cells. Owner: eligibility as
one function next to the mouse protocol type, taking the connection bool, the
pane extent and the report, used by the capture stream (per client and pane),
the admission gate and the encoder; the memo stops being an authority. Shares
its fix with CON-023. The spec found the mode bit read four ways (the dirty
snapshot too, and `InputModes` holds it twice), and that surface activation
also clears the outbox memo and deliberately does not re-tell until the
replay, so pixel reports in that window are all downgraded. Spec:
`notes/spec-pixel-geometry.md` landings 2 and 3 (every client is published the
pane's one extent and captures pixels when its host cell is exact, the focused
pane is in 1016 with a known extent, and it shows the pane at the PTY's grid).
(terminal, server-serving)

## CON-023 - How big is a pane in pixels?

`shepr-core` `PaneGeometry::text_area_px()` (clamped to `u16`; drives the PTY
winsize, `CSI 14 t`, the 2048 report, vt `width_px()/height_px()` and mux
`PaneRuntime::pixel_size()` as `PanePixelSize`); server
`pane_surface.rs::render_pane_surface` computes `inner_rect` times the
rendering client's `HostCellSize`, unclamped, with `(0, 0)` as unknown,
published as `PaneSurfacePane.pixel_width/height`; mux `encode_mouse_event`'s
`cell_pitch` divides `width_px()` by cols again. With several clients of
different cell sizes the child is told the geometry-source client's extent
while each client is published its own, so the admission gate has to reject
the mismatch; an oversized pane compares unequal through the clamp and always
downgrades. Owner: `PaneGeometry` publishes the pane's extent once
(`Option<PanePixelSize>`, clamped one way) and the wire carries that rather
than a per-client product; the mux pitch derives from the same value. Spec:
`notes/spec-pixel-geometry.md` (`PaneGeometry::pixel_extent()`, landing 3).
(terminal, server-serving)

## CON-114 - Pane surface patch admission has two rules

The shared admission accepts any pane metadata, while the client adds a
stricter `pane_geometry_matches` (`shell/presentation/surfaces.rs`). Spec:
`notes/spec-pixel-geometry.md` landing 3 narrows the client's check so a patch
carrying an extent change is not refused. (spec E)

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
them (`ui/surface.rs`, `retained_surface.rs`, `pane_surface.rs`): a deliberate
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
and termio's `TextEditor::word_boundary`. The first two apply to the same pane text and
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

## CON-056 - Pane chrome content fields are finished outside geometry

The border-only inner rect is built correctly, zoomed panes included, and one
`content_layout` serves spawn, UI, resume and retained paths. Still open:
`PaneChromeInfo`'s content and scrollbar fields need the screen-dependent gutter
finalized by callers in `crates/shepr-server/src/ui/panes.rs` and
`server/headless/retained_surface.rs`; separate pre- and post-finalization
types would enforce it. (mux-state, server-app, server-serving)

## CON-061 - find_pane still walks workspaces

`terminal_of` no longer scans on a miss, and every assembly path maintains the
pane to terminal index. Still open: `find_pane` in
`crates/shepr-server/src/app/ids.rs` walks workspaces for the position and
`PaneState`, and attachment is recorded both on the workspace pane and in the
index (the reason the index alone cannot answer is at the code). Spec:
`notes/spec-data-model.md` landing 4 deletes the index (records own their
terminals) and keeps lookup by pane as a deliberate walk, one hash probe per
workspace. (server-app, mux-panes)

## CON-076 - Several render and delivery steps still resolve the viewed workspace themselves

Both render paths use one `projection_due` rule and `ViewedWorkspace`. Still
open: within one pass, `surface_deliverable`, the shared-surface key counting
(`pane_surface_render_key`) and `render_client_full` in
`crates/shepr-server/src/server/headless/render.rs` each resolve the client's
viewed workspace again (`shell_target_for_client` or
`ViewedWorkspace::for_location`), as do `sync_immediate_pty_sources` and
`any_shell_surface_contains_pane`, each through a linear `workspace_index`
scan. Geometry settlement also resolves every client's view once per
workspace. Resolving once per client per pass and passing the view down removes
the repeats. Spec: `notes/spec-app-loop.md` (`SurfaceTarget`). (server-serving)

## CON-113 - Whether this boot persists is recorded three times

`AppPolicy`, the saver's `SavePolicy` and `HostShutdownFreeze::persist_session`
each hold it, kept in step by hand. Spec: `notes/spec-app-loop.md` deletes
`AppPolicy` and leaves `SavePolicy` the one record. (spec B)

## Persistence and session saves

## CON-009 - Publish paths still keep their own staging names

One platform `publish_file` (`crates/shepr-platform/src/publish_file.rs`) serves
persistence, agent config replacement and the SSH metadata cache, each keeping
its durability policy. Still open: lease-protected fixed staging names in
persistence and the integration collision-test seams keep their own temp naming.
(foundation)

## App and server loop

## Wire, handshake and surfaces

## Config and keybindings

## Edges

## Client
