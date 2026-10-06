//! Headless server mode - runs the shepr event loop without a real terminal.
//!
//! The server:
//! - Does not enter raw mode or read stdin
//! - Serves one socket for the JSON API and TUI clients
//! - Initializes AppState and all PTYs from session restore or fresh state
//! - Runs the main event loop (drain events, drain API requests, scheduled tasks)
//! - Derives render work per connection from its view, baseline and surface slot
//! - Renders virtual surfaces directly to wire-cell frames in memory
//! - Accepts TUI connections after pane restore
//! - Streams frames to connected clients after each render
//! - Routes client input events through the existing input pipeline
//! - Continues running after client disconnect
//! - Handles stale socket cleanup, explicit server stop, and pane spawn failure
//!   during restore

use crate::server::ClientId;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tracing::debug;

use crate::app;
use crate::limits::{
    PANE_TEARDOWN_WAIT, SERVER_EVENT_CHANNEL_CAPACITY, SERVER_EVENT_DRAIN_LIMIT, STOP_ANSWER_WAIT,
};
use crate::server::client_transport::ServerEvent;
use crate::server::clients::{
    ClientConnection, ClientDeparture, ClientRegistry, ClientShellLocationGeneration,
    ClientShellState, ShellSessionGeneration, render_targets,
};
use crate::server::outbox::{ClientOutbox, Delivery, ReleaseMode};
use crate::server::pane_input::apply_client_pane_input_events;
use crate::server::pane_surface::render_pane_surface as render_client_shell_pane_surface;
use crate::server::render_stream::ViewEpoch;
use shepr_mux::events::AppEvent;
use shepr_protocol::{FrameData, ServerMessage};

mod api_dispatcher;
mod bootstrap;
mod client_views;
mod endpoint_requests;
mod internal_events;
mod lifecycle;
mod render;
mod retained_surface;
mod schedule;
mod surface_interest;

pub use bootstrap::{RunServerError, ServerReady, run_server};
use lifecycle::UnexpectedPhase;
use lifecycle::{ShutdownLifecycle, ShutdownPhase};
use schedule::WakeInputs;

/// Samples the clock app state reads. App code never reads the clock itself
/// (the `app-state-reads-the-clock-seam` textlint); the server samples it
/// here and hands it in with `App::set_clock` before app work runs.
pub(super) fn sample_app_clock() -> app::AppClock {
    app::AppClock {
        // headless-clock-sample-ok: this is the server-owned sampling seam.
        now: Instant::now(),
        wall_now: std::time::SystemTime::now(),
    }
}

// ---------------------------------------------------------------------------
// Loop event enum for the headless server event loop
// ---------------------------------------------------------------------------

/// Events that the headless server event loop can process.
enum LoopEvent {
    Timer,
    Internal(AppEvent),
    Api(Box<shepr_api::ApiRequestMessage>),
    ServerEvent(ServerEvent),
}

struct PendingCheckpointedPaneExit {
    event: shepr_mux::events::AppEvent,
    checkpoint_generation: app::CheckpointGeneration,
}

// ---------------------------------------------------------------------------
// Headless server
// ---------------------------------------------------------------------------

/// The per-client inputs to the immediate PTY sources and the host input
/// modes. A presenting client whose key changes, appears or departs marks
/// both stale (`HeadlessServer::refresh_client_view_keys`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct ClientViewKey {
    presenting: bool,
    location_generation: ClientShellLocationGeneration,
    terminal_size: shepr_core::geometry::GridSize,
    host_cell: shepr_core::geometry::HostCell,
}

/// Coordinates the event loop without a real terminal. Lifecycle policy owns
/// its rules separately. Client geometry application stays here because it
/// can start resumes and send focus reports;
/// render coordination settles each connection's location, baseline and outbox
/// against the same app revision, rather than owning a second client registry.
pub(crate) struct HeadlessServer {
    // Crate-visible so `app` module tests can drive an `App` through the
    // server's event envelope (`handle_test_runtime_exit_and_replay`).
    pub(crate) app: app::App,
    /// What the app and its pane runtimes publish (events, render requests,
    /// finished saves): the loop owns what it waits on.
    pub(crate) outputs: app::AppOutputs,
    view_epoch: ViewEpoch,
    /// The server socket: startup opens its TUI gate
    /// (`open_client_protocol`), and dropping it tears down the listener and
    /// removes the socket file (`release_socket_after_save`).
    api_server: Option<shepr_api::ServerHandle>,
    clients: ClientRegistry,
    /// Last small client-view inputs used to invalidate PTY visibility and
    /// host input modes when a connection's view changes.
    client_view_keys: HashMap<ClientId, ClientViewKey>,
    /// Identity used to reject shell replacements from an earlier server boot.
    /// `run_server` mints it once for the server lifetime and hands the same
    /// value to the socket listener, so it is what `ping` reports and the stop
    /// guard compares.
    client_shell_boot_id: shepr_protocol::BootId,
    /// Shared session source for shell projections; `None` until a shell
    /// connection or render needs it.
    shell_session_cache: Option<render::ShellSessionCache>,
    /// Moves whenever shell projections must be recomputed: the cache was
    /// rebuilt for a new application revision, or the cwd timer found a
    /// projection that changed. Each shell client records the generation it
    /// last projected.
    shell_session_generation: ShellSessionGeneration,
    /// Panes last told they hold terminal focus (`sync_pane_focus`), derived
    /// from the clients' views; the record of what the panes were sent, not a
    /// view of its own.
    focused_panes: HashSet<client_views::ShellFocusTarget>,
    /// Whether the set of panes whose PTY output should wake the loop at once
    /// (`sync_immediate_pty_sources`) may be stale. That set depends only on
    /// the clients' workspace views, pane membership, and focused pane when a
    /// workspace is zoomed. A PTY render wake changes none of those.
    /// Client activation, location, and geometry changes mark it through the
    /// per-client key; topology changes mark it where they are applied.
    /// Recomputing it on every loop wake walked every pane per PTY notify. A
    /// missed mark would only delay a visible pane's repaint to the normal
    /// render cadence, never drop it: visibility at render time is computed
    /// fresh.
    immediate_pty_sources_dirty: bool,
    /// Whether the host mouse-capture and keyboard modes pushed to clients
    /// (`stream_host_mouse_capture_mode`, `stream_shell_keyboard_mode`)
    /// may be stale. They follow the focused pane's terminal modes, which only
    /// PTY output changes, plus the same client/topology changes as above.
    /// Client changes mark it through the per-client key and a render request
    /// carrying PTY sources is taken; every render is followed by another loop
    /// iteration, which pushes the modes before the loop sleeps again.
    host_input_modes_dirty: bool,
    /// Reason captured by the retained renderer and reported after the full
    /// render that recovers from it.
    retained_surface_fallback_reason: Option<retained_surface::RetainedSurfaceFallback>,
    /// Fallback reasons already reported once, so each report can say whether
    /// its reason is recurring. Every fallback is still logged.
    retained_surface_fallbacks_reported: HashSet<retained_surface::RetainedSurfaceFallback>,
    /// Owns running, host-shutdown warning/freeze, cancellation and stopping.
    lifecycle: ShutdownLifecycle,
    /// Channel for receiving server events from client connection threads.
    server_event_rx: mpsc::Receiver<ServerEvent>,
    /// Production sender cloned into the client transport handler when startup
    /// opens the protocol. Retaining it keeps the receiver alive across gaps
    /// between client connection threads; tests also use it to inject events.
    server_event_tx: mpsc::Sender<ServerEvent>,
    /// Bounded requests received by the JSON API listener.
    api_request_rx: mpsc::Receiver<shepr_api::ApiRequestMessage>,
    /// False once `api_request_rx` reported closed; the loop then stops
    /// selecting it instead of spinning on a receiver that resolves at once.
    api_request_open: bool,
    /// Outboxes of new clients dequeued after stopping, held until their
    /// queued commands receive refusals before the shutdown notice is sent.
    shutdown_unregistered_clients: HashMap<ClientId, ClientOutbox>,
    /// Acknowledgements for shutdown frames queued to client writer threads.
    shutdown_flushes: Vec<tokio::sync::oneshot::Receiver<()>>,
    /// Pane exits held until their pre-removal session checkpoint reaches disk.
    pending_checkpointed_pane_exits: VecDeque<PendingCheckpointedPaneExit>,
    /// Raised by client outboxes on closure or control-lane progress, client
    /// writers after a render drains, and the host shutdown monitor. Wakes an
    /// idle loop to reap, release replies, refresh surfaces, or sync shutdown.
    outbox_wake: Arc<tokio::sync::Notify>,
    /// The render cadence and the automatic workspace's retry backoff: the
    /// loop's own timing, folded into its next wake by `LoopSchedule`.
    schedule: schedule::LoopSchedule,
}

