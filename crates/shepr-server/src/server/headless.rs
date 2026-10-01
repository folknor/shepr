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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use ratatui::layout::Rect;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use base64::Engine;

use crate::app;
use crate::limits::SERVER_EVENT_CHANNEL_CAPACITY;
use crate::server::client_shell::render_pane_surface as render_client_shell_pane_surface;
use crate::server::client_transport::ServerEvent;
use crate::server::clients::{ClientConnection, ClientRegistry, ClientShellState, render_targets};
use crate::server::outbox::{ClientOutbox, Delivery, ReleaseMode, ReplyTicket};
use crate::server::pane_input::apply_client_pane_input_events;
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
mod surface_interest;
mod worker;

pub use bootstrap::{RunServerError, ServerReady, run_server};
use lifecycle::{ShutdownLifecycle, ShutdownPhase};

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
    WorkerCompletion(worker::WorkerCompletion),
}

struct PendingCheckpointedPaneExit {
    event: shepr_mux::events::AppEvent,
    checkpoint_generation: u64,
}

// ---------------------------------------------------------------------------
// Headless server
// ---------------------------------------------------------------------------

/// The headless server - runs the shepr event loop without a real terminal.
pub struct HeadlessServer {
    app: app::App,
    view_epoch: ViewEpoch,
    headless_settled: ViewEpoch,
    /// Kept alive only for its `Drop` impl, which tears down the server socket listener.
    _api_server: Option<shepr_api::ServerHandle>,
    clients: ClientRegistry,
    /// Process-local identity used to reject shell replacements from an earlier server boot.
    client_shell_boot_id: shepr_protocol::BootId,
    /// Shared session source for shell projections; `None` until a shell
    /// connection or render needs it.
    shell_session_cache: Option<render::ShellSessionCache>,
    /// Moves whenever shell projections must be recomputed: the cache was
    /// rebuilt for a new application revision, or the cwd timer found a
    /// projection that changed. Each shell client records the generation it
    /// last projected.
    shell_session_generation: u64,
    /// Panes last told they hold terminal focus (`sync_pane_focus`), derived
    /// from the clients' views; the record of what the panes were sent, not a
    /// view of its own.
    focused_panes: HashSet<(shepr_protocol::WorkspaceId, shepr_core::layout::PaneId)>,
    /// Whether the set of panes whose PTY output should wake the loop at once
    /// (`sync_immediate_pty_sources`) may be stale. That set depends only on
    /// the clients' workspace views, pane membership, and focused pane when a
    /// workspace is zoomed. A PTY render wake changes none of those.
    /// Recomputing it on every loop wake walked every pane per PTY notify. A
    /// missed mark would only delay a visible pane's repaint to the normal
    /// render cadence, never drop it: visibility at render time is computed
    /// fresh.
    immediate_pty_sources_dirty: bool,
    /// Whether the host mouse-capture and keyboard modes pushed to clients
    /// (`stream_host_mouse_capture_mode`, `stream_shell_keyboard_mode`)
    /// may be stale. They follow the focused pane's terminal modes, which only
    /// PTY output changes, plus the same client/topology changes as above. Set
    /// whenever a render request carrying PTY sources is taken; every render
    /// is followed by another loop iteration, which pushes the modes before
    /// the loop sleeps again.
    host_input_modes_dirty: bool,
    /// Reason captured by the retained renderer and reported after the full
    /// render that recovers from it.
    retained_surface_fallback_reason: Option<&'static str>,
    /// Fallback reasons already reported once, so each report can say whether
    /// its reason is recurring. Every fallback is still logged.
    retained_surface_fallbacks_reported: HashSet<&'static str>,
    /// Owns running, host-shutdown warning/freeze, cancellation and stopping.
    lifecycle: ShutdownLifecycle,
    /// Watches logind for shutdown warnings; `None` until `run` starts it.
    /// Releasing a shutdown delay inhibitor leaves the monitor installed so it
    /// can observe a cancellation and inhibit the next shutdown.
    host_shutdown_monitor: Option<lifecycle::HostShutdownMonitor>,
    /// Channel for receiving server events from client connection threads.
    server_event_rx: mpsc::Receiver<ServerEvent>,
    /// Sender for server events (cloned for each client thread).
    server_event_tx: mpsc::Sender<ServerEvent>,
    /// Bounded requests received by the JSON API listener.
    api_request_rx: mpsc::Receiver<shepr_api::ApiRequestMessage>,
    /// Outboxes of new clients dequeued after stopping, held until their
    /// queued commands receive refusals before the shutdown notice is sent.
    shutdown_unregistered_clients: HashMap<ClientId, ClientOutbox>,
    /// Acknowledgements for shutdown frames queued to client writer threads.
    shutdown_flushes: Vec<tokio::sync::oneshot::Receiver<()>>,
    /// Pane exits held until their pre-removal session checkpoint reaches disk.
    pending_checkpointed_pane_exits: VecDeque<PendingCheckpointedPaneExit>,
    /// Set only while a ready held exit is routed back through the forwarding
    /// handler, which then skips its initial App preparation step.
    replaying_checkpointed_pane_exit: Option<u64>,
    /// Raised by a client outbox when it closes, and when a control lane a
    /// held reply waits on makes room; wakes an idle loop to reap or release.
    outbox_wake: Arc<tokio::sync::Notify>,
    worker_tx: tokio::sync::mpsc::UnboundedSender<worker::WorkerCompletion>,
    worker_rx: tokio::sync::mpsc::UnboundedReceiver<worker::WorkerCompletion>,
    checkout_root_runner: worker::CheckoutRootRunner,
    resume_cwd_checks_in_flight: HashSet<(shepr_protocol::TerminalId, PathBuf)>,
}

