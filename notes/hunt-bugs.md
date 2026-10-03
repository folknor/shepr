# Bugs from the design hunt

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

Defects the design hunters turned up on the way. Unverified: each is a
hunter's reading, with the hunter's own confidence where they gave one. The
raw reports are in the commit that precedes this file's.

Specs for the large items live beside this file in `notes/spec-*.md`; an
entry that one of them closes names the spec and landing.

## Defects

## BUG-068 - OSC 7 reports from a fully qualified hostname are dropped

`shepr_platform::hostname()` cuts the name to its short form (tmux `#h`
style), and that one value is now what the OSC 7 parser matches against
(`parse_file_uri_cwd` in `crates/shepr-mux/src/pane/osc.rs`, fed through pane
construction). So `is_same_host`'s "local qualified name" branch can never
fire: on a machine whose `$HOSTNAME` is fully qualified, vte.sh and fish report
`file://box.lan/...`, which is compared against `box` and rejected as another
machine, and the pane's cwd silently stops following. Fix: platform exposes
the full node name for OSC 7 matching while the window title keeps the short
form. (bug round, hostname fix)

## BUG-069 - The default-workspace retry is never reset

The reset branch in `App::create_default_workspace` cannot be reached, because
`create_automatic_workspace` returns early whenever a workspace exists. Retry
failures pile up across separate empty periods, and a stale retry time still
wakes the loop once. Spec: `notes/spec-app-loop.md` landing 4 (`CreationRetry`).
(spec B)

## BUG-070 - The shell's render loses scroll positions and reveals

Found while specifying the shell render purity work (STR-043); each has a
named failing-first test in the spec.

- The navigator's effective scroll is computed while drawing and never
  stored, so after scrolling down with the keyboard, Up drags the viewport
  instead of moving the selection inside it.
- One frame with an empty list body resets the expanded workspace scroll to 0
  (through `metrics.start()`) and the agent list scroll (an explicit `= 0`), so
  a briefly tiny terminal loses both positions.
- `reveal_endpoint_agent` drops the reveal when the agent body height is 0;
  both immediate reveals use the last frame's body heights, which are wrong
  after a sidebar toggle in the same input batch; the collapsed sidebar
  consumes its reveal flags even when its area is empty.
- A Help overlay too big for the window has its scroll reset to 0 by the clamp
  at the end of `compose`.

Spec: `notes/spec-shell-render.md` landings 1 to 3. (spec C)

## BUG-071 - `CSI 16 t` answers the raw cell while every other report is clamped

The PTY winsize, `CSI 14 t` and mode 2048 report `PaneGeometry`'s clamped
extent, but `CSI 16 t` answers the raw cell size. Spec:
`notes/spec-pixel-geometry.md` landing 2. (spec E)

## Latent defects

## BUG-073 - Losing the shown endpoint drops never-sent commands as interrupted

`transition_endpoint_status` runs before `EndpointCommands::disconnect`, so
queued commands that were never sent are dropped through the blanket
`Interrupted` path instead of the lane's unsent split. Nobody sees it today:
the "Action interrupted" notice it can push is immediately overwritten by the
connection-lost notice. Spec: `notes/spec-shell-requests.md` landing 1.
(spec D)

## BUG-074 - Restore reserves agent sessions before the workspace can be refused

Restore reserves agent sessions and carries history before
`Workspace::from_restored` validates the workspace, so a refused workspace
would leak its reservations; it is unreachable today only through the order of
two calls. Spec: `notes/spec-data-model.md` landing 3 (`TreePlan` makes the
order structural). (spec A)

## BUG-075 - The Git runner's deadline does not bound a hung child

`kill_and_reap` in `crates/shepr-git/src/runner.rs` calls `child.wait()` with no
bound after a timeout, so a git child stuck in an uninterruptible wait on a
hung mount cannot be reaped and the 5 s probe deadline does not hold; the
runner's comments overstate it. When the drain deadline passes,
`join_drains_until` leaves the `shepr-git-pipe` reader threads detached for as
long as a grandchild holds the pipe, with nothing bounding them. Worker
abandonment (`GitStatusWorker::abandon_stalled`) now covers the refresh, but a
stall with no step running (a destructor) is abandoned with no paths to keep
out, so a recurring one uses up an abandoned-thread slot each time. (bug
round, Git worker abandonment)

## BUG-076 - An expired parked hook start keeps reading as parked