impl HeadlessServer {
    /// Builds the event-loop coordinator without admitting TUI clients.
    /// Startup opens the client protocol explicitly after pane restore.
    /// `boot_id` is the boot `api_server` was started with.
    pub(super) fn new(
        app: app::App,
        outputs: app::AppOutputs,
        api_request_rx: mpsc::Receiver<shepr_api::ApiRequestMessage>,
        api_server: shepr_api::ServerHandle,
        stop_signal: Arc<shepr_api::ServerStopSignal>,
        boot_id: shepr_protocol::BootId,
    ) -> Self {
        // Channel for server events from client threads.
        let server_events = mpsc::channel(SERVER_EVENT_CHANNEL_CAPACITY);
        Self::assemble(
            app,
            outputs,
            api_request_rx,
            Some(api_server),
            stop_signal,
            boot_id,
            server_events,
        )
    }

    /// The one place the struct's field list exists: `new` and the test
    /// constructor differ only in what they pass.
    fn assemble(
        app: app::App,
        outputs: app::AppOutputs,
        api_request_rx: mpsc::Receiver<shepr_api::ApiRequestMessage>,
        api_server: Option<shepr_api::ServerHandle>,
        stop_signal: Arc<shepr_api::ServerStopSignal>,
        boot_id: shepr_protocol::BootId,
        (server_event_tx, server_event_rx): (
            mpsc::Sender<ServerEvent>,
            mpsc::Receiver<ServerEvent>,
        ),
    ) -> Self {
        let outbox_wake = Arc::new(tokio::sync::Notify::new());

        Self {
            app,
            outputs,
            view_epoch: ViewEpoch::INITIAL,
            api_server,
            clients: ClientRegistry::default(),
            client_view_keys: HashMap::new(),
            client_shell_boot_id: boot_id,
            shell_session_cache: None,
            shell_session_generation: ShellSessionGeneration::default(),
            focused_panes: HashSet::new(),
            schedule: schedule::LoopSchedule::default(),
            immediate_pty_sources_dirty: true,
            host_input_modes_dirty: true,
            retained_surface_fallback_reason: None,
            retained_surface_fallbacks_reported: HashSet::new(),
            lifecycle: ShutdownLifecycle::new(stop_signal),
            server_event_rx,
            server_event_tx,
            api_request_rx,
            api_request_open: true,
            shutdown_unregistered_clients: HashMap::new(),
            shutdown_flushes: Vec::new(),
            pending_checkpointed_pane_exits: VecDeque::new(),
            outbox_wake,
        }
    }

    /// Opens the TUI gate after startup has restored panes. Until this step
    /// the bound socket answers ping with `starting` and refuses TUI clients.
    pub(super) fn open_client_protocol(&self) {
        if let Some(api) = &self.api_server {
            api.client_gate().open(Arc::new(
                crate::server::client_transport::ClientTransportHandler {
                    server_event_tx: self.server_event_tx.clone(),
                    stop_signal: Arc::clone(self.lifecycle.stop_signal()),
                    wake: Arc::clone(&self.outbox_wake),
                    ids: crate::server::clients::ClientIdAllocator::default(),
                },
            ));
        }
    }

    fn mark_view_changed(&mut self) {
        self.view_epoch.advance();
    }

