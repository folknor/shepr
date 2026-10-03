# Spec: App behind a narrow surface, the loop owning its schedules

A plan, written against `reference/technical-implementation-spec.md`. It covers
STR-030 (`notes/hunt-structure.md`), BUG-057 and BUG-060 (`notes/hunt-bugs.md`)
and CON-076 (`notes/hunt-consolidations.md`), re-verified against the code on
2026-10-03. The owner's framing: this is the biggest maintainability win left
on the server. The target is `App` and its fields private behind a narrow
surface the server loop uses; cadence, the deadline fold and the
default-workspace retry in loop-owned schedulers; coordinating methods that
return effects instead of poking signals; the session saver owning its policy;
and the window title as a pure function of settings and state.

Neighbouring specs written at the same time: Spec A (`notes/spec-data-model.md`)
owns `AppState`'s shape, invariants, terminal ownership and the workspace pane
tree; this spec treats `AppState` as opaque behind accessors. Spec E
(`notes/spec-pixel-geometry.md`) owns `stream_host_mouse_capture_mode` and pixel
mouse eligibility in `render.rs` and `pane_input.rs`. The last section lists
what this spec assumes of them and what it offers them.

## Standing references

- Contract this spec is written against: `reference/technical-implementation-spec.md`.
- Spawned from: STR-030 in `notes/hunt-structure.md`, BUG-057 and BUG-060 in
  `notes/hunt-bugs.md`, CON-076 in `notes/hunt-consolidations.md`.

## Contracts inventoried

- `reference/` holds only `technical-implementation-spec.md`. There is no
  `docs/` folder.
- `AGENTS.md`, Principles: state separated from runtime; render pure
  (`compute_surface_for()` in `crates/shepr-server/src/ui/surface.rs` reads
  `AppState` by shared reference); presentation per client (`render_plan` in
  `crates/shepr-server/src/server/headless/render.rs`,
  `workspace_geometry_source` in `client_views.rs`, `sync_pane_focus`); no god
  objects (`AppState` in `app/state.rs`, `App` behaviour across `app/`); hot
  paths multiply. Every statement there stays true after this spec; no
  `AGENTS.md` edit is owed. The function names it cites (`compute_surface_for`,
  `render_plan`, `workspace_geometry_source`, `sync_pane_focus`) are kept.
- `brokkr.toml` textlints that touch this ground:
  `app-state-reads-the-clock-seam` (no clock reads under `app/`),
  `headless-loop-reads-the-app-clock` (only the sampler reads the clock in
  `server/headless*`; its message names `app.clock`), and
  `headless-internal-events-go-through-forwarding`. Landing 1 rewords the
  second one's message; the rules themselves are unchanged.
- Workspace lints in the root `Cargo.toml`: `unreachable_pub = "deny"` and
  `unused = "deny"`. Landing 9 depends on both.

## Stopping rule

In scope: `App`'s fields and surface, the loop's schedulers and deadline fold,
`AppPolicy`, the window title, the clock, the effect returns that replace
`App::invalidate_shared_view` and `runtimes_replaced_panes`, the per-wake work
of `render_plan` and the render pass (BUG-057, BUG-060, CON-076), the
`AppEvent` channel and render signal ownership, the crate's public surface, and
the relocation of the server's tests.

Out of scope, named so nothing is silently deferred:

- `AppState`'s field layout, invariants, the pane/terminal index and the
  workspace tree: Spec A. This spec only changes the `pub` keyword on
  `AppState` items where Landing 9's `unreachable_pub` forces it, and deletes
  the dead `AppState::clock_now`.
- `stream_host_mouse_capture_mode`, `downgrade_ineligible_pixel_mouse` and
  pixel-mouse eligibility: Spec E. This spec only re-spells their reads of
  `App` through the new accessors.
- STR-029 (constants beside their policy) is its own entry; constants this
  spec's code uses stay in `crate::limits`.
- `HeadlessServer`'s client registry, outbox, lifecycle phases and endpoint
  workers keep their design; only what is named below changes in them.

## Survey of the ground

### `App` today (`crates/shepr-server/src/app/mod.rs`)

`lib.rs` has `pub mod app` and `pub mod server`; the daemon
(`crates/shepr-daemon/src/main.rs`) links only
`shepr_server::server::headless::{RunServerError, ServerReady, run_server}`.
`App` is `pub struct App` with:

| field | visibility | who reads it outside `app/` |
|---|---|---|
| `state: AppState` | `pub` | everywhere in `server/` |
| `clock: AppClock` | `pub(crate)` | `render.rs` (`rebuild_shell_session_cache`), `client_views.rs` (`finish_shell_workspace_geometry_change`), `headless.rs` (`ShellConnected` arm), tests (`pane_exit.rs` assigns `app.clock.now` directly) |
| `terminal_runtimes` | `pub(crate)` | `pane_surface.rs`, `retained_surface.rs`, `render.rs`, `client_views.rs` (`apply_workspace_geometry` resizes through `PaneResizer`), `headless.rs` (pane input, held-input release, exit `clear()`) |
| `event_tx` | `pub` | tests only (`internal_event_drain.rs`, `tests/mod.rs`) |
| `event_rx` | `pub(crate)` | `headless.rs` `next_loop_event`, `internal_events.rs` drains |
| `policy: AppPolicy` | `pub(crate)` | `headless.rs` final save gate, `lifecycle.rs` |
| `git_refresh: GitRefreshScheduler` | `pub(crate)` | tests only (its flag fields are `pub(crate)` for tests) |
| `resume_schedule` | `pub(crate)` | nobody outside `app/` |
| `live_host_theme_reported` | private | `agent_resume.rs`, `host_theme.rs` |
| `default_workspace_retry_at`, `default_workspace_retry_failures` | private | `create_default_workspace`, the deadline fold |
| `runtimes_replaced_panes` | `pub(crate)` | `client_views.rs` `sync_pane_focus` drains it |
| `session_saver` | `pub(crate)` | `headless.rs` (`save_finished()` in the select, `is_due` in scheduled tasks), tests |
| `hostname`, `window_title_template` | private | `app/window_title.rs` |
| `persist_pane_history` | `pub(crate)` | `app/session.rs` only |
| `last_render_at`, `last_presentation_at` | `pub(crate)` | `app/runtime.rs` only |
| `render_notify` | `pub` | `headless.rs` select; `App::invalidate_shared_view` |
| `pane_teardowns`, `pane_launcher` | private | `app/` |
| `render_dirty` | `pub(crate)` | `headless.rs` render branch, `render.rs` `sync_immediate_pty_sources`, `terminal_titles.rs`, tests |
| `paths` | `pub(crate)` | `app/creation.rs`, `app/session.rs`, `app/api/checkout_root.rs`, tests |
| `restore_notice` | `pub(crate)` | `render.rs` `snapshot_from_viewed_workspace` |

`App::with_paths(config, paths, lease, policy: AppPolicy, clock)` builds the
event channel, `render_notify`, `render_dirty` and `save_finished`, opens the
session, then calls `configure_validated_window_title` after construction.

Loop-only logic living on `App`:

- `app/runtime.rs`: `can_render_now`, `can_present_now`,
  `record_render_attempt` (over `last_render_at`/`last_presentation_at`) and
  `next_headless_loop_deadline_with_git_refresh`, which folds the git deadline,
  `pending_agent_resume_wakeup`, `session_saver.deadline()`,
  `default_workspace_retry_at` and the render cadence deadline.
- `App::create_default_workspace` carries a hand-rolled retry
  (`default_workspace_retry_at`, `default_workspace_retry_failures`,
  `Backoff::new(DEFAULT_WORKSPACE_RETRY_MIN, DEFAULT_WORKSPACE_RETRY_MAX)`).
  Its only production caller is `HeadlessServer::create_automatic_workspace`
  (`client_views.rs`).

Signals `App` pokes instead of returning:

- `App::invalidate_shared_view` (`app/api.rs`) marks the projection dirty,
  calls `render_dirty.request_generic()` and `render_notify.notify_one()`.
  Callers: `handle_git_status_refreshed` and the cwd-report branch of
  `apply_internal_event` (`app/events.rs`), the failed and unconfirmed launch
  arms of `handle_pane_launch_settled` (`app/pane_launch.rs`),
  `sync_pending_terminal_titles` (`app/terminal_titles.rs`) and
  `set_host_terminal_theme` (`app/host_theme.rs`). The loop turns
  `RenderRequest::generic` into `mark_view_changed`.