impl HeadlessServer {
    /// Creates the server once panes are restored: builds the server event
    /// channel and, given the socket's handle (tests pass `None`), opens its
    /// TUI gate with the client transport handler. From then on `ping`
    /// answers without `starting` and TUI connections are served.
    pub(super) fn new(
        app: app::App,
        api_request_rx: mpsc::Receiver<shepr_api::ApiRequestMessage>,
        api_server: Option<shepr_api::ServerHandle>,
        stop_requested: Arc<shepr_api::ServerStopSignal>,
    ) -> Self {
        // Channel for server events from client threads.
        let (server_event_tx, server_event_rx) = mpsc::channel(SERVER_EVENT_CHANNEL_CAPACITY);

        let (worker_tx, worker_rx) = worker::channel();
        let outbox_wake = Arc::new(tokio::sync::Notify::new());

        if let Some(api) = &api_server {
            api.client_gate().open(Arc::new(
                crate::server::client_transport::ClientTransportHandler {
                    server_event_tx: server_event_tx.clone(),
                    should_quit: Arc::clone(&stop_requested),
                    wake: Arc::clone(&outbox_wake),
                    ids: crate::server::clients::ClientIdAllocator::default(),
                },
            ));
        }
        Self {
            app,
            view_epoch: ViewEpoch::INITIAL,
            headless_settled: ViewEpoch::ZERO,
            _api_server: api_server,
            clients: ClientRegistry::default(),
            client_shell_boot_id: shepr_protocol::BootId::for_this_process(),
            shell_session_cache: None,
            shell_session_generation: 0,
            focused_panes: HashSet::new(),
            immediate_pty_sources_dirty: true,
            host_input_modes_dirty: true,
            retained_surface_fallback_reason: None,
            retained_surface_fallbacks_reported: HashSet::new(),
            lifecycle: ShutdownLifecycle::new(stop_requested),
            host_shutdown_monitor: None,
            server_event_rx,
            server_event_tx,
            api_request_rx,
            shutdown_unregistered_clients: HashMap::new(),
            shutdown_flushes: Vec::new(),
            pending_checkpointed_pane_exits: VecDeque::new(),
            replaying_checkpointed_pane_exit: None,
            outbox_wake,
            worker_tx,
            worker_rx,
            checkout_root_runner: worker::default_checkout_root_runner(),
            resume_cwd_checks_in_flight: HashSet::new(),
        }
    }