    /// Rechecks scalar per-client view inputs after a client mutation. The
    /// pane list is still rebuilt only when one of these inputs changes or
    /// application topology explicitly changes it.
    pub(super) fn refresh_client_view_keys(&mut self) {
        let next = self
            .clients
            .iter()
            .map(|(&client_id, client)| {
                (
                    client_id,
                    ClientViewKey {
                        presenting: self.clients.is_presenting(&client_id),
                        location_generation: client.shell_state().location.generation(),
                        terminal_size: client.terminal_size,
                        host_cell: client.host_cell,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let changed =
            self.client_view_keys
                .iter()
                .any(|(client_id, previous)| match next.get(client_id) {
                    Some(current) => {
                        (previous.presenting || current.presenting) && previous != current
                    }
                    None => previous.presenting,
                })
                || next.iter().any(|(client_id, current)| {
                    !self.client_view_keys.contains_key(client_id) && current.presenting
                });
        self.client_view_keys = next;
        if changed {
            self.immediate_pty_sources_dirty = true;
            self.host_input_modes_dirty = true;
        }
    }

    /// Starts request dispatch only while the lifecycle accepts work.
    /// Stop requests can arrive after a loop batch dequeues an item, so each
    /// dispatch entry uses this gate before it applies the item.
    pub(super) fn begin_request_dispatch(&mut self) -> bool {
        if self.lifecycle.stop_requested() {
            self.initiate_shutdown();
        }
        self.lifecycle.phase() != ShutdownPhase::Stopping
    }

    fn invalidate_pane_viewers(&mut self, pane: shepr_core::layout::PaneId) {
        for id in self.pane_viewers(pane) {
            if let Some(client) = self.clients.get_mut(&id) {
                client.render_state.invalidate();
            }
        }
    }

    fn retry_refused_viewers(&mut self, sources: &HashSet<shepr_core::layout::PaneId>) -> bool {
        let mut changed = false;
        for pane in sources {
            for id in self.pane_viewers(*pane) {
                if let Some(client) = self.clients.get_mut(&id) {
                    changed |= client.render_state.retry_refused();
                }
            }
        }
        changed
    }

    /// Hands the app a fresh clock sample and returns its monotonic half.
    fn refresh_app_clock(&mut self) -> Instant {
        let clock = sample_app_clock();
        self.app.set_clock(clock);
        clock.now
    }

    /// Runs the headless server event loop until shutdown.
    ///
    /// This is the server's main runtime loop. It:
    /// - Drains internal events (pane death, state changes)
    /// - Drains API requests (from the JSON socket)
    /// - Accepts new client connections
    /// - Reads client messages and routes input
    /// - Handles scheduled tasks (session save, metadata expiry, etc.)
    /// - Renders virtually and streams frames to clients
    pub(crate) async fn run(&mut self) -> Result<(), RunServerError> {
        crate::logging::startup();
        // The fallible setup below returns before the loop, so it skips the
        // final save; `Drop` still releases the lease and socket in order. No
        // save is owed: no client has connected and no event has been applied,
        // so nothing has changed since bootstrap left the session on disk.
        // Every failure inside the loop goes through `initiate_shutdown` and
        // the save after it.
        // Register SIGINT handler for graceful shutdown.
        let stop_signal = Arc::clone(self.lifecycle.stop_signal());
        let signal_quit = Arc::clone(self.lifecycle.signal_quit_request_flag());
        ctrlc_handler(stop_signal, signal_quit).map_err(RunServerError::SignalInstall)?;
        self.lifecycle
            .start_host_shutdown_monitor(&self.outbox_wake);

        let mut run_error = None;
        loop {
            // If shutdown has been initiated, complete it and exit.
            if self.lifecycle.phase() == ShutdownPhase::Stopping {
                // Release any reply still held before waiting for client
                // flushes. Replies held when shutdown began were already
                // queued ahead of the shutdown notice.
                self.release_endpoint_replies(ReleaseMode::Shutdown);
                if let Err(err) = self.complete_shutdown().await {
                    run_error.get_or_insert(RunServerError::Shutdown(err));
                }
                break;
            }

            self.reap_closed_clients();

            // Every pass through the loop starts with a fresh clock sample, so
            // the event and API handlers below read this iteration's time.
            self.refresh_app_clock();

            // Check if we should start shutting down. The drain applies queued
            // state and agent-session reports so the final save carries them;
            // after a signal it leaves pane deaths out (see
            // `signal_quit_requested`).
            if self.lifecycle.stop_requested() {
                self.drain_all_internal_events_with_forwarding();
                self.initiate_shutdown();
                continue;
            }

            // 2. Drain a bounded internal-event batch. API handlers perform an
            // exhaustive forwarding-aware drain before reading pane/runtime state.
            if self.drain_internal_events_with_forwarding() {
                self.mark_view_changed();
            }
            if self.lifecycle.stop_requested() {
                continue;
            }
            self.refresh_app_clock();

            // 3. Drain API requests.
            if self.drain_api_requests_with_shutdown_check() {
                self.mark_view_changed();
            }
            if self.lifecycle.stop_requested() {
                continue;
            }

            self.app.sync_session_save_schedule();

            // 4. Drain server events from client threads.
            self.drain_server_events();
            if self.lifecycle.stop_requested() {
                continue;
            }

            // 5. Handle scheduled tasks.
            let now = self.refresh_app_clock();
            if self.handle_scheduled_tasks_headless(now) {
                self.mark_view_changed();
            }

            if self.create_automatic_workspace(None) {
                self.mark_view_changed();
            }
            if self.shell_cwd_refresh_due(now) && self.refresh_shell_projection_sources() {
                self.mark_view_changed();
            }

            if std::mem::take(&mut self.immediate_pty_sources_dirty) {
                self.sync_immediate_pty_sources();
                self.host_input_modes_dirty = true;
            }
            // PTY output reaches this through the render request: the mode push
            // runs on the iteration right after the render that took it.
            if std::mem::take(&mut self.host_input_modes_dirty) {
                self.stream_host_mouse_capture_mode();
                self.stream_shell_keyboard_mode();
            }

            // Derive each connection's work from its view, baseline and slot.
            let render_signal_pending = self.outputs.render().is_pending();
            let plan = self.render_plan(render_signal_pending);
            let render_cadence_due = self.schedule.cadence.can_render(now);
            if (plan.has_full() || render_signal_pending)
                && (render_cadence_due
                    || (self.schedule.cadence.can_present(now)
                        && (plan.has_full() || self.outputs.render().has_immediate_work())))
            {
                let planned_at = self.view_epoch;
                let request = self.outputs.render().take();
                let pty_dirty = !request.pty_sources.is_empty();
                if pty_dirty {
                    self.host_input_modes_dirty = true;
                }
                if self.sync_terminal_title_sources(&request.terminal_title_sources) {
                    self.mark_view_changed();
                }
                let retried = pty_dirty && self.retry_refused_viewers(&request.pty_sources);
                let plan = if self.view_epoch != planned_at
                    || retried
                    || pty_dirty != render_signal_pending
                {
                    self.render_plan(pty_dirty)
                } else {
                    plan
                };
                if !plan.has_full() && !pty_dirty {
                    continue;
                }
                let hidden_only = pty_dirty
                    && !plan.has_full()
                    && !self.pty_sources_visible_to_any_render_target(&request.pty_sources);
                let sources = if hidden_only {
                    HashSet::new()
                } else {
                    request.pty_sources
                };
                self.render_pass(&plan, &sources);
                self.report_retained_surface_fallback();
                self.schedule.cadence.record(now, !hidden_only);
                self.release_endpoint_replies(ReleaseMode::WithinBudget);
                continue;
            }
            // Replies wait for any projection owed by a cadence-held pass.
            if !plan.has_full() && !render_signal_pending {
                self.release_endpoint_replies(ReleaseMode::WithinBudget);
            }
            let next_deadline = self.schedule.next_wake(
                now,
                WakeInputs {
                    render_owed: plan.has_full() || render_signal_pending,
                    app: self.app.next_deadline(self.has_app_client()),
                    shell_cwd: self.shell_cwd_refresh_deadline(),
                },
            );
            let event = self.next_loop_event(next_deadline).await;
            // The wait above can last until the next deadline; dispatch reads
            // the time the event arrived, not the time the wait began.
            self.refresh_app_clock();

            if self.lifecycle.stop_requested() {
                // This request was already dequeued when the stop arrived.
                // Queue its refusal now; shutdown cleanup broadcasts the
                // notice after it settles events still waiting in the channel.
                if let LoopEvent::ServerEvent(ServerEvent::ShellEndpointRequest {
                    client_id,
                    boot_id,
                    request_id,
                    ..
                }) = &event
                {
                    self.reject_endpoint_request_for_shutdown(
                        *client_id,
                        boot_id.clone(),
                        request_id.clone(),
                    );
                }
                self.initiate_shutdown();
                match event {
                    LoopEvent::Internal(ev) => {
                        self.lifecycle.sync_host_shutdown_freeze(&mut self.app);
                        self.handle_internal_event_with_forwarding(ev);
                    }
                    LoopEvent::ServerEvent(ServerEvent::ShellConnected {
                        client_id,
                        outbox,
                        ..
                    }) => {
                        self.shutdown_unregistered_clients.insert(client_id, outbox);
                    }
                    // Already dequeued, so the shutdown drain would never see
                    // it; answer it here.
                    LoopEvent::Api(msg) => self.reject_api_request_for_shutdown(&msg),
                    // A client endpoint request lands here, its refusal
                    // already queued above; nothing else needs an answer.
                    _ => {}
                }
                continue;
            }

            match event {
                LoopEvent::Timer => {}
                LoopEvent::Internal(ev) => {
                    self.lifecycle.sync_host_shutdown_freeze(&mut self.app);
                    if self.handle_internal_event_with_forwarding(ev) {
                        self.mark_view_changed();
                    }
                }
                LoopEvent::Api(msg) => {
                    if self.handle_api_request_with_shutdown_check(*msg) {
                        self.mark_view_changed();
                    }
                }
                LoopEvent::ServerEvent(ev) => {
                    self.handle_server_event(ev);
                }
            }
        }

        // There is deliberately no forced stop. `server.stop` is accepted on
        // the API connection thread, so it can wake this loop even while the
        // app channel is full; the response waits for this loop's final save.
        // The final save and the writer's retirement wait for the persister
        // without a deadline.
        // Only two things can wedge here: a loop bug (deadlock or spin) and a
        // data directory on a hung filesystem. Against either, a watchdog that
        // exits after a deadline does no more than SIGKILL. A thread blocked in
        // uninterruptible IO holds the flock lease until the IO returns
        // whoever ends the process, and a killable wait dies to SIGKILL just
        // the same. Recovery from a SIGKILL needs nothing extra: the kernel
        // drops the flock lease, the socket file left behind probes as stale
        // and the next bind reclaims it, and saves publish atomically, so the
        // last autosave survives whole and is what the next start restores. A
        // deadline would only add the risk of cutting short a slow but healthy
        // final save. SIGINT, SIGTERM and SIGHUP only latch the stop (see
        // `ctrlc_handler`); SIGKILL is the escape hatch.
        //
        // Save session on exit. During a host shutdown saving is frozen, so
        // this writes nothing and the checkpoint taken on the warning stands;
        // the writer is still retired.
        self.refresh_app_clock();
        let final_save_started = self.app.clock().now;
        let final_save = self
            .app
            .save_session_for_exit(self.lifecycle.signal_quit_at())
            .await;
        let final_save_error = final_save.as_ref().err().map(ToString::to_string);
        self.lifecycle
            .stop_signal()
            .complete_final_save(final_save_error);
        // The save can take seconds; the duration below and the teardown wait
        // after it read a fresh sample.
        self.refresh_app_clock();
        shepr_platform::structured_log!(
            INFO,
            event = persist.save,
            outcome = if final_save.is_err() {
                "failed"
            } else if self.app.session_saves_stopped() {
                "stopped"
            } else if self.app.session_saves_blocked_on_backup() {
                "blocked_on_backup"
            } else if self.app.session_saves_frozen() {
                "frozen"
            } else {
                "completed"
            },
            kind = "final",
            duration_ms = self
                .app
                .clock()
                .now
                .saturating_duration_since(final_save_started)
                .as_millis(),
            "final session save finished"
        );
        if let Err(error) = final_save {
            run_error.get_or_insert(RunServerError::Runtime(error));
        }
        // The production headless runtime is multi-threaded. Hand this worker
        // back to Tokio while the tracker waits on its condition variable, so
        // child watchers can still reap processes and signal teardown workers.
        // Direct current-thread runtime drives cannot use `block_in_place`.
        let unfinished = if tokio::runtime::Handle::current().runtime_flavor()
            == tokio::runtime::RuntimeFlavor::MultiThread
        {
            tokio::task::block_in_place(|| self.app.shut_down_pane_runtimes(PANE_TEARDOWN_WAIT))
        } else {
            self.app.shut_down_pane_runtimes(PANE_TEARDOWN_WAIT)
        };
        if !unfinished.is_empty() {
            shepr_platform::structured_log!(
                WARN, event = shutdown.pane_teardown, outcome = "timeout",
                count = unfinished.len(), panes = ?unfinished,
                "pane session teardown did not finish before server exit"
            );
        }
        // The save and the teardown wait can each take seconds.
        self.refresh_app_clock();
        if let Err(error) = self.app.retire_session_writer_async().await {
            run_error.get_or_insert(RunServerError::Runtime(error));
        }
        // A stop request's answer carries the final save result; the teardown
        // above has normally given it time to be written, and an exit under
        // it would hand the stopping client an empty answer instead.
        if !self
            .lifecycle
            .stop_signal()
            .wait_for_stop_answers(STOP_ANSWER_WAIT)
        {
            debug!("a stop request's answer was not written before the server exit");
        }
        self.release_socket_after_save();

        // A successor can start once the lease is free, while this process
        // still logs to the same server log until it exits. That is safe: the
        // log writer is built for several processes sharing one file (appends
        // under a shared flock, and each record follows another process's
        // rotation), so the two interleave lines and lose none.
        shepr_platform::structured_log!(
            INFO,
            event = server.shutdown,
            outcome = "released",
            "headless server exiting"
        );
        run_error.map_or(Ok(()), Err)
    }

    /// Waits for the next thing that needs the loop: an event, a wakeup or
    /// the deadline. A capped drain leaves queued work in its receiver, so the
    /// matching branch stays ready and starts another pass immediately.
    async fn next_loop_event(&mut self, next_deadline: Option<Instant>) -> LoopEvent {
        let stop_signal = Arc::clone(self.lifecycle.stop_signal());
        // A closed receiver resolves at once on every poll, so a branch whose
        // channel closed must stop being selected or the loop would spin on
        // Timer events.
        let api_open = self.api_request_open;
        tokio::select! {
            // A `server.stop` from the API sets the latch on another thread;
            // this is what wakes an idle loop to act on it.
            () = stop_signal.notified() => LoopEvent::Timer,
            // Outbox progress, render completion, and host shutdown all wake
            // the loop through this coalescing state-change notification.
            () = self.outbox_wake.notified() => LoopEvent::Timer,
            // Losing every API sender means ingress has ended. Stop the
            // server so its unreachable socket and lease can be retired.
            maybe_api = self.api_request_rx.recv(), if api_open => match maybe_api {
                Some(msg) => LoopEvent::Api(Box::new(msg)),
                None => {
                    self.api_request_open = false;
                    stop_signal.request();
                    shepr_platform::structured_log!(
                        ERROR, event = api.channel, outcome = "closed",
                        "API request channel closed; stopping server"
                    );
                    LoopEvent::Timer
                }
            },
            // An app event, a pane runtime's render request or a finished
            // session save: the outputs the app publishes. A signal only
            // wakes the pass, which reaps the save and renders what is owed.
            wake = self.outputs.next() => match wake {
                app::AppWake::Event(ev) => LoopEvent::Internal(ev),
                app::AppWake::Signal => LoopEvent::Timer,
            },
            // The server holds a sender for its own event channel (it is
            // cloned to clients), so this cannot close while the loop runs.
            maybe_server_ev = self.server_event_rx.recv() => match maybe_server_ev {
                Some(ev) => LoopEvent::ServerEvent(ev),
                None => LoopEvent::Timer,
            },
            _ = sleep_until_or_pending(next_deadline) => LoopEvent::Timer,
        }
    }

    /// Colours the panes with the foreground client's host theme: panes have
    /// one theme (their default colours and the answers to colour queries),
    /// and the client the user was last active in supplies it. A client with
    /// no host color report yet leaves the current theme (a live client's, or
    /// the one saved with the session) in place. Its appearance still applies
    /// independently. Returns whether anything changed.
    ///
    /// A change marks the view changed here (pane cells are drawn with the
    /// theme), so a caller may discard the result and still render.
    fn sync_host_theme_from_foreground(&mut self) -> bool {
        let Some(shell) = self
            .clients
            .foreground_client_id()
            .and_then(|client_id| self.clients.get(&client_id))
            .map(ClientConnection::shell_state)
        else {
            return false;
        };
        if shell.host_terminal_theme.is_empty()
            && shell.host_terminal_appearance == app::HostAppearanceReport::Unknown
        {
            return false;
        }
        let theme = shell.host_terminal_theme;
        let appearance = shell.host_terminal_appearance;
        let mut changed = self.app.set_host_terminal_appearance_state(appearance);
        if !theme.is_empty() && self.app.set_host_terminal_theme(theme) {
            // The theme colours pane cells: every client needs a full pass.
            self.mark_view_changed();
            changed = true;
        }
        changed
    }

    /// Records activity from `client_id`, making it the foreground client if
    /// it is presenting a shell surface. Returns whether the foreground client changed.
    fn promote_client_to_foreground(&mut self, client_id: ClientId) -> bool {
        let changed = self.clients.promote_to_foreground(client_id);
        if changed {
            self.sync_host_theme_from_foreground();
        }
        changed
    }

    fn app_client_count(&self) -> usize {
        self.clients.app_client_count()
    }

    fn has_app_client(&self) -> bool {
        self.app_client_count() > 0
    }

    fn remove_client(&mut self, client_id: ClientId) -> bool {
        let Some(departure) = self.clients.remove_client(client_id) else {
            return false;
        };
        self.apply_client_departures(vec![(client_id, departure)])
    }

    /// Applies effects for clients that stopped presenting or left the
    /// registry. The registry has already released ownership; one settlement
    /// now releases held input, focus, geometry, and the shared view epoch.
    fn apply_client_departures(&mut self, departures: Vec<(ClientId, ClientDeparture)>) -> bool {
        if departures.is_empty() {
            return false;
        }
        let mut foreground_changed = false;
        for (client_id, departure) in departures {
            let (changed, held_inputs) = match departure {
                ClientDeparture::SurfaceDeactivated {
                    foreground_changed,
                    held_inputs,
                }
                | ClientDeparture::ConnectionRemoved {
                    foreground_changed,
                    held_inputs,
                } => (foreground_changed, held_inputs),
            };
            foreground_changed |= changed;
            self.release_client_shell_inputs(client_id, held_inputs);
        }
        if foreground_changed {
            self.sync_host_theme_from_foreground();
        }
        self.sync_pane_focus();
        self.refresh_client_view_keys();
        if self.lifecycle.phase() != ShutdownPhase::Stopping {
            self.reapply_controlled_shell_workspace_geometry(client_views::PendingResumes::Start);
            self.mark_view_changed();
        }
        foreground_changed
    }

    fn release_client_shell_inputs(
        &mut self,
        client_id: ClientId,
        held_inputs: Vec<crate::server::clients::ClientShellHeldInput>,
    ) {
        for held in held_inputs {
            let pane_id = held.target;
            let Some(runtime_pane_id) = self
                .app
                .state()
                .resolve_pane(&pane_id)
                .map(|pane| pane.id())
            else {
                continue;
            };
            let Some(runtime) = self.app.pane_runtime(runtime_pane_id) else {
                continue;
            };
            // The client is gone, so there is nobody to show a failure to.
            let result = apply_client_pane_input_events(runtime, &[held.release]);
            if let Err(err) = result {
                shepr_platform::structured_log!(WARN, event = client.teardown_release, outcome = "error", ?client_id, error = %err, "client shell teardown release failed");
            }
        }
    }

    /// Logs a shell client's failed pane input and, when input was dropped
    /// because the pane's PTY queue is full, tells that client. Losing
    /// keystrokes or a paste silently is worse than a visible error: the user
    /// would otherwise keep typing into a pane that is not reading.
    fn report_client_shell_input_failures(
        &mut self,
        client_id: ClientId,
        pane_id: &shepr_protocol::PublicPaneId,
        failures: &crate::server::pane_input::PaneInputFailures,
    ) {
        shepr_platform::structured_log!(WARN, event = client.input, outcome = "error", ?client_id, public_pane_id = %pane_id, error = %failures, "targeted client shell input failed");
        let dropped = failures.dropped_for_backpressure();
        if dropped == 0 {
            return;
        }
        self.send_to_client(
            client_id,
            &ServerMessage::ClientShellError {
                kind: shepr_protocol::NoticeKind::PaneInputDropped {
                    pane_id: *pane_id,
                    events: dropped,
                },
            },
        );
    }

    fn remove_client_if_present(&mut self, client_id: ClientId) -> bool {
        // Reader-side exits arrive as events, ordered after that client's
        // input. Closing an outbox makes its reader report EOF too, so ignore
        // a detach or disconnect for a client already removed (by the reap).
        if !self.clients.contains_key(&client_id) {
            return false;
        }
        self.remove_client(client_id);
        true
    }

    /// Removes every client whose outbox closed, the one place a transport-side
    /// close (overflow, failed write, failed pong, server-side close) becomes a
    /// registry change. A running server then hands the departed clients'
    /// geometry on and marks the view changed; a stopping one only removes.
    /// Returns whether any client was removed.
    fn reap_closed_clients(&mut self) -> bool {
        let closed = self
            .clients
            .iter()
            .filter_map(|(&id, client)| client.outbox.is_closed().then_some(id))
            .collect::<Vec<_>>();
        if closed.is_empty() {
            return false;
        }
        let mut departures = Vec::with_capacity(closed.len());
        for client_id in closed {
            shepr_platform::structured_log!(
                INFO,
                event = client.connection,
                outcome = "closed",
                ?client_id,
                "client connection closed"
            );
            if let Some(departure) = self.clients.remove_client(client_id) {
                departures.push((client_id, departure));
            }
        }
        self.apply_client_departures(departures);
        true
    }

    /// Drains server events from the dedicated channel.
    fn drain_server_events(&mut self) {
        for _ in 0..SERVER_EVENT_DRAIN_LIMIT {
            // Recheck before each dequeue so a stop during this batch leaves
            // later events for shutdown settlement.
            if self.lifecycle.stop_requested() {
                break;
            }
            let Ok(ev) = self.server_event_rx.try_recv() else {
                break;
            };
            self.handle_server_event(ev);
        }
    }

    /// Closes the server event channel and settles what is left in it: a
    /// client that connected too late gets its shutdown notice, and an
    /// endpoint command still queued gets the shutdown refusal rather than
    /// being left to its client's command timeout. This runs before connected
    /// clients receive their notice, so queued refusals stay readable.
    async fn reject_late_client_connections(&mut self) {
        let mut unregistered_clients = std::mem::take(&mut self.shutdown_unregistered_clients);
        self.server_event_rx.close();
        while let Some(event) = self.server_event_rx.recv().await {
            match event {
                ServerEvent::ShellConnected {
                    client_id, outbox, ..
                } => {
                    unregistered_clients.insert(client_id, outbox);
                }
                ServerEvent::ShellEndpointRequest {
                    client_id,
                    boot_id,
                    request_id,
                    ..
                } => {
                    if self.clients.contains_key(&client_id) {
                        self.reject_endpoint_request_for_shutdown(client_id, boot_id, request_id);
                    } else if let Some(outbox) = unregistered_clients.get(&client_id) {
                        self.reject_unregistered_endpoint_request_for_shutdown(
                            client_id, outbox, boot_id, request_id,
                        );
                    }
                }
                _ => {}
            }
        }
        for (client_id, outbox) in unregistered_clients {
            self.send_shutdown_to_unregistered_client(client_id, &outbox);
        }
    }

    fn reject_unregistered_endpoint_request_for_shutdown(
        &mut self,
        client_id: ClientId,
        outbox: &ClientOutbox,
        boot_id: shepr_protocol::BootId,
        request_id: shepr_protocol::RequestId,
    ) {
        let response = crate::server::client_commands::error_message(
            boot_id,
            request_id,
            shepr_protocol::command::EndpointError::ShuttingDown,
        );
        if outbox.send(&response) == Delivery::Closed {
            debug!(?client_id, "late client left before its endpoint refusal");
        } else {
            self.shutdown_flushes.push(outbox.flush_barrier());
        }
    }

    fn send_shutdown_to_unregistered_client(&mut self, client_id: ClientId, outbox: &ClientOutbox) {
        if outbox.send(&ServerMessage::server_shutdown()) == Delivery::Closed {
            debug!(?client_id, "late client left before its shutdown notice");
        } else {
            self.shutdown_flushes.push(outbox.flush_barrier());
        }
    }

    /// Pulls the titles of the panes the render signal still holds as dirty,
    /// without taking the request, so an API request or endpoint command reads
    /// current agent metadata. Returns whether any title changed (the shell
    /// projection is then dirty); the caller folds that into its own change.
    fn sync_pending_terminal_titles(&mut self) -> bool {
        let sources = self.outputs.render().pending_terminal_title_sources();
        let changes = self.app.sync_terminal_titles(&sources);
        changes.raw_changed || changes.stripped_changed
    }

    /// Pulls only titles reported dirty by the PTY parser. Returns whether any
    /// title changed: a changed title updates the shell agent metadata, so it
    /// requires a projection.
    fn sync_terminal_title_sources(
        &mut self,
        sources: &HashSet<shepr_core::layout::PaneId>,
    ) -> bool {
        let changes = self.app.sync_terminal_titles(sources);
        changes.raw_changed || changes.stripped_changed
    }

    /// Sends a message to all connected clients.
    /// Failed deliveries close the outbox for the next loop iteration to reap.
    ///
    /// Each client gets its own copy of the framed bytes. That is deliberate:
    /// the only caller is `complete_shutdown`, once per server lifetime.
    /// Render output never goes through here; each client's frame or patch is
    /// diffed against that client's own baseline (`render_pass`), so there is no shared frame
    /// to hand out. Making the writer queue carry `Arc<[u8]>` would add a
    /// refcount to every per-client render send to save one tiny copy here.
    fn send_to_all_clients(&mut self, msg: &ServerMessage) {
        for (_, client) in &self.clients {
            client.outbox.send(msg);
        }
    }

    /// Sends a client-local side effect to the foreground client only.
    fn send_to_foreground_client(&mut self, msg: &ServerMessage) -> bool {
        let Some(client_id) = self.clients.foreground_client_id() else {
            return false;
        };
        self.send_to_client(client_id, msg)
    }

    /// Sends a message to a specific client. Returns false if the client
    /// was not found or its outbox could not accept the message.
    fn send_to_client(&mut self, client_id: ClientId, msg: &ServerMessage) -> bool {
        self.clients
            .get(&client_id)
            .map(|client| &client.outbox)
            .is_some_and(|outbox| outbox.send(msg) == Delivery::Queued)
    }

    /// Holds replies until their command's projection has reached the control lane.
    fn queue_endpoint_reply(&mut self, client_id: ClientId, message: &ServerMessage) {
        if let Some(outbox) = self
            .clients
            .get_mut(&client_id)
            .map(|client| &mut client.outbox)
        {
            outbox.hold_reply(message);
        }
    }

    /// Moves every client's ready held replies onto its control lane. Called
    /// after a render pass (which projected every stale or moved client, so a
    /// reply follows the snapshot its command changed on the control FIFO)
    /// and when no pass is owed. Surface frames use the independent slot and
    /// can reach the socket after the reply. A client whose outbox closes
    /// here is left for the reap.
    fn release_endpoint_replies(&mut self, mode: ReleaseMode) {
        for (_, client) in &mut self.clients {
            client.outbox.release_replies(mode);
        }
    }

    /// Handles a server event, then reports any change in which panes hold
    /// terminal focus. Each arm records shared or client-local view changes.
    fn handle_server_event(&mut self, ev: ServerEvent) {
        if !self.begin_request_dispatch() {
            match ev {
                ServerEvent::ShellConnected {
                    client_id, outbox, ..
                } => {
                    self.shutdown_unregistered_clients.insert(client_id, outbox);
                }
                ServerEvent::ShellEndpointRequest {
                    client_id,
                    boot_id,
                    request_id,
                    ..
                } => {
                    self.reject_endpoint_request_for_shutdown(client_id, boot_id, request_id);
                }
                _ => {}
            }
            return;
        }
        if matches!(
            &ev,
            ServerEvent::Detached { client_id }
                | ServerEvent::Disconnected { client_id }
                if !self.clients.contains_key(client_id)
        ) {
            return;
        }
        // Endpoint commands and client departures settle pane focus in their
        // own shared effect path; only an outer focus report needs this step.
        // Failed sends close the outbox and the next reap handles departure.
        let may_move_focus = matches!(ev, ServerEvent::ShellFocus { .. });
        self.apply_server_event(ev);
        if may_move_focus {
            self.sync_pane_focus();
        }
    }

    fn apply_server_event(&mut self, ev: ServerEvent) {
        match ev {
            ServerEvent::ShellConnected {
                client_id,
                geometry,
                mouse_capture,
                surface_active,
                outbox,
            } => {
                shepr_platform::structured_log!(
                    INFO, event = client.connection, outcome = "connected",
                    ?client_id,
                    cols = geometry.cols(),
                    rows = geometry.rows(),
                    cell = ?geometry.cell(),
                    surface_active,
                    "client connected"
                );
                let first_app_client = self.app_client_count() == 0;
                let last_activity = self.clients.allocate_activity_stamp();
                let mut connection = ClientConnection::with_shell(
                    ClientShellState::with_surface_active(surface_active),
                    geometry.grid(),
                    geometry.cell(),
                    last_activity,
                    outbox,
                );
                let shell = &mut connection.shell;
                shell.mouse_capture = mouse_capture;
                shell.projection_revision = shepr_protocol::ProjectionRevision::FIRST;
                // The location is initialised before anything is projected: a
                // new client starts where the session's bookmark is.
                shell.location = self.initial_client_location();
                self.clients.insert(client_id, connection);
                // A known connection with an empty session can create the
                // workspace it will view. Either way the locations are settled
                // once more: a bookmark-less session leaves the new client
                // viewing nothing until the reconcile lands it.
                if self.create_automatic_workspace(Some(client_id)) {
                    self.mark_view_changed();
                }
                self.reconcile_client_shell_locations();
                self.refresh_client_view_keys();
                let Some((location, projection_revision)) =
                    self.clients.get(&client_id).map(|client| {
                        (
                            client.shell_state().location.clone(),
                            client.shell_state().projection_revision,
                        )
                    })
                else {
                    return;
                };
                self.refresh_stale_shell_session_cache();
                let Some(session_cache) = self.shell_session_cache.as_ref() else {
                    shepr_platform::structured_log!(
                        WARN,
                        event = client.session_cache,
                        outcome = "missing",
                        ?client_id,
                        "shell session cache missing while seeding client"
                    );
                    self.remove_client(client_id);
                    return;
                };
                let seed_snapshot = Self::snapshot_from_session(
                    &self.app,
                    &session_cache.session,
                    &self.client_shell_boot_id,
                    projection_revision,
                    &location,
                );
                let snapshot_message = shepr_protocol::endpoint::snapshot_message(&seed_snapshot);
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return;
                };
                let shell = client.shell_state_mut();
                shell.projected_location_generation = shell.location.generation();
                shell.snapshot = Some(seed_snapshot);
                shell.session_generation = self.shell_session_generation;
                self.send_to_client(client_id, &snapshot_message);
                if surface_active {
                    self.clients.promote_to_foreground(client_id);
                    self.sync_host_theme_from_foreground();
                }
                if first_app_client {
                    self.app.mark_git_status_refresh_due(self.app.clock().now);
                }
                // A second surface changes no workspace's size: controlled
                // workspaces keep their controller and uncontrolled ones keep
                // theirs.
                if self.claim_client_geometry(client_id, client_views::GeometryClaimReason::Connect)
                {
                    self.mark_view_changed();
                }
            }
            ServerEvent::PasteRejected { client_id, size } => {
                // Every rejection is a separate user action, so each one is
                // reported.
                self.send_to_client(
                    client_id,
                    &ServerMessage::ClientShellError {
                        kind: shepr_protocol::NoticeKind::LimitExceeded(
                            shepr_protocol::LimitExceeded::new(
                                shepr_protocol::Limit::new(
                                    shepr_protocol::LimitKind::InputPayloadBytes,
                                    shepr_protocol::MAX_INPUT_PAYLOAD,
                                ),
                                size,
                            ),
                        ),
                    },
                );
            }
            ServerEvent::ShellResize {
                client_id,
                geometry,
            } => {
                let active = {
                    let Some(client) = self.clients.get_mut(&client_id) else {
                        return;
                    };
                    let previous_geometry = (client.terminal_size, client.host_cell);
                    client.terminal_size = geometry.grid();
                    client.host_cell = client.host_cell.refreshed_by(geometry.cell());
                    if previous_geometry == (client.terminal_size, client.host_cell) {
                        return;
                    }
                    let active = client.is_active_shell_client();
                    if active {
                        client.request_repaint();
                    }
                    active
                };
                self.refresh_client_view_keys();
                if !active {
                    return;
                }
                // A resize reports view geometry, not user activity. Window
                // layout and font changes must not switch the host theme or
                // pane-less clipboard destination.
                // Geometry settlement invalidates the affected workspace's
                // viewers; a resize must not advance unrelated clients' epoch.
                // A host window or sidebar drag sends a run of these, so the
                // panes' PTYs follow once the size settles.
                self.resize_shell_workspaces_sized_for(
                    client_id,
                    client_views::PendingResumes::Start,
                    crate::app::PaneResizeTiming::Settled,
                );
            }
            ServerEvent::ShellHostTheme { client_id, update } => {
                let is_foreground = self.clients.foreground_client_id() == Some(client_id);
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return;
                };
                if !client.update_host_theme(&update) {
                    return;
                }
                if !client.presents_surface() || !is_foreground {
                    return;
                }
                // Pane colours changed under every surface. The epoch alone
                // sends every client through a full pass, which redraws the
                // panes from their cores (the new defaults included) and
                // diffs the result against its baseline; a forced recompute
                // would add nothing.
                self.sync_host_theme_from_foreground();
            }
            ServerEvent::ShellFocus { client_id, focused } => {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return;
                };
                if !client.presents_surface()
                    || client.shell_state().outer_terminal_focus
                        == crate::server::clients::OuterFocus::reported(focused)
                {
                    return;
                }
                // Recorded on this connection only; the panes it views learn
                // of it through `sync_pane_focus` once the event is applied.
                client.shell_state_mut().outer_terminal_focus =
                    crate::server::clients::OuterFocus::reported(focused);
                if focused {
                    self.promote_client_to_foreground(client_id);
                    if self
                        .claim_client_geometry(client_id, client_views::GeometryClaimReason::Focus)
                    {
                        self.mark_view_changed();
                    }
                }
            }
            ServerEvent::ShellReplayHostEffects { client_id } => {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return;
                };
                if !client.presents_surface() {
                    return;
                }
                client.outbox.forget_presentation();
                self.stream_host_mouse_capture_mode();
                self.stream_shell_keyboard_mode();
            }
            ServerEvent::ShellPaneInput {
                client_id,
                pane_id,
                events,
            } => {
                if !self
                    .clients
                    .get(&client_id)
                    .is_some_and(ClientConnection::presents_surface)
                {
                    return;
                }
                let Some(host_cell) = self.clients.get(&client_id).map(|client| client.host_cell)
                else {
                    return;
                };
                let mut events = events;
                let Some((workspace_id, runtime_pane_id)) = self
                    .app
                    .state()
                    .resolve_pane(&pane_id)
                    .map(|pane| (pane.workspace().id(), pane.id()))
                else {
                    return;
                };
                let Some(runtime) = self.app.pane_runtime(runtime_pane_id) else {
                    return;
                };
                super::pane_input::admit_pixel_reports(
                    &mut events,
                    host_cell,
                    runtime.read().pixel_mouse(),
                );
                if !self.shell_client_views_pane(client_id, &workspace_id, runtime_pane_id) {
                    let Some(runtime) = self.app.pane_runtime(runtime_pane_id) else {
                        return;
                    };
                    let releases = events
                        .into_iter()
                        .filter(client_pane_input_releases_press)
                        .collect::<Vec<_>>();
                    if releases.is_empty() {
                        return;
                    }
                    if let Some(client) = self.clients.get_mut(&client_id) {
                        client.track_shell_input(&pane_id, &releases);
                    }
                    let result = apply_client_pane_input_events(runtime, &releases);
                    let scrolled = match &result {
                        Ok(change) => change.is_changed(),
                        Err(failures) => failures.surface_change().is_changed(),
                    };
                    if let Err(failures) = result {
                        self.report_client_shell_input_failures(client_id, &pane_id, &failures);
                    }
                    if scrolled {
                        self.invalidate_pane_viewers(runtime_pane_id);
                    }
                    return;
                }
                let interaction = client_pane_input_has_interaction(&events);
                if let Some(client) = self.clients.get_mut(&client_id) {
                    client.track_shell_input(&pane_id, &events);
                }
                if interaction {
                    self.promote_client_to_foreground(client_id);
                }
                let geometry_changed = interaction
                    && self.claim_client_geometry(
                        client_id,
                        client_views::GeometryClaimReason::Interaction,
                    );
                if geometry_changed {
                    self.mark_view_changed();
                }
                let Some(runtime) = self.app.pane_runtime(runtime_pane_id) else {
                    return;
                };
                let result = apply_client_pane_input_events(runtime, &events);
                let scrolled = match &result {
                    Ok(change) => change.is_changed(),
                    Err(failures) => failures.surface_change().is_changed(),
                };
                if let Err(failures) = result {
                    self.report_client_shell_input_failures(client_id, &pane_id, &failures);
                }
                if scrolled {
                    self.invalidate_pane_viewers(runtime_pane_id);
                }
            }
            ServerEvent::ShellEndpointRequest {
                client_id,
                boot_id,
                request_id,
                command,
            } => {
                self.handle_client_shell_endpoint_request(client_id, boot_id, request_id, *command);
            }
            ServerEvent::Detached { client_id } => {
                if !self.remove_client_if_present(client_id) {
                    return;
                }
                shepr_platform::structured_log!(
                    INFO,
                    event = client.connection,
                    outcome = "detached",
                    ?client_id,
                    "client detached"
                );
            }
            ServerEvent::Disconnected { client_id } => {
                if !self.remove_client_if_present(client_id) {
                    return;
                }
                shepr_platform::structured_log!(
                    INFO,
                    event = client.connection,
                    outcome = "disconnected",
                    ?client_id,
                    "client disconnected"
                );
            }
        }
    }