- `start_pending_agent_resume` pushes onto `runtimes_replaced_panes`, which
  `HeadlessServer::sync_pane_focus` drains.

Split or duplicated state:

- Clock: `App.clock` and `AppState.clock_now`. `set_clock` writes both, but
  `AppState.clock_now` is read by no production code (only
  `app/actions/tests.rs`).
- Persistence policy is recorded three times: `AppPolicy`
  (`Production`/`Suspended`) on `App`, `SavePolicy` (with
  `Frozen { resume_to }`) inside `SessionSaver`, and
  `HostShutdownFreeze::persist_session` with `restored_policy()` and
  `ShutdownLifecycle::frozen_session_policy()` in `lifecycle.rs`.
  `open_session` always returns the policy it was given
  (`crates/shepr-mux/src/persist/open.rs`), so `AppPolicy` is derivable from
  the saver's policy at every point.
- Host theme: the value and appearance live in `AppState.host_terminal_*`;
  `App.live_host_theme_reported` is read only by the resume schedule
  (`pending_agent_resume_wakeup`, `start_pending_agent_resumes`).

Window title (`app/window_title.rs`): `window_title_for_target` is a pure
function of the template, the hostname and `AppState`, wrapped in `App`
methods; `HeadlessServer::configured_window_title`, `sync_window_title` and
`sync_terminal_title_sources` (`headless.rs`) call them.

### The loop (`crates/shepr-server/src/server/headless.rs`)

`HeadlessServer::run` per pass: reap clients, `refresh_app_clock`, stop check,
bounded internal-event drain, API drain, `app.sync_session_save_schedule()`,
server-event drain, `handle_scheduled_tasks_headless(now)` (git refresh start,
save reap/start, checkpointed-exit replay, `start_pending_agent_resumes`),
`create_automatic_workspace(None)`, shell cwd refresh, immediate PTY sources,
host input modes, then the render decision (`render_plan`,
`app.can_render_now`, `app.can_present_now`, `render_dirty.take()`, title
sync, replan, `render_pass`, `app.record_render_attempt`), else the deadline
fold plus `shell_cwd_refresh_deadline` and `next_loop_event`. The select in
`next_loop_event` borrows `self.app.event_rx`,
`self.app.session_saver.save_finished()` and `self.app.render_notify`. After
the loop, the final save is gated on
`app.policy.persists_session() || lifecycle.frozen_session_policy()`.

`test_headless_server` (`tests/mod.rs`) builds `HeadlessServer` by struct
literal, a second copy of `HeadlessServer::new`'s field list.

### Render pass (`render.rs`, `client_views.rs`, `pane_surface.rs`, `ui/`)

- `render_plan` runs on every wake, sometimes twice (the replan after the
  render request is taken). It calls `settle_workspace_geometry_before_plan`
  (which allocates `workspace_order()` and, on a PTY-dirty plan, a
  `visible_pane_runtimes` Vec per workspace), then `render_targets(&clients)`
  (`clients.rs`), which collects every client into a Vec and sorts it, because
  `ClientRegistry` keeps a `HashMap<ClientId, ClientConnection>`.
- A client with surface debt and a free slot reaches `surface_deliverable`,
  which locks every visible pane core of its workspace
  (`workspace_surface_held` -> `synchronized_output_state()`) once per
  workspace per plan through a `held` memo. `render_full` calls
  `render_targets` again, builds a fresh memo and a `deliverable` map, and
  locks again.
- Viewed-workspace resolution per pass: `surface_deliverable` and the
  shared-surface key counting call `shell_target_for_client`;
  `render_client_full` calls `ViewedWorkspace::for_location`;
  `sync_immediate_pty_sources` and `any_shell_surface_contains_pane` resolve
  via `shell_target_for_client` and then `workspace_index` again. Each is a
  linear `AppState::workspace_index` scan.
- Per surface render: `render_pane_surface` resolves `workspace_index` three
  times, and inside it `compute_surface_for`, `render_panes` (`ui/panes.rs`)
  and `surface_cursor` (`ui/surface.rs`) each resolve again: six linear scans
  per client surface.
- `Workspace::display_name()` (`crates/shepr-mux/src/workspace.rs`) returns
  an owned `String`; production readers are the window title and the session
  snapshot label (`app/creation.rs`).

### Agent resume scans (`app/agent_resume.rs`)

`start_pending_agent_resumes` runs every loop pass and starts with
`has_pending_agent_resumes`, a scan of every terminal's resume state;
`pending_agent_resume_wakeup` scans again inside the deadline fold. Resume plans
are minted only by session restore inside `App::with_paths` (no production
call of `TerminalState::plan_agent_resume` exists; `pane_launch.rs` documents
that a resume is attempted at most once per restore and never re-planned).

### Tests

`server/headless/tests/mod.rs` is 6502 lines of fixtures and tests on every
subject; seven sibling files (`already_running.rs`, `internal_event_drain.rs`,
`locations.rs`, `pane_exit.rs`, `server_stop.rs`, `surface_delta.rs`,
`surface_interest.rs`) are declared with redundant `#[cfg(test)]` and
`#[path]` attributes. `app/mod.rs`'s test module mixes session-saver, git
refresh, deadline-fold and pane-split tests.

### Hunt entries re-verified

- STR-030: accurate. One understatement: `AppState.clock_now` is dead, so the
  clock duplication is a dead field, not two live copies.
- BUG-057: accurate. Also: `render_full` re-collects and re-sorts the
  targets, and `settle_workspace_geometry_before_plan` allocates per plan.
- BUG-060: accurate and understated: the resume scan runs at least twice per
  pass (start and wakeup), and each surface render resolves the workspace six
  times, not three.
- CON-076: accurate. Also `settle_workspace_geometry_before_plan` and
  `apply_all_workspace_geometry` call `ViewedWorkspace::for_location` per
  client per workspace.

## Target

### Module and visibility

```rust
// crates/shepr-server/src/lib.rs
pub(crate) mod app;
mod backoff;
pub(crate) mod limits;
pub(crate) mod logging;
pub(crate) mod server;
mod ui;
pub use server::headless::{RunServerError, ServerReady, run_server};
```

The daemon imports `shepr_server::{RunServerError, ServerReady, run_server}`.
`HeadlessServer` and `HeadlessServer::run` become `pub(crate)`. Every other
`pub` in `app/` and `server/` becomes `pub(crate)` or narrower.

`Backoff` moves from `app/mod.rs` to `crates/shepr-server/src/backoff.rs`
(`crate::backoff::Backoff`, unchanged API): the session saver, the logind
reconnect in `lifecycle/host_shutdown.rs` and the loop's creation retry all use
it, so it belongs to neither `app` nor `server`.

### `App`

```rust
pub(crate) struct App {
    state: AppState,
    clock: AppClock,
    terminal_runtimes: PaneRuntimeRegistry,
    git_refresh: git_refresh::GitRefreshScheduler,
    resume_schedule: resume_schedule::ResumeSchedule,
    session_saver: session::SessionSaver,
    pane_teardowns: Arc<PaneTeardownTracker>,
    pane_launcher: PaneLauncher,
    paths: AppPaths,
    restore_notice: Option<SessionRestoreNotice>,
}
```

