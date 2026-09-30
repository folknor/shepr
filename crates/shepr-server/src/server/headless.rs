//! Headless server mode - runs the shepr event loop without a real terminal.
//!
//! The server:
//! - Does not enter raw mode or read stdin
//! - Creates and listens on the API and client sockets
//! - Initializes AppState and all PTYs from session restore or fresh state
//! - Runs the main event loop (drain events, drain API requests, scheduled tasks)
//! - Renders virtual surfaces directly to wire-cell frames in memory
//! - Accepts client connections on the client socket
//! - Streams frames to connected clients after each render
//! - Routes client input events through the existing input pipeline
//! - Continues running after client disconnect
//! - Handles stale socket cleanup, explicit server stop, and pane spawn failure
//!   during restore

use crate::server::ClientId;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use interprocess::local_socket::ListenerNonblockingMode;
use interprocess::local_socket::traits::Listener as _;
use ratatui::layout::Rect;
use tokio::io::unix::{AsyncFd, AsyncFdReadyGuard};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use base64::Engine;

use crate::app::{self, RenderDemand};
use crate::limits::SERVER_EVENT_CHANNEL_CAPACITY;
use crate::server::client_accept::{self, accept_client_connection};
use crate::server::client_shell::{
    render_pane_surface as render_client_shell_pane_surface, snapshot as client_shell_snapshot,
};
use crate::server::client_transport::ServerEvent;
use crate::server::clients::{ClientConnection, ClientRegistry, ClientShellState, render_targets};
use crate::server::pane_input::apply_client_pane_input_events;
use crate::server::socket_paths::client_socket_path;
use shepr_mux::events::AppEvent;
use shepr_platform::ipc::{
    LocalListener, SocketFileIdentity, SocketStartupLock, bind_private_socket,
    remove_socket_file_if_owned,
};
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

pub use bootstrap::{RunServerError, ServerReady, ServerSocket, run_server};
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
enum LoopEvent<'a> {
    Timer,
    Internal(AppEvent),
    Api(Box<shepr_api::ApiRequestMessage>),
    ServerEvent(ServerEvent),
    RenderRequested,
    ClientListenerReady(AsyncFdReadyGuard<'a, ListenerFd>),
    ClientListenerError(io::Error),
}

struct PendingCheckpointedPaneExit {
    event: shepr_mux::events::AppEvent,
    checkpoint_generation: u64,
}

struct ListenerFd(RawFd);

impl AsRawFd for ListenerFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Headless server
// ---------------------------------------------------------------------------

/// The headless server - runs the shepr event loop without a real terminal.
pub struct HeadlessServer {
    app: app::App,
    /// Kept alive only for its `Drop` impl, which tears down the JSON API socket server.
    _api_server: Option<shepr_api::ServerHandle>,
    client_listener: LocalListener,
    client_socket_path: PathBuf,
    client_socket_identity: SocketFileIdentity,
    clients: ClientRegistry,
    /// Process-local identity used to reject shell replacements from an earlier server boot.
    client_shell_boot_id: shepr_protocol::BootId,
    /// The config this server was launched with, immutable for its lifetime.
    /// Each connection's welcome carries it; snapshots never do.
    config: Arc<shepr_config::ValidatedConfig>,
    /// Shared session source for shell projections; `None` until a render
    /// with a shell client builds it.
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
    /// the clients and on workspace/pane topology, which change only while
    /// handling an internal event, an API request, a server event or a client
    /// removal; a PTY render wake changes neither. Recomputing it on every loop
    /// wake walked every pane per PTY notify. A missed mark would only delay a
    /// visible pane's repaint to the normal render cadence, never drop it:
    /// visibility at render time is computed fresh.
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
    /// Watches logind for shutdown warnings; `None` before `run` and while the
    /// server has dropped it to release its delay lock (see
    /// `freeze_for_host_shutdown`).
    host_shutdown_monitor: Option<shepr_platform::HostShutdownMonitor>,
    /// Channel for receiving server events from client connection threads.
    server_event_rx: mpsc::Receiver<ServerEvent>,
    /// Sender for server events (cloned for each client thread).
    server_event_tx: mpsc::Sender<ServerEvent>,
    /// Acknowledgements for shutdown frames queued to client writer threads.
    shutdown_flushes: Vec<tokio::sync::oneshot::Receiver<()>>,
    /// Pane exits held until their pre-removal session checkpoint reaches disk.
    pending_checkpointed_pane_exits: VecDeque<PendingCheckpointedPaneExit>,
    /// Set only while a ready held exit is routed back through the forwarding
    /// handler, which then skips its initial App preparation step.
    replaying_checkpointed_pane_exit: Option<u64>,
    /// Answers to client-shell endpoint commands, in the order the commands
    /// ran, waiting for the render that shows their effect. The loop queues
    /// them to the clients after that render (or at once when no render is
    /// pending), so a reply never reaches a client ahead of the projection
    /// its command changed. See `flush_endpoint_replies`.
    endpoint_replies: Vec<(ClientId, ServerMessage)>,
    // Kept after the listener so it is released only after socket cleanup and listener drop.
    _client_socket_startup_lock: SocketStartupLock,
}