The pane's last unapplied hook report (shown by detect explain) is not cleared
when the pending start it records expires (`PARKED_START_LIFETIME` in
`observe_process`, shepr-detect ownership), so explain keeps calling it parked
with a growing age. (bug round, hook outcome in explain)

## BUG-066 - Server overlay glyph repair is weaker than the client compositor's

`shepr_surface::glyph_repair` holds two repair operations that share the blank
cell and the width rule but decide differently. `split_glyph_cells` (the
client compositor) repairs the whole covered region; `put_run` and
`overlay_buffer` (server chrome overlays) repair only each run's edges, and
only when the neighbour is an empty-symbol tail. So the server leaves an
orphaned empty tail after a narrow cell, keeps a wide lead whose tail is a
space continuation (the form chrome itself writes), and copies a scratch
continuation at the left edge where the client blanks it. The likely visible
effect is a wide glyph spilling over a neighbouring column next to an overlay.
One repair rule would settle it, at the cost of changing server output. The
header comment of `crates/shepr-server/src/ui/chrome.rs`, which says `put_run`
cannot leave half a glyph of what it replaces, is wrong until then.
(wave-7 review)

## Hot-path costs

## BUG-055 - Per-cell work that cannot contribute in `terminal_cell_paint`

`cells.fg_color()` is `None` exactly when `basic.style.fg_color` is `None`, so
the `.or_else(|| cells.fg_color()..)` arms never contribute, yet `cell_color`
is computed twice per cell. `terminal_buffer_symbol_into` re-measures
`symbol.width()` for every cell of every dirty row. Both run per cell per patch
collection. (terminal)

## BUG-057 - `render_plan` allocates per wake and locks cores for clients with surface debt

`render_plan` runs on every loop wake, sometimes twice, and builds and sorts
`render_targets` each time. A client with surface debt and a free slot reaches
`surface_deliverable`, which locks every visible pane core
(`synchronized_output_state`) once per workspace per plan through the `held`
memo; `render_full` repeats the check with a fresh memo. A cached `held` per
workspace per epoch, or a mux-side "synchronized output ended" signal, avoids
the locks, and caching the targets avoids the allocation. The settle step also
allocates on every plan. Spec: `notes/spec-app-loop.md` (`ClientRegistry` as a
`BTreeMap`, a lock-free synchronized-output mirror in shepr-mux).
(server-serving)

## BUG-060 - Per-loop scans in the app

`start_pending_agent_resumes` runs every loop iteration and starts with
`has_pending_agent_resumes`, a scan of every terminal's resume state, at least
twice per pass; per client per frame, `compute_surface_for`, `render_panes`
and `surface_cursor` resolve the target `WorkspaceId` by linear scan
(`AppState::workspace_index`), six times per surface render in all;
`Workspace::display_name()` clones a `String` per read on projection and
title paths. The clauses split across specs: the resume scan goes with
`notes/spec-app-loop.md` (a retired latch on `ResumeSchedule`); the repeated
resolution goes with the same spec's once-per-client `SurfaceTarget`, while
`notes/spec-data-model.md` landing 4 keeps the linear workspace lookup itself
deliberately (a handful of workspaces, no index to keep in step) and comments
it at `WorkspaceSet::get`; `display_name()` returns `&str` in
`notes/spec-data-model.md` landing 3 (spec B claims the same change). Reported
by server-app and mux-state.

## BUG-077 - `invalidate_shared_view` wakes the loop it is already running in

It calls `notify_one()` from code already inside a loop pass, so every such
change triggers one extra empty pass. Spec: `notes/spec-app-loop.md` (methods
return effects instead of poking signals). (spec B)

## BUG-061 - `has_consistent_panes` runs on every drag-resize event

`set_split_ratio_at` (the mouse-drag path) re-proves layout and record
agreement (a `Vec` and a `HashSet`) per drag event; `resize_pane` does the same
per keyboard resize. The drag handler also resolves its path with core's
`split_path_for_children` per event (a `pane_ids` Vec per node, quadratic
membership checks). The consistency re-check disappears with the pane tree
(STR-024, `notes/spec-data-model.md` landing 3); the per-event path resolution
does not, and goes with the server-minted layout epoch of CON-109. (mux-state)

## BUG-065 - `ValidatedClientConfig::live_keybinds()` clones the whole keymap per call

(contracts)