Deleted from `App`: `event_tx`, `event_rx`, `render_notify`, `render_dirty`
(to `AppOutputs`), `policy` (the saver's), `persist_pane_history` (the
saver's), `live_host_theme_reported` (the resume schedule's), the two
default-workspace retry fields and the two cadence fields (the loop's
schedule), `runtimes_replaced_panes` (an effect return), `hostname` and
`window_title_template` (the loop's title settings).

Construction:

```rust
impl App {
    /// Opens the session and returns the app with the outputs the loop owns.
    pub(crate) fn open(
        config: &ValidatedServerConfig,
        paths: &AppPaths,
        lease: DataDirLease,
        persistence: shepr_mux::persist::SessionOpenPolicy,
        clock: AppClock,
    ) -> (Self, AppOutputs);
}
```

`AppOutputs` (new `app/outputs.rs`) is what the app and its pane runtimes
publish, owned by whoever runs the loop:

```rust
pub(crate) struct AppOutputs {
    events: mpsc::Receiver<AppEvent>,
    render: Arc<RenderSignal>,
    render_wake: Arc<Notify>,
    save_finished: Arc<Notify>,
    #[cfg(test)]
    event_sender: mpsc::Sender<AppEvent>,
}

pub(crate) enum AppWake {
    Event(AppEvent),
    /// A pane runtime asked for a render, or a session save ended.
    Signal,
}

impl AppOutputs {
    /// Cancel safe: mpsc `recv` and `Notify::notified` both are.
    pub(crate) async fn next(&mut self) -> AppWake;
    pub(crate) fn try_next_event(&mut self) -> Option<AppEvent>;
    pub(crate) fn queued_events(&self) -> usize;
    pub(crate) fn render(&self) -> &RenderSignal;
}
```

`App::open` creates the channel (`APP_EVENT_CHANNEL_CAPACITY`), the render
signal and the two `Notify`s; it hands clones to `PaneSpawnHandles`, the git
worker and `open_session`, and keeps none of the receive sides. The
`save_finished` clone the persister fires is no longer stored in
`SessionSaver` (its only other use is `persist_for_test`, which moves to
`TestApp`).

Read surface the loop uses:

```rust
impl App {
    pub(crate) fn state(&self) -> &AppState;
    pub(crate) fn clock(&self) -> AppClock;
    pub(crate) fn set_clock(&mut self, clock: AppClock);
    /// The runtime of `pane_id` in the workspace at `workspace_index`.
    pub(crate) fn pane_runtime(&self, workspace_index: usize, pane_id: PaneId) -> Option<&PaneRuntime>;
    /// The pure render inputs: shared references to state and runtimes.
    pub(crate) fn render_view(&self) -> RenderView<'_>;
    pub(crate) fn restore_notice(&self) -> Option<&SessionRestoreNotice>;
    pub(crate) fn session_saves_stopped(&self) -> bool;
    /// Whether this boot persists the session, frozen or not.
    pub(crate) fn session_persists(&self) -> bool;
    /// The earliest of the app's own deadlines: the git refresh (only when
    /// `git_refresh`), the resume wakeup and the save deadline.
    pub(crate) fn next_deadline(&self, git_refresh: bool) -> Option<Instant>;
    // Kept as they are: public_workspace_id, public_pane_id,
    // resolve_workspace_id, resolve_pane_id, find_pane,
    // headless_spawn_geometry, resolved_new_workspace_cwd, session_snapshot,
    // pane_exit_checkpoint_generation_settled,
    // host_shutdown_checkpoint_result_ready.
}

#[derive(Clone, Copy)]
pub(crate) struct RenderView<'a> {
    pub(crate) state: &'a AppState,
    pub(crate) runtimes: &'a PaneRuntimeRegistry,
}
```

Mutation surface the loop uses (each a named operation; no `&mut AppState`
leaves `app/` outside `#[cfg(test)]`):

```rust
impl App {
    // Events and requests (kept): admit_runtime_event,
    // handle_internal_event_with_view_change, handle_prepared_pane_exit,
    // handle_api_request_with_render, handle_endpoint_app_command_with_render,
    // prepare_workspace_checkout_root, create_workspace (startup seed),
    // send_pane_focus_event, set_host_terminal_appearance_state,
    // start_git_status_refresh_if_due, mark_git_status_refresh_due,
    // request/take/cancel_host_shutdown_checkpoint, retire_session_writer,
    // sync_session_save_schedule.

    /// Publishes the exit and decides its checkpoint; replaces the loop's
    /// `observe_projection_change` closure around `prepare_pane_exit`.
    pub(crate) fn prepare_pane_exit(&mut self, pane_id: PaneId, reason: ChildExitReason, ended_at: Instant) -> PaneExitPrepared;
    pub(crate) fn create_default_workspace(&mut self, geometry: SpawnGeometry) -> DefaultWorkspace;
    #[must_use]
    pub(crate) fn start_pending_agent_resumes(&mut self, now: Instant) -> ResumeOutcome;
    #[must_use]
    pub(crate) fn set_host_terminal_theme(&mut self, theme: TerminalTheme) -> bool;
    /// Copies the titles of `sources` from their runtimes; marks the shell
    /// projection dirty when one changed.
    pub(crate) fn sync_terminal_titles(&mut self, sources: &HashSet<PaneId>) -> TerminalTitleChanges;
    /// Reaps a finished save and starts the next one when it is due.
    pub(crate) fn service_session_saves(&mut self, now: Instant);
    pub(crate) fn freeze_session_saves(&mut self);
    pub(crate) fn thaw_session_saves(&mut self);
    /// Thaw after a cancelled host shutdown: the live session is dirty again.
    pub(crate) fn resume_session_saves_after_cancel(&mut self);
    /// The final save of this boot, when the boot persists. A signal quit's
    /// instant adopts checkpoint candidates first.
    pub(crate) async fn save_session_for_exit(&mut self, signal_quit_at: Option<Instant>);
    /// Drops every runtime and waits for their teardowns; false on timeout.
    pub(crate) fn shut_down_pane_runtimes(&mut self, timeout: Duration) -> bool;
    /// Reconciles the bookmark and drops geometry of vanished workspaces.
    pub(crate) fn reconcile_workspace_topology(&mut self);
    /// The bookmark follows an active client's navigation.
    pub(crate) fn navigate_bookmark(&mut self, workspace_id: &WorkspaceId) -> bool;
    /// Resizes the workspace's visible panes for `geometry` and records it;
    /// true when the recorded geometry changed.
    pub(crate) fn apply_workspace_geometry(&mut self, workspace_id: &WorkspaceId, geometry: SpawnGeometry) -> bool;
}

pub(crate) struct PaneExitPrepared {
    pub(crate) prepared: PreparedPaneExit,
    pub(crate) projection_changed: bool,
}

pub(crate) enum DefaultWorkspace { Exists, Created, Failed }

pub(crate) struct ResumeOutcome {
    /// Some plan was consumed (launched or abandoned).
    pub(crate) consumed: bool,
    /// Panes whose runtime a launch replaced; their focus is re-reported.
    pub(crate) replaced_runtimes: Vec<PaneId>,
}
```

`observe_projection_change` and `handle_api_request_after_internal_events_drained`
become private to `app/`.

### Effects instead of pokes

| today | target |
|---|---|
| `invalidate_shared_view` in `handle_git_status_refreshed` | marks the projection dirty and returns `changed`, which `handle_internal_event_with_view_change` already returns to the loop |
| same in the cwd-report branch of `apply_internal_event` | dropped: the projection revision moved, so the call already returns true |
| same in `handle_pane_launch_settled` (failed, unconfirmed) | marks the projection dirty, returns true (already does) |
| same in `sync_pending_terminal_titles` | the method is deleted; `sync_terminal_titles` marks the projection dirty, and the loop syncs pending titles itself before dispatching an API request or an endpoint command (`HeadlessServer::sync_pending_terminal_titles`) and folds the change into its own `changed` |
| same in `set_host_terminal_theme` | returns `bool` (`#[must_use]`); `HeadlessServer::sync_host_theme_from_foreground` calls `mark_view_changed` when the setters changed anything, so callers that discard its result (`promote_client_to_foreground`, `apply_client_departures`) still render |
| `runtimes_replaced_panes.push` | `ResumeOutcome::replaced_runtimes`; `HeadlessServer::sync_pane_focus_after(&[PaneId])` re-sends focus-in to replaced panes that were and still are focused, then diffs as today; `sync_pane_focus()` is `sync_pane_focus_after(&[])` |

With `invalidate_shared_view` gone nothing calls `RenderSignal::request_generic`:
`RenderRequest::generic`, `request_generic` and the `generic` arm of
`has_immediate_work` are deleted from `crates/shepr-mux/src/render_signal.rs`,
and the loop's `if request.generic { mark_view_changed() }` goes. Every former
caller ran on the loop thread inside a pass, so the `render_notify.notify_one()`
it did only left a stored permit that woke the loop once more for nothing
(finding 3).

`start_pending_agent_resumes` marks the shell projection dirty when it consumed
a plan, so the loop's `self.app.state.mark_shell_projection_dirty()` after
`handle_scheduled_tasks_headless` goes (a replayed exit already marks it on
removal).

### Resume schedule

`ResumeSchedule` (`app/resume_schedule.rs`) gains two fields:

```rust
pub(crate) struct ResumeSchedule {
    theme_wait: Duration,
    spacing: Duration,
    pending: Option<Pending>,
    /// A live foreground client reported host colours this boot.
    live_theme_reported: bool,
    /// No plan is pending and none can appear: plans are minted only by
    /// session restore, before the first pass. Once set, nothing scans.
    retired: bool,
}

impl ResumeSchedule {
    pub(crate) fn note_live_theme(&mut self);
    pub(crate) fn is_retired(&self) -> bool;
    pub(crate) fn observe(&mut self, now: Instant, has_pending_plans: bool, eligible: bool); // !has_pending_plans retires
    pub(crate) fn wakeup(&self, now: Instant, eligible: bool) -> Option<Instant>;
    pub(crate) fn is_due(&self, now: Instant, eligible: bool) -> bool;
}
```

`App::has_pending_agent_resumes` returns false without scanning once the
schedule is retired; `pending_agent_resume_wakeup` and
`start_pending_agent_resumes` check `is_retired()` first. `set_host_terminal_theme`
calls `note_live_theme()` where it set `live_host_theme_reported`.

### The saver owns its policy

`AppPolicy` is deleted. `SessionSaver::new(persister, policy: SessionOpenPolicy,
pane_history: bool)`; `SavePolicy` stays the one record and gains:

```rust
impl SavePolicy {
    /// Persisting or stopped, now or before a freeze.
    fn persists_this_boot(self) -> bool;
}
impl SessionSaver {
    pub(crate) fn persists_this_boot(&self) -> bool;
    pub(crate) fn pane_history(&self) -> bool;
}
```

`capture_session_save_job` and `capture_save_job_from_preserved_layout` read
`session_saver.pane_history()`. `freeze_session_saves` only freezes the saver;
`thaw_session_saves` restores the saver's own `resume_to`. In `lifecycle.rs`,
`HostShutdownFreeze` keeps only `generation`; `persist_session`,
`restored_policy`, `frozen_session_policy` and
`set_frozen_session_policy_for_test` are deleted; `freeze_for_host_shutdown` and
the `HostShutdownWarning` arm ask `app.session_persists()`.
`save_session_for_exit` replaces the loop's gate: it returns at once unless
`persists_this_boot()`, which is exactly the old
`policy.persists_session() || frozen_session_policy().unwrap_or(false)`.

### Loop-owned schedules (`server/headless/schedule.rs`, new)

```rust
/// When the loop may render, and when a held render is due.
#[derive(Default)]
pub(super) struct RenderCadence {
    last_render_at: Option<Instant>,
    last_presentation_at: Option<Instant>,
}
impl RenderCadence {
    pub(super) fn can_render(&self, now: Instant) -> bool;
    pub(super) fn can_present(&self, now: Instant) -> bool;
    pub(super) fn record(&mut self, now: Instant, presentation: bool);
    /// `last_render_at + MIN_RENDER_INTERVAL` while a render is owed and that
    /// time is still ahead.
    pub(super) fn deadline(&self, now: Instant, render_owed: bool) -> Option<Instant>;
}

/// Backoff of the automatic workspace after a failed creation.
pub(super) struct CreationRetry {
    retry_at: Option<Instant>,
    failures: u32,
}
impl CreationRetry {
    pub(super) fn may_attempt(&self, now: Instant) -> bool;
    pub(super) fn failed(&mut self, now: Instant);   // Backoff(DEFAULT_WORKSPACE_RETRY_MIN, _MAX)
    pub(super) fn reset(&mut self);                  // a workspace exists
    /// A future retry instant; a past one waits for the next wake.
    pub(super) fn deadline(&self, now: Instant) -> Option<Instant>;
}

#[derive(Default)]
pub(super) struct LoopSchedule {
    pub(super) cadence: RenderCadence,
    pub(super) creation: CreationRetry,
}

pub(super) struct WakeInputs {
    pub(super) render_owed: bool,
    pub(super) app: Option<Instant>,
    pub(super) shell_cwd: Option<Instant>,
}

impl LoopSchedule {
    /// The one deadline fold: the earliest of the app's deadline, the render
    /// cadence, the creation retry and the shell cwd refresh.
    pub(super) fn next_wake(&self, now: Instant, inputs: WakeInputs) -> Option<Instant>;
}
```

`HeadlessServer` gains `schedule: LoopSchedule`. `create_automatic_workspace`
becomes:

```rust
if !self.app.state().workspaces.is_empty() {
    self.schedule.creation.reset();
    return false;
}
// ... trigger and source as today ...
let now = self.app.clock().now;
if !self.schedule.creation.may_attempt(now) {
    return false;
}
match self.app.create_default_workspace(geometry) {
    DefaultWorkspace::Created => self.schedule.creation.reset(),
    DefaultWorkspace::Exists => { self.schedule.creation.reset(); return false; }
    DefaultWorkspace::Failed => { self.schedule.creation.failed(now); return false; }
}
// ... controller, reconcile, geometry, focus as today ...
```

The reset on a non-empty session fixes finding 2.

The render branch of `run` reads `self.schedule.cadence` where it read
`self.app.can_render_now`/`can_present_now`/`record_render_attempt`, and the
deadline becomes:

```rust
let next_deadline = self.schedule.next_wake(now, WakeInputs {
    render_owed: plan.has_full() || render_signal_pending,
    app: self.app.next_deadline(self.has_app_client()),
    shell_cwd: self.shell_cwd_refresh_deadline(),
});
```

### Window title (`ui/window_title.rs`, new)

```rust
pub(crate) struct WindowTitleSettings {
    template: WindowTitleTemplate,
    hostname: String,
}
impl WindowTitleSettings {
    /// `None` when `ui.window_title` is unset or empty: titles disabled.
    pub(crate) fn from_config(template: Option<&WindowTitleTemplate>, hostname: String) -> Option<Self>;
    pub(crate) fn uses_terminal_title(&self) -> bool;
}
/// The title for a client viewing the workspace at `workspace_index`, or no
/// workspace. Pure: settings and state only.
pub(crate) fn render_window_title(settings: &WindowTitleSettings, state: &AppState, workspace_index: Option<usize>) -> String;
```

`HeadlessServer` gains `window_title: Option<WindowTitleSettings>`, passed to
`HeadlessServer::new`; `run_server` resolves `shepr_platform::hostname()` once
and builds it from `config.ui().window_title`. `configured_window_title`,
`sync_window_title` and `sync_terminal_title_sources` read it. The settings
are the server's, beside the pane chrome it draws (AGENTS.md: each server
applies its own config to the window title); they never enter `AppState`.