impl HeadlessServer {
    /// Creates and starts the headless server.
    ///
    /// This:
    /// 1. Locks and prepares the client socket path (cleaning up stale sockets)
    /// 2. Binds the private client socket listener
    /// 3. Returns the server ready to run
    ///
    /// A client socket another server holds comes back as the
    /// [`shepr_platform::ipc::SocketBusy`] refusal naming it; [`run_server`]
    /// turns that into [`RunServerError::AlreadyRunning`].
    pub fn new(
        app: app::App,
        api_server: Option<shepr_api::ServerHandle>,
        config: Arc<shepr_config::ValidatedConfig>,
        stop_requested: Arc<shepr_api::ServerStopSignal>,
    ) -> io::Result<Self> {
        let client_path = client_socket_path(&app.paths);
        let (listener, client_socket_startup_lock, client_socket_identity) =
            bind_private_socket(&client_path)?;
        info!(path = %client_path.display(), "client protocol socket listening");

        // Accept all queued connections when the listener becomes readable.
        if let Err(error) = listener.set_nonblocking(ListenerNonblockingMode::Accept) {
            if let Err(cleanup_error) =
                remove_socket_file_if_owned(&client_path, &client_socket_identity)
                && cleanup_error.kind() != io::ErrorKind::NotFound
            {
                warn!(
                    path = %client_path.display(),
                    error = %cleanup_error,
                    "failed to remove client socket after listener setup failed"
                );
            }
            drop(listener);
            drop(client_socket_startup_lock);
            return Err(error);
        }

        // Channel for server events from client threads.
        let (server_event_tx, server_event_rx) = mpsc::channel(SERVER_EVENT_CHANNEL_CAPACITY);

        Ok(Self {
            app,
            _api_server: api_server,
            client_listener: listener,
            client_socket_path: client_path,
            client_socket_identity,
            clients: ClientRegistry::default(),
            client_shell_boot_id: shepr_protocol::BootId::for_this_process(),
            config,
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
            shutdown_flushes: Vec::new(),
            pending_checkpointed_pane_exits: VecDeque::new(),
            replaying_checkpointed_pane_exit: None,
            endpoint_replies: Vec::new(),
            _client_socket_startup_lock: client_socket_startup_lock,
        })
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
        let listener_fd = match &self.client_listener {
            LocalListener::UdSocket(socket) => socket.as_fd().as_raw_fd(),
        };
        let client_listener_ready = AsyncFd::new(ListenerFd(listener_fd))?;

        // Register SIGINT handler for graceful shutdown.
        let stop_requested = Arc::clone(self.lifecycle.stop_signal());
        let signal_quit = Arc::clone(self.lifecycle.signal_quit_request_flag());
        ctrlc_handler(stop_requested, signal_quit)?;
        self.start_host_shutdown_monitor();

        let mut render_demand = RenderDemand::Full;
        let mut run_error = None;
        // Set while the client listener rests after running out of descriptors
        // or memory; its readiness is left set, so it is not polled until then.
        let mut client_accept_paused_until: Option<Instant> = None;

        loop {
            // If shutdown has been initiated, complete it and exit.
            if self.lifecycle.phase() == ShutdownPhase::Stopping {
                // Commands answered before the stop (a command that arrived
                // while stopping is answered with the refusal) still reach
                // their clients, ahead of the shutdown notice.
                self.flush_endpoint_replies();
                if let Err(err) = self.complete_shutdown().await {
                    run_error.get_or_insert(err);
                }
                break;
            }

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

            // 1. Check the coalesced render signal from PTY readers and generic runtime work.
            if self.app.render_dirty.is_pending() {
                render_demand.join(RenderDemand::Partial);
            }
            // 2. Drain a bounded internal-event batch. API handlers perform an
            // exhaustive forwarding-aware drain before reading pane/runtime state.
            if self.drain_internal_events_with_forwarding() {
                render_demand.join(RenderDemand::Full);
            }
            if self.lifecycle.stop_requested(self.app.state.should_quit) {
                continue;
            }
            self.refresh_app_clock();

            // 3. Drain API requests.
            if self.drain_api_requests_with_shutdown_check() {
                render_demand.join(RenderDemand::Full);
            }
            if self.lifecycle.stop_requested(self.app.state.should_quit) {
                continue;
            }

            self.app.sync_session_save_schedule();

            // 4. Drain server events from client threads.
            if self.drain_server_events() {
                render_demand.join(RenderDemand::Full);
            }
            if self.lifecycle.stop_requested(self.app.state.should_quit) {
                continue;
            }

            // 5. Handle scheduled tasks.
            let now = self.refresh_app_clock();
            if self.handle_scheduled_tasks_headless(now) {
                self.app.state.mark_shell_projection_dirty();
                render_demand.join(RenderDemand::Full);
            }

            if self.create_automatic_workspace(None) {
                render_demand.join(RenderDemand::Full);
            }
            if self.shell_cwd_refresh_due(now) && self.refresh_shell_projection_sources() {
                render_demand.join(RenderDemand::Full);
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

            // 6. Render virtually and stream frames. Hidden-only PTY work keeps a
            // bounded classification cadence without delaying presentation work
            // that joins the same coalesced request.
            let render_cadence_due = self.app.can_render_now(now);
            if render_demand != RenderDemand::None
                && (render_cadence_due
                    || (self.app.can_present_now(now)
                        && self.has_pending_presentation_work(render_demand)))
            {
                let render_request = self.app.render_dirty.take();
                let pty_dirty = !render_request.pty_sources.is_empty();
                if pty_dirty {
                    self.host_input_modes_dirty = true;
                }
                if render_request.generic {
                    render_demand.join(RenderDemand::Full);
                }
                let (sidebar_title_changed, outer_title_synced) =
                    self.sync_terminal_title_sources(&render_request.terminal_title_sources);
                if sidebar_title_changed {
                    render_demand.join(RenderDemand::Full);
                }
                if render_demand == RenderDemand::Full && !outer_title_synced {
                    self.sync_window_title();
                }
                if render_demand != RenderDemand::Full && !pty_dirty {
                    // A synchronized-output OSC title can be the only pending work.
                    // Its deferred PTY repaint has its own signal; do not manufacture
                    // a full UI render for this client-local side effect.
                    render_demand = RenderDemand::None;
                    continue;
                }
                let hidden_only = pty_dirty
                    && render_demand != RenderDemand::Full
                    && !self.pty_sources_visible_to_any_render_target(&render_request.pty_sources);
                if hidden_only {
                    // Hidden-only PTY work keeps a bounded classification cadence
                    // without delaying presentation work that joins the same
                    // coalesced request.
                } else if render_demand != RenderDemand::Full
                    && self.render_retained_pane_surface_and_stream(&render_request.pty_sources)
                {
                    // retained pane surface path
                } else {
                    self.render_and_stream();
                    self.report_retained_surface_fallback();
                }
                self.app.record_render_attempt(now, !hidden_only);
                render_demand = RenderDemand::None;
                // After the render, so each reply follows the projection its
                // command changed on the client's control lane.
                self.flush_endpoint_replies();
                continue;
            }

            // 7. Wait for next event. With no render pending, no reply has a
            // surface update to wait for. A pending render that the cadence
            // holds back keeps its replies until it runs.
            if render_demand == RenderDemand::None {
                self.flush_endpoint_replies();
            }
            let next_deadline = self.app.next_headless_loop_deadline_with_git_refresh(
                now,
                render_demand != RenderDemand::None,
                self.has_app_client(),
            );
            let next_deadline = self
                .shell_cwd_refresh_deadline()
                .map_or(next_deadline, |cwd| {
                    Some(next_deadline.map_or(cwd, |current| current.min(cwd)))
                });
            client_accept_paused_until = client_accept_paused_until.filter(|until| *until > now);
            let next_deadline = client_accept_paused_until.map_or(next_deadline, |until| {
                Some(next_deadline.map_or(until, |current| current.min(until)))
            });
            let stop_signal = Arc::clone(self.lifecycle.stop_signal());
            let event = {
                tokio::select! {
                    // A `server.stop` from the API sets the latch on another
                    // thread; this is what wakes an idle loop to act on it.
                    () = stop_signal.notified() => LoopEvent::Timer,
                    maybe_api = self.app.api_rx.recv() => match maybe_api {
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
                    _ = sleep_until_or_pending(next_deadline) => LoopEvent::Timer,
                    // A save ended: the scheduled tasks reap it and start
                    // whatever save waited for it.
                    () = self.app.session_saver.save_finished().notified() => LoopEvent::Timer,
                    _ = self.app.render_notify.notified() => LoopEvent::RenderRequested,
                    ready = client_listener_ready.readable(),
                        if client_accept_paused_until.is_none() => {
                        match ready {
                            Ok(guard) => LoopEvent::ClientListenerReady(guard),
                            Err(err) => LoopEvent::ClientListenerError(err),
                        }
                    },
                }
            };
            // The wait above can last until the next deadline; dispatch reads
            // the time the event arrived, not the time the wait began.
            let event_time = self.refresh_app_clock();

            if self.lifecycle.stop_requested(self.app.state.should_quit) {
                self.initiate_shutdown();
                match event {
                    LoopEvent::Internal(ev) => {
                        self.handle_internal_event_with_forwarding(ev);
                    }
                    LoopEvent::ServerEvent(ServerEvent::ClientShellConnected {
                        client_id,
                        writer,
                        ..
                    }) => {
                        if let Ok(message) =
                            Self::frame_server_message(&ServerMessage::ServerShutdown {
                                reason: Some(shepr_protocol::ShutdownReason::Message(
                                    "server is shutting down".to_owned(),
                                )),
                            })
                        {
                            // A closed writer means the client already left;
                            // there is nothing to flush for it.
                            if writer.control.send(message).is_err() {
                                debug!(?client_id, "client left before its shutdown notice");
                            } else {
                                self.shutdown_flushes.push(writer.flush());
                            }
                        }
                    }
                    // Already dequeued, so the shutdown drain would never see
                    // it; answer it here.
                    LoopEvent::Api(msg) => self.reject_api_request_for_shutdown(&msg),
                    LoopEvent::ServerEvent(ServerEvent::ClientShellEndpointRequest {
                        client_id,
                        boot_id,
                        request_id,
                        ..
                    }) => self.reject_endpoint_request_for_shutdown(client_id, boot_id, request_id),
                    LoopEvent::ClientListenerError(err) => {
                        tracing::error!(error = %err, "client listener readiness failed");
                        run_error.get_or_insert(err);
                    }
                    _ => {}
                }
                continue;
            }

            match event {
                LoopEvent::Timer => {}
                LoopEvent::Internal(ev) => {
                    if self.handle_internal_event_with_forwarding(ev) {
                        render_demand.join(RenderDemand::Full);
                    }
                }
                LoopEvent::Api(msg) => {
                    if self.handle_api_request_with_shutdown_check(*msg) {
                        render_demand.join(RenderDemand::Full);
                    }
                }
                LoopEvent::ServerEvent(ev) => {
                    if self.handle_server_event_with_render_impact(ev) == RenderDemand::Full {
                        render_demand.join(RenderDemand::Full);
                    }
                }
                LoopEvent::RenderRequested => {
                    if self.app.render_dirty.is_pending() {
                        render_demand.join(RenderDemand::Partial);
                    }
                }
                LoopEvent::ClientListenerReady(mut ready) => {
                    // Keep readiness set while accepts succeed. `try_io` clears
                    // it only when accept observes WouldBlock, so a hard error
                    // cannot strand connections still waiting in the backlog.
                    loop {
                        if self.lifecycle.stop_requested(self.app.state.should_quit) {
                            break;
                        }
                        match ready.try_io(|_| self.accept_client_connection()) {
                            Err(_) => break,
                            Ok(Ok(())) => {}
                            // Out of descriptors or memory: leave the backlog
                            // queued and readiness set, and try again later.
                            Ok(Err(err)) if client_accept::accept_resources_exhausted(&err) => {
                                client_accept_paused_until =
                                    Some(event_time + crate::limits::CLIENT_ACCEPT_RETRY_DELAY);
                                break;
                            }
                            Ok(Err(err)) => {
                                run_error.get_or_insert(err);
                                self.initiate_shutdown();
                                break;
                            }
                        }
                    }
                }
                LoopEvent::ClientListenerError(err) => {
                    tracing::error!(error = %err, "client listener readiness failed");
                    run_error.get_or_insert(err);
                    self.initiate_shutdown();
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
        self.app.retire_session_writer();
        self.release_sockets_after_save();

        info!("headless server exiting");
        run_error.map_or(Ok(()), Err)
    }

    /// Colours the panes with the foreground client's host theme: panes have
    /// one theme (their default colours and the answers to colour queries),
    /// and the client the user was last active in supplies it. A client that
    /// has reported nothing yet leaves the current theme (a live client's, or
    /// the one saved with the session) in place. Returns whether it changed.
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
        changed |= self.app.set_host_terminal_theme(theme);
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

    fn remove_client_and_resize_if_needed(&mut self, client_id: ClientId) {
        self.remove_client(client_id);
        // Removing the client dropped its geometry controller mappings. Each
        // workspace it controlled goes to a remaining viewer, or every
        // workspace to the headless size when no surface remains, so no pane
        // keeps the departed client's size.
        self.reapply_controlled_shell_workspace_geometry(true);
    }

    /// Accepts one client connection from the non-blocking listener.
    fn accept_client_connection(&mut self) -> io::Result<()> {
        accept_client_connection(
            &self.client_listener,
            &mut self.clients,
            self.lifecycle.stop_signal(),
            &self.server_event_tx,
            &self.config,
        )
    }

    /// Drains server events from the dedicated channel.
    fn drain_server_events(&mut self) -> bool {
        let mut changed = false;
        while !self.lifecycle.stop_requested(self.app.state.should_quit) {
            let Ok(ev) = self.server_event_rx.try_recv() else {
                break;
            };
            changed |= self.handle_server_event_with_render_impact(ev) == RenderDemand::Full;
        }
        changed
    }

    /// Closes the server event channel and settles what is left in it: a
    /// client that connected too late is sent the shutdown notice, and an
    /// endpoint command still queued is answered with the shutdown refusal
    /// rather than left to its client's command timeout. Everything else is
    /// moot once the server stops.
    async fn reject_late_client_connections(&mut self) {
        self.server_event_rx.close();
        while let Some(event) = self.server_event_rx.recv().await {
            match event {
                ServerEvent::ClientShellConnected {
                    client_id, writer, ..
                } => {
                    let Ok(message) = Self::frame_server_message(&ServerMessage::ServerShutdown {
                        reason: Some(shepr_protocol::ShutdownReason::Message(
                            "server is shutting down".to_owned(),
                        )),
                    }) else {
                        continue;
                    };
                    // A closed writer means the client already left; there is
                    // nothing to flush for it.
                    if writer.control.send(message).is_err() {
                        debug!(?client_id, "late client left before its shutdown notice");
                    } else {
                        self.shutdown_flushes.push(writer.flush());
                    }
                }
                ServerEvent::ClientShellEndpointRequest {
                    client_id,
                    boot_id,
                    request_id,
                    ..
                } => self.reject_endpoint_request_for_shutdown(client_id, boot_id, request_id),
                _ => {}
            }
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
                    .is_some_and(|client| client.sent_window_title.as_ref() != Some(title))
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
        // `send_to_client` reports false for a missing or writer-less client,
        // so nothing is cached against a client that never got the title.
        let sent = self.send_to_client(
            client_id,
            &ServerMessage::WindowTitle {
                title: title.clone(),
            },
        );
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.sent_window_title = sent.then_some(title);
        }
        sent
    }

    /// Encodes a server message as its frames: one for a payload that fits in
    /// `MAX_FRAME_SIZE`, several for a larger one. The frames are one buffer,
    /// so the writer puts them on the socket back to back.
    ///
    /// Only a payload over `MAX_MESSAGE_SIZE` fails, with
    /// `FramingError::Oversized`, since the client refuses a larger message.
    fn frame_server_message(msg: &ServerMessage) -> Result<Vec<u8>, shepr_protocol::FramingError> {
        shepr_protocol::encode_message(msg)
    }

    /// Sends a message to all connected clients.
    /// Broken connections are tracked and cleaned up.
    ///
    /// Each client gets its own copy of the framed bytes. That is deliberate:
    /// the only caller is `initiate_shutdown`, once per server lifetime.
    /// Render output never goes through here; each client's frame or patch is
    /// diffed against that client's own baseline (`render_and_stream`,
    /// `render_retained_pane_surface_and_stream`), so there is no shared frame
    /// to hand out. Making the writer queue carry `Arc<[u8]>` would add a
    /// refcount to every per-client render send to save one tiny copy here.
    fn send_to_all_clients(&mut self, msg: &ServerMessage) {
        let serialized = match Self::frame_server_message(msg) {
            Ok(framed) => framed,
            Err(err) => {
                warn!(error = %err, "failed to serialize message for clients");
                return;
            }
        };

        let mut broken_clients: Vec<ClientId> = Vec::new();
        for (&client_id, client) in &mut self.clients {
            if let Some(writer) = &client.writer
                && writer.control.send(serialized.clone()).is_err()
            {
                debug!(?client_id, "client writer channel closed during broadcast");
                broken_clients.push(client_id);
            }
        }

        // Remove broken clients.
        for client_id in broken_clients {
            self.remove_client_and_resize_if_needed(client_id);
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
    /// was not found or the send failed (client removed).
    fn send_to_client(&mut self, client_id: ClientId, msg: &ServerMessage) -> bool {
        let serialized = match Self::frame_server_message(msg) {
            Ok(framed) => framed,
            Err(err) => {
                warn!(?client_id, error = %err, "failed to serialize message for client");
                return false;
            }
        };

        let Some(client) = self.clients.get(&client_id) else {
            return false;
        };
        // Only test fixtures build a client without a writer; nothing was
        // queued for it, so report the send as not delivered.
        let Some(writer) = &client.writer else {
            return false;
        };
        if writer.control.send(serialized).is_err() {
            debug!(
                ?client_id,
                "client writer channel closed during targeted send"
            );
            self.remove_client_and_resize_if_needed(client_id);
            return false;
        }
        true
    }

    /// Holds an endpoint command's reply for the post-render flush.
    fn queue_endpoint_reply(&mut self, client_id: ClientId, message: ServerMessage) {
        self.endpoint_replies.push((client_id, message));
    }

    /// Hands every held endpoint reply to its client's control lane, in the
    /// order the commands ran. The render before this queued each client's
    /// changed projection on the same lane, so a reply follows it there. The
    /// pane frame of that render travels in the render slot, which the
    /// writer drains after pending control messages, so the reply can reach
    /// the socket before that frame. A reply for a client that left in the
    /// meantime is dropped by `send_to_client`.
    fn flush_endpoint_replies(&mut self) {
        if self.endpoint_replies.is_empty() {
            return;
        }
        for (client_id, message) in std::mem::take(&mut self.endpoint_replies) {
            self.send_to_client(client_id, &message);
        }
    }

    /// Handles a server event, then reports any change in which panes hold
    /// terminal focus. Returns true if the event requires a re-render.
    fn handle_server_event(&mut self, ev: ServerEvent) -> bool {
        // Pane input and writer drains, the per-keystroke and per-frame
        // events, move no client's view and no outer focus; a client they
        // remove on a failed send is settled by `remove_client`.
        let may_move_focus = matches!(
            ev,
            ServerEvent::ClientShellConnected { .. }
                | ServerEvent::ClientShellResize { .. }
                | ServerEvent::ClientShellFocus { .. }
                | ServerEvent::ClientShellEndpointRequest { .. }
                | ServerEvent::ClientDetach { .. }
                | ServerEvent::ClientDisconnected { .. }
        );
        let changed = self.apply_server_event(ev);
        if may_move_focus {
            self.sync_pane_focus();
        }
        changed
    }

    fn apply_server_event(&mut self, ev: ServerEvent) -> bool {
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
                writer,
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
                    Some(writer),
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
                self.create_automatic_workspace(Some(client_id));
                self.reconcile_client_shell_locations();
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                let seed_snapshot = client_shell_snapshot(
                    &self.app,
                    &self.client_shell_boot_id,
                    client.shell_state().projection_revision.get(),
                    &client.shell_state().location,
                );
                let snapshot_message = shepr_protocol::endpoint::snapshot_message(&seed_snapshot);
                let shell = client.shell_state_mut();
                shell.projected_location_generation = shell.location.generation();
                shell.snapshot = Some(seed_snapshot);
                shell.session_generation = self.shell_session_generation;
                self.send_to_client(client_id, &snapshot_message);
                if surface_active {
                    self.promote_client_to_foreground(client_id);
                }
                if first_app_client {
                    self.app.mark_git_status_refresh_due(self.app.clock.now);
                }
                // A second surface changes no workspace's size: controlled
                // workspaces keep their controller and uncontrolled ones keep
                // theirs.
                self.claim_unowned_shell_workspace_geometry(client_id, true);
                true
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
                false
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
                    return false;
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
                    return false;
                }
                if !client.is_active_shell_client() {
                    return false;
                }
                client.request_repaint();
                self.immediate_pty_sources_dirty = true;
                self.host_input_modes_dirty = true;
                self.promote_client_to_foreground(client_id);
                self.resize_shell_workspaces_sized_for(client_id, true);
                true
            }
            ServerEvent::ClientShellHostTheme { client_id, update } => {
                let is_foreground = self.clients.foreground_client_id() == Some(client_id);
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                if !client.update_host_theme(&update) {
                    return false;
                }
                if !client.shell_state().surface_active || !is_foreground {
                    return false;
                }
                let changed = self.sync_host_theme_from_foreground();
                if changed {
                    // Pane colours changed under every surface.
                    for client in self.clients.values_mut() {
                        client.request_recompute();
                    }
                }
                changed
            }
            ServerEvent::ClientShellFocus { client_id, focused } => {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                if !client.is_active_shell_client()
                    || client.shell_state().outer_terminal_focus == Some(focused)
                {
                    return false;
                }
                // Recorded on this connection only; the panes it views learn
                // of it through `sync_pane_focus` once the event is applied.
                client.shell_state_mut().outer_terminal_focus = Some(focused);
                if focused {
                    self.promote_client_to_foreground(client_id);
                    self.claim_shell_workspace_geometry(client_id, false);
                }
                true
            }
            ServerEvent::ClientShellPresentationSync { client_id, token } => {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                if !client.is_active_shell_client() {
                    return false;
                }
                client.host_mouse_capture_active = None;
                client.host_sgr_pixels_active = None;
                client.shell_state_mut().host_keyboard_report_all_active = None;
                client.sent_window_title = None;
                self.stream_host_mouse_capture_mode();
                self.stream_shell_keyboard_mode();
                self.sync_window_title();
                self.send_to_client(client_id, &ServerMessage::PresentationReady(token))
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
                    return false;
                }
                let pixel_mouse = self.clients.get(&client_id).is_some_and(|client| {
                    client.pixel_mouse && client.host_sgr_pixels_active == Some(true)
                });
                let mut events = events;
                let Some((workspace_index, runtime_pane_id)) = self.app.parse_pane_id(&pane_id)
                else {
                    return false;
                };
                let Some(runtime) = self.app.state.runtime_for_pane_in_workspace(
                    &self.app.terminal_runtimes,
                    workspace_index,
                    runtime_pane_id,
                ) else {
                    return false;
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
                        return false;
                    };
                    let releases = events
                        .into_iter()
                        .filter(client_pane_input_releases_press)
                        .collect::<Vec<_>>();
                    if releases.is_empty() {
                        return false;
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
                    return scrolled;
                }
                let interaction = client_pane_input_has_interaction(&events);
                if let Some(client) = self.clients.get_mut(&client_id) {
                    client.track_shell_input(&pane_id, &events);
                }
                let foreground_changed =
                    interaction && self.promote_client_to_foreground(client_id);
                let geometry_changed =
                    interaction && self.claim_shell_workspace_geometry(client_id, false);
                let Some(runtime) = self.app.state.runtime_for_pane_in_workspace(
                    &self.app.terminal_runtimes,
                    workspace_index,
                    runtime_pane_id,
                ) else {
                    return foreground_changed | geometry_changed;
                };
                let scroll_before = runtime.scroll_metrics();
                let result = apply_client_pane_input_events(runtime, &events);
                let scrolled = runtime.scroll_metrics() != scroll_before;
                if let Err(failures) = result {
                    self.report_client_shell_input_failures(client_id, &pane_id, &failures);
                }
                foreground_changed | geometry_changed || scrolled
            }
            ServerEvent::ClientShellEndpointRequest {
                client_id,
                boot_id,
                request_id,
                command,
            } => {
                self.handle_client_shell_endpoint_request(client_id, boot_id, request_id, *command)
            }
            ServerEvent::ClientDetach { client_id } => {
                info!(?client_id, "client detached");
                self.remove_client_and_resize_if_needed(client_id);
                true
            }
            ServerEvent::ClientDisconnected { client_id } => {
                info!(?client_id, "client disconnected");
                self.remove_client_and_resize_if_needed(client_id);
                true
            }
            ServerEvent::ClientWriterDrained { client_id } => {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                client.take_deferred_render() != RenderDemand::None
            }
            ServerEvent::QuitSignal => {
                // The quit check at the top of the loop handles this.
                // No render needed - the next iteration will initiate shutdown.
                false
            }
        }
    }

    fn handle_server_event_with_render_impact(&mut self, ev: ServerEvent) -> RenderDemand {
        // Writer readiness is a transport signal. Preserve the deferred demand
        // without touching application projection or per-client input sources.
        if let ServerEvent::ClientWriterDrained { client_id } = ev {
            return self
                .clients
                .get_mut(&client_id)
                .map_or(RenderDemand::None, ClientConnection::take_deferred_render);
        }
        // Presentation events change connection state only. Shared application
        // mutations publish their projection revision at the mutation site.
        if self.handle_server_event(ev) {
            RenderDemand::Full
        } else {
            RenderDemand::None
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
        changed |= outcome.render != RenderDemand::None;
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
        changed | resumed
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
        // The lease before the sockets, as on a clean exit; see
        // `release_sockets_after_save`. Without this the sockets went first
        // and the lease with the fields dropped after this body.
        self.release_sockets_after_save();
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