    fn mark_view_changed(&mut self) {
        self.view_epoch.advance();
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
    pub async fn run(&mut self) -> io::Result<()> {
        crate::logging::startup("server");
        // The fallible setup below returns before the loop, so it skips the
        // final save; `Drop` still releases the lease and socket in order. No
        // save is owed: no client has connected and no event has been applied,
        // so nothing has changed since bootstrap left the session on disk.
        // Every failure inside the loop goes through `initiate_shutdown` and
        // the save after it.
        // Register SIGINT handler for graceful shutdown.
        let stop_requested = Arc::clone(self.lifecycle.stop_signal());
        let signal_quit = Arc::clone(self.lifecycle.signal_quit_request_flag());
        ctrlc_handler(stop_requested, signal_quit)?;
        self.start_host_shutdown_monitor();

        let mut run_error = None;
        loop {
            // If shutdown has been initiated, complete it and exit.
            if self.lifecycle.phase() == ShutdownPhase::Stopping {
                // Finalize any reply still in the outbox before waiting for
                // client flushes. Replies held when shutdown began were
                // already queued ahead of the shutdown notice.
                self.resolve_pending_endpoint_replies_for_shutdown();
                self.release_endpoint_replies(ReleaseMode::Shutdown);
                if let Err(err) = self.complete_shutdown().await {
                    run_error.get_or_insert(err);
                }
                break;
            }

            self.reap_closed_clients();

            // Every pass through the loop starts with a fresh clock sample, so
            // the event and API handlers below read this iteration's time.
            let iteration_start = self.refresh_app_clock();

            // A host shutdown warning checkpoints the session and freezes
            // saving; it does not stop the server (see `sync_host_shutdown_freeze`).
            self.sync_host_shutdown_freeze(iteration_start);

            // Check if we should start shutting down. The drain applies queued
            // state and agent-session reports so the final save carries them;
            // after a signal it leaves pane deaths out (see
            // `signal_quit_requested`).
            if self.lifecycle.stop_requested(self.app.state.should_quit) {
                self.drain_internal_events_with_forwarding_up_to(
                    crate::app::APP_EVENT_CHANNEL_CAPACITY,
                );
                self.initiate_shutdown();
                continue;
            }

            // 2. Drain a bounded internal-event batch. API handlers perform an
            // exhaustive forwarding-aware drain before reading pane/runtime state.
            if self.drain_internal_events_with_forwarding() {
                self.mark_view_changed();
            }
            if self.lifecycle.stop_requested(self.app.state.should_quit) {
                continue;
            }
            self.refresh_app_clock();

            // 3. Drain API requests.
            if self.drain_api_requests_with_shutdown_check() {
                self.mark_view_changed();
            }
            if self.lifecycle.stop_requested(self.app.state.should_quit) {
                continue;
            }

            self.app.sync_session_save_schedule();

            // 4. Drain server events from client threads.
            self.drain_server_events();
            if self.lifecycle.stop_requested(self.app.state.should_quit) {
                continue;
            }

            // 5. Handle scheduled tasks.
            let now = self.refresh_app_clock();
            if self.handle_scheduled_tasks_headless(now) {
                self.app.state.mark_shell_projection_dirty();
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
            let render_signal_pending = self.app.render_dirty.is_pending();
            let plan = self.render_plan(render_signal_pending);
            let render_cadence_due = self.app.can_render_now(now);
            if (plan.has_full() || render_signal_pending)
                && (render_cadence_due
                    || (self.app.can_present_now(now)
                        && (plan.has_full() || self.app.render_dirty.has_immediate_work())))
            {
                let planned_at = self.view_epoch;
                let request = self.app.render_dirty.take();
                let pty_dirty = !request.pty_sources.is_empty();
                if pty_dirty {
                    self.host_input_modes_dirty = true;
                }
                if request.generic {
                    self.mark_view_changed();
                }
                let (sidebar_changed, title_synced) =
                    self.sync_terminal_title_sources(&request.terminal_title_sources);
                if sidebar_changed {
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
                if plan.has_full() && !title_synced {
                    self.sync_window_title();
                }
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
                self.app.record_render_attempt(now, !hidden_only);
                self.release_endpoint_replies(ReleaseMode::WithinBudget);
                continue;
            }
            // Replies wait for any projection owed by a cadence-held pass.
            if !plan.has_full() && !render_signal_pending {
                self.release_endpoint_replies(ReleaseMode::WithinBudget);
            }
            let next_deadline = self.app.next_headless_loop_deadline_with_git_refresh(
                now,
                plan.has_full() || render_signal_pending,
                self.has_app_client(),
            );
            let next_deadline = self
                .shell_cwd_refresh_deadline()
                .map_or(next_deadline, |cwd| {
                    Some(next_deadline.map_or(cwd, |current| current.min(cwd)))
                });
            let stop_signal = Arc::clone(self.lifecycle.stop_signal());
            // A capped drain leaves queued work in its receiver. The matching
            // receive branch stays ready and starts another pass immediately.
            let event = {
                tokio::select! {
                    // A `server.stop` from the API sets the latch on another
                    // thread; this is what wakes an idle loop to act on it.
                    () = stop_signal.notified() => LoopEvent::Timer,
                    () = self.outbox_wake.notified() => LoopEvent::Timer,
                    maybe_api = self.api_request_rx.recv() => match maybe_api {
                        Some(msg) => LoopEvent::Api(Box::new(msg)),
                        None => LoopEvent::Timer,
                    },
                    maybe_ev = self.app.event_rx.recv() => match maybe_ev {
                        Some(ev) => LoopEvent::Internal(ev),
                        None => LoopEvent::Timer,
                    },
                    maybe_server_ev = self.server_event_rx.recv() => match maybe_server_ev {
                        Some(ev) => LoopEvent::ServerEvent(ev),
                        None => LoopEvent::Timer,
                    },
                    maybe_worker = self.worker_rx.recv() => match maybe_worker {
                        Some(completion) => LoopEvent::WorkerCompletion(completion),
                        None => LoopEvent::Timer,
                    },
                    _ = sleep_until_or_pending(next_deadline) => LoopEvent::Timer,
                    // A save ended: the scheduled tasks reap it and start
                    // whatever save waited for it.
                    () = self.app.session_saver.save_finished().notified() => LoopEvent::Timer,
                    _ = self.app.render_notify.notified() => LoopEvent::Timer,
                }
            };
            // The wait above can last until the next deadline; dispatch reads
            // the time the event arrived, not the time the wait began.
            let event_time = self.refresh_app_clock();

            if self.lifecycle.stop_requested(self.app.state.should_quit) {
                // This request was already dequeued when the stop arrived.
                // Queue its refusal now; shutdown cleanup broadcasts the
                // notice after it settles events still waiting in the channel.
                if let LoopEvent::ServerEvent(ServerEvent::ClientShellEndpointRequest {
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
                        self.handle_internal_event_with_forwarding(ev);
                    }
                    LoopEvent::ServerEvent(ServerEvent::ClientShellConnected {
                        client_id,
                        outbox,
                        ..
                    }) => {
                        self.shutdown_unregistered_clients.insert(client_id, outbox);
                    }
                    // Already dequeued, so the shutdown drain would never see
                    // it; answer it here.
                    LoopEvent::Api(msg) => self.reject_api_request_for_shutdown(&msg),
                    // A worker completion and a client endpoint request land
                    // here: the shutdown flush answers a pending reply with
                    // the shutdown refusal.
                    _ => {}
                }
                continue;
            }

            match event {
                LoopEvent::Timer => {}
                LoopEvent::Internal(ev) => {
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
                LoopEvent::WorkerCompletion(completion) => {
                    if self.handle_worker_completion(completion, event_time) {
                        self.mark_view_changed();
                    }
                }
            }
        }

        // Save session on exit. During a host shutdown saving is frozen
        // (session persistence is suspended), so this writes nothing and the
        // checkpoint taken on the warning stands; the writer is still retired.
        self.refresh_app_clock();
        if self.app.policy.persists_session()
            || self.lifecycle.frozen_session_policy().unwrap_or(false)
        {
            self.app.save_session_before_teardown_async().await;
        }
        self.app.terminal_runtimes.clear();
        if !self
            .app
            .wait_for_pane_teardowns(crate::limits::PANE_TEARDOWN_WAIT)
        {
            warn!("pane session teardown did not finish before server exit");
        }
        // The save and the teardown wait can each take seconds.
        self.refresh_app_clock();
        self.release_socket_after_save();

        // A successor can start once the lease is free, while this process
        // still logs to the same server log until it exits. That is safe: the
        // log writer is built for several processes sharing one file (appends
        // under a shared flock, and each record follows another process's
        // rotation), so the two interleave lines and lose none.
        info!("headless server exiting");
        run_error.map_or(Ok(()), Err)
    }

    /// Colours the panes with the foreground client's host theme: panes have
    /// one theme (their default colours and the answers to colour queries),
    /// and the client the user was last active in supplies it. A client with
    /// no host color report yet leaves the current theme (a live client's, or
    /// the one saved with the session) in place. Its appearance still applies
    /// independently. Returns whether anything changed.
    fn sync_host_theme_from_foreground(&mut self) -> bool {
        let Some(shell) = self
            .clients
            .foreground_client_id()
            .and_then(|client_id| self.clients.get(&client_id))
            .map(ClientConnection::shell_state)
        else {
            return false;
        };
        if shell.host_terminal_theme.is_empty() && shell.host_terminal_appearance.is_none() {
            return false;
        }
        let theme = shell.host_terminal_theme;
        let appearance = shell.host_terminal_appearance;
        let appearance_explicit = shell.host_terminal_appearance_explicit;
        let mut changed = self
            .app
            .set_host_terminal_appearance_state(appearance, appearance_explicit);
        if !theme.is_empty() {
            changed |= self.app.set_host_terminal_theme(theme);
        }
        changed
    }

    /// Records activity from `client_id`, making it the foreground client if
    /// it is an active shell. Returns whether the foreground client changed.
    fn promote_client_to_foreground(&mut self, client_id: ClientId) -> bool {
        let changed = self.clients.promote_to_foreground(client_id);
        if changed {
            self.sync_host_theme_from_foreground();
        }
        changed
    }

    fn promote_latest_remaining_client(&mut self) -> bool {
        let changed = self.clients.promote_latest_remaining();
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
        self.immediate_pty_sources_dirty = true;
        let (removed, was_foreground) = self.clients.remove_client(client_id);
        if let Some(mut removed) = removed {
            let held_inputs = removed.drain_shell_held_inputs();
            self.release_client_shell_inputs(client_id, held_inputs);
        }
        // The departed client no longer holds focus on the pane it viewed.
        self.sync_pane_focus();
        if was_foreground {
            self.promote_latest_remaining_client()
        } else {
            false
        }
    }

    fn release_client_shell_inputs(
        &mut self,
        client_id: ClientId,
        held_inputs: Vec<crate::server::clients::ClientShellHeldInput>,
    ) {
        for held in held_inputs {
            let pane_id = held.target;
            let Some((workspace_index, runtime_pane_id)) = self.app.parse_pane_id(&pane_id) else {
                continue;
            };
            let Some(runtime) = self.app.state.runtime_for_pane_in_workspace(
                &self.app.terminal_runtimes,
                workspace_index,
                runtime_pane_id,
            ) else {
                continue;
            };
            // The client is gone, so there is nobody to show a failure to.
            let result = apply_client_pane_input_events(runtime, &[held.release]);
            if let Err(err) = result {
                warn!(?client_id, error = %err, "client shell teardown release failed");
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
        warn!(?client_id, pane_id = %pane_id, error = %failures, "targeted client shell input failed");
        let dropped = failures.dropped_for_backpressure();
        if dropped == 0 {
            return;
        }
        self.send_to_client(
            client_id,
            &ServerMessage::ClientShellError {
                kind: shepr_protocol::NoticeKind::PaneInputDropped {
                    pane_id: pane_id.clone(),
                    events: dropped,
                },
            },
        );
    }

    fn remove_client_and_resize_if_needed(&mut self, client_id: ClientId) -> bool {
        // Reader-side exits arrive as events, ordered after that client's
        // input. Closing an outbox makes its reader report EOF too, so ignore
        // a detach or disconnect for a client already removed (by the reap).
        if !self.clients.contains_key(&client_id) {
            return false;
        }
        self.remove_client(client_id);
        // Removing the client dropped its geometry controller mappings. Each
        // workspace it controlled goes to a remaining viewer, or every
        // workspace to the headless size when no surface remains, so no pane
        // keeps the departed client's size.
        if self.lifecycle.phase() != ShutdownPhase::Stopping {
            self.reapply_controlled_shell_workspace_geometry(true);
        }
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
        for client_id in closed {
            info!(?client_id, "client connection closed");
            self.remove_client(client_id);
        }
        if self.lifecycle.phase() != ShutdownPhase::Stopping {
            self.reapply_controlled_shell_workspace_geometry(true);
            self.mark_view_changed();
        }
        true
    }

    /// Drains server events from the dedicated channel.
    fn drain_server_events(&mut self) {
        for _ in 0..crate::limits::SERVER_EVENT_DRAIN_LIMIT {
            // Recheck before each dequeue so a stop during this batch leaves
            // later events for shutdown settlement.
            if self.lifecycle.stop_requested(self.app.state.should_quit) {
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
                ServerEvent::ClientShellConnected {
                    client_id, outbox, ..
                } => {
                    unregistered_clients.insert(client_id, outbox);
                }
                ServerEvent::ClientShellEndpointRequest {
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
        if outbox.send(&ServerMessage::ServerShutdown {
            reason: Some(shepr_protocol::ShutdownReason::Message(
                "server is shutting down".to_owned(),
            )),
        }) == Delivery::Closed
        {
            debug!(?client_id, "late client left before its shutdown notice");
        } else {
            self.shutdown_flushes.push(outbox.flush_barrier());
        }
    }

    /// Pulls only titles reported dirty by the PTY parser. A title of a pane
    /// some client has focused is forwarded as that client's window title.
    /// Any changed title also updates the shell agent metadata, so it requires
    /// a projection.
    fn sync_terminal_title_sources(
        &mut self,
        sources: &HashSet<shepr_core::layout::PaneId>,
    ) -> (bool, bool) {
        let focused_source = self
            .window_title_clients()
            .into_iter()
            .filter_map(|client_id| self.shell_focus_target(client_id))
            .any(|target| sources.contains(&target.pane_id));
        let changes = self.app.sync_terminal_titles(sources);
        if changes.raw_changed || changes.stripped_changed {
            self.app.state.mark_shell_projection_dirty();
        }
        let outer_title_synced = focused_source && self.app.window_title_uses_terminal_title();
        if outer_title_synced {
            self.sync_window_title();
        }
        (
            changes.raw_changed || changes.stripped_changed,
            outer_title_synced,
        )
    }

    /// The clients that show a window title: every active shell surface.
    fn window_title_clients(&self) -> Vec<ClientId> {
        let mut clients = self
            .clients
            .iter()
            .filter(|(_, client)| client.is_active_shell_client())
            .map(|(&client_id, _)| client_id)
            .collect::<Vec<_>>();
        clients.sort_unstable();
        clients
    }

    /// Renders `ui.window_title` against `client_id`'s own view. `None` means
    /// window titles are disabled or every token resolved empty, which leaves
    /// the client on Shepr's default title.
    fn configured_window_title(&self, client_id: ClientId) -> Option<String> {
        self.shell_target_for_client(client_id)
            .map_or_else(
                || self.app.window_title_without_workspace(),
                |target| {
                    self.app
                        .state
                        .workspace_index(&target)
                        .and_then(|workspace_index| self.app.window_title_for(workspace_index))
                },
            )
            .and_then(|title| shepr_config::sanitize_window_title_text(&title))
    }

    /// Pushes each client the configured outer window title of its own view
    /// when that changed since it was last delivered. Shepr consumes each
    /// pane's own `OSC 0`/`OSC 2`, so without this the host terminal title
    /// never follows the session - which is what window managers read for tab
    /// and group bar labels.
    fn sync_window_title(&mut self) {
        if !self.app.window_title_configured() {
            return;
        }
        let pending = self
            .window_title_clients()
            .into_iter()
            .map(|client_id| (client_id, self.configured_window_title(client_id)))
            .filter(|(client_id, title)| {
                self.clients
                    .get(client_id)
                    .is_some_and(|client| !client.outbox.window_title_is_current(title))
            })
            .collect::<Vec<_>>();
        for (client_id, title) in pending {
            self.send_window_title(client_id, title);
        }
    }

    /// Sends a client its window title and remembers it only when the client
    /// took it, so a client that did not is written to again rather than
    /// skipped.
    fn send_window_title(&mut self, client_id: ClientId, title: Option<String>) -> bool {
        self.clients
            .get_mut(&client_id)
            .is_some_and(|client| client.outbox.tell_window_title(title) == Delivery::Queued)
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

    fn reserve_endpoint_reply(
        &mut self,
        client_id: ClientId,
        message: &ServerMessage,
    ) -> Option<ReplyTicket> {
        let seq = self
            .clients
            .get_mut(&client_id)?
            .outbox
            .reserve_reply(message)?;
        Some(ReplyTicket { client_id, seq })
    }

    fn complete_endpoint_reply(&mut self, ticket: ReplyTicket, message: &ServerMessage) {
        if let Some(outbox) = self
            .clients
            .get_mut(&ticket.client_id)
            .map(|client| &mut client.outbox)
        {
            outbox.complete_reply(ticket.seq, message);
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

    fn resolve_pending_endpoint_replies_for_shutdown(&mut self) {
        for (_, client) in &mut self.clients {
            client.outbox.resolve_replies_for_shutdown();
        }
    }

    /// Handles a server event, then reports any change in which panes hold
    /// terminal focus. Each arm records shared or client-local view changes.
    fn handle_server_event(&mut self, ev: ServerEvent) {
        // Writer readiness only wakes the loop; its next plan derives surface debt.
        if matches!(ev, ServerEvent::ClientWriterDrained) {
            return;
        }
        if matches!(
            &ev,
            ServerEvent::ClientDetach { client_id }
                | ServerEvent::ClientDisconnected { client_id }
                if !self.clients.contains_key(client_id)
        ) {
            return;
        }
        // Pane input and writer drains, the per-keystroke and per-frame
        // events, move no client's view and no outer focus. Failed sends close
        // the outbox; registry changes wait for the next iteration's reap.
        let may_move_focus = matches!(
            ev,
            ServerEvent::ClientShellConnected { .. }
                | ServerEvent::ClientShellResize { .. }
                | ServerEvent::ClientShellFocus { .. }
                | ServerEvent::ClientShellEndpointRequest { .. }
                | ServerEvent::ClientDetach { .. }
                | ServerEvent::ClientDisconnected { .. }
        );
        self.apply_server_event(ev);
        if may_move_focus {
            self.sync_pane_focus();
        }
    }

    fn apply_server_event(&mut self, ev: ServerEvent) {
        match ev {
            ServerEvent::ClientShellConnected {
                client_id,
                surface_cols,
                surface_rows,
                cell_width_px,
                cell_height_px,
                pixel_mouse,
                mouse_capture,
                surface_active,
                outbox,
            } => {
                info!(
                    ?client_id,
                    cols = surface_cols,
                    rows = surface_rows,
                    cell_width_px,
                    cell_height_px,
                    surface_active,
                    "client connected"
                );
                let first_app_client = self.app_client_count() == 0;
                let last_activity = self.clients.allocate_activity_stamp();
                let observed = shepr_termio::host_term::cell_size::HostCellSize {
                    width_px: cell_width_px,
                    height_px: cell_height_px,
                };
                let mut connection = ClientConnection::with_shell(
                    ClientShellState::active(),
                    shepr_core::geometry::GridSize::clamped(surface_cols, surface_rows),
                    observed,
                    last_activity,
                    outbox,
                );
                connection.pixel_mouse = pixel_mouse && observed.is_known();
                let shell = &mut connection.shell;
                shell.mouse_capture = mouse_capture;
                shell.surface_active = surface_active;
                shell.projection_revision = shepr_protocol::ProjectionRevision::new(1);
                // The location is initialised before anything is projected: a
                // new client starts where the session's bookmark is.
                shell.location = self.initial_client_location();
                self.clients.insert(client_id, connection);
                self.immediate_pty_sources_dirty = true;
                self.host_input_modes_dirty = true;
                // A known connection with an empty session can create the
                // workspace it will view. Either way the locations are settled
                // once more: a bookmark-less session leaves the new client
                // viewing nothing until the reconcile lands it.
                if self.create_automatic_workspace(Some(client_id)) {
                    self.mark_view_changed();
                }
                self.reconcile_client_shell_locations();
                let Some((location, projection_revision)) =
                    self.clients.get(&client_id).map(|client| {
                        (
                            client.shell_state().location.clone(),
                            client.shell_state().projection_revision.get(),
                        )
                    })
                else {
                    return;
                };
                self.refresh_stale_shell_session_cache();
                let Some(session_cache) = self.shell_session_cache.as_ref() else {
                    warn!(
                        ?client_id,
                        "shell session cache missing while seeding client"
                    );
                    self.remove_client(client_id);
                    return;
                };
                let seed_snapshot = crate::server::client_shell::snapshot_from_session(
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
                    if self.sync_host_theme_from_foreground() {
                        self.mark_view_changed();
                    }
                }
                if first_app_client {
                    self.app.mark_git_status_refresh_due(self.app.clock.now);
                }
                // A second surface changes no workspace's size: controlled
                // workspaces keep their controller and uncontrolled ones keep
                // theirs.
                if self.claim_unowned_shell_workspace_geometry(client_id, true) {
                    self.mark_view_changed();
                }
            }
            ServerEvent::ClientPasteRejected {
                client_id,
                size,
                max,
            } => {
                // Every rejection is a separate user action, so each one is
                // reported.
                self.send_to_client(
                    client_id,
                    &ServerMessage::ClientShellError {
                        kind: shepr_protocol::NoticeKind::PasteRejected { size, max },
                    },
                );
            }
            ServerEvent::ClientShellResize {
                client_id,
                surface_cols,
                surface_rows,
                cell_width_px,
                cell_height_px,
                pixel_mouse,
            } => {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return;
                };
                let previous_geometry =
                    (client.terminal_size, client.cell_size, client.pixel_mouse);
                client.terminal_size =
                    shepr_core::geometry::GridSize::clamped(surface_cols, surface_rows);
                let observed = shepr_termio::host_term::cell_size::HostCellSize {
                    width_px: cell_width_px,
                    height_px: cell_height_px,
                };
                if observed.is_known() {
                    client.cell_size = observed;
                }
                client.pixel_mouse = pixel_mouse && observed.is_known();
                if previous_geometry == (client.terminal_size, client.cell_size, client.pixel_mouse)
                {
                    return;
                }
                if !client.is_active_shell_client() {
                    return;
                }
                client.request_repaint();
                self.immediate_pty_sources_dirty = true;
                self.host_input_modes_dirty = true;
                // A resize reports view geometry, not user activity. Window
                // layout and font changes must not switch the host theme or
                // pane-less clipboard destination.
                if self.resize_shell_workspaces_sized_for(client_id, true) {
                    self.mark_view_changed();
                }
            }
            ServerEvent::ClientShellHostTheme { client_id, update } => {
                let is_foreground = self.clients.foreground_client_id() == Some(client_id);
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return;
                };
                if !client.update_host_theme(&update) {
                    return;
                }
                if !client.shell_state().surface_active || !is_foreground {
                    return;
                }
                if self.sync_host_theme_from_foreground() {
                    // Pane colours changed under every surface.
                    for client in self.clients.values_mut() {
                        client.request_recompute();
                    }
                    self.mark_view_changed();
                }
            }
            ServerEvent::ClientShellFocus { client_id, focused } => {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return;
                };
                if !client.is_active_shell_client()
                    || client.shell_state().outer_terminal_focus == Some(focused)
                {
                    return;
                }
                // Recorded on this connection only; the panes it views learn
                // of it through `sync_pane_focus` once the event is applied.
                client.shell_state_mut().outer_terminal_focus = Some(focused);
                if focused {
                    if self.promote_client_to_foreground(client_id) {
                        self.mark_view_changed();
                    }
                    if self.claim_shell_workspace_geometry(client_id, false) {
                        self.mark_view_changed();
                    }
                }
            }
            ServerEvent::ClientShellPresentationSync { client_id, token } => {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return;
                };
                if !client.is_active_shell_client() {
                    return;
                }
                client.outbox.forget_presentation();
                self.stream_host_mouse_capture_mode();
                self.stream_shell_keyboard_mode();
                self.sync_window_title();
                self.send_to_client(client_id, &ServerMessage::PresentationReady(token));
            }
            ServerEvent::ClientShellPaneInput {
                client_id,
                pane_id,
                events,
            } => {
                if !self
                    .clients
                    .get(&client_id)
                    .is_some_and(ClientConnection::is_active_shell_client)
                {
                    return;
                }
                let pixel_mouse = self
                    .clients
                    .get(&client_id)
                    .is_some_and(|client| client.pixel_mouse && client.outbox.told_sgr_pixels());
                let mut events = events;
                let Some((workspace_index, runtime_pane_id)) = self.app.parse_pane_id(&pane_id)
                else {
                    return;
                };
                let Some(runtime) = self.app.state.runtime_for_pane_in_workspace(
                    &self.app.terminal_runtimes,
                    workspace_index,
                    runtime_pane_id,
                ) else {
                    return;
                };
                super::pane_input::downgrade_ineligible_pixel_mouse(
                    &mut events,
                    pixel_mouse,
                    runtime.grid_size(),
                    runtime.pixel_size(),
                );
                if !self.shell_client_views_pane(client_id, workspace_index, runtime_pane_id) {
                    let Some(runtime) = self.app.state.runtime_for_pane_in_workspace(
                        &self.app.terminal_runtimes,
                        workspace_index,
                        runtime_pane_id,
                    ) else {
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
                    let scroll_before = runtime.scroll_metrics();
                    let result = apply_client_pane_input_events(runtime, &releases);
                    let scrolled = runtime.scroll_metrics() != scroll_before;
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
                let foreground_changed =
                    interaction && self.promote_client_to_foreground(client_id);
                let geometry_changed =
                    interaction && self.claim_shell_workspace_geometry(client_id, false);
                if foreground_changed | geometry_changed {
                    self.mark_view_changed();
                }
                let Some(runtime) = self.app.state.runtime_for_pane_in_workspace(
                    &self.app.terminal_runtimes,
                    workspace_index,
                    runtime_pane_id,
                ) else {
                    return;
                };
                let scroll_before = runtime.scroll_metrics();
                let result = apply_client_pane_input_events(runtime, &events);
                let scrolled = runtime.scroll_metrics() != scroll_before;
                if let Err(failures) = result {
                    self.report_client_shell_input_failures(client_id, &pane_id, &failures);
                }
                if scrolled {
                    self.invalidate_pane_viewers(runtime_pane_id);
                }
            }
            ServerEvent::ClientShellEndpointRequest {
                client_id,
                boot_id,
                request_id,
                command,
            } => {
                self.handle_client_shell_endpoint_request(client_id, boot_id, request_id, *command);
            }
            ServerEvent::ClientDetach { client_id } => {
                if !self.remove_client_and_resize_if_needed(client_id) {
                    return;
                }
                info!(?client_id, "client detached");
                self.mark_view_changed();
            }
            ServerEvent::ClientDisconnected { client_id } => {
                if !self.remove_client_and_resize_if_needed(client_id) {
                    return;
                }
                info!(?client_id, "client disconnected");
                self.mark_view_changed();
            }
            // `handle_server_event` consumes writer-drain signals before
            // application dispatch; retain that arm for enum exhaustiveness.
            // The host-shutdown monitor updates its flag before sending its
            // wake. The loop checks that flag to checkpoint and freeze saves;
            // a host warning does not stop the server.
            ServerEvent::ClientWriterDrained | ServerEvent::HostShutdownWake => {}
        }
    }

    fn handle_api_request_with_shutdown_check_inner(
        &mut self,
        msg: shepr_api::ApiRequestMessage,
    ) -> bool {
        if self.lifecycle.stop_requested(self.app.state.should_quit) {
            self.initiate_shutdown();
        }
        if self.lifecycle.phase() == ShutdownPhase::Stopping {
            self.reject_api_request_for_shutdown(&msg);
            return false;
        }
        let request_id = msg.request.id.clone();
        let method = msg.request.method.traits().name;

        let mut changed = self.drain_all_internal_events_with_forwarding();

        // API handlers read each workspace's recorded layout area for directional
        // focus, resize steps, layout snapshots and spawn sizes; the geometry
        // paths keep it current, so there is nothing to project first.
        let outcome = self.app.handle_api_request_with_render(msg.request);
        changed |= outcome.view_changed;
        shepr_api::send_api_response(&msg.respond_to, &request_id, method, outcome.response);

        changed |= self.create_automatic_workspace(None);

        changed
    }

    /// Handle scheduled tasks for the headless server.
    ///
    /// Similar to the former App scheduler but without terminal resize polling.
    fn handle_scheduled_tasks_headless(&mut self, now: Instant) -> bool {
        let mut changed = false;

        // No resize polling needed - server has no terminal.
        // Client resize messages drive size changes instead.

        if self.has_app_client() {
            self.app.start_git_status_refresh_if_due(now);
        }

        // The persister's completion signal wakes the loop when a save ends;
        // reaping it may make the next save (or a held checkpoint) startable.
        let save_reaped = self.app.reap_finished_session_save();
        if save_reaped || self.app.session_saver.is_due(now) {
            self.app.start_background_session_save();
        }

        self.sync_host_shutdown_freeze(now);
        let pane_exit_checkpoint_ready = self.app.take_pane_exit_checkpoint_ready();
        if pane_exit_checkpoint_ready
            || (!self.app.policy.persists_session() && !self.app.pane_exit_checkpoint_requested())
        {
            let queued = self.pending_checkpointed_pane_exits.len();
            for _ in 0..queued {
                if let Some(pending) = self.pending_checkpointed_pane_exits.pop_front() {
                    if self
                        .app
                        .pane_exit_checkpoint_generation_settled(pending.checkpoint_generation)
                    {
                        self.replaying_checkpointed_pane_exit = Some(pending.checkpoint_generation);
                        changed |= self.handle_internal_event_with_forwarding(pending.event);
                    } else {
                        self.pending_checkpointed_pane_exits.push_back(pending);
                    }
                }
            }
        }

        // The resume schedule derives its own wakeup and keeps its theme wait
        // across passes, so running this on every iteration (a pane printing
        // keeps one busy) cannot postpone the first restored agent.
        let resumed = self.app.start_pending_agent_resumes(now);
        if resumed {
            // A resumed agent runs in a fresh runtime; one whose pane a
            // client has focused gets its focus-in report now rather than on
            // the next focus change.
            self.sync_pane_focus();
        }
        let resumed_after_check_start = self.schedule_resume_cwd_checks(now);
        changed | resumed | resumed_after_check_start
    }

    fn schedule_resume_cwd_checks(&mut self, now: Instant) -> bool {
        let mut check_start_failed = false;
        for (terminal_id, cwd) in self.app.pending_agent_resume_cwd_checks(now) {
            let key = (terminal_id.clone(), cwd.clone());
            if !self.resume_cwd_checks_in_flight.insert(key) {
                continue;
            }
            if let Err(error) =
                worker::resume_cwd_check(&self.worker_tx, terminal_id.clone(), cwd.clone())
            {
                warn!(
                    terminal = %terminal_id,
                    cwd = %cwd.display(),
                    %error,
                    "failed to start saved agent resume directory check"
                );
                self.resume_cwd_checks_in_flight
                    .remove(&(terminal_id.clone(), cwd.clone()));
                self.app
                    .record_pending_agent_resume_cwd_check(terminal_id, cwd, false);
                check_start_failed = true;
            }
        }
        if !check_start_failed {
            return false;
        }
        let resumed = self.app.start_pending_agent_resumes(now);
        if resumed {
            self.sync_pane_focus();
        }
        resumed
    }

    fn handle_worker_completion(
        &mut self,
        completion: worker::WorkerCompletion,
        now: Instant,
    ) -> bool {
        match completion {
            worker::WorkerCompletion::CheckoutRoot {
                ticket,
                boot_id,
                request_id,
                home,
                result,
            } => {
                let result = result
                    .map(
                        |root| shepr_protocol::command::EndpointReply::WorkspaceCheckoutRoot {
                            root,
                            home,
                        },
                    )
                    .map_err(shepr_protocol::command::EndpointError::Rejected);
                self.complete_endpoint_reply(
                    ticket,
                    &crate::server::client_commands::response_message(boot_id, request_id, result),
                );
                false
            }
            worker::WorkerCompletion::ResumeCwdChecked {
                terminal_id,
                cwd,
                result,
            } => {
                self.resume_cwd_checks_in_flight
                    .remove(&(terminal_id.clone(), cwd.clone()));
                let available = match result {
                    Ok(available) => available,
                    Err(error) => {
                        warn!(
                            terminal = %terminal_id,
                            cwd = %cwd.display(),
                            %error,
                            "saved agent resume directory cannot be read"
                        );
                        false
                    }
                };
                self.app
                    .record_pending_agent_resume_cwd_check(terminal_id, cwd, available);
                let resumed = self.app.start_pending_agent_resumes(now);
                if resumed {
                    self.sync_pane_focus();
                }
                let resumed_after_check_start = self.schedule_resume_cwd_checks(now);
                resumed | resumed_after_check_start
            }
        }
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
    stop_requested: Arc<shepr_api::ServerStopSignal>,
    signal_quit: Arc<AtomicBool>,
) -> io::Result<()> {
    ctrlc::set_handler(move || {
        // Before the stop request, so the loop never sees the quit without it.
        signal_quit.store(true, Ordering::Release);
        stop_requested.request();
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
            .into_iter()
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