### Clock

`AppState::clock_now` is deleted (dead). `App.clock` is private; `set_clock`
is its only writer and `clock()` its read accessor. The loop's three reads and
the `pane_exit.rs` test helper use them.

### Render pass

`ui` gains a resolved target, carried from the one resolution per client per
pass to every reader:

```rust
/// A workspace resolved against the state of this pass: its position and id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SurfaceTarget {
    pub(crate) index: usize,
    pub(crate) id: WorkspaceId,
}
```

- `compute_surface_for(state, runtimes, target: Option<SurfaceTarget>, area)`
  looks the workspace up with `state.workspaces.get(target.index)` and draws
  nothing when its id is not `target.id` (an O(1) guard against a stale
  target). `SurfaceLayout::target` and `SurfaceView::target` become
  `Option<SurfaceTarget>`; `render_panes` and `surface_cursor` use
  `target.index`. `render_pane_surface(app, target: Option<SurfaceTarget>, area,
  cell_size)` deletes its three `workspace_index` calls; `SurfaceBoundary::render`
  takes `Option<SurfaceTarget>`; `pane_surface_render_key` keys on
  `target.map(|t| t.id)`.
- `ViewedWorkspace` gains `fn target(&self) -> SurfaceTarget` and
  `fn at(app: &App, target: SurfaceTarget) -> Option<Self>` (O(1), id-checked).
- `render_full` resolves each target client's view once into a local
  `Vec<(RenderTarget, Option<SurfaceTarget>)>`, and `surface_deliverable`,
  the key counting and `render_client_full` take that view. `render_client_full`
  rebuilds its `ViewedWorkspace` with `ViewedWorkspace::at`.