    fn dispatch_api_request(&mut self, msg: shepr_api::ApiRequestMessage) -> bool {
        let request_id = msg.request.id.clone();
        let method = msg.request.method.traits().name;

        let mut changed = self.drain_all_internal_events_with_forwarding();
        changed |= self.sync_pending_terminal_titles();

        let outcome = self.app.handle_api_request_with_render(msg.request);
        changed |= outcome.view_changed;
        shepr_api::send_api_response(&msg.respond_to, &request_id, method, outcome.response);

        changed |= self.create_automatic_workspace(None);

        changed
    }

    /// Services Git refresh, session saves and pending pane resizes.
    fn handle_scheduled_tasks_headless(&mut self, now: Instant) -> bool {
        let mut changed = false;

        if self.has_app_client() {
            self.app.start_git_status_refresh_if_due(now);
        }

        self.app.service_session_saves(now);
        // Reaping can make the warning checkpoint ready without another wake.
        // Claim it before sleeping or replaying checkpointed pane deaths.
        self.lifecycle.sync_host_shutdown_freeze(&mut self.app);

        // Resized panes have only their viewers recompute (as a geometry
        // settlement does), so this does not count as a shared view change.
        self.apply_due_pane_resizes();

        let mut synced_host_shutdown_for_exits = false;
        for _ in 0..self.pending_checkpointed_pane_exits.len() {
            let Some(pending) = self.pending_checkpointed_pane_exits.pop_front() else {
                break;
            };
            if self
                .app
                .pane_exit_checkpoint_generation_settled(pending.checkpoint_generation)
            {
                if !synced_host_shutdown_for_exits {
                    // Check immediately before replaying this separate
                    // internal-event batch, after the main queue was drained.
                    self.lifecycle.sync_host_shutdown_freeze(&mut self.app);
                    synced_host_shutdown_for_exits = true;
                }
                changed |= self.replay_checkpointed_internal_event(
                    pending.event,
                    pending.checkpoint_generation,
                );
            } else {
                self.pending_checkpointed_pane_exits.push_back(pending);
            }
        }

        // The resume schedule derives its own wakeup and keeps its theme wait
        // across passes, so running this on every iteration (a pane printing
        // keeps one busy) cannot postpone the first restored agent.
        changed | self.finish_pending_agent_resume_pass(now)
    }
}

