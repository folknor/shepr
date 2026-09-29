//! Headless server mode - runs the shepr event loop without a real terminal.
//!
//! The server:
//! - Does not enter raw mode or read stdin
//! - Creates and listens on the API and client sockets
//! - Initializes AppState and all PTYs from session restore or fresh state
//! - Runs the main event loop (drain events, drain API requests, scheduled tasks)
//! - Renders to a virtual ratatui Buffer in memory
//! - Accepts client connections on the client socket
//! - Streams frames to connected clients after each render
//! - Routes client input events through the existing input pipeline
//! - Continues running after client disconnect
//! - Handles stale socket cleanup, explicit server stop, minimum terminal size,
//!   and pane spawn failure during restore

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
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use base64::Engine;

use crate::app::{self, RenderDemand};
use crate::limits::SERVER_EVENT_CHANNEL_CAPACITY;
use crate::server::client_accept::accept_pending_client_connections;
use crate::server::client_shell::{
    render_pane_surface as render_client_shell_pane_surface, snapshot as client_shell_snapshot,
};
use crate::server::client_transport::ServerEvent;
use crate::server::clients::{
    AttachClaim, ClientConnection, ClientConnectionMode, ClientRegistry, render_targets,
    terminal_stream_client_ids,
};
use crate::server::pane_input::{
    apply_client_pane_input_events, apply_terminal_attach_input, apply_terminal_attach_scroll,
    terminal_attach_mouse_position,
};
use crate::server::socket_paths::client_socket_path;
use shepr_mux::events::AppEvent;
use shepr_platform::ipc::{
    LocalListener, SocketFileIdentity, SocketStartupLock, bind_private_socket,
    remove_socket_file_if_owned,
};
use shepr_protocol::{AttachScrollDirection, AttachScrollSource, FrameData, ServerMessage};

mod api_dispatcher;
mod bootstrap;
mod client_views;
mod endpoint_requests;
mod internal_events;
mod lifecycle;
mod render;
mod retained_surface;
mod surface_interest;

use api_dispatcher::AltScreenReadConflict;
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
enum LoopEvent {
    Timer,
    Internal(AppEvent),
    Api(Box<shepr_api::ApiRequestMessage>),
    AgentManifestReload(AgentManifestReloadCompletion),
    ServerEvent(ServerEvent),
    RenderRequested,
    ClientListenerReady,
}

pub(super) struct AgentManifestReloadCompletion {
    pub(super) request_token: u64,
    pub(super) result: Result<Vec<shepr_agent::detect::manifest::AgentManifestSummary>, String>,
}

/// Whether one direct terminal-attach input reached the pane, for
/// `report_terminal_attach_input`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttachInputDelivery {
    Delivered,
    /// Dropped because the pane's PTY input queue is full.
    Dropped,
    /// Failed for another reason (pane closing, input not encodable).
    Failed,
}

impl AttachInputDelivery {
    fn of(result: &Result<(), crate::server::pane_input::PaneInputError>) -> Self {
        match result {
            Ok(()) => Self::Delivered,
            Err(crate::server::pane_input::PaneInputError::Backpressure(_)) => Self::Dropped,
            Err(_) => Self::Failed,
        }
    }