- `sync_immediate_pty_sources`, `any_shell_surface_contains_pane` and
  `pty_sources_visible_to_any_render_target` resolve each presenting client's
  view once (`presented_views()`, an iterator) instead of per pane per client.
- `settle_workspace_geometry_before_plan` and `apply_all_workspace_geometry`
  iterate the state's workspaces by index (no `workspace_order()` Vec) and
  compare client views by resolved id once per client;
  `visible_pane_runtimes` returns `impl Iterator<Item = &PaneRuntime>`.
- `ClientRegistry::connections` becomes a `BTreeMap<ClientId, ClientConnection>`
  (`ClientId` is `Ord`); its iterator types follow. `render_targets` returns
  `impl Iterator<Item = RenderTarget>` in id order with no Vec and no sort;
  the `sort_unstable` calls in `window_title_clients` and `pane_viewers` go.
- Synchronized-output holds read lock-free. In `crates/shepr-mux/src/pane/terminal.rs`,
  `PaneTerminal` gains `synchronized_output: AtomicBool`, and a
  `commit_mutation(&self, core: &mut PaneTerminalCore, mutation: CoreMutation<'_>)`
  that calls `core.record_mutation(mutation)` and then stores
  `core.terminal.mode_get(DecMode::SynchronizedOutput)` with `Release`. All
  eight `core.record_mutation` call sites in `pane/terminal/backend.rs` call
  `commit_mutation` instead, so the mirror cannot miss a core mutation.
  `PaneTerminal::synchronized_output_active` reads the mirror (`Acquire`), and
  `PaneRuntimeRead` gains `surface_held(&self) -> bool` = core poisoned (lock
  free `Mutex::is_poisoned`) or the mirror. `workspace_surface_held` uses
  `surface_held`; the `held` memo parameter of `surface_deliverable` and both
  memos are deleted. `render_pane_surface` keeps its locked
  `synchronized_output_state()` reads: they carry the epoch its before/after
  check compares, and it remains the authority that defers a racing surface.
- `Workspace::display_name` returns `&str`.

### The loop pass after this spec

```text
reap clients; sample clock -> app.set_clock
stop check
drain internal events (outputs.try_next_event)          -> mark_view_changed
drain API requests (each: sync_pending_terminal_titles, app call)
app.sync_session_save_schedule()
drain server events
scheduled tasks: git start, app.service_session_saves, exit replay,
                 resumes (ResumeOutcome -> sync_pane_focus_after)
create_automatic_workspace(None)   (schedule.creation)
shell cwd refresh; immediate PTY sources; host input modes
render decision: render_plan, schedule.cadence, outputs.render().take(),
                 title sync, replan, render_pass, schedule.cadence.record
else: schedule.next_wake(..) and next_loop_event (selects outputs.next())
```

After the loop: `app.save_session_for_exit(lifecycle.signal_quit_at()).await`,
`app.shut_down_pane_runtimes(PANE_TEARDOWN_WAIT)`, `release_socket_after_save`.

## Migration

Eleven landings, each one coherent change kept or reverted on its gate, ordered
so `brokkr check` is green at every boundary. Each lists what it deletes. The
command for every landing is:

```
brokkr check
```

and is not repeated below. Separate `brokkr test` lines appear only where a
test must be seen failing with its production half reverted.

### Landing 0 - Relocate tests (pure moves)

No production change; test bodies unchanged except `use` lines.

- `server/headless/tests/mod.rs` keeps only fixtures and helpers
  (`test_headless_server`, `insert_test_client`, `place_test_client_on_workspace`,
  `focused_test_pane`, `shutdown_test_runtimes`, `read_server_message`,
  `handle_server_event`, `render_now`, `dispatch_lifecycle_messages`,
  `test_client_writer`, the writer and lane receivers, `install_focused_test_runtime`,
  `retained_test_server_with_control`, `with_terminal_session_test_server`) and
  plain `mod` declarations. The existing seven files drop `#[path]`,
  `#[cfg(test)]` and the `_tests` suffix (`mod pane_exit;` and so on).
- New files under `server/headless/tests/`, by subject:
  - `shutdown.rs`: `server_stop_interrupts_server_event_backlog`, the
    `complete_shutdown_*`, `an_endpoint_request_queued_at_shutdown_is_answered`,
    `a_queued_new_client_*`, `a_dequeued_new_client_*`,
    `api_request_selected_during_shutdown_is_answered`,
    `host_shutdown_warning_freezes_saves_*`, `signal_quit_drain_keeps_dying_panes_in_the_layout`.
  - `api_requests.rs`: `headless_api_reads_latest_title`,
    `a_closed_api_channel_stops_being_selected`,
    `headless_api_request_drains_all_pending_internal_events_*`,
    `api_request_drain_is_bounded_*`, `server_event_drain_is_bounded_*`.
  - `window_title.rs`: the tests from `window_title_waits_for_a_client_to_exist`
    through `promoted_client_window_title_uses_its_own_view`, with their helpers.
  - `endpoint_commands.rs`: `client_shell_attach_seeds_workspace` through
    `an_endpoint_reply_for_a_departed_client_is_dropped`, and
    `a_completion_for_a_departed_client_is_dropped`.
  - `projection.rs`: `client_shell_receives_metadata_then_shell_free_pane_surface`
    through `create_default_workspace_invalidates_the_shell_projection`,
    `unchanged_internal_events_leave_projection_and_sources_clean`,
    `missing_pane_exit_has_no_invalidation`, the two git-refresh render tests.
  - `retained_render.rs`: the synchronized-pane and retained-patch tests
    (`unrelated_render_keeps_synchronized_pane_frame_committed` through
    `full_render_backpressure_does_not_disable_responsive_peer_patches`),
    `a_surface_larger_than_one_frame_crosses_in_parts`,
    `server_message_encoding_splits_payloads_over_the_frame_cap`.
  - `geometry.rs`: `default_headless_size_lays_out_workspaces_without_clients`,
    `last_shell_disconnect_restores_headless_pane_size`,
    `first_shell_surface_resizes_*`, the controller geometry tests,
    `client_shell_workspaces_render_accept_input_and_resize_independently`,
    `pane_death_reapplies_controller_geometry`.
  - `navigation.rs`: `a_client_command_neither_drags_*` through
    `navigation_moves_pane_focus_between_workspaces_once`,
    `workspace_focus_moves_only_its_client`, the two projection-replacement
    focus tests, `pane_death_reconciles_each_client_view_and_focus`,
    `client_shell_focus_promotes_and_reaches_reporting_pane`.
  - `client_input.rs`: the `client_shell_*input*`, mouse motion, paste,
    pixel-mouse, wheel and page-key tests, `client_shell_streams_*`,
    `client_shell_release_cleanup_*`, `client_shell_mouse_capture_*`.
  - `host_theme.rs`: `client_shell_host_theme_follows_foreground_client`,
    `resizing_a_background_shell_*`.
  - `agent_resume.rs`: the two `headless_scheduled_tasks_*` resume tests and
    `settle_resume_launch`.
  - `clipboard.rs`: the four clipboard tests.
  - `clients.rs`: `disconnect_after_detach_has_no_render_impact`,
    `a_failed_health_pong_*`, `closing_the_foreground_client_*`,
    `a_stopping_server_reaps_*`, `a_reaped_client_marks_the_view_changed`.
- `app/mod.rs` tests: the session-saver tests
  (`session_dirty_flag_schedules_debounced_save` through
  `durable_mutation_after_pane_exit_checkpoint_wins_on_shutdown`, and the
  restore backup test) move to `app/session.rs`'s test module; the three git
  tests to `app/git_refresh.rs`; the four `pane_split_request_*`/split tests
  and the two `pane_close_request_*` tests to `app/api/panes/tests.rs`;
  `headless_next_loop_deadline_*` stay until Landing 4 moves them.

Gate: `brokkr check`, and the passed-test count of
`brokkr test -p shepr-server server::headless::tests` equals the count before
the move.

### Landing 1 - One clock

- Delete `AppState::clock_now`, its initialisers in `App::with_paths` and
  `AppState::test_new`, and its write in `App::set_clock`; `app/actions/tests.rs`
  uses a local sample.
- `App.clock` private; add `App::clock()`. Replace `self.app.clock.now` in
  `render.rs`, `client_views.rs` and `headless.rs`; the `pane_exit.rs` helper
  calls `set_clock` (keeping `wall_now`).