fn client_pane_input_releases_press(event: &shepr_protocol::ClientPaneInputEvent) -> bool {
    matches!(
        event,
        shepr_protocol::ClientPaneInputEvent::Key {
            kind: shepr_protocol::ClientKeyKind::Release,
            ..
        } | shepr_protocol::ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Up(_),
            ..
        }
    )
}

fn client_pane_input_has_interaction(events: &[shepr_protocol::ClientPaneInputEvent]) -> bool {
    events
        .iter()
        .any(|event| !client_pane_input_releases_press(event))
}

impl Drop for HeadlessServer {
    fn drop(&mut self) {
        // The lease before the socket, as on a clean exit; see
        // `release_socket_after_save`. Without this the socket went first
        // and the lease with the fields dropped after this body.
        self.release_socket_after_save();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Installs the SIGINT/SIGTERM/SIGHUP handler (ctrlc's `termination`
/// feature). It marks the quit as signal-driven and requests the stop, which
/// also wakes the event loop.
///
/// Failing to install it is an error: without it a signal kills the server
/// without the shutdown sequence that saves the session.
fn ctrlc_handler(
    stop_signal: Arc<shepr_api::ServerStopSignal>,
    signal_quit: Arc<std::sync::OnceLock<std::time::Instant>>,
) -> io::Result<()> {
    ctrlc::set_handler(move || {
        // Before the stop request, so the loop never sees the quit without it.
        // The first signal's own time: the final save compares agent exits
        // with it, and the loop may notice the signal much later. ctrlc runs
        // this on its own thread, not in signal context.
        // headless-clock-sample-ok: the moment the signal arrived, on the
        // handler's thread, not a loop iteration's sample.
        signal_quit.set(Instant::now()).ok();
        stop_signal.request();
    })
    .map_err(|err| io::Error::other(format!("installing the termination signal handler: {err}")))
}

/// Sleep until a deadline, or return pending if none.
async fn sleep_until_or_pending(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
impl HeadlessServer {
    fn render_now(&mut self) {
        self.mark_view_changed();
        let plan = self.render_plan(false);
        self.render_pass(&plan, &HashSet::new());
    }
    fn try_render_patches(&mut self, sources: &HashSet<shepr_core::layout::PaneId>) -> bool {
        if self.render_plan(true).has_full() {
            return false;
        }
        let ids = render_targets(&self.clients)
            .filter(|target| {
                self.clients
                    .get(&target.client_id)
                    .is_some_and(ClientConnection::is_active_shell_client)
            })
            .map(|target| target.client_id)
            .collect::<Vec<_>>();
        let outcome = self.render_patches(&ids, sources);
        outcome.promote.is_empty()
    }
}