    fn of_batch(result: &Result<(), crate::server::pane_input::PaneInputFailures>) -> Self {
        match result {
            Ok(()) => Self::Delivered,
            Err(failures) if failures.dropped_for_backpressure() > 0 => Self::Dropped,
            Err(_) => Self::Failed,
        }
    }
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
    /// Config bytes are stable for the server lifetime and are encoded before
    /// serving. Only the first snapshot of each shell connection carries them;
    /// later snapshots send an empty payload that the client reads as "reuse".
    resolved_config: Vec<u8>,
    /// Shared session source for shell projections; `None` until a render
    /// with a shell client builds it.
    shell_session_cache: Option<render::ShellSessionCache>,
    /// Moves whenever shell projections must be recomputed: the cache was
    /// rebuilt for a new application revision, or the cwd timer found a
    /// projection that changed. Each shell client records the generation it
    /// last projected.
    shell_session_generation: u64,
    /// Outer window title last pushed, paired with the client that received it.
    /// Keying on the client means a newly attached terminal is written to even
    /// when the title itself has not changed, without every code path that
    /// changes the foreground client having to remember to invalidate this.
    sent_window_title: Option<(ClientId, Option<String>)>,
    /// Window title set through `client.window_title.set`. While present it wins
    /// over the configured `ui.window_title` until the API clears it again.
    api_window_title: Option<String>,
    /// Pending API work lives with the server state it routes and mutates.
    /// Each retained item belongs to an API request awaiting its response.
    /// The API listener's active-connection limit, with one request per
    /// connection, bounds these queues together. The manifest reload queue
    /// also has a cap of its own (`AGENT_MANIFEST_RELOAD_QUEUE_CAPACITY`,
    /// answered with `EndpointBusy`), so it stays bounded independently of
    /// how the API admits connections.
    /// Alternate-screen reads that are being captured without an attached client.
    pending_alt_screen_reads: Vec<crate::server::alt_screen_read::PendingAltScreenRead>,
    deferred_alt_screen_reads: Vec<shepr_api::ApiRequestMessage>,
    /// Requests answered by the running manifest reload; empty when none runs.
    running_agent_manifest_reload: Vec<shepr_api::ApiRequestMessage>,
    /// Requests that arrived while a reload was running. They get a fresh
    /// reload once it finishes, so they see files changed since it started.
    queued_agent_manifest_reloads: Vec<shepr_api::ApiRequestMessage>,
    /// Identifies the running manifest reload's completion.
    agent_manifest_reload_token: u64,
    agent_manifest_reload_tx: mpsc::UnboundedSender<AgentManifestReloadCompletion>,
    agent_manifest_reload_rx: mpsc::UnboundedReceiver<AgentManifestReloadCompletion>,
    /// Whether the set of panes whose PTY output should wake the loop at once
    /// (`sync_immediate_pty_sources`) may be stale. That set depends only on
    /// the clients and on workspace/tab/pane topology, which change only while
    /// handling an internal event, an API request, a server event or a client
    /// removal; a PTY render wake changes neither. Recomputing it on every loop
    /// wake walked every pane per PTY notify. A missed mark would only delay a
    /// visible pane's repaint to the normal render cadence, never drop it:
    /// visibility at render time is computed fresh.
    immediate_pty_sources_dirty: bool,
    /// Whether the host mouse-capture and keyboard modes pushed to clients
    /// (`stream_host_mouse_capture_mode`, `stream_direct_terminal_keyboard_mode`)
    /// may be stale. They follow the focused pane's terminal modes, which only
    /// PTY output changes, plus the same client/topology changes as above. Set
    /// whenever a render request carrying PTY sources is taken; every render
    /// is followed by another loop iteration, which pushes the modes before
    /// the loop sleeps again.
    host_input_modes_dirty: bool,
    /// Reason captured by the retained renderer and reported after the full
    /// render that recovers from it.
    retained_surface_fallback_reason: Option<&'static str>,
    /// Keeps repeated retained-render fallbacks from logging on every PTY wake.
    retained_surface_fallbacks_reported: HashSet<&'static str>,
    /// Shared pane runtime size derived from the foreground client, or the
    /// configured headless size when no clients are connected.
    effective_size: shepr_core::geometry::GridSize,
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
    pending_checkpointed_pane_exits: VecDeque<shepr_mux::events::AppEvent>,
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
    /// A client socket another server holds comes back as
    /// [`io::ErrorKind::AddrInUse`]; [`run_server`] turns that into
    /// [`RunServerError::AlreadyRunning`].
    pub fn new(
        app: app::App,
        api_server: Option<shepr_api::ServerHandle>,
        resolved_config: Vec<u8>,
        stop_requested: Arc<AtomicBool>,
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
        let (agent_manifest_reload_tx, agent_manifest_reload_rx) = mpsc::unbounded_channel();

        let effective_size = app.state.settings.headless_size;
        Ok(Self {
            app,
            _api_server: api_server,
            client_listener: listener,
            client_socket_path: client_path,
            client_socket_identity,
            clients: ClientRegistry::default(),
            client_shell_boot_id: shepr_protocol::BootId::for_this_process(),
            resolved_config,
            shell_session_cache: None,
            shell_session_generation: 0,
            sent_window_title: None,
            api_window_title: None,
            pending_alt_screen_reads: Vec::new(),
            deferred_alt_screen_reads: Vec::new(),
            running_agent_manifest_reload: Vec::new(),
            queued_agent_manifest_reloads: Vec::new(),
            agent_manifest_reload_token: 0,
            agent_manifest_reload_tx,
            agent_manifest_reload_rx,
            immediate_pty_sources_dirty: true,
            host_input_modes_dirty: true,
            retained_surface_fallback_reason: None,
            retained_surface_fallbacks_reported: HashSet::new(),
            effective_size,
            lifecycle: ShutdownLifecycle::new(stop_requested),
            host_shutdown_monitor: None,
            server_event_rx,
            server_event_tx,
            shutdown_flushes: Vec::new(),
            pending_checkpointed_pane_exits: VecDeque::new(),
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
        let stop_requested = Arc::clone(self.lifecycle.stop_request_flag());
        let signal_quit = Arc::clone(self.lifecycle.signal_quit_request_flag());
        let quit_notify = self.server_event_tx.clone();
        ctrlc_handler(stop_requested, signal_quit, quit_notify)?;
        self.start_host_shutdown_monitor();

        let mut render_demand = RenderDemand::Full;
        let mut run_error = None;

        loop {
            // If shutdown has been initiated, complete it and exit.
            if self.lifecycle.phase() == ShutdownPhase::Stopping {
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
            let drained_at = self.refresh_app_clock();
            if self.app.expire_due_metadata(drained_at) {
                render_demand.join(RenderDemand::Full);
            }

            // 3. Drain API requests.
            if self.drain_api_requests_with_shutdown_check() {
                render_demand.join(RenderDemand::Full);
            }
            if self.lifecycle.stop_requested(self.app.state.should_quit) {
                continue;
            }

            self.app.sync_focus_events();
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

            self.poll_pending_alt_screen_reads(now);
            if self.process_deferred_alt_screen_reads() {
                self.app.state.mark_shell_projection_dirty();
                render_demand.join(RenderDemand::Full);
            }

            if self.clients.latest_shell_client().is_some() && self.app.ensure_default_workspace() {
                self.immediate_pty_sources_dirty = true;
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
                self.stream_direct_terminal_keyboard_mode();
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
                continue;
            }

            // 7. Wait for next event.
            let next_deadline = self.app.next_headless_loop_deadline_with_git_refresh(
                now,
                render_demand != RenderDemand::None,
                self.has_app_client(),
            );
            let next_deadline = self
                .next_pending_alt_screen_read_deadline()
                .map_or(next_deadline, |pending| {
                    Some(next_deadline.map_or(pending, |current| current.min(pending)))
                });
            let next_deadline = self
                .shell_cwd_refresh_deadline()
                .map_or(next_deadline, |cwd| {
                    Some(next_deadline.map_or(cwd, |current| current.min(cwd)))
                });
            let event = {
                tokio::select! {
                    maybe_api = self.app.api_rx.recv() => match maybe_api {
                        Some(msg) => LoopEvent::Api(Box::new(msg)),
                        None => LoopEvent::Timer,
                    },
                    maybe_reload = self.agent_manifest_reload_rx.recv() => match maybe_reload {
                        Some(completion) => LoopEvent::AgentManifestReload(completion),
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
                    _ = self.app.render_notify.notified() => LoopEvent::RenderRequested,
                    ready = client_listener_ready.readable() => {
                        match ready {
                            Ok(mut guard) => {
                                guard.clear_ready();
                                LoopEvent::ClientListenerReady
                            }
                            Err(err) => return Err(err),
                        }
                    },
                }
            };
            // The wait above can last until the next deadline; dispatch reads
            // the time the event arrived, not the time the wait began.
            self.refresh_app_clock();

            if self.lifecycle.stop_requested(self.app.state.should_quit) {
                self.initiate_shutdown();
                match event {
                    LoopEvent::Internal(ev) => {
                        self.handle_internal_event_with_forwarding(ev);
                    }
                    LoopEvent::ServerEvent(
                        ServerEvent::ClientConnected {
                            client_id, writer, ..
                        }
                        | ServerEvent::ClientShellConnected {
                            client_id, writer, ..
                        },
                    ) => {
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
                LoopEvent::AgentManifestReload(completion) => {
                    if self.complete_agent_manifest_reload(completion) {
                        self.app.state.mark_shell_projection_dirty();
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
                LoopEvent::ClientListenerReady => {
                    if let Err(err) = self.accept_client_connections() {
                        run_error = Some(err);
                        self.initiate_shutdown();
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
        self.app.retire_session_writer();
        self.release_sockets_after_save();

        info!("headless server exiting");
        run_error.map_or(Ok(()), Err)
    }

    /// Re-applies the foreground client's tab geometry when it controls that
    /// tab. The foreground client is always an active shell connection
    /// (`promote_client_to_foreground` and registry shell selection admit nothing
    /// else), so there is no whole-session resize path: pane geometry is owned
    /// per tab by its shell controller.
    fn resize_foreground_shell_tab_if_controller(&mut self, start_pending_agent_resumes: bool) {
        if let Some(client_id) = self.clients.foreground_client_id() {
            self.resize_shell_tab_if_controller(client_id, start_pending_agent_resumes);
        }
    }

    fn sync_runtime_view_geometry(&mut self) {
        self.app.state.view = crate::ui::compute_view(
            &self.app.state,
            &self.app.terminal_runtimes,
            Rect::new(
                0,
                0,
                self.effective_size.cols.get(),
                self.effective_size.rows.get(),
            ),
        );
    }

    fn sync_foreground_client_state(&mut self) {
        let foreground_client_id = self.clients.foreground_client_id();
        self.app.pixel_mouse_available = foreground_client_id.is_some_and(|id| {
            self.clients
                .get(&id)
                .is_some_and(|client| client.pixel_mouse)
        });
        let Some(client_id) = foreground_client_id else {
            self.effective_size = self.app.state.settings.headless_size;
            self.app.state.outer_terminal_focus = None;
            self.app.state.host_cell_size =
                shepr_termio::host_term::cell_size::HostCellSize::default();
            self.sync_runtime_view_geometry();
            return;
        };
        let Some(client) = self.clients.get(&client_id) else {
            self.clients.set_foreground_client_id(None);
            self.effective_size = self.app.state.settings.headless_size;
            self.app.state.outer_terminal_focus = None;
            self.app.state.host_cell_size =
                shepr_termio::host_term::cell_size::HostCellSize::default();
            self.sync_runtime_view_geometry();
            return;
        };
        let Some(shell) = client.shell_state() else {
            self.clients.set_foreground_client_id(None);
            self.effective_size = self.app.state.settings.headless_size;
            self.app.state.outer_terminal_focus = None;
            self.app.state.host_cell_size =
                shepr_termio::host_term::cell_size::HostCellSize::default();
            self.sync_runtime_view_geometry();
            return;
        };

        let terminal_size = client.terminal_size;
        let host_cell_size = client.cell_size.or_default();
        let host_terminal_theme = shell.host_terminal_theme;
        let host_terminal_appearance = shell.host_terminal_appearance;
        let host_terminal_appearance_explicit = shell.host_terminal_appearance_explicit;

        self.effective_size = terminal_size;
        self.sync_runtime_view_geometry();
        self.app.state.host_cell_size = host_cell_size;
        self.sync_foreground_focus_state();
        self.app.set_host_terminal_appearance_state(
            host_terminal_appearance,
            host_terminal_appearance_explicit,
        );
        self.app.set_host_terminal_theme(host_terminal_theme);
    }

    /// Mirrors the foreground client's outer-terminal focus into `AppState`.
    ///
    /// This is all agent state and hook reports need before they are applied:
    /// they change neither client geometry nor layout, so they do not rerun
    /// `compute_view` through the full
    /// `sync_foreground_client_state`.
    fn sync_foreground_focus_state(&mut self) {
        let Some(client) = self
            .clients
            .foreground_client_id()
            .and_then(|client_id| self.clients.get(&client_id))
        else {
            self.app.state.outer_terminal_focus = None;
            return;
        };
        self.app.state.outer_terminal_focus = client
            .shell_state()
            .and_then(|shell| shell.outer_terminal_focus);
    }

    fn promote_client_to_foreground(&mut self, client_id: ClientId) -> bool {
        // Only an active shell connection may drive session-wide presentation;
        // a direct terminal stream never becomes the foreground client.
        let changed = self.clients.promote_to_foreground(client_id);
        if !self
            .clients
            .get(&client_id)
            .is_some_and(crate::server::clients::ClientConnection::is_active_shell_client)
        {
            return false;
        }
        self.sync_foreground_client_state();
        changed
    }

    fn promote_latest_remaining_client(&mut self) -> bool {
        let changed = self.clients.promote_latest_remaining();
        self.sync_foreground_client_state();
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
        let disconnected_focus = self
            .clients
            .get(&client_id)
            .filter(|client| {
                client.is_active_shell_client()
                    && client
                        .shell_state()
                        .is_some_and(|shell| shell.outer_terminal_focus == Some(true))
            })
            .and_then(|_| self.shell_focus_target(client_id));
        let should_release_focus = disconnected_focus.as_ref().is_some_and(|target| {
            !self.clients.iter().any(|(&other_id, client)| {
                other_id != client_id
                    && client.is_active_shell_client()
                    && client
                        .shell_state()
                        .is_some_and(|shell| shell.outer_terminal_focus == Some(true))
                    && self.shell_tab_id_for_client(other_id).as_deref()
                        == Some(target.tab_id.as_str())
            })
        });
        let (removed, was_foreground) = self.clients.remove_client(client_id);
        if let Some(mut removed) = removed {
            let held_inputs = removed.drain_shell_held_inputs();
            self.release_client_shell_inputs(client_id, held_inputs);
            if let ClientConnectionMode::TerminalAttach { terminal_id, .. } = removed.mode {
                self.app
                    .state
                    .direct_attach_resize_locks
                    .remove(&terminal_id);
            }
        }
        if should_release_focus && let Some(target) = disconnected_focus.as_ref() {
            self.send_shell_focus_target(target, shepr_vt::FocusEvent::Lost);
        }
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
        let was_terminal_attach = self.clients.get(&client_id).is_some_and(|client| {
            matches!(
                &client.mode,
                crate::server::clients::ClientConnectionMode::TerminalAttach { .. }
            )
        });
        let restore_shell_controller = self.clients.get(&client_id).and_then(|client| {
            let ClientConnectionMode::TerminalAttach { terminal_id, .. } = &client.mode else {
                return None;
            };
            self.shell_geometry_controller_for_terminal(terminal_id.as_str())
        });
        let was_shell_client = self
            .clients
            .get(&client_id)
            .is_some_and(crate::server::clients::ClientConnection::is_active_shell_client);
        self.remove_client(client_id);
        if let Some((controller_id, target)) = restore_shell_controller {
            self.restore_shell_tab_geometry(controller_id, target);
        } else if self.has_app_client() {
            // Removing the client dropped its geometry controller mappings.
            // Hand each tab it controlled to a remaining viewer and resize
            // to that viewer, so no pane keeps the departed client's size.
            self.reapply_controlled_shell_tab_geometry(true);
        } else if was_shell_client || was_terminal_attach {
            // With no shell surfaces, every departing client releases its
            // geometry. Only a shell departure also settles pending resumes.
            self.resize_tabs_to_headless_size(was_shell_client);
        }
    }

    /// Accepts pending client connections from the non-blocking listener.
    fn accept_client_connections(&mut self) -> io::Result<()> {
        accept_pending_client_connections(
            &self.client_listener,
            &mut self.clients,
            self.app.paths.session_id(),
            self.lifecycle.stop_request_flag(),
            &self.server_event_tx,
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

    async fn reject_late_client_connections(&mut self) {
        self.server_event_rx.close();
        while let Some(event) = self.server_event_rx.recv().await {
            if let ServerEvent::ClientConnected {
                client_id, writer, ..
            }
            | ServerEvent::ClientShellConnected {
                client_id, writer, ..
            } = event
                && let Ok(message) = Self::frame_server_message(&ServerMessage::ServerShutdown {
                    reason: Some(shepr_protocol::ShutdownReason::Message(
                        "server is shutting down".to_owned(),
                    )),
                })
            {
                // A closed writer means the client already left; there is
                // nothing to flush for it.
                if writer.control.send(message).is_err() {
                    debug!(?client_id, "late client left before its shutdown notice");
                } else {
                    self.shutdown_flushes.push(writer.flush());
                }
            }
        }
    }

    fn handle_terminal_attach_scroll(
        &mut self,
        client_id: ClientId,
        source: AttachScrollSource,
        direction: AttachScrollDirection,
        lines: u16,
        column: Option<u16>,
        row: Option<u16>,
        modifiers: shepr_protocol::WireModifiers,
    ) -> bool {
        let Some(ClientConnection {
            mode: ClientConnectionMode::TerminalAttach { terminal_id, .. },
            ..
        }) = self.clients.get(&client_id)
        else {
            return false;
        };
        let Some(runtime) = self.app.terminal_runtimes.get(terminal_id) else {
            return false;
        };

        let result = apply_terminal_attach_scroll(
            runtime,
            source,
            direction,
            lines,
            column,
            row,
            modifiers.bits(),
        );
        if let Err(err) = &result {
            warn!(?client_id, terminal_id = %terminal_id, error = %err, "terminal attach scroll failed");
        }
        self.report_terminal_attach_input(client_id, AttachInputDelivery::of(&result));
        true
    }

    fn handle_terminal_attach_mouse(
        &mut self,
        client_id: ClientId,
        kind: shepr_protocol::ClientMouseKind,
        position: shepr_protocol::ClientMousePosition,
        geometry: Option<shepr_protocol::ClientMouseGeometry>,
        modifiers: shepr_protocol::WireModifiers,
        lines: u16,
    ) -> bool {
        let Some(client) = self.clients.get(&client_id) else {
            return false;
        };
        let ClientConnectionMode::TerminalAttach { terminal_id, .. } = &client.mode else {
            return false;
        };
        let terminal_id = terminal_id.clone();
        let terminal_size = client.terminal_size;
        let cell_size = client.cell_size;
        let pixel_mouse = client.pixel_mouse;
        let host_sgr_pixels_active = client.host_sgr_pixels_active == Some(true);
        let Some(runtime) = self.app.terminal_runtimes.get(&terminal_id) else {
            return false;
        };
        let Some(position) = terminal_attach_mouse_position(
            runtime,
            terminal_size,
            cell_size,
            pixel_mouse,
            host_sgr_pixels_active,
            position,
            geometry,
        ) else {
            return false;
        };
        let event = shepr_protocol::ClientPaneInputEvent::Mouse {
            kind,
            position,
            geometry: None,
            modifiers,
            lines: lines.max(1),
        };
        let result = apply_client_pane_input_events(runtime, &[event]);
        if let Err(err) = &result {
            warn!(?client_id, terminal_id = %terminal_id, error = %err, "terminal attach mouse input failed");
        }
        self.report_terminal_attach_input(client_id, AttachInputDelivery::of_batch(&result));
        true
    }

    /// Tells a direct terminal-attach client when its input stops reaching
    /// the pane because the pane's PTY queue is full (the child is not
    /// reading). Losing keystrokes silently is worse than a visible notice,
    /// but one is enough: the notice is sent on the first drop and re-armed
    /// by the next input that gets through. Runs per attach input event, so
    /// the delivered case is one map lookup and a flag store.
    fn report_terminal_attach_input(&mut self, client_id: ClientId, delivery: AttachInputDelivery) {
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        let terminal_id = match &client.mode {
            ClientConnectionMode::TerminalAttach { terminal_id, .. } => terminal_id.clone(),
            ClientConnectionMode::ClientShell(_) | ClientConnectionMode::TerminalPending => {
                return;
            }
        };
        match delivery {
            AttachInputDelivery::Delivered => {
                if let Some(state) = client.terminal_attach_state_mut() {
                    state.input_drop_reported = false;
                }
                return;
            }
            // The pane is going away or the input was malformed; the log at
            // the call site covers it, and a closing pane ends the attach
            // with its own shutdown message.
            AttachInputDelivery::Failed => return,
            AttachInputDelivery::Dropped => {}
        }
        let Some(state) = client.terminal_attach_state_mut() else {
            return;
        };
        if std::mem::replace(&mut state.input_drop_reported, true) {
            return;
        }
        self.send_to_client(
            client_id,
            &ServerMessage::DirectTerminalNotice {
                kind: shepr_protocol::NoticeKind::InputDropped {
                    terminal_id: terminal_id.clone(),
                },
            },
        );
    }

    /// Pulls only titles reported dirty by the PTY parser. A focused pane title
    /// is forwarded as an independent client side effect. Any changed title
    /// also updates the shell agent metadata, so it requires a projection.
    fn sync_terminal_title_sources(
        &mut self,
        sources: &HashSet<shepr_core::layout::PaneId>,
    ) -> (bool, bool) {
        let focused_source = self
            .foreground_window_title_target()
            .or_else(|| self.default_shell_target())
            .and_then(|target| {
                let (workspace_index, tab_index) = target.resolve(&self.app.state)?;
                self.app
                    .state
                    .workspaces
                    .get(workspace_index)?
                    .tabs()
                    .get(tab_index)
            })
            .map(|tab| tab.layout().focused())
            .is_some_and(|pane_id| sources.contains(&pane_id));
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

    fn foreground_window_title_target(&self) -> Option<crate::ui::TabSurfaceTarget> {
        self.clients
            .foreground_client_id()
            .filter(|client_id| {
                self.clients
                    .get(client_id)
                    .is_some_and(ClientConnection::is_active_shell_client)
            })
            .and_then(|client_id| self.shell_target_for_client(client_id))
    }

    /// Renders `ui.window_title` against the foreground client view. `None` means
    /// window titles are disabled or every token resolved empty, which leaves
    /// the client on Shepr's default title.
    fn configured_window_title(&self) -> Option<String> {
        self.foreground_window_title_target()
            .map_or_else(
                || self.app.window_title(),
                |target| {
                    target
                        .resolve(&self.app.state)
                        .and_then(|(workspace_index, tab_index)| {
                            self.app.window_title_for(workspace_index, tab_index)
                        })
                },
            )
            .and_then(|title| shepr_config::sanitize_window_title_text(&title))
    }

    /// Pushes the configured outer window title to the foreground client when it
    /// changed. Shepr consumes each pane's own `OSC 0`/`OSC 2`, so without this
    /// the host terminal title never follows the session - which is what window
    /// managers read for tab and group bar labels.
    fn sync_window_title(&mut self) {
        let title = match &self.api_window_title {
            Some(title) => Some(title.clone()),
            None if self.app.window_title_configured() => self.configured_window_title(),
            None => return,
        };
        if let (Some(client_id), Some((sent_client_id, sent_title))) = (
            self.clients.foreground_client_id(),
            self.sent_window_title.as_ref(),
        ) && *sent_client_id == client_id
            && *sent_title == title
        {
            return;
        }
        self.send_window_title(title);
    }

    /// Sends a window title and remembers it only when a foreground client took
    /// it, so the next client to attach is written to rather than skipped.
    fn send_window_title(&mut self, title: Option<String>) -> bool {
        let Some(client_id) = self.clients.foreground_client_id() else {
            self.sent_window_title = None;
            return false;
        };
        // `send_to_client` reports false for a missing or writer-less client,
        // so nothing is cached against a client that never got the title.
        let sent = self.send_to_client(
            client_id,
            &ServerMessage::WindowTitle {
                title: title.clone(),
            },
        );
        self.sent_window_title = sent.then_some((client_id, title));
        sent
    }

    fn handle_client_window_title_api(
        &mut self,
        title: Option<String>,
    ) -> shepr_api::error::ApiResult {
        use shepr_api::schema::{ClientWindowTitleReason, ResponseResult};

        let title = match title {
            Some(title) => match shepr_config::sanitize_window_title_text(&title) {
                Some(title) => Some(title),
                None => {
                    return Err(shepr_api::error::ApiError::new(
                        shepr_api::error::ApiErrorCode::InvalidParams,
                        "window title is empty",
                    ));
                }
            },
            None => None,
        };
        let set_title = title.is_some();
        // An explicit title suppresses `ui.window_title` until it is cleared,
        // and clearing restores the configured title rather than only "shepr".
        self.api_window_title = title.clone();
        let title = title.or_else(|| self.configured_window_title());
        let changed = self.send_window_title(title);
        let reason = match (changed, set_title) {
            (true, true) => ClientWindowTitleReason::Set,
            (true, false) => ClientWindowTitleReason::Cleared,
            (false, _) => ClientWindowTitleReason::NoForegroundClient,
        };
        Ok(ResponseResult::ClientWindowTitle { changed, reason })
    }

    /// Encodes a server message into a length-prefixed frame.
    ///
    /// A payload over `MAX_FRAME_SIZE` fails with `FramingError::Oversized`:
    /// `shepr_protocol::write_message` refuses it before writing anything, since
    /// every reader would drop the connection on such a frame.
    fn frame_server_message(msg: &ServerMessage) -> Result<Vec<u8>, shepr_protocol::FramingError> {
        shepr_protocol::encode_frame(msg)
    }

    /// Sends a message to all connected clients.
    /// Broken connections are tracked and cleaned up.
    ///
    /// Each client gets its own copy of the framed bytes. That is deliberate:
    /// the only callers are the two shutdown notices (a few dozen bytes, once
    /// per server lifetime). Render output never goes through here; every
    /// client's frame or patch is diffed against that client's own baseline
    /// (`render_and_stream`, `render_retained_pane_surface_and_stream`), so
    /// there is no shared frame to hand out. Making the writer queue carry
    /// `Arc<[u8]>` would add a refcount to every per-client render send to
    /// save one tiny copy here.
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

    fn shutdown_terminal_stream_clients(
        &mut self,
        terminal_id: &shepr_protocol::TerminalId,
        reason: &str,
    ) {
        let client_ids = terminal_stream_client_ids(&self.clients, terminal_id);

        for client_id in client_ids {
            self.send_to_client(
                client_id,
                &ServerMessage::ServerShutdown {
                    reason: Some(shepr_protocol::ShutdownReason::Message(reason.to_owned())),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
        }
    }

    fn send_terminal_stream_detach_shutdown(&mut self, client_id: ClientId) {
        if matches!(
            self.clients.get(&client_id).map(|client| &client.mode),
            Some(ClientConnectionMode::TerminalAttach { .. })
        ) {
            self.send_to_client(
                client_id,
                &ServerMessage::ServerShutdown {
                    reason: Some(shepr_protocol::ShutdownReason::Detached),
                },
            );
        }
    }

    fn attach_terminal_client(
        &mut self,
        client_id: ClientId,
        terminal_id: &shepr_protocol::TerminalId,
        takeover: bool,
    ) -> bool {
        if !self.client_is_pending_terminal_mode(client_id) {
            self.send_to_client(
                client_id,
                &ServerMessage::ServerShutdown {
                    reason: Some(shepr_protocol::ShutdownReason::Message(
                        "terminal attach failed: connection is not pending terminal attach"
                            .to_owned(),
                    )),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
            return false;
        }

        if !self.app.state.terminals.contains_key(terminal_id) {
            self.send_to_client(
                client_id,
                &ServerMessage::ServerShutdown {
                    reason: Some(shepr_protocol::ShutdownReason::Message(format!(
                        "terminal attach failed: terminal {terminal_id} not found"
                    ))),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
            return false;
        }

        let real_terminal_id = terminal_id.clone();

        if self.has_pending_read_for(real_terminal_id.as_str()) {
            self.send_to_client(
                client_id,
                &ServerMessage::ServerShutdown {
                    reason: Some(shepr_protocol::ShutdownReason::Message(format!(
                        "terminal attach failed: terminal {terminal_id} has a read in progress; retry"
                    ))),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
            return false;
        }

        match self
            .clients
            .attach_claim(&real_terminal_id, client_id, takeover)
        {
            AttachClaim::Reject { .. } => {
                self.send_to_client(
                    client_id,
                    &ServerMessage::ServerShutdown {
                        reason: Some(shepr_protocol::ShutdownReason::Message(format!(
                            "terminal attach failed: terminal {terminal_id} already has an attached client; retry with --takeover"
                        ))),
                    },
                );
                self.remove_client_and_resize_if_needed(client_id);
                return false;
            }
            AttachClaim::Takeover { owner } => {
                self.send_to_client(
                    owner,
                    &ServerMessage::ServerShutdown {
                        reason: Some(shepr_protocol::ShutdownReason::Message(
                            "terminal attach taken over".to_owned(),
                        )),
                    },
                );
                self.remove_client_and_resize_if_needed(owner);
            }
            AttachClaim::Available | AttachClaim::AlreadyOwned => {}
        }

        let stamp = self.clients.allocate_activity_stamp();
        let was_foreground = self.clients.foreground_client_id() == Some(client_id);
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        let (cols, rows) = (
            client.terminal_size.cols.get(),
            client.terminal_size.rows.get(),
        );
        let cell_size = client.cell_size;
        let transitioned = client.attach_to_terminal(real_terminal_id.clone());
        if !transitioned {
            return false;
        }
        client.render_state.reset_baseline();
        client.last_activity = stamp;
        if was_foreground {
            self.promote_latest_remaining_client();
        }

        info!(?client_id, cols, rows, terminal_id = %terminal_id, "terminal attach client connected");
        self.clients
            .set_attach_owner(real_terminal_id.clone(), client_id);
        self.app
            .state
            .direct_attach_resize_locks
            .insert(real_terminal_id.clone());
        self.app
            .start_pending_agent_resume_for_terminal(&real_terminal_id, rows, cols, true);
        if let Some(runtime) = self.app.terminal_runtimes.get(&real_terminal_id) {
            runtime.resize(shepr_core::geometry::PaneGeometry::new(
                cols,
                rows,
                cell_size.width_px,
                cell_size.height_px,
            ));
        }
        true
    }

    fn client_is_pending_terminal_mode(&self, client_id: ClientId) -> bool {
        self.clients
            .get(&client_id)
            .is_some_and(|client| matches!(client.mode, ClientConnectionMode::TerminalPending))
    }

    /// Handles a server event. Returns true if the event requires a re-render.
    fn handle_server_event(&mut self, ev: ServerEvent) -> bool {
        self.immediate_pty_sources_dirty = true;
        match ev {
            ServerEvent::ClientConnected {
                client_id,
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                pixel_mouse,
                writer,
            } => {
                info!(
                    ?client_id,
                    cols, rows, cell_width_px, cell_height_px, "direct terminal client connected"
                );
                let last_activity = self.clients.allocate_activity_stamp();
                let observed = shepr_termio::host_term::cell_size::HostCellSize {
                    width_px: cell_width_px,
                    height_px: cell_height_px,
                };
                let pixel_mouse = pixel_mouse && observed.is_known();
                let mut connection = ClientConnection::new_with_mode(
                    ClientConnectionMode::TerminalPending,
                    shepr_core::geometry::GridSize::clamped(cols, rows),
                    observed,
                    last_activity,
                    shepr_protocol::RenderEncoding::TerminalAnsi,
                    Some(writer),
                );
                connection.pixel_mouse = pixel_mouse;
                self.clients.insert(client_id, connection);
                false
            }
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
                    render_encoding = ?shepr_protocol::RenderEncoding::SemanticFrame,
                    "client connected"
                );
                self.app.ensure_default_workspace();
                let first_app_client = self.app_client_count() == 0;
                let last_activity = self.clients.allocate_activity_stamp();
                let observed = shepr_termio::host_term::cell_size::HostCellSize {
                    width_px: cell_width_px,
                    height_px: cell_height_px,
                };
                let mut connection = ClientConnection::new_with_mode(
                    ClientConnectionMode::shell(),
                    shepr_core::geometry::GridSize::clamped(surface_cols, surface_rows),
                    observed,
                    last_activity,
                    shepr_protocol::RenderEncoding::SemanticFrame,
                    Some(writer),
                );
                connection.pixel_mouse = pixel_mouse && observed.is_known();
                let Some(shell) = connection.shell_state_mut() else {
                    warn!(?client_id, "created shell connection without shell state");
                    return false;
                };
                shell.mouse_capture = mouse_capture;
                shell.surface_active = surface_active;
                shell.projection_revision = shepr_protocol::ProjectionRevision::new(1);
                let seed_snapshot = client_shell_snapshot(
                    &self.app,
                    &self.resolved_config,
                    &self.client_shell_boot_id,
                    shell.projection_revision.get(),
                    None,
                );
                let location =
                    crate::server::clients::ClientShellLocation::from_snapshot(&seed_snapshot);
                let snapshot_message = shepr_protocol::endpoint::snapshot_message(&seed_snapshot);
                shell.location = Some(location);
                shell.snapshot = Some(seed_snapshot);
                shell.session_generation = self.shell_session_generation;
                if let Some(snapshot) = shell.snapshot.as_mut() {
                    // The initial frame carries config; later frames use the connection cache.
                    snapshot.resolved_config.clear();
                }
                self.clients.insert(client_id, connection);
                self.send_to_client(client_id, &snapshot_message);
                if surface_active {
                    self.clients.set_foreground_client_id(Some(client_id));
                }
                if first_app_client {
                    self.app.mark_git_status_refresh_due(self.app.clock.now);
                }
                self.sync_foreground_client_state();
                self.claim_unowned_shell_tab_geometry(client_id, true);
                true
            }
            ServerEvent::ClientAttachTerminal {
                client_id,
                terminal_id,
                takeover,
            } => self.attach_terminal_client(client_id, &terminal_id, takeover),
            ServerEvent::ClientAttachScroll {
                client_id,
                source,
                direction,
                lines,
                column,
                row,
                modifiers,
            } => self.handle_terminal_attach_scroll(
                client_id, source, direction, lines, column, row, modifiers,
            ),
            ServerEvent::ClientAttachMouse {
                client_id,
                kind,
                position,
                geometry,
                modifiers,
                lines,
            } => self.handle_terminal_attach_mouse(
                client_id, kind, position, geometry, modifiers, lines,
            ),
            ServerEvent::ClientInput { client_id, data } => {
                let Some(ClientConnection {
                    mode: ClientConnectionMode::TerminalAttach { terminal_id, .. },
                    ..
                }) = self.clients.get(&client_id)
                else {
                    return false;
                };
                let Some(runtime) = self.app.terminal_runtimes.get(terminal_id) else {
                    return true;
                };
                let result = apply_terminal_attach_input(runtime, data);
                if let Err(err) = &result {
                    warn!(?client_id, terminal_id = %terminal_id, error = %err, "terminal attach input failed");
                }
                self.report_terminal_attach_input(client_id, AttachInputDelivery::of(&result));
                true
            }
            ServerEvent::ClientPasteRejected {
                client_id,
                size,
                max,
            } => {
                let detail = format!("Input message is {size} bytes; Shepr's limit is {max} bytes");
                if matches!(
                    self.clients.get(&client_id).map(|client| &client.mode),
                    Some(ClientConnectionMode::ClientShell(_))
                ) {
                    self.send_to_client(
                        client_id,
                        &ServerMessage::ClientShellError {
                            kind: shepr_protocol::NoticeKind::PasteRejected { size, max },
                        },
                    );
                } else {
                    // Direct attach has no shell chrome, so it gets its own
                    // notice; every rejection is a separate user action, so
                    // each one is reported.
                    warn!(client = ?client_id, %detail, "paste rejected for direct terminal client");
                    self.send_to_client(
                        client_id,
                        &ServerMessage::DirectTerminalNotice {
                            kind: shepr_protocol::NoticeKind::PasteRejected { size, max },
                        },
                    );
                }
                false
            }
            ServerEvent::ClientResize {
                client_id,
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                pixel_mouse,
            } => {
                info!(
                    ?client_id,
                    cols, rows, cell_width_px, cell_height_px, pixel_mouse, "client resize"
                );
                let observed = shepr_termio::host_term::cell_size::HostCellSize {
                    width_px: cell_width_px,
                    height_px: cell_height_px,
                };
                let pixel_mouse = pixel_mouse && observed.is_known();
                let direct_terminal_id = if let Some(ClientConnection {
                    mode: ClientConnectionMode::TerminalAttach { terminal_id, .. },
                    terminal_size,
                    cell_size,
                    pixel_mouse: client_pixel_mouse,
                    render_state,
                    ..
                }) = self.clients.get_mut(&client_id)
                {
                    *terminal_size = shepr_core::geometry::GridSize::clamped(cols, rows);
                    *cell_size = observed;
                    *client_pixel_mouse = pixel_mouse;
                    render_state.request_repaint();
                    Some((terminal_id.clone(), *cell_size))
                } else {
                    None
                };
                if let Some((terminal_id, cell_size)) = direct_terminal_id {
                    if let Some(runtime) = self.app.terminal_runtimes.get(&terminal_id) {
                        runtime.resize(shepr_core::geometry::PaneGeometry::new(
                            cols,
                            rows,
                            cell_size.width_px,
                            cell_size.height_px,
                        ));
                    }
                    return true;
                }
                if let Some(ClientConnection {
                    mode: ClientConnectionMode::TerminalPending,
                    terminal_size,
                    cell_size,
                    pixel_mouse: client_pixel_mouse,
                    render_state,
                    ..
                }) = self.clients.get_mut(&client_id)
                {
                    *terminal_size = shepr_core::geometry::GridSize::clamped(cols, rows);
                    *cell_size = observed;
                    *client_pixel_mouse = pixel_mouse;
                    render_state.request_repaint();
                    return true;
                }
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
                if !matches!(client.mode, ClientConnectionMode::ClientShell(_)) {
                    return false;
                }
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
                if !client.is_active_shell_client() {
                    return false;
                }
                client.request_repaint();
                self.promote_client_to_foreground(client_id);
                self.resize_shell_tab_if_controller(client_id, true);
                true
            }
            ServerEvent::ClientShellHostTheme { client_id, update } => {
                let is_foreground = self.clients.foreground_client_id() == Some(client_id);
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                if !client.is_shell_client() {
                    return false;
                }
                if !client.update_host_theme(&update) {
                    return false;
                }
                let Some(shell) = client.shell_state() else {
                    return false;
                };
                if !shell.surface_active || !is_foreground {
                    return false;
                }
                let appearance = shell.host_terminal_appearance;
                let appearance_explicit = shell.host_terminal_appearance_explicit;
                let theme = shell.host_terminal_theme;
                let mut changed = self
                    .app
                    .set_host_terminal_appearance_state(appearance, appearance_explicit);
                changed |= self.app.set_host_terminal_theme(theme);
                if changed {
                    self.resize_foreground_shell_tab_if_controller(false);
                }
                changed
            }
            ServerEvent::ClientShellFocus { client_id, focused } => {
                let Some(client) = self.clients.get(&client_id) else {
                    return false;
                };
                if !client.is_active_shell_client()
                    || client
                        .shell_state()
                        .is_none_or(|shell| shell.outer_terminal_focus == Some(focused))
                {
                    return false;
                }
                let tab_id = self.shell_tab_id_for_client(client_id);
                let another_focused_viewer = self.clients.iter().any(|(&other_id, client)| {
                    other_id != client_id
                        && client.is_active_shell_client()
                        && client
                            .shell_state()
                            .is_some_and(|shell| shell.outer_terminal_focus == Some(true))
                        && self.shell_tab_id_for_client(other_id) == tab_id
                });
                if let Some(client) = self.clients.get_mut(&client_id)
                    && let Some(shell) = client.shell_state_mut()
                {
                    shell.outer_terminal_focus = Some(focused);
                }
                if focused {
                    self.promote_client_to_foreground(client_id);
                    self.claim_shell_tab_geometry(client_id, false);
                    if !another_focused_viewer
                        && let Some(target) = self.shell_focus_target(client_id)
                    {
                        self.send_shell_focus_target(&target, shepr_vt::FocusEvent::Gained);
                    }
                    true
                } else {
                    if self.clients.foreground_client_id() == Some(client_id) {
                        self.app.state.outer_terminal_focus = Some(false);
                    }
                    if !another_focused_viewer
                        && let Some(target) = self.shell_focus_target(client_id)
                    {
                        self.send_shell_focus_target(&target, shepr_vt::FocusEvent::Lost);
                    }
                    true
                }
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
                if let Some(shell) = client.shell_state_mut() {
                    shell.host_keyboard_report_all_active = None;
                }
                self.sent_window_title = None;
                self.stream_host_mouse_capture_mode();
                self.stream_direct_terminal_keyboard_mode();
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
                    interaction && self.claim_shell_tab_geometry(client_id, false);
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
            ServerEvent::ClientShellEndpointRequestError {
                client_id,
                boot_id,
                request_id,
                code,
                message,
            } => {
                let Some(client) = self.clients.get(&client_id) else {
                    return false;
                };
                if !matches!(client.mode, ClientConnectionMode::ClientShell(_)) {
                    self.remove_client_and_resize_if_needed(client_id);
                    return true;
                }
                let message = crate::server::client_commands::error_message(
                    boot_id, request_id, code, message,
                );
                self.send_to_client(client_id, &message);
                false
            }
            ServerEvent::ClientShellEndpointRequest {
                client_id,
                boot_id,
                request,
            } => self.handle_client_shell_endpoint_request(client_id, boot_id, request),
            ServerEvent::ClientShellEndpointResponseChunkReady {
                client_id,
                boot_id,
                request_id,
                final_chunk,
                data,
            } => {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                if !matches!(client.mode, ClientConnectionMode::ClientShell(_))
                    || !client
                        .shell_state()
                        .is_some_and(|shell| shell.endpoint_command_in_flight)
                    || boot_id != self.client_shell_boot_id
                {
                    return false;
                }
                if final_chunk && let Some(shell) = client.shell_state_mut() {
                    shell.endpoint_command_in_flight = false;
                }
                self.send_to_client(
                    client_id,
                    &ServerMessage::ClientShellEndpointResponseChunk {
                        boot_id,
                        request_id,
                        final_chunk,
                        data,
                    },
                );
                false
            }
            ServerEvent::ClientDetach { client_id } => {
                info!(?client_id, "client detached");
                self.send_terminal_stream_detach_shutdown(client_id);
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
        let pane_input = matches!(ev, ServerEvent::ClientShellPaneInput { .. });
        if self.handle_server_event(ev) {
            if !pane_input {
                self.app.state.mark_shell_projection_dirty();
            }
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
        self.immediate_pty_sources_dirty = true;

        let frozen_alt_screen_read = match self.alt_screen_read_conflict(&msg.request) {
            AltScreenReadConflict::None => None,
            AltScreenReadConflict::Frozen(snapshot) => Some(snapshot),
            AltScreenReadConflict::Defer => {
                self.defer_alt_screen_read_request(msg);
                return false;
            }
        };

        let metadata_expired = self.app.expire_due_metadata(self.app.clock.now);

        match &msg.request.method {
            shepr_api::schema::Method::ClientWindowTitleSet(params) => {
                let response = self.handle_client_window_title_api(Some(params.title.clone()));
                shepr_api::send_api_response(&msg.respond_to, &request_id, method, response);
                return true;
            }
            shepr_api::schema::Method::ClientWindowTitleClear(_) => {
                let response = self.handle_client_window_title_api(None);
                shepr_api::send_api_response(&msg.respond_to, &request_id, method, response);
                return true;
            }
            _ => {}
        }

        let mut changed = metadata_expired;
        changed |= self.drain_all_internal_events_with_forwarding();

        // The full sync (including the view recompute) stays on this path:
        // API handlers read `app.state.view` for directional focus, splits and
        // resume geometry, and an earlier request may have changed the layout
        // without anything cheaper recording that it did.
        self.sync_foreground_client_state();
        if let Some(error) = self.agent_read_not_idle_error(&msg.request) {
            let response = Err(shepr_api::error::ApiError::from_body(error));
            shepr_api::send_api_response(&msg.respond_to, &request_id, method, response);
            return changed;
        }
        let alt_screen_read_spec = self.alt_screen_read_spec(&msg.request);
        if self
            .clients
            .foreground_client_id()
            .is_some_and(|client_id| {
                self.clients.get(&client_id).is_some_and(|client| {
                    matches!(client.mode, ClientConnectionMode::ClientShell(_))
                })
            })
        {
            self.app.state.view.terminal_area = Rect::new(
                0,
                0,
                self.effective_size.cols.get(),
                self.effective_size.rows.get(),
            );
        }
        let outcome = self.app.handle_api_request_with_render(msg.request);
        changed |= outcome.render != RenderDemand::None;
        let mut response = outcome.response;
        if let Some(snapshot) = frozen_alt_screen_read
            && let Ok(shepr_api::schema::ResponseResult::PaneRead { read }) = &mut response
        {
            read.text = snapshot.text;
            read.truncated = snapshot.truncated;
        }
        if let Some(spec) = alt_screen_read_spec
            && let Ok(shepr_api::schema::ResponseResult::PaneRead { read }) = &response
        {
            let pending = crate::server::alt_screen_read::PendingAltScreenRead::start(
                spec.terminal_id,
                request_id.clone().into(),
                msg.respond_to,
                response.clone(),
                read.clone(),
                spec.lines,
                spec.unwrap,
                spec.initial,
                spec.content_seq,
                self.app.clock.now,
            );
            self.push_pending_alt_screen_read(pending);
            return changed;
        }
        shepr_api::send_api_response(&msg.respond_to, &request_id, method, response);

        if self.clients.latest_shell_client().is_some() {
            changed |= self.app.ensure_default_workspace();
        }

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

        if self.app.session_saver.is_due(now) {
            self.app.start_background_session_save();
        }

        self.sync_host_shutdown_freeze(now);
        let pane_exit_checkpoint_ready = self.app.take_pane_exit_checkpoint_ready();
        if pane_exit_checkpoint_ready
            || (!self.app.policy.persists_session() && !self.app.pane_exit_checkpoint_requested())
        {
            let queued = self.pending_checkpointed_pane_exits.len();
            for _ in 0..queued {
                if let Some(ev) = self.pending_checkpointed_pane_exits.pop_front() {
                    changed |= self.handle_internal_event_with_forwarding(ev);
                }
            }
        }

        if let Some(deadline) = self
            .app
            .agent_metadata_deadline
            .filter(|deadline| now >= *deadline)
        {
            self.app.expire_metadata_at(deadline, now);
            changed = true;
        }

        changed |= self.app.handle_tab_bar_status_tasks(now);

        // A pending render says nothing about geometry: PTY output from any pane,
        // hidden ones included, sets it. Geometry changes run through the client
        // resize/claim paths, which settle the resume deadline themselves. Gating
        // on "render pending" here used to clear the theme-wait deadline, so a
        // pane printing at least every theme-wait interval postponed the first
        // restored agent indefinitely.
        self.app.sync_pending_agent_resume_deadline(now);
        changed |= self.app.expire_due_agent_resume_names(now);
        changed |= self
            .app
            .start_pending_agent_resumes(now, self.app.pending_agent_resume_due(now));
        changed
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
        self.cleanup_sockets();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Installs the SIGINT/SIGTERM/SIGHUP handler (ctrlc's `termination`
/// feature). It marks the quit as signal-driven, sets the stop request flag, and
/// wakes up the event loop by sending a QuitSignal on the server event channel.
///
/// Failing to install it is an error: without it a signal kills the server
/// without the shutdown sequence that saves the session.
fn ctrlc_handler(
    stop_requested: Arc<AtomicBool>,
    signal_quit: Arc<AtomicBool>,
    server_event_tx: mpsc::Sender<ServerEvent>,
) -> io::Result<()> {
    ctrlc::set_handler(move || {
        // Before the stop request, so the loop never sees the quit without it.
        signal_quit.store(true, Ordering::Release);
        stop_requested.store(true, Ordering::Release);
        // Wake up the event loop so the quit flag is checked promptly. Only a
        // wakeup, the flags above carry the quit: a full channel already
        // wakes the loop, and a closed one means the loop has exited.
        let (Ok(())
        | Err(mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_))) =
            server_event_tx.try_send(ServerEvent::QuitSignal);
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