- `brokkr.toml`: the `headless-loop-reads-the-app-clock` message reads
  "read the loop's sample through app.clock(); only the sampler reads the clock".

Deletes: `AppState::clock_now`. Gate: the `tests/pane_exit.rs` replay tests
and `internal_event_drain.rs` (both read the clock).

### Landing 2 - Window title as a pure function

- Add `ui/window_title.rs` as specified; `HeadlessServer::new` takes
  `window_title: Option<WindowTitleSettings>`; `run_server` builds it;
  `test_headless_server` sets `None` and the window-title tests set
  `WindowTitleSettings::for_test("...")` (`#[cfg(test)]`, parses like
  validation and panics on an invalid template).
- `Workspace::display_name` returns `&str`; `app/creation.rs` takes
  `.to_owned()`; test call sites that `.clone()` it take `.to_owned()`.

Deletes: `app/window_title.rs` (`configure_validated_window_title`,
`window_title_configured`, `window_title_uses_terminal_title`,
`window_title_without_workspace`, `window_title_for`, `window_title_for_target`,
`configure_window_title`), `App.hostname`, `App.window_title_template`.

Gate: the moved unit tests in `ui/window_title.rs`, rewritten over
`AppState::test_new` (no `App`, no PTY): `renders_the_workspace_name`,
`renders_focused_pane_label_and_terminal_title`,
`a_client_with_no_workspace_renders_no_workspace_or_pane_target`,
`empty_template_disables_window_titles` (now: `from_config` of an empty
template is `None`), `unset_tokens_render_empty`,
`invalid_template_is_rejected_before_it_reaches_the_app`; the server tests in
`tests/window_title.rs`.

### Landing 3 - The saver owns its policy

- `App::with_paths` becomes `App::open(config, paths, lease, persistence:
  SessionOpenPolicy, clock)` (still returning `App` only; outputs come in
  Landing 6). `run_server` passes `Persist`; the `AgentReportHarness` and the
  bootstrap gate test pass `Never`; tests that passed `AppPolicy::Production`
  pass `Persist`. The test constructor becomes `App::new(&config)` (every
  caller passed `Suspended`).
- `SessionSaver::new(persister, save_finished, policy, pane_history)`;
  `SavePolicy::persists_this_boot`, `SessionSaver::persists_this_boot`,
  `SessionSaver::pane_history`; `App::session_persists`,
  `App::thaw_session_saves()` (no argument),
  `App::resume_session_saves_after_cancel`, `App::save_session_for_exit`.
- `lifecycle.rs` as specified; `thaw_after_host_shutdown` calls
  `resume_session_saves_after_cancel`, the restart path `thaw_session_saves`.
- `run`'s final save calls `save_session_for_exit`.
- `persist_for_test` sets only the saver's policy.

Deletes: `AppPolicy`, `App.policy`, `App.persist_pane_history`,
`HostShutdownFreeze::persist_session`, `HostShutdownFreeze::restored_policy`,
`ShutdownLifecycle::frozen_session_policy`,
`ShutdownLifecycle::set_frozen_session_policy_for_test`.

Gate: new `persists_this_boot_survives_freeze_and_stop` (`app/session.rs`:
`Persisting`, frozen `Persisting`, `Stopped`, frozen `Stopped` all true;
`Never` and frozen `Never` false); new
`a_frozen_persisting_server_runs_the_final_save_and_writes_nothing`
(`tests/shutdown.rs`: freeze for a host shutdown, then
`save_session_for_exit` leaves the checkpoint file as the warning wrote it);
the lifecycle `phase_tests` with their `frozen_session_policy` asserts
replaced by `app.session_persists()`; `host_shutdown_warning_freezes_saves_*`.

### Landing 4 - Loop-owned schedules

- Move `Backoff` to `crate::backoff`.
- Add `server/headless/schedule.rs`; `HeadlessServer.schedule`;
  `create_automatic_workspace` as specified; the render branch and deadline
  as specified. `App::next_deadline(git_refresh)`;
  `App::create_default_workspace -> DefaultWorkspace`.

Deletes: `App.last_render_at`, `App.last_presentation_at`,
`App::can_render_now`, `App::can_present_now`, `App::record_render_attempt`,
`App::next_headless_loop_deadline_with_git_refresh` (`app/runtime.rs` keeps
only the runtime shutdown helpers), `App.default_workspace_retry_at`,
`App.default_workspace_retry_failures`.

Gate:
- Moved: `hidden_render_attempt_keeps_presentation_cadence_available`
  (`schedule.rs`); `headless_next_loop_deadline_ignores_resize_poll` and
  `..._returns_none_when_resize_poll_is_only_deadline` become
  `app_deadline_is_the_save_deadline` and `app_deadline_is_none_when_idle`
  (`app/session.rs`); `headless_deadline_can_suppress_git_refresh_timer`
  becomes `app_deadline_omits_git_unless_asked` (`app/git_refresh.rs`).
- New in `schedule.rs`: `creation_retry_backs_off_and_resets`,
  `a_past_creation_retry_does_not_wake_the_loop`,
  `next_wake_takes_the_earliest_future_deadline`,
  `render_deadline_only_while_a_render_is_owed`.
- New in `tests/geometry.rs`: `a_workspace_appearing_resets_the_creation_retry`
  (record a failure on `server.schedule.creation`, give the session a
  workspace, call `create_automatic_workspace(None)`, assert
  `schedule.creation.deadline(now)` is `None` and the next failure waits the
  minimum backoff). Seen failing with the reset line in
  `create_automatic_workspace` removed:

```
brokkr test -p shepr-server a_workspace_appearing_resets_the_creation_retry
```

### Landing 5 - Effects instead of signal pokes

- Delete `App::invalidate_shared_view` and rewrite its six callers per the
  effects table. Add `HeadlessServer::sync_pending_terminal_titles` (reads
  `self.app.render_dirty`'s pending title sources until Landing 6, then
  `self.outputs.render()`), called in `dispatch_api_request` and
  `handle_client_shell_app_command` after the internal-event drain and before
  the app call. `App::sync_terminal_titles` marks the projection dirty itself;
  the loop's mark in `sync_terminal_title_sources` goes.
- `sync_host_theme_from_foreground` marks the view changed on a change; the
  `if ... { mark_view_changed() }` wrappers at its call sites go.
- `ResumeOutcome`; `sync_pane_focus_after`; resumes mark the projection
  dirty; the loop's mark after scheduled tasks goes.
- `ResumeSchedule::live_theme_reported`, `note_live_theme`, `retired`.
- mux: delete `RenderRequest::generic`, `RenderSignal::request_generic` and
  the `generic` term in `has_immediate_work`; the loop's generic arm goes.

Deletes: `App::invalidate_shared_view`, `App::sync_pending_terminal_titles`,
`App.runtimes_replaced_panes`, `App.live_host_theme_reported`,
`RenderRequest::generic`, `RenderSignal::request_generic`, mux test
`keeps_generic_and_pty_requests_distinct`.

Gate:
- Rewritten: `syncing_pending_titles_preserves_sidebar_render_impact` becomes
  `title_sync_moves_the_shell_projection` (`app/terminal_titles.rs`);
  `git_status_event_marks_render_dirty_when_status_changes` becomes
  `a_changed_git_status_reports_a_view_change` (returns true, projection
  revision moved); `no_pending_plans_resets_the_schedule` becomes
  `no_pending_plans_retires_the_schedule` (after `observe(_, false, _)`,
  `observe(_, true, true)` leaves it retired, `wakeup` is `None`, `is_due`
  false); `a_live_theme_report_bypasses_the_wait_but_not_a_barrier` and the
  other `resume_schedule.rs` tests call `note_live_theme()` instead of passing
  the flag.
- New `a_host_theme_change_from_input_promotion_renders`
  (`tests/host_theme.rs`): a background client with its own host colours
  sends pane input; the view epoch advances and the next plan is full for
  every presenting client, with no render signal pending. Seen failing with
  the `mark_view_changed` in `sync_host_theme_from_foreground` removed:

```
brokkr test -p shepr-server a_host_theme_change_from_input_promotion_renders
```

- New `a_resumed_runtime_in_a_focused_pane_is_told_focus_in`
  (`tests/agent_resume.rs`): a focus-reporting client views a pane whose
  resume launches; the new runtime receives `CSI I`.
- New `resume_scans_stop_once_no_plan_is_pending` (`app/agent_resume.rs`):
  after the last plan is consumed, `resume_schedule.is_retired()` and
  `pending_agent_resume_wakeup()` is `None`.

### Landing 6 - The loop owns what it waits on

- Add `app/outputs.rs`; `App::open` returns `(App, AppOutputs)`.
- `HeadlessServer` gains `outputs: AppOutputs`; `HeadlessServer::new(app,
  outputs, ...)`. One private `HeadlessServer::assemble` builds the struct for
  both `new` and `test_headless_server`, so the field list exists once.
- `next_loop_event` selects `self.outputs.next()` in place of the three app
  arms; the drains use `try_next_event` and `queued_events`; the render branch
  and `sync_immediate_pty_sources` use `self.outputs.render()`.
- `APP_EVENT_CHANNEL_CAPACITY` and `APP_EVENT_DRAIN_LIMIT` are re-exported
  from `app::outputs` instead of `app`.
- Test harness `app/test_app.rs` (`#[cfg(test)]`):

```rust
pub(crate) struct TestApp {
    app: App,
    outputs: AppOutputs,
}
impl Deref for TestApp { type Target = App; }
impl DerefMut for TestApp {}
impl TestApp {
    pub(crate) fn into_parts(self) -> (App, AppOutputs);
    pub(crate) async fn next_event(&mut self) -> AppEvent;
    pub(crate) fn blocking_next_event(&mut self) -> AppEvent;
    pub(crate) fn event_sender(&self) -> mpsc::Sender<AppEvent>;
    /// The old `persist_for_test`: a threaded persister on the same data
    /// directory, fired through this harness's `save_finished`.
    pub(crate) fn persist(&mut self);
}
```

  `App::new(&config)` returns `TestApp`. Helpers returning `App` return
  `TestApp`; `server.app = test_app()` becomes
  `server.install_test_app(test_app())` (replaces both `app` and `outputs`).
  `HeadlessServer::replay_test_exit_for_app` keeps swapping only the `App`.

Deletes: `App.event_tx`, `App.event_rx`, `App.render_notify`,
`App.render_dirty`, `SessionSaver.save_finished` and
`SessionSaver::save_finished()`, `App::persist_for_test`.

Gate: `internal_event_drain.rs` (now through `server.outputs`), the git
refresh tests that wait for `GitStatusRefreshed`, the agent-resume settle
tests, `bootstrap_opens_the_gate_after_restore`, and new
`app_outputs_wake_for_events_renders_and_saves` (`app/outputs.rs`: each of an
event, `render_wake.notify_one()` and `save_finished.notify_one()` resolves
`next()`).

### Landing 7 - Narrow surface, private fields

- Make every `App` field private. Add the accessors and named mutations of the
  target surface; route `server/` through them:
  - `client_views.rs`: `reconcile_client_shell_locations` calls
    `app.reconcile_workspace_topology()` first;
    `navigate_shell_client` calls `app.navigate_bookmark`;
    `apply_workspace_geometry` calls `app.apply_workspace_geometry` (the
    `PaneResizer` and `record_workspace_geometry` pair moves into `App`);
    reads through `app.state()` and `app.pane_runtime`.
  - `pane_surface.rs`, `retained_surface.rs`, `render_stream.rs`: take
    `app.render_view()`.
  - `headless.rs`: pane-input and held-release arms use `app.pane_runtime`;
    `handle_scheduled_tasks_headless` calls `app.service_session_saves(now)`;
    after the loop `shut_down_pane_runtimes`.
  - `internal_events.rs`: `prepare_pane_exit` returns `PaneExitPrepared`; the
    closure over `observe_projection_change` goes.
  - `bootstrap.rs`: `seed_startup_workspace_if_empty` reads `app.state()`.
- `GitRefreshScheduler` fields private; `app/git_refresh.rs` tests (and the
  ones Landing 0 moved there) reach them as a child module.
- `#[cfg(test)]` seams on `App` for `server/` tests:
  `test_state_mut() -> &mut AppState`, `test_runtimes_mut()`,
  `test_saver() -> &mut SessionSaver`, `test_paths() -> &AppPaths`. Server
  tests are rewritten mechanically: reads of `server.app.state.X` become
  `server.app.state().X`, writes become `server.app.test_state_mut().X`, and
  likewise for runtimes, saver and paths. Tests under `app/` keep field access
  (a child module sees `App`'s private fields).

Deletes: `pub(crate)` on every `App` field;
`App::observe_projection_change` and
`App::handle_api_request_after_internal_events_drained` leave the crate
surface (private to `app/`).

Gate: the whole suite (this landing changes no behaviour). The rewrite is
large and mechanical; a script under `scripts/` that the implementer writes,
runs and deletes is the expected tool, per the owner's instruction for
complicated edits.

### Landing 8 - Render pass resolves once and plans lock-free

- `ui::SurfaceTarget` and its threading as specified; `ViewedWorkspace::target`
  and `ViewedWorkspace::at`; `render_full`'s per-pass views;
  `presented_views()`; the geometry settlement iterating by index;
  `visible_pane_runtimes` as an iterator.
- `ClientRegistry` on a `BTreeMap`; `render_targets` as an iterator.
- mux: `PaneTerminal::synchronized_output`, `commit_mutation`, the eight call
  sites, `PaneTerminal::synchronized_output_active` on the mirror,
  `PaneRuntimeRead::surface_held`; server `workspace_surface_held` on
  `surface_held`.

Deletes: the `held` memo parameter of `surface_deliverable` and both
`HashMap`s that fed it, the `deliverable` map in `render_full` (the view list
carries it), the six `workspace_index` resolutions per surface render,
`render_targets`' Vec and sort, `window_title_clients`' and `pane_viewers`'
sorts, `workspace_order()` in the settlement.

Gate:
- New mux test `synchronized_output_mirror_follows_every_core_mutation`
  (`crates/shepr-mux/src/pane/terminal/tests.rs`): `CSI ? 2026 h` sets the
  mirror, `CSI ? 2026 l` clears it, a timed-out update's flush through `tick`
  clears it, a resize during an update keeps it, all observed through
  `synchronized_output_active` without holding the core lock.
- New `a_stale_surface_target_draws_no_workspace` (`ui/surface.rs`): a target
  whose index holds another id lays out no panes.
- New `render_targets_iterate_in_client_id_order` (`server/clients.rs`).
- Existing gates for behaviour that must not move:
  `unrelated_render_keeps_synchronized_pane_frame_committed`,
  `sibling_retained_output_waits_for_synchronized_pane_to_finish`,
  `zoom_hidden_synchronized_pane_does_not_block_surface`,
  `retained_snapshot_survives_a_writer_waiting_for_the_terminal_core`,
  `different_size_shells_receive_geometry_specific_patches_from_one_dirty_collection`,
  `explicit_surface_layout_drives_render_cursor_and_hyperlinks`.

### Landing 9 - Seal the crate

- `lib.rs` as specified; the daemon's imports.
- `pub` to `pub(crate)` wherever `unreachable_pub` now reports, including
  `AppState`'s fields and `pub fn`s (keyword only; shape untouched).
- Remove whatever `unused` now reports as never read outside tests (items
  that were kept alive only by being `pub`).

Deletes: `pub mod app`, `pub mod server`, the crate's every public item but
the three re-exports.

Gate: `brokkr check` (its clippy phase runs the deny lints that prove the
seal).

### Landing 10 - Hunt notes

With Landing 8 in, delete STR-030, BUG-057, BUG-060 and CON-076 from their
hunt notes (the notes keep only current gaps), and record what this spec
leaves open in their place only if something is left. Bundled with Landing 9's
commit, not committed alone.

## Test strategy

- Pure units first. Everything this spec moves out of `App` becomes testable
  without an `App` or a PTY: `RenderCadence`, `CreationRetry` and
  `LoopSchedule::next_wake` in `schedule.rs`; `render_window_title` over
  `AppState::test_new`; `SavePolicy::persists_this_boot`; `ResumeSchedule`
  with its two new fields.
- `App` behaviour stays tested next to its module under `app/`, constructed
  with `App::new(&config)` (a `TestApp`). Session-saver tests live in
  `app/session.rs`, git refresh tests in `app/git_refresh.rs`, resume tests in
  `app/agent_resume.rs`.
- Loop behaviour stays in `server/headless/tests/`, now one file per subject
  (Landing 0). New loop tests go in the subject file named in their landing.
- Bug fixes carry a test seen failing with the production half reverted
  (Landings 4 and 5 give the commands).
- Spec E is likely to touch `tests/client_input.rs` (pixel mouse) and
  `render.rs`; Landing 0 puts those tests in one file so its edits do not
  collide with this spec's.

## Risks

- Removing the generic render request drops a render that some path got only
  from the poke. The six callers are enumerated above, and each already
  returns its change to the loop except the host theme, which
  `sync_host_theme_from_foreground` now marks. A missed one would show as a
  view that updates on the next unrelated wake. The new host-theme test and
  the existing projection tests in `tests/projection.rs` cover them.
- The resume latch ignores a plan minted after the schedule retired. No
  production path mints one; the landing's test pins the latch, and the
  `ResumeSchedule` doc states the invariant. If Spec A gives `AppState` a
  pending-resume count, the latch can give way to it (see neighbours).
- The synchronized-output mirror is only as good as `commit_mutation` being
  the one way to record a core mutation. `record_mutation` stays private to
  `PaneTerminalCore`, and the mux test checks each mutation kind. A stale
  mirror at worst lets the plan send a client into `render_pane_surface`,
  whose locked check still defers the surface.
- `BTreeMap` iteration changes the order of per-client sends that did not
  sort (shutdown notices, mode streams) from hash order to id order. Nothing
  depends on hash order.
- `TestApp`'s `Deref` makes method calls resolve to `App`; `TestApp` must not
  define a method whose name `App` also has.
- Title sync moves from inside the app's API and endpoint handlers to the loop
  just before them. App-level tests that called `handle_api_request` and
  relied on the sync would stop seeing fresh titles; the only App-level caller
  is `app/api/detect.rs`'s test, which does not read titles.
- Landing 7's mechanical rewrite is large; it changes no behaviour, so the
  full suite is its gate, but review should spot-check that writes did not
  become reads of a clone.
- Spec A landing first with private `AppState` fields changes the spelling of
  every `state().X` read this spec introduces; whichever lands second adapts
  mechanically.

## Findings

1. `AppState::clock_now` is never read in production; only
   `app/actions/tests.rs` reads it. STR-030 describes it as a second live copy.
2. Bug: the default-workspace retry is never reset in production.
   `App::create_default_workspace` resets `default_workspace_retry_at` and
   `default_workspace_retry_failures` when the session has a workspace, but its
   only caller, `HeadlessServer::create_automatic_workspace`, returns before
   calling it whenever the session has one. The failure count therefore grows
   across separate empty periods (each later failure backs off longer than its
   own streak warrants), and a retry instant left in the future still wakes the
   loop once. Fixed by Landing 4.
3. `App::invalidate_shared_view` calls `render_notify.notify_one()` from code
   that already runs on the loop thread inside a pass; the stored permit makes
   the next `next_loop_event` return at once for nothing. Gone with Landing 5.
4. BUG-060 is understated: a surface render resolves its workspace six times
   (three in `render_pane_surface`, one each in `compute_surface_for`,
   `render_panes`, `surface_cursor`), and the resume scan runs at least twice
   per pass (start and wakeup).
5. BUG-057 is understated: `render_full` re-collects and re-sorts the targets,
   and `settle_workspace_geometry_before_plan` allocates a `workspace_order()`
   Vec on every plan and a `visible_pane_runtimes` Vec per workspace on every
   PTY-dirty plan.
6. CON-076 is understated: `settle_workspace_geometry_before_plan` and
   `apply_all_workspace_geometry` resolve every client's view per workspace
   (`ViewedWorkspace::for_location` in the inner loop).
7. Persistence policy is recorded three times (`AppPolicy`, `SavePolicy`,
   `HostShutdownFreeze::persist_session`) and kept in step by hand in
   `freeze_session_saves`, `thaw_session_saves` and the lifecycle. Landing 3
   leaves one.
8. `test_headless_server` duplicates `HeadlessServer::new`'s field list; a
   field added to one and defaulted differently in the other is silent.
9. `server/headless/tests/mod.rs` declares its sibling files with redundant
   `#[cfg(test)]` (inside a test-only module) and `#[path]` attributes, one of
   them (`server_stop_tests`) without the `#[cfg(test)]` the others carry.
10. `HeadlessServer::handle_internal_event_with_origin` admits a runtime event,
    unwraps it, re-wraps it with `preserve_runtime_origin`, and the app admits
    it again (two runtime lookups per pane exit). Harmless, but the second
    admission exists only because the app's entry points take the envelope;
    worth folding when the internal-event path is next touched.
11. `App::observe_projection_change` is `pub(crate)` and takes a closure over
    `&mut App`, which handed the loop arbitrary access to the app for one call
    site. Landing 7 replaces it with `PaneExitPrepared`.
12. For Spec E: `stream_host_mouse_capture_mode` and
    `stream_shell_keyboard_mode` each call `shell_focused_runtime` per client,
    resolving the viewed workspace twice per client whenever host modes are
    dirty; `presented_views()` from Landing 8 is available to them.
13. `ResumeSchedule`'s test `no_pending_plans_resets_the_schedule` asserts
    that new plans after an empty period start fresh, a case production never
    reaches; Landing 5 replaces it.

## Neighbours

### What this spec assumes of Spec A (`notes/spec-data-model.md`)

- `AppState` keeps offering, under whatever names: the ordered workspaces and
  an id-to-index lookup (`workspace_index`), the bookmark index and its
  reconcile and set operations, recorded workspace geometry (read, record,
  retain-live), the runtime lookup of a pane through its terminal
  (`runtime_for_pane_in_workspace`, `runtime_of`), terminal lookup for title
  sync and the resume scan, the host theme and appearance values, the
  session-dirty and shell-projection-revision signals, and `settings`
  (`AppSettings`, pane chrome only; this spec adds nothing to it).
- The `PaneRuntimeRegistry` stays on `App`, outside `AppState` (AGENTS.md).
  `App::pane_runtime` and `App::render_view` are the seams if A moves it.
- Resume plans are minted only by session restore. If A's design lets a plan
  appear later, or gives `AppState` a pending-resume count, tell this spec's
  implementer: the `retired` latch is replaced by that count.
- A accepts this spec deleting the dead `AppState::clock_now` (Landing 1)
  and changing only the `pub` keyword on `AppState` items in Landing 9.
- `Workspace::display_name` returning `&str` (Landing 2) does not conflict
  with A's workspace tree; if A owns that method's file region at the time,
  A makes the change instead.

### What this spec offers Spec A

- After Landing 7 no `&mut AppState` leaves `app/` outside `#[cfg(test)]`:
  every mutation from `server/` goes through a named `App` method
  (`reconcile_workspace_topology`, `navigate_bookmark`,
  `apply_workspace_geometry`, the event and request handlers). A can enforce
  `AppState` invariants at those methods without auditing the loop.
- Reads from `server/` go through `App::state()`, a single spelling to update
  when A makes fields private.
- `App::test_state_mut()` is the one test seam for server tests that set up
  state.

### What this spec assumes of Spec E (`notes/spec-pixel-geometry.md`)

- E keeps `stream_host_mouse_capture_mode`, `downgrade_ineligible_pixel_mouse`
  and pixel eligibility in `render.rs` and `pane_input.rs`; this spec only
  re-spells their reads (`self.app.pane_runtime(..)` for
  `runtime_for_pane_in_workspace(&self.app.terminal_runtimes, ..)`) in
  Landing 7.
- E's tests go in `server/headless/tests/client_input.rs` after Landing 0.

### What this spec offers Spec E

- `App::pane_runtime(workspace_index, pane_id)` and `App::render_view()`.
- `ui::SurfaceTarget` and `ViewedWorkspace::target`/`at` for resolving a
  client's view once, and `HeadlessServer::presented_views()` (Landing 8) for
  per-pass resolution in the mode streams.
- `PaneRuntimeRead::surface_held()` and the lock-free
  `synchronized_output_active()` if E needs either.
- `ClientRegistry` iterating in client-id order (Landing 8), so E needs no
  sort of its own.
