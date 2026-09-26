//! Headless server mode - runs the shepr event loop without a real terminal.
//!
//! The server:
//! - Does not enter raw mode or read stdin
//! - Creates and listens on both `shepr.sock` (existing JSON API) and
//!   `shepr-client.sock` (new binary protocol)
//! - Initializes AppState and all PTYs from session restore or fresh state
//! - Runs the main event loop (drain events, drain API requests, scheduled tasks)
//! - Renders to a virtual ratatui Buffer in memory
//! - Accepts client connections on the client socket
//! - Streams frames to connected clients after each render
//! - Routes client input events through the existing input pipeline
//! - Continues running after client disconnect
//! - Handles stale socket cleanup, explicit server stop, minimum terminal size,
//!   and pane spawn failure during restore

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use interprocess::local_socket::ListenerNonblockingMode;
use interprocess::local_socket::traits::Listener as _;
use ratatui::layout::Rect;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use base64::Engine;

use crate::api;
use crate::app;
use crate::config;
use crate::events::AppEvent;
#[cfg(test)]
use crate::ipc::bind_local_listener;
use crate::ipc::{
    LocalListener, SocketFileIdentity, remove_socket_file_if_owned, socket_file_identity,
};
use crate::protocol::{self, AttachScrollDirection, AttachScrollSource, FrameData, ServerMessage};
use crate::server::client_accept::accept_pending_client_connections;
use crate::server::client_shell::{
    render_pane_surface as render_client_shell_pane_surface,
    snapshot_with_completions as client_shell_snapshot,
};
use crate::server::client_transport::ServerEvent;
use crate::server::clients::{
    ClientConnection, ClientConnectionMode, DeferredRender, latest_shell_client, render_targets,
    terminal_stream_client_ids,
};
use crate::server::pane_input::{
    apply_client_pane_input_events, apply_terminal_attach_input, apply_terminal_attach_scroll,
    terminal_attach_mouse_position,
};
use crate::server::socket_paths::{client_socket_path, prepare_socket_path};

mod bootstrap;
mod client_views;
mod endpoint_requests;
mod internal_events;
mod lifecycle;
mod render;
mod retained_surface;
mod surface_interest;

pub use bootstrap::run_server;
use lifecycle::HostShutdownFreeze;

#[cfg(test)]
use crate::protocol::MAX_FRAME_SIZE;
#[cfg(test)]
use crate::protocol::RenderEncoding;
#[cfg(test)]
use crate::server::client_transport::ClientWriter;
#[cfg(test)]
use std::fs;

// ---------------------------------------------------------------------------
// Loop event enum for the headless server event loop
// ---------------------------------------------------------------------------

/// Events that the headless server event loop can process.
enum LoopEvent {
    Timer,
    Internal(AppEvent),
    Api(Box<api::ApiRequestMessage>),
    ServerEvent(ServerEvent),
    RenderRequested,
}

/// Presentation work caused by a server event.
///
/// Keeping this classification separate from event delivery makes the input
/// contract explicit: events may reach a PTY without necessarily repainting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderImpact {
    None,
    Full,
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

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// How often the idle headless loop wakes to poll the local listener for new
/// client connections.
///
/// The listener is non-blocking and not integrated into `tokio::select!`, so
/// a low-frequency wake is required to notice new thin-client attaches while
/// otherwise idle. Keep this much slower than the old resize-poll cadence to
/// avoid reintroducing the idle CPU spin.
const CLIENT_ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// Headless server
// ---------------------------------------------------------------------------

struct AltScreenReadSpec {
    terminal_id: crate::terminal::TerminalId,
    lines: usize,
    unwrap: bool,
    initial: crate::terminal::ScreenSnapshot,
    content_seq: u64,
}

enum AltScreenReadConflict {
    None,
    Frozen(crate::pane::TerminalReadSnapshot),
    Defer,
}

/// The headless server - runs the shepr event loop without a real terminal.
pub struct HeadlessServer {
    app: app::App,
    /// Kept alive only for its `Drop` impl, which tears down the JSON API socket server.
    _api_server: Option<api::ServerHandle>,
    client_listener: LocalListener,
    client_socket_path: PathBuf,
    client_socket_identity: SocketFileIdentity,
    clients: HashMap<u64, ClientConnection>,
    next_client_id: u64,
    /// The client currently driving session-wide host presentation and side effects.
    foreground_client_id: Option<u64>,
    /// Ephemeral shell connection controlling PTY geometry for each stable tab id.
    tab_geometry_controllers: HashMap<String, u64>,
    /// Process-local identity used to reject shell replacements from an earlier server boot.
    client_shell_boot_id: String,
    /// Outer window title last pushed, paired with the client that received it.
    /// Keying on the client means a newly attached terminal is written to even
    /// when the title itself has not changed, without every code path that
    /// changes the foreground client having to remember to invalidate this.
    sent_window_title: Option<(u64, Option<String>)>,
    /// Window title set through `client.window_title.set`. While present it wins
    /// over the configured `ui.window_title` until the API clears it again.
    api_window_title: Option<String>,
    /// Full server config warning shown to shell clients that use the server's
    /// (endpoint) keybindings. Config is read once at launch, so this and the
    /// variant below are fixed for the server's lifetime; each shell snapshot
    /// picks one per client.
    server_config_diagnostic: Option<String>,
    /// Server config warning with keybinding diagnostics removed for local-keybinding clients.
    server_config_diagnostic_without_keybindings: Option<String>,
    /// Writable direct attach owner per terminal id string.
    terminal_attach_owners: HashMap<String, u64>,
    /// Deferred application-history reads currently driving alternate-screen viewports.
    pending_alt_screen_reads: Vec<crate::server::alt_screen_read::PendingAltScreenRead>,
    /// Reads waiting for an alternate-screen traversal of the same terminal to finish.
    deferred_alt_screen_reads: Vec<api::ApiRequestMessage>,
    /// Monotonic activity counter used to pick the most recently active client.
    next_activity_stamp: u64,
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
    /// Configured virtual terminal size used when no clients are connected.
    headless_size: (u16, u16),
    /// Shared pane runtime size derived from the foreground client, or the
    /// configured headless size when no clients are connected.
    effective_size: (u16, u16),
    /// Flag set when shutdown is initiated.
    shutting_down: bool,
    /// Flag set by Ctrl+C or `server stop` signal.
    should_quit: Arc<AtomicBool>,
    /// Set by the host shutdown monitor on logind's `PrepareForShutdown(true)`.
    /// See `sync_host_shutdown_freeze` for what the server does with it and
    /// what the monitor is expected to do in return.
    host_shutdown_requested: Arc<AtomicBool>,
    /// Present from the host shutdown warning until the shutdown completes
    /// (the process exits) or is found to be cancelled.
    host_shutdown_freeze: Option<HostShutdownFreeze>,
    /// Watches logind for shutdown warnings; `None` before `run` and while the
    /// server has dropped it to release its delay lock (see
    /// `freeze_for_host_shutdown`).
    host_shutdown_monitor: Option<crate::platform::HostShutdownMonitor>,
    /// Set by the SIGINT/SIGTERM/SIGHUP handler before it sets `should_quit`.
    /// A signal usually arrives as part of an external teardown (logout,
    /// `kill` of the session) that signals the panes at the same time, so pane
    /// deaths seen from then on are not removed from the layout that the final
    /// session save captures. `server stop` does not set it: a deliberate stop
    /// with live panes still applies deaths the user caused just before.
    signal_quit_requested: Arc<AtomicBool>,
    /// Channel for receiving server events from client connection threads.
    server_event_rx: mpsc::Receiver<ServerEvent>,
    /// Sender for server events (cloned for each client thread).
    server_event_tx: mpsc::Sender<ServerEvent>,
}

impl HeadlessServer {
    /// Creates and starts the headless server.
    ///
    /// This:
    /// 1. Prepares the client socket path (cleans up stale sockets)
    /// 2. Binds the client socket listener
    /// 3. Returns the server ready to run
    pub fn new(
        app: app::App,
        config_diagnostics: &[String],
        api_server: Option<api::ServerHandle>,
        should_quit: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let client_path = client_socket_path();
        prepare_socket_path(&client_path)?;

        let listener = bind_owner_only_listener(&client_path)?;
        let client_socket_identity = socket_file_identity(&client_path)?;
        info!(path = %client_path.display(), "client protocol socket listening");

        // Set non-blocking on Unix so we can poll it from the event loop.
        listener.set_nonblocking(ListenerNonblockingMode::Accept)?;

        // Channel for server events from client threads.
        let (server_event_tx, server_event_rx) = mpsc::channel(64);

        let headless_size = app.state.headless_size;
        let (server_config_diagnostic, server_config_diagnostic_without_keybindings) =
            server_config_diagnostic_summaries(config_diagnostics);
        Ok(Self {
            app,
            _api_server: api_server,
            client_listener: listener,
            client_socket_path: client_path,
            client_socket_identity,
            clients: HashMap::new(),
            next_client_id: 1,
            foreground_client_id: None,
            tab_geometry_controllers: HashMap::new(),
            client_shell_boot_id: format!(
                "{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            sent_window_title: None,
            api_window_title: None,
            server_config_diagnostic,
            server_config_diagnostic_without_keybindings,
            terminal_attach_owners: HashMap::new(),
            pending_alt_screen_reads: Vec::new(),
            deferred_alt_screen_reads: Vec::new(),
            next_activity_stamp: 1,
            immediate_pty_sources_dirty: true,
            host_input_modes_dirty: true,
            headless_size,
            effective_size: headless_size,
            shutting_down: false,
            host_shutdown_requested: Arc::new(AtomicBool::new(false)),
            host_shutdown_freeze: None,
            host_shutdown_monitor: None,
            signal_quit_requested: Arc::new(AtomicBool::new(false)),
            should_quit,
            server_event_rx,
            server_event_tx,
        })
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

        // Register SIGINT handler for graceful shutdown.
        let should_quit = Arc::clone(&self.should_quit);
        let signal_quit = Arc::clone(&self.signal_quit_requested);
        let quit_notify = self.server_event_tx.clone();
        ctrlc_handler(should_quit, signal_quit, quit_notify);
        self.start_host_shutdown_monitor();

        let mut needs_render = true;
        let mut needs_full_render = true;
        let mut run_error = None;

        loop {
            // If shutdown has been initiated, complete it and exit.
            if self.shutting_down {
                if let Err(err) = self.complete_shutdown().await {
                    run_error.get_or_insert(err);
                }
                break;
            }

            // A host shutdown warning checkpoints the session and freezes
            // saving; it does not stop the server (see `sync_host_shutdown_freeze`).
            self.sync_host_shutdown_freeze(Instant::now());

            // Check if we should start shutting down. The drain applies queued
            // state and agent-session reports so the final save carries them;
            // after a signal it leaves pane deaths out (see
            // `signal_quit_requested`).
            if self.app.state.should_quit || self.should_quit.load(Ordering::Acquire) {
                self.drain_internal_events_with_forwarding_up_to(
                    crate::app::APP_EVENT_CHANNEL_CAPACITY,
                );
                self.initiate_shutdown();
                continue;
            }

            // 1. Check the coalesced render signal from PTY readers and generic runtime work.
            if self.app.render_dirty.is_pending() {
                needs_render = true;
            }
            // 2. Drain a bounded internal-event batch. API handlers perform an
            // exhaustive forwarding-aware drain before reading pane/runtime state.
            if self.drain_internal_events_with_forwarding() {
                needs_render = true;
                needs_full_render = true;
            }
            if self.should_quit.load(Ordering::Acquire) {
                continue;
            }
            if self.app.expire_due_metadata(Instant::now()) {
                needs_render = true;
                needs_full_render = true;
            }

            // 3. Drain API requests.
            if self.drain_api_requests_with_shutdown_check() {
                needs_render = true;
                needs_full_render = true;
            }
            if self.should_quit.load(Ordering::Acquire) {
                continue;
            }

            self.app.sync_focus_events();
            self.app.sync_session_save_schedule();

            // 4. Accept new client connections.
            if let Err(err) = self.accept_client_connections() {
                run_error = Some(err);
                self.initiate_shutdown();
                continue;
            }

            // 5. Drain server events from client threads.
            if self.drain_server_events() {
                needs_render = true;
                needs_full_render = true;
            }
            if self.should_quit.load(Ordering::Acquire) {
                continue;
            }

            // 6. Handle scheduled tasks.
            let now = Instant::now();
            if self.handle_scheduled_tasks_headless(now) {
                needs_render = true;
                needs_full_render = true;
            }

            self.poll_pending_alt_screen_reads(now);
            if self.process_deferred_alt_screen_reads() {
                needs_render = true;
                needs_full_render = true;
            }

            if latest_shell_client(&self.clients).is_some() && self.app.ensure_default_workspace() {
                self.immediate_pty_sources_dirty = true;
                needs_render = true;
                needs_full_render = true;
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

            // 7. Render virtually and stream frames. Hidden-only PTY work keeps a
            // bounded classification cadence without delaying presentation work
            // that joins the same coalesced request.
            let render_cadence_due = self.app.can_render_now(now);
            if needs_render
                && (render_cadence_due
                    || (self.app.can_present_now(now)
                        && self.has_pending_presentation_work(needs_full_render)))
            {
                let render_request = self.app.render_dirty.take();
                let pty_dirty = !render_request.pty_sources.is_empty();
                if pty_dirty {
                    self.host_input_modes_dirty = true;
                }
                if render_request.generic {
                    needs_full_render = true;
                }
                let (sidebar_title_changed, outer_title_synced) =
                    self.sync_terminal_title_sources(&render_request.terminal_title_sources);
                if sidebar_title_changed {
                    needs_full_render = true;
                }
                if needs_full_render && !outer_title_synced {
                    self.sync_window_title();
                }
                if !needs_full_render && !pty_dirty {
                    // A synchronized-output OSC title can be the only pending work.
                    // Its deferred PTY repaint has its own signal; do not manufacture
                    // a full UI render for this client-local side effect.
                    needs_render = false;
                    continue;
                }
                let hidden_only = pty_dirty
                    && !needs_full_render
                    && !self.pty_sources_visible_to_any_render_target(&render_request.pty_sources);
                if hidden_only {
                    // Hidden-only PTY work keeps a bounded classification cadence
                    // without delaying presentation work that joins the same
                    // coalesced request.
                } else if !needs_full_render
                    && self.render_retained_pane_surface_and_stream(&render_request.pty_sources)
                {
                    // retained pane surface path
                } else {
                    self.render_and_stream();
                }
                self.app.record_render_attempt(now, !hidden_only);
                needs_render = false;
                needs_full_render = false;
                continue;
            }

            // 8. Wait for next event.
            let next_deadline = self
                .app
                .next_headless_loop_deadline_with_git_refresh(
                    now,
                    needs_render,
                    self.has_app_client(),
                )
                .map(|deadline| deadline.min(now + CLIENT_ACCEPT_POLL_INTERVAL))
                .or(Some(now + CLIENT_ACCEPT_POLL_INTERVAL));
            let next_deadline = self
                .pending_alt_screen_reads
                .iter()
                .map(crate::server::alt_screen_read::PendingAltScreenRead::next_deadline)
                .fold(next_deadline, |deadline, pending| {
                    Some(deadline.map_or(pending, |current| current.min(pending)))
                });
            let event = {
                tokio::select! {
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
                    _ = self.app.render_notify.notified() => LoopEvent::RenderRequested,
                }
            };

            if self.should_quit.load(Ordering::Acquire) {
                match event {
                    LoopEvent::Internal(ev) => {
                        self.handle_internal_event_with_forwarding(ev);
                    }
                    LoopEvent::ServerEvent(
                        ServerEvent::ClientConnected { writer, .. }
                        | ServerEvent::ClientShellConnected { writer, .. },
                    ) => {
                        if let Ok(message) =
                            Self::frame_server_message(&ServerMessage::ServerShutdown {
                                reason: Some("server is shutting down".to_owned()),
                            })
                        {
                            let _ = writer.control.send(message);
                        }
                    }
                    // Already dequeued, so the shutdown drain would never see
                    // it; answer it here.
                    LoopEvent::Api(msg) => Self::reject_api_request_for_shutdown(*msg),
                    _ => {}
                }
                continue;
            }

            match event {
                LoopEvent::Timer => {}
                LoopEvent::Internal(ev) => {
                    if self.handle_internal_event_with_forwarding(ev) {
                        needs_render = true;
                        needs_full_render = true;
                    }
                }
                LoopEvent::Api(msg) => {
                    if self.handle_api_request_with_shutdown_check(*msg) {
                        needs_render = true;
                        needs_full_render = true;
                    }
                }
                LoopEvent::ServerEvent(ev) => {
                    if self.handle_server_event_with_render_impact(ev) == RenderImpact::Full {
                        needs_render = true;
                        needs_full_render = true;
                    }
                }
                LoopEvent::RenderRequested => {
                    if self.app.render_dirty.is_pending() {
                        needs_render = true;
                    }
                }
            }
        }

        // Save session on exit. During a host shutdown saving is frozen
        // (`policy.persist_session` is off), so this writes nothing and the
        // checkpoint taken on the warning stands; the writer is still retired.
        if self.app.policy.persist_session
            || self
                .host_shutdown_freeze
                .as_ref()
                .is_some_and(|freeze| freeze.persist_session)
        {
            self.app.save_session_before_teardown();
        }
        self.app.terminal_runtimes.clear();
        if !crate::pane::wait_for_pane_session_teardowns(Duration::from_secs(3)) {
            warn!("pane session teardown did not finish before server exit");
        }
        self.app.retire_session_writer();
        self.release_sockets_after_save()?;

        info!("headless server exiting");
        run_error.map_or(Ok(()), Err)
    }

    fn allocate_activity_stamp(&mut self) -> u64 {
        let stamp = self.next_activity_stamp;
        self.next_activity_stamp = self.next_activity_stamp.saturating_add(1);
        stamp
    }

    /// Re-applies the foreground client's tab geometry when it controls that
    /// tab. The foreground client is always an active shell connection
    /// (`promote_client_to_foreground` and `latest_shell_client` admit nothing
    /// else), so there is no whole-session resize path: pane geometry is owned
    /// per tab by its shell controller.
    fn resize_foreground_shell_tab_if_controller(&mut self, start_pending_agent_resumes: bool) {
        if let Some(client_id) = self.foreground_client_id {
            self.resize_shell_tab_if_controller(client_id, start_pending_agent_resumes);
        }
    }

    fn sync_runtime_view_geometry(&mut self) {
        crate::ui::compute_view_without_resizing_panes(
            &mut self.app.state,
            &self.app.terminal_runtimes,
            Rect::new(0, 0, self.effective_size.0, self.effective_size.1),
        );
    }

    fn sync_foreground_client_state(&mut self) {
        self.app.pixel_mouse_available = self.foreground_client_id.is_some_and(|id| {
            self.clients
                .get(&id)
                .is_some_and(|client| client.pixel_mouse)
        });
        let Some(client_id) = self.foreground_client_id else {
            self.effective_size = self.headless_size;
            self.app.state.outer_terminal_focus = None;
            self.app.state.tab_viewer = crate::app::state::TabViewer::Nobody;
            self.app.state.host_cell_size = crate::terminal_cell_size::HostCellSize::default();
            self.sync_runtime_view_geometry();
            return;
        };
        let Some(client) = self.clients.get(&client_id) else {
            self.foreground_client_id = None;
            self.effective_size = self.headless_size;
            self.app.state.outer_terminal_focus = None;
            self.app.state.tab_viewer = crate::app::state::TabViewer::Nobody;
            self.app.state.host_cell_size = crate::terminal_cell_size::HostCellSize::default();
            self.sync_runtime_view_geometry();
            return;
        };

        let terminal_size = client.terminal_size;
        let host_cell_size = if client.cell_size.is_known() {
            client.cell_size
        } else {
            crate::terminal_cell_size::HostCellSize::default()
        };
        let host_terminal_theme = client.host_terminal_theme;
        let host_terminal_appearance = client.host_terminal_appearance;
        let host_terminal_appearance_explicit = client.host_terminal_appearance_explicit;

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

    /// Mirrors the foreground client's outer-terminal focus into `AppState`
    /// and, while that terminal is focused, marks the tab the client is
    /// looking at as seen.
    ///
    /// The tab is the foreground client's own `shell_location` tab, not the
    /// global `app.state.active` one. With several clients, endpoint requests
    /// from one client move `app.state.active` (see
    /// `set_default_shell_target_from_client`) while another is the focused
    /// foreground; marking the global active tab let the focused client clear
    /// "done" markers on a tab only the other client had open.
    ///
    /// This is all agent state and hook reports need before they are applied:
    /// they change neither client geometry nor layout, so they do not rerun
    /// `compute_view_without_resizing_panes` through the full
    /// `sync_foreground_client_state`.
    fn sync_foreground_focus_state(&mut self) {
        let foreground = self
            .foreground_client_id
            .and_then(|client_id| Some((client_id, self.clients.get(&client_id)?)));
        let Some((client_id, client)) = foreground else {
            self.app.state.outer_terminal_focus = None;
            self.app.state.tab_viewer = crate::app::state::TabViewer::Nobody;
            return;
        };
        let outer_terminal_focus = client.outer_terminal_focus;
        self.app.state.outer_terminal_focus = outer_terminal_focus;
        self.app.state.tab_viewer = self
            .shell_target_for_client(client_id)
            .and_then(|target| {
                let workspace = self.app.state.workspaces.get(target.workspace_index)?;
                let tab = workspace.tabs.get(target.tab_index)?;
                Some(crate::app::state::TabViewer::Tab {
                    workspace_id: workspace.id.clone(),
                    tab_number: tab.number,
                })
            })
            .unwrap_or(crate::app::state::TabViewer::Nobody);
        if outer_terminal_focus == Some(true) {
            self.mark_client_shell_tab_seen(client_id);
        }
    }

    fn mark_client_shell_tab_seen(&mut self, client_id: u64) -> bool {
        let Some(target) = self.shell_target_for_client(client_id) else {
            return false;
        };
        let Some(tab) = self
            .app
            .state
            .workspaces
            .get_mut(target.workspace_index)
            .and_then(|workspace| workspace.tabs.get_mut(target.tab_index))
        else {
            return false;
        };
        let mut changed = false;
        for pane in tab.panes.values_mut() {
            if !pane.seen {
                pane.seen = true;
                changed = true;
            }
        }
        changed
    }

    fn promote_client_to_foreground(&mut self, client_id: u64) -> bool {
        let stamp = self.allocate_activity_stamp();
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        // Only an active shell connection may drive session-wide presentation;
        // a direct terminal stream never becomes the foreground client.
        if !client.is_active_shell_client() {
            return false;
        }
        client.last_activity = stamp;

        let changed = self.foreground_client_id != Some(client_id);
        self.foreground_client_id = Some(client_id);
        self.sync_foreground_client_state();
        changed
    }

    fn promote_latest_remaining_client(&mut self) -> bool {
        let next_foreground = latest_shell_client(&self.clients);
        let changed = next_foreground != self.foreground_client_id;
        self.foreground_client_id = next_foreground;
        self.sync_foreground_client_state();
        changed
    }

    fn app_client_count(&self) -> usize {
        self.clients
            .values()
            .filter(|client| client.is_active_shell_client() && client.writer.is_some())
            .count()
    }

    fn has_app_client(&self) -> bool {
        self.app_client_count() > 0
    }

    fn remove_client(&mut self, client_id: u64) -> bool {
        self.immediate_pty_sources_dirty = true;
        let disconnected_focus = self
            .clients
            .get(&client_id)
            .filter(|client| {
                client.is_active_shell_client() && client.outer_terminal_focus == Some(true)
            })
            .and_then(|_| self.shell_focus_target(client_id));
        let should_release_focus = disconnected_focus.as_ref().is_some_and(|target| {
            !self.clients.iter().any(|(&other_id, client)| {
                other_id != client_id
                    && client.is_active_shell_client()
                    && client.outer_terminal_focus == Some(true)
                    && self.shell_tab_id_for_client(other_id).as_deref()
                        == Some(target.tab_id.as_str())
            })
        });
        let was_foreground = self.foreground_client_id == Some(client_id);
        let removed = self.clients.remove(&client_id);
        self.tab_geometry_controllers
            .retain(|_, controller_id| *controller_id != client_id);
        if let Some(mut removed) = removed {
            let held_inputs = removed.drain_shell_held_inputs();
            self.release_client_shell_inputs(client_id, held_inputs);
            if let ClientConnectionMode::TerminalAttach { terminal_id } = removed.mode {
                self.terminal_attach_owners.remove(&terminal_id);
                if let Some(terminal_id) = self.terminal_id_by_string(&terminal_id).cloned() {
                    self.app
                        .state
                        .direct_attach_resize_locks
                        .remove(&terminal_id);
                }
            }
        }
        if should_release_focus && let Some(target) = disconnected_focus.as_ref() {
            self.send_shell_focus_target(target, crate::ghostty::FocusEvent::Lost);
        }
        if was_foreground {
            self.promote_latest_remaining_client()
        } else {
            false
        }
    }

    fn release_client_shell_inputs(
        &mut self,
        client_id: u64,
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
                warn!(client_id, err = %err, "client shell teardown release failed");
            }
        }
    }

    /// Logs a shell client's failed pane input and, when input was dropped
    /// because the pane's PTY queue is full, tells that client. Losing
    /// keystrokes or a paste silently is worse than a visible error: the user
    /// would otherwise keep typing into a pane that is not reading.
    fn report_client_shell_input_failures(
        &mut self,
        client_id: u64,
        pane_id: &str,
        failures: &crate::server::pane_input::PaneInputFailures,
    ) {
        warn!(client_id, pane_id, err = %failures, "targeted client shell input failed");
        let dropped = failures.dropped_for_backpressure();
        if dropped == 0 {
            return;
        }
        let events = if dropped == 1 { "event" } else { "events" };
        self.send_to_client(
            client_id,
            &ServerMessage::ClientShellError {
                message: format!(
                    "Input to pane {pane_id} dropped ({dropped} {events}): the pane is not reading its input"
                ),
            },
        );
    }

    fn remove_client_and_resize_if_needed(&mut self, client_id: u64) {
        let restore_shell_controller = self.clients.get(&client_id).and_then(|client| {
            let ClientConnectionMode::TerminalAttach { terminal_id } = &client.mode else {
                return None;
            };
            self.shell_geometry_controller_for_terminal(terminal_id)
        });
        self.remove_client(client_id);
        if let Some((controller_id, target)) = restore_shell_controller {
            self.restore_shell_tab_geometry(controller_id, target);
        } else {
            self.resize_tabs_for_only_shell_client(true);
        }
    }

    /// Accepts pending client connections from the non-blocking listener.
    fn accept_client_connections(&mut self) -> io::Result<()> {
        accept_pending_client_connections(
            &self.client_listener,
            &mut self.next_client_id,
            &self.should_quit,
            &self.server_event_tx,
        )
    }

    /// Drains server events from the dedicated channel.
    fn drain_server_events(&mut self) -> bool {
        let mut changed = false;
        while !self.should_quit.load(Ordering::Acquire) {
            let Ok(ev) = self.server_event_rx.try_recv() else {
                break;
            };
            changed |= self.handle_server_event_with_render_impact(ev) == RenderImpact::Full;
        }
        changed
    }

    async fn reject_late_client_connections(&mut self) {
        self.server_event_rx.close();
        while let Some(event) = self.server_event_rx.recv().await {
            if let ServerEvent::ClientConnected { writer, .. }
            | ServerEvent::ClientShellConnected { writer, .. } = event
                && let Ok(message) = Self::frame_server_message(&ServerMessage::ServerShutdown {
                    reason: Some("server is shutting down".to_owned()),
                })
            {
                let _ = writer.control.send(message);
            }
        }
    }

    /// Resolves a direct-attach terminal id string to the live `TerminalId`.
    ///
    /// Still a scan over the session's terminals (tens, not thousands): the
    /// terminal map is keyed by `TerminalId`, which has no `Borrow<str>` and no
    /// public constructor from a string, so a hashed lookup by `&str` is not
    /// available from here. The scan compares borrowed strings; it used to
    /// allocate a `to_string()` per terminal on every attach keystroke, mouse
    /// event and render.
    fn terminal_id_by_string(&self, terminal_id: &str) -> Option<&crate::terminal::TerminalId> {
        self.app
            .state
            .terminals
            .keys()
            .find(|id| id.as_str() == terminal_id)
    }

    fn runtime_for_terminal_id_string(
        &self,
        terminal_id: &str,
    ) -> Option<&crate::terminal::TerminalRuntime> {
        let terminal_id = self.terminal_id_by_string(terminal_id)?;
        self.app.terminal_runtimes.get(terminal_id)
    }

    fn handle_terminal_attach_scroll(
        &mut self,
        client_id: u64,
        source: AttachScrollSource,
        direction: AttachScrollDirection,
        lines: u16,
        column: Option<u16>,
        row: Option<u16>,
        modifiers: u8,
    ) -> bool {
        let Some(ClientConnection {
            mode: ClientConnectionMode::TerminalAttach { terminal_id },
            ..
        }) = self.clients.get(&client_id)
        else {
            return false;
        };
        let Some(runtime) = self.runtime_for_terminal_id_string(terminal_id) else {
            return false;
        };

        let result =
            apply_terminal_attach_scroll(runtime, source, direction, lines, column, row, modifiers);
        if let Err(err) = &result {
            warn!(client_id, terminal_id = %terminal_id, err = %err, "terminal attach scroll failed");
        }
        self.report_terminal_attach_input(client_id, AttachInputDelivery::of(&result));
        true
    }

    fn handle_terminal_attach_mouse(
        &mut self,
        client_id: u64,
        kind: protocol::ClientMouseKind,
        position: protocol::ClientMousePosition,
        geometry: Option<protocol::ClientMouseGeometry>,
        modifiers: u8,
        lines: u16,
    ) -> bool {
        let Some(client) = self.clients.get(&client_id) else {
            return false;
        };
        let ClientConnectionMode::TerminalAttach { terminal_id } = &client.mode else {
            return false;
        };
        let terminal_id = terminal_id.clone();
        let terminal_size = client.terminal_size;
        let cell_size = client.cell_size;
        let pixel_mouse = client.pixel_mouse;
        let host_sgr_pixels_active = client.host_sgr_pixels_active == Some(true);
        let Some(runtime) = self.runtime_for_terminal_id_string(&terminal_id) else {
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
        let event = protocol::ClientPaneInputEvent::Mouse {
            kind,
            position,
            geometry: None,
            modifiers,
            lines: lines.max(1),
        };
        let result = apply_client_pane_input_events(runtime, &[event]);
        if let Err(err) = &result {
            warn!(client_id, terminal_id = %terminal_id, err = %err, "terminal attach mouse input failed");
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
    fn report_terminal_attach_input(&mut self, client_id: u64, delivery: AttachInputDelivery) {
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        match delivery {
            AttachInputDelivery::Delivered => {
                client.attach_input_drop_reported = false;
                return;
            }
            // The pane is going away or the input was malformed; the log at
            // the call site covers it, and a closing pane ends the attach
            // with its own shutdown message.
            AttachInputDelivery::Failed => return,
            AttachInputDelivery::Dropped => {}
        }
        if std::mem::replace(&mut client.attach_input_drop_reported, true) {
            return;
        }
        let ClientConnectionMode::TerminalAttach { terminal_id } = &client.mode else {
            return;
        };
        let message =
            format!("Input to terminal {terminal_id} dropped: the pane is not reading its input");
        self.send_to_client(client_id, &ServerMessage::DirectTerminalNotice { message });
    }

    /// Pulls only titles reported dirty by the PTY parser. A focused pane title
    /// is forwarded as an independent client side effect; only sidebar title
    /// tokens require a UI render.
    fn sync_terminal_title_sources(
        &mut self,
        sources: &HashSet<crate::layout::PaneId>,
    ) -> (bool, bool) {
        let focused_source = self
            .foreground_window_title_target()
            .or_else(|| self.default_shell_target())
            .and_then(|target| {
                self.app
                    .state
                    .workspaces
                    .get(target.workspace_index)?
                    .tabs
                    .get(target.tab_index)
            })
            .map(|tab| tab.layout.focused())
            .is_some_and(|pane_id| sources.contains(&pane_id));
        let changes = self.app.sync_terminal_titles(sources);
        let outer_title_synced = focused_source && self.app.window_title_uses_terminal_title();
        if outer_title_synced {
            self.sync_window_title();
        }
        (
            self.app.terminal_title_sidebar_changed(&changes),
            outer_title_synced,
        )
    }

    fn foreground_window_title_target(&self) -> Option<crate::ui::TabSurfaceTarget> {
        self.foreground_client_id
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
                    self.app
                        .window_title_for(target.workspace_index, target.tab_index)
                },
            )
            .and_then(|title| crate::config::sanitize_window_title_text(&title))
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
        if let (Some(client_id), Some((sent_client_id, sent_title))) =
            (self.foreground_client_id, self.sent_window_title.as_ref())
            && *sent_client_id == client_id
            && *sent_title == title
        {
            return;
        }
        self.send_window_title(title);
    }

    /// Sends a window title and remembers it only when a foreground client took
    /// it, so the next client to attach is written to rather than skipped.
    fn send_window_title(&mut self, title: Option<String>) -> bool {
        let Some(client_id) = self.foreground_client_id else {
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

    fn handle_client_window_title_api(&mut self, id: String, title: Option<String>) -> String {
        use api::schema::{ClientWindowTitleReason, ResponseResult};

        let title = match title {
            Some(title) => match crate::config::sanitize_window_title_text(&title) {
                Some(title) => Some(title),
                None => {
                    return serde_json::to_string(&api::schema::ErrorResponse {
                        id,
                        error: api::schema::ErrorBody {
                            code: "invalid_params".into(),
                            message: "window title is empty".into(),
                        },
                    })
                    .unwrap_or_else(|_| "{}".to_string());
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
        serde_json::to_string(&api::schema::SuccessResponse {
            id,
            result: ResponseResult::ClientWindowTitle { changed, reason },
        })
        .unwrap_or_else(|_| "{}".to_string())
    }

    /// Encodes a server message into a length-prefixed frame.
    ///
    /// A payload over `MAX_FRAME_SIZE` fails with `FramingError::Oversized`:
    /// `protocol::write_message` refuses it before writing anything, since
    /// every reader would drop the connection on such a frame.
    fn frame_server_message(msg: &ServerMessage) -> Result<Vec<u8>, protocol::FramingError> {
        protocol::encode_frame(msg)
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
                warn!(err = %err, "failed to serialize message for clients");
                return;
            }
        };

        let mut broken_clients: Vec<u64> = Vec::new();
        for (&client_id, client) in &mut self.clients {
            if let Some(writer) = &client.writer
                && writer.control.send(serialized.clone()).is_err()
            {
                debug!(client_id, "client writer channel closed during broadcast");
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
        let Some(client_id) = self.foreground_client_id else {
            return false;
        };
        self.send_to_client(client_id, msg)
    }

    /// Sends a message to a specific client. Returns false if the client
    /// was not found or the send failed (client removed).
    fn send_to_client(&mut self, client_id: u64, msg: &ServerMessage) -> bool {
        let serialized = match Self::frame_server_message(msg) {
            Ok(framed) => framed,
            Err(err) => {
                warn!(client_id, err = %err, "failed to serialize message for client");
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
                client_id,
                "client writer channel closed during targeted send"
            );
            self.remove_client_and_resize_if_needed(client_id);
            return false;
        }
        true
    }

    fn shutdown_terminal_stream_clients(&mut self, terminal_id: &str, reason: &str) {
        let client_ids = terminal_stream_client_ids(&self.clients, terminal_id);

        for client_id in client_ids {
            self.send_to_client(
                client_id,
                &ServerMessage::ServerShutdown {
                    reason: Some(reason.to_owned()),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
        }
    }

    fn send_terminal_stream_detach_shutdown(&mut self, client_id: u64) {
        if matches!(
            self.clients.get(&client_id).map(|client| &client.mode),
            Some(ClientConnectionMode::TerminalAttach { .. })
        ) {
            self.send_to_client(
                client_id,
                &ServerMessage::ServerShutdown {
                    reason: Some("detached".to_owned()),
                },
            );
        }
    }

    fn attach_terminal_client(
        &mut self,
        client_id: u64,
        terminal_id: &str,
        takeover: bool,
    ) -> bool {
        if !self.client_is_pending_terminal_mode(client_id) {
            self.send_to_client(
                client_id,
                &ServerMessage::ServerShutdown {
                    reason: Some(
                        "terminal attach failed: connection is not pending terminal attach"
                            .to_owned(),
                    ),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
            return false;
        }

        let Some(real_terminal_id) = self.terminal_id_by_string(terminal_id).cloned() else {
            self.send_to_client(
                client_id,
                &ServerMessage::ServerShutdown {
                    reason: Some(format!(
                        "terminal attach failed: terminal {terminal_id} not found"
                    )),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
            return false;
        };

        if self
            .pending_alt_screen_reads
            .iter()
            .any(|pending| pending.terminal_id == real_terminal_id)
        {
            self.send_to_client(
                client_id,
                &ServerMessage::ServerShutdown {
                    reason: Some(format!(
                        "terminal attach failed: terminal {terminal_id} has a read in progress; retry"
                    )),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
            return false;
        }

        if let Some(existing_owner) = self.terminal_attach_owners.get(terminal_id).copied() {
            if existing_owner != client_id && !takeover {
                self.send_to_client(
                    client_id,
                    &ServerMessage::ServerShutdown {
                        reason: Some(format!(
                            "terminal attach failed: terminal {terminal_id} already has an attached client; retry with --takeover"
                        )),
                    },
                );
                self.remove_client_and_resize_if_needed(client_id);
                return false;
            }
            if existing_owner != client_id {
                self.send_to_client(
                    existing_owner,
                    &ServerMessage::ServerShutdown {
                        reason: Some("terminal attach taken over".to_owned()),
                    },
                );
                self.remove_client_and_resize_if_needed(existing_owner);
            }
        }

        let stamp = self.allocate_activity_stamp();
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        let (cols, rows) = client.terminal_size;
        let cell_size = client.cell_size;
        client.mode = ClientConnectionMode::TerminalAttach {
            terminal_id: terminal_id.to_owned(),
        };
        client.render_state.reset_baseline();
        client.last_activity = stamp;
        let was_foreground = self.foreground_client_id == Some(client_id);
        if was_foreground {
            self.promote_latest_remaining_client();
        }

        info!(client_id, cols, rows, terminal_id = %terminal_id, "terminal attach client connected");
        self.terminal_attach_owners
            .insert(terminal_id.to_owned(), client_id);
        self.app
            .state
            .direct_attach_resize_locks
            .insert(real_terminal_id.clone());
        self.app
            .start_pending_agent_resume_for_terminal(&real_terminal_id, rows, cols, true);
        if let Some(runtime) = self.app.terminal_runtimes.get(&real_terminal_id) {
            runtime.resize(rows, cols, cell_size.width_px, cell_size.height_px);
        }
        true
    }

    fn client_is_pending_terminal_mode(&self, client_id: u64) -> bool {
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
                    client_id,
                    cols, rows, cell_width_px, cell_height_px, "direct terminal client connected"
                );
                let last_activity = self.allocate_activity_stamp();
                let observed = crate::terminal_cell_size::HostCellSize {
                    width_px: cell_width_px,
                    height_px: cell_height_px,
                };
                let pixel_mouse = pixel_mouse && observed.is_known();
                let mut connection = ClientConnection::new_with_mode(
                    ClientConnectionMode::TerminalPending,
                    (cols, rows),
                    observed,
                    last_activity,
                    protocol::RenderEncoding::TerminalAnsi,
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
                endpoint_keybindings,
                mouse_capture,
                surface_active,
                writer,
            } => {
                info!(
                    client_id,
                    cols = surface_cols,
                    rows = surface_rows,
                    cell_width_px,
                    cell_height_px,
                    surface_active,
                    render_encoding = ?protocol::RenderEncoding::SemanticFrame,
                    "client connected"
                );
                self.app.ensure_default_workspace();
                let first_app_client = self.app_client_count() == 0;
                let last_activity = self.allocate_activity_stamp();
                let observed = crate::terminal_cell_size::HostCellSize {
                    width_px: cell_width_px,
                    height_px: cell_height_px,
                };
                let mut connection = ClientConnection::new_with_mode(
                    ClientConnectionMode::ClientShell,
                    (surface_cols, surface_rows),
                    observed,
                    last_activity,
                    protocol::RenderEncoding::SemanticFrame,
                    Some(writer),
                );
                connection.pixel_mouse = pixel_mouse && observed.is_known();
                connection.shell_uses_endpoint_keybindings = endpoint_keybindings;
                connection.shell_mouse_capture = mouse_capture;
                connection.shell_surface_active = surface_active;
                connection.shell_projection_revision = 1;
                let config_diagnostic = if endpoint_keybindings {
                    self.server_config_diagnostic.as_deref()
                } else {
                    self.server_config_diagnostic_without_keybindings.as_deref()
                };
                let (seed_snapshot, completion_projection) = client_shell_snapshot(
                    &self.app,
                    &self.client_shell_boot_id,
                    connection.shell_projection_revision,
                    config_diagnostic,
                    None,
                );
                let location =
                    crate::server::clients::ClientShellLocation::from_snapshot(&seed_snapshot);
                let snapshot_message =
                    match crate::protocol::endpoint::snapshot_message(&seed_snapshot) {
                        Ok(message) => message,
                        Err(err) => {
                            warn!(client_id, err = %err, "failed to encode endpoint snapshot");
                            return false;
                        }
                    };
                let completion_message = match crate::protocol::endpoint::agent_completions_message(
                    &completion_projection,
                ) {
                    Ok(message) => message,
                    Err(err) => {
                        warn!(client_id, err = %err, "failed to encode agent completions");
                        return false;
                    }
                };
                connection.shell_location = Some(location);
                connection.shell_snapshot = Some(seed_snapshot);
                connection.shell_agent_completions = Some(completion_projection);
                self.clients.insert(client_id, connection);
                self.send_to_client(client_id, &completion_message);
                self.send_to_client(client_id, &snapshot_message);
                if surface_active {
                    self.foreground_client_id = Some(client_id);
                }
                if first_app_client {
                    self.app.mark_git_status_refresh_due(Instant::now());
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
                    mode: ClientConnectionMode::TerminalAttach { terminal_id },
                    ..
                }) = self.clients.get(&client_id)
                else {
                    return false;
                };
                let Some(runtime) = self.runtime_for_terminal_id_string(terminal_id) else {
                    return true;
                };
                let result = apply_terminal_attach_input(runtime, data);
                if let Err(err) = &result {
                    warn!(client_id, terminal_id = %terminal_id, err = %err, "terminal attach input failed");
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
                    Some(ClientConnectionMode::ClientShell)
                ) {
                    self.send_to_client(
                        client_id,
                        &ServerMessage::ClientShellError {
                            message: format!("Paste rejected: {detail}"),
                        },
                    );
                } else {
                    // Direct attach has no shell chrome, so it gets its own
                    // notice; every rejection is a separate user action, so
                    // each one is reported.
                    warn!(client = client_id, %detail, "paste rejected for direct terminal client");
                    self.send_to_client(
                        client_id,
                        &ServerMessage::DirectTerminalNotice {
                            message: format!("Paste rejected: {detail}"),
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
                    client_id,
                    cols, rows, cell_width_px, cell_height_px, pixel_mouse, "client resize"
                );
                let observed = crate::terminal_cell_size::HostCellSize {
                    width_px: cell_width_px,
                    height_px: cell_height_px,
                };
                let pixel_mouse = pixel_mouse && observed.is_known();
                let direct_terminal_id = if let Some(ClientConnection {
                    mode: ClientConnectionMode::TerminalAttach { terminal_id },
                    terminal_size,
                    cell_size,
                    pixel_mouse: client_pixel_mouse,
                    render_state,
                    ..
                }) = self.clients.get_mut(&client_id)
                {
                    *terminal_size = (cols, rows);
                    *cell_size = observed;
                    *client_pixel_mouse = pixel_mouse;
                    render_state.request_repaint();
                    Some((terminal_id.clone(), *cell_size))
                } else {
                    None
                };
                if let Some((terminal_id, cell_size)) = direct_terminal_id {
                    if let Some(runtime) = self.runtime_for_terminal_id_string(&terminal_id) {
                        runtime.resize(rows, cols, cell_size.width_px, cell_size.height_px);
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
                    *terminal_size = (cols, rows);
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
                if !matches!(client.mode, ClientConnectionMode::ClientShell) {
                    return false;
                }
                client.terminal_size = (surface_cols, surface_rows);
                let observed = crate::terminal_cell_size::HostCellSize {
                    width_px: cell_width_px,
                    height_px: cell_height_px,
                };
                if observed.is_known() {
                    client.cell_size = observed;
                }
                client.pixel_mouse = pixel_mouse && observed.is_known();
                if !client.shell_surface_active {
                    return false;
                }
                client.request_repaint();
                self.promote_client_to_foreground(client_id);
                self.resize_shell_tab_if_controller(client_id, true);
                true
            }
            ServerEvent::ClientShellHostTheme { client_id, update } => {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                if !matches!(client.mode, ClientConnectionMode::ClientShell) {
                    return false;
                }
                if !client.update_host_theme(&update) {
                    return false;
                }
                if !client.shell_surface_active || self.foreground_client_id != Some(client_id) {
                    return false;
                }
                let mut changed = self.app.set_host_terminal_appearance_state(
                    client.host_terminal_appearance,
                    client.host_terminal_appearance_explicit,
                );
                changed |= self.app.set_host_terminal_theme(client.host_terminal_theme);
                if changed {
                    self.resize_foreground_shell_tab_if_controller(false);
                }
                changed
            }
            ServerEvent::ClientShellFocus { client_id, focused } => {
                let Some(client) = self.clients.get(&client_id) else {
                    return false;
                };
                if !client.is_active_shell_client() || client.outer_terminal_focus == Some(focused)
                {
                    return false;
                }
                let tab_id = self.shell_tab_id_for_client(client_id);
                let another_focused_viewer = self.clients.iter().any(|(&other_id, client)| {
                    other_id != client_id
                        && client.is_active_shell_client()
                        && client.outer_terminal_focus == Some(true)
                        && self.shell_tab_id_for_client(other_id) == tab_id
                });
                if let Some(client) = self.clients.get_mut(&client_id) {
                    client.outer_terminal_focus = Some(focused);
                }
                if focused {
                    self.promote_client_to_foreground(client_id);
                    self.claim_shell_tab_geometry(client_id, false);
                    if !another_focused_viewer
                        && let Some(target) = self.shell_focus_target(client_id)
                    {
                        self.send_shell_focus_target(&target, crate::ghostty::FocusEvent::Gained);
                    }
                    true
                } else {
                    if self.foreground_client_id == Some(client_id) {
                        self.app.state.outer_terminal_focus = Some(false);
                    }
                    if !another_focused_viewer
                        && let Some(target) = self.shell_focus_target(client_id)
                    {
                        self.send_shell_focus_target(&target, crate::ghostty::FocusEvent::Lost);
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
                client.host_keyboard_report_all_active = None;
                self.sent_window_title = None;
                self.stream_host_mouse_capture_mode();
                self.stream_direct_terminal_keyboard_mode();
                self.sync_window_title();
                self.send_to_client(
                    client_id,
                    &ServerMessage::EndpointControl {
                        kind: crate::protocol::endpoint::PRESENTATION_EFFECTS_READY_KIND.into(),
                        data: token,
                    },
                )
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
                    runtime.current_size(),
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
                if !matches!(client.mode, ClientConnectionMode::ClientShell) {
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
                if !matches!(client.mode, ClientConnectionMode::ClientShell)
                    || !client.shell_endpoint_command_in_flight
                    || boot_id != self.client_shell_boot_id
                {
                    return false;
                }
                if final_chunk {
                    client.shell_endpoint_command_in_flight = false;
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
                info!(client_id, "client detached");
                self.send_terminal_stream_detach_shutdown(client_id);
                self.remove_client_and_resize_if_needed(client_id);
                true
            }
            ServerEvent::ClientDisconnected { client_id } => {
                info!(client_id, "client disconnected");
                self.remove_client_and_resize_if_needed(client_id);
                true
            }
            ServerEvent::ClientWriterDrained { client_id } => {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                client.take_deferred_render() != DeferredRender::None
            }
            ServerEvent::QuitSignal => {
                // The quit check at the top of the loop handles this.
                // No render needed - the next iteration will initiate shutdown.
                false
            }
        }
    }

    fn handle_server_event_with_render_impact(&mut self, ev: ServerEvent) -> RenderImpact {
        if self.handle_server_event(ev) {
            RenderImpact::Full
        } else {
            RenderImpact::None
        }
    }

    fn agent_read_not_idle_error(
        &self,
        request: &api::schema::Request,
    ) -> Option<api::schema::ErrorBody> {
        use api::schema::{Method, ReadFormat, ReadSource};

        let Method::AgentRead(params) = &request.method else {
            return None;
        };
        let requested = params.lines?;
        if params.format != ReadFormat::Text
            || !matches!(
                params.source,
                ReadSource::Recent | ReadSource::RecentUnwrapped
            )
        {
            return None;
        }
        let target = self.app.resolve_agent_target(&params.target).ok()?;
        let terminal = self
            .app
            .state
            .terminals
            .values()
            .find(|terminal| terminal.id.as_str() == target.terminal_id)?;
        if terminal.effective_known_agent().is_none()
            || terminal.state == crate::detect::AgentState::Idle
        {
            return None;
        }
        let runtime = self.app.terminal_runtimes.get(&terminal.id)?;
        let (screen, snapshot) = runtime.screen_text_snapshot()?;
        if screen != crate::ghostty::ActiveScreen::Alternate
            || snapshot.rows.len() >= requested.min(1000) as usize
        {
            return None;
        }
        let status = crate::detect::manifest::agent_state_label(terminal.state);
        Some(api::schema::ErrorBody {
            code: "agent_not_idle".into(),
            message: format!(
                "cannot read {requested} lines while {} is {status}: its alternate-screen history can only be captured by scrolling while idle. Wait and retry, or use --source visible",
                params.target
            ),
        })
    }

    fn alt_screen_read_spec(&self, request: &api::schema::Request) -> Option<AltScreenReadSpec> {
        use api::schema::{Method, ReadFormat, ReadIntent, ReadSource};

        let (target, source, lines, format) = match &request.method {
            Method::AgentRead(params) => (
                self.app.resolve_agent_target(&params.target).ok()?,
                params.source,
                params.lines,
                params.format,
            ),
            Method::PaneRead(params) if params.intent == ReadIntent::Interactive => (
                self.app.resolve_terminal_target(&params.pane_id).ok()?,
                params.source,
                params.lines,
                params.format,
            ),
            _ => return None,
        };
        if format != ReadFormat::Text
            || !matches!(source, ReadSource::Recent | ReadSource::RecentUnwrapped)
        {
            return None;
        }
        let lines = lines.unwrap_or(80).min(1000) as usize;
        if lines == 0
            || self
                .terminal_attach_owners
                .contains_key(target.terminal_id.as_str())
            || self
                .pending_alt_screen_reads
                .iter()
                .any(|pending| pending.terminal_id.as_str() == target.terminal_id)
        {
            return None;
        }
        let terminal = self
            .app
            .state
            .terminals
            .values()
            .find(|terminal| terminal.id.as_str() == target.terminal_id)?;
        if terminal.effective_known_agent().is_none()
            || terminal.state != crate::detect::AgentState::Idle
        {
            return None;
        }
        let runtime = self.app.terminal_runtimes.get(&terminal.id)?;
        if runtime.wheel_routing() != Some(crate::pane::WheelRouting::MouseReport) {
            return None;
        }
        let (screen, initial, content_seq) = runtime.screen_text_snapshot_with_seq()?;
        if screen != crate::ghostty::ActiveScreen::Alternate || initial.rows.len() >= lines {
            return None;
        }
        Some(AltScreenReadSpec {
            terminal_id: terminal.id.clone(),
            lines,
            unwrap: source == ReadSource::RecentUnwrapped,
            initial,
            content_seq,
        })
    }

    fn poll_pending_alt_screen_reads(&mut self, now: Instant) {
        let pending = std::mem::take(&mut self.pending_alt_screen_reads);
        for read in pending {
            let runtime = self.app.terminal_runtimes.get(&read.terminal_id);
            let remains_idle = self
                .app
                .state
                .terminals
                .get(&read.terminal_id)
                .is_some_and(|terminal| terminal.state == crate::detect::AgentState::Idle);
            let attached = self
                .terminal_attach_owners
                .contains_key(read.terminal_id.as_str());
            let outcome = if remains_idle && !attached {
                read.poll(runtime, now)
            } else {
                read.abort(runtime, now)
            };
            if let Some(read) = outcome {
                self.pending_alt_screen_reads.push(read);
            }
        }
    }

    fn alt_screen_read_conflict(&self, request: &api::schema::Request) -> AltScreenReadConflict {
        let (target, source, lines, format) = match &request.method {
            api::schema::Method::AgentRead(params) => (
                self.app.resolve_agent_target(&params.target).ok(),
                params.source,
                params.lines,
                params.format,
            ),
            api::schema::Method::PaneRead(params) => (
                self.app.resolve_terminal_target(&params.pane_id).ok(),
                params.source,
                params.lines,
                params.format,
            ),
            _ => return AltScreenReadConflict::None,
        };
        let Some(target) = target else {
            return AltScreenReadConflict::None;
        };
        let Some(pending) = self
            .pending_alt_screen_reads
            .iter()
            .find(|pending| pending.terminal_id.as_str() == target.terminal_id)
        else {
            return AltScreenReadConflict::None;
        };
        if format == api::schema::ReadFormat::Text {
            AltScreenReadConflict::Frozen(pending.frozen_snapshot(source, lines))
        } else {
            AltScreenReadConflict::Defer
        }
    }

    fn process_deferred_alt_screen_reads(&mut self) -> bool {
        let deferred = std::mem::take(&mut self.deferred_alt_screen_reads);
        let mut changed = false;
        for msg in deferred {
            match self.alt_screen_read_conflict(&msg.request) {
                AltScreenReadConflict::None => {
                    changed |= self.handle_api_request_with_shutdown_check(msg);
                }
                AltScreenReadConflict::Frozen(_) | AltScreenReadConflict::Defer => {
                    self.deferred_alt_screen_reads.push(msg);
                }
            }
        }
        changed
    }

    /// Drains API requests with shutdown awareness.
    ///
    /// During shutdown, remaining requests get a `server_unavailable` error.
    fn drain_api_requests_with_shutdown_check(&mut self) -> bool {
        let mut changed = false;
        while !self.should_quit.load(Ordering::Acquire) {
            let Ok(msg) = self.app.api_rx.try_recv() else {
                break;
            };
            changed |= self.handle_api_request_with_shutdown_check(msg);
        }
        changed
    }

    /// Closes the API request channel and answers everything still in it.
    ///
    /// Closing first means a request the API thread dispatches from here on
    /// fails to send and is answered `server_unavailable` by that thread at
    /// once, instead of sitting in the channel until the server drops it
    /// after the session save.
    fn reject_queued_api_requests_for_shutdown(&mut self) {
        self.app.api_rx.close();
        while let Ok(msg) = self.app.api_rx.try_recv() {
            Self::reject_api_request_for_shutdown(msg);
        }
    }

    /// Answers requests that were parked behind an alternate-screen traversal,
    /// and the traversals themselves, before the loop that drives them exits.
    fn finish_alt_screen_reads_for_shutdown(&mut self) {
        for msg in std::mem::take(&mut self.deferred_alt_screen_reads) {
            Self::reject_api_request_for_shutdown(msg);
        }
        for read in std::mem::take(&mut self.pending_alt_screen_reads) {
            read.finish_for_shutdown();
        }
    }

    fn reject_api_request_for_shutdown(msg: api::ApiRequestMessage) {
        let response = serde_json::to_string(&api::schema::ErrorResponse {
            id: msg.request.id,
            error: api::schema::ErrorBody {
                code: "server_unavailable".into(),
                message: "server is shutting down".into(),
            },
        })
        .unwrap_or_else(|_| {
            r#"{"id":"","error":{"code":"server_unavailable","message":"server is shutting down"}}"#
                .to_string()
        });
        let _ = msg.respond_to.send(response);
    }

    fn handle_api_request_with_shutdown_check_inner(
        &mut self,
        msg: api::ApiRequestMessage,
    ) -> bool {
        if self.shutting_down {
            Self::reject_api_request_for_shutdown(msg);
            return false;
        }
        self.immediate_pty_sources_dirty = true;

        let frozen_alt_screen_read = match self.alt_screen_read_conflict(&msg.request) {
            AltScreenReadConflict::None => None,
            AltScreenReadConflict::Frozen(snapshot) => Some(snapshot),
            AltScreenReadConflict::Defer => {
                self.deferred_alt_screen_reads.push(msg);
                return false;
            }
        };

        let metadata_expired = self.app.expire_due_metadata(Instant::now());

        match &msg.request.method {
            api::schema::Method::ClientWindowTitleSet(params) => {
                let response = self.handle_client_window_title_api(
                    msg.request.id.clone(),
                    Some(params.title.clone()),
                );
                let _ = msg.respond_to.send(response);
                return true;
            }
            api::schema::Method::ClientWindowTitleClear(_) => {
                let response = self.handle_client_window_title_api(msg.request.id.clone(), None);
                let _ = msg.respond_to.send(response);
                return true;
            }
            _ => {}
        }

        let mut changed = metadata_expired | api::request_changes_ui(&msg.request);
        changed |= self.drain_all_internal_events_with_forwarding();

        // The full sync (including the view recompute) stays on this path:
        // API handlers read `app.state.view` for directional focus, splits and
        // resume geometry, and an earlier request may have changed the layout
        // without anything cheaper recording that it did.
        self.sync_foreground_client_state();
        if let Some(error) = self.agent_read_not_idle_error(&msg.request) {
            let response = serde_json::to_string(&api::schema::ErrorResponse {
                id: msg.request.id.clone(),
                error,
            })
            .unwrap_or_else(|_| "{}".to_owned());
            let _ = msg.respond_to.send(response);
            return changed;
        }
        let alt_screen_read_spec = self.alt_screen_read_spec(&msg.request);
        if matches!(&msg.request.method, api::schema::Method::AgentPrompt(_)) {
            let deferred_changed = self
                .app
                .handle_deferred_agent_api_request(msg.request, msg.respond_to);
            return changed | deferred_changed;
        }
        if self.foreground_client_id.is_some_and(|client_id| {
            self.clients
                .get(&client_id)
                .is_some_and(|client| matches!(client.mode, ClientConnectionMode::ClientShell))
        }) {
            self.app.state.view.terminal_area =
                Rect::new(0, 0, self.effective_size.0, self.effective_size.1);
        }
        let mut response = self
            .app
            .handle_api_request_after_internal_events_drained(msg.request);
        if let Some(snapshot) = frozen_alt_screen_read
            && let Ok(mut success) = serde_json::from_str::<api::schema::SuccessResponse>(&response)
            && let api::schema::ResponseResult::PaneRead { read } = &mut success.result
        {
            read.text = snapshot.text;
            read.truncated = snapshot.truncated;
            if let Ok(serialized) = serde_json::to_string(&success) {
                response = serialized;
            }
        }
        if let Some(spec) = alt_screen_read_spec
            && let Ok(success) = serde_json::from_str::<api::schema::SuccessResponse>(&response)
            && let api::schema::ResponseResult::PaneRead { read } = success.result
        {
            let pending = crate::server::alt_screen_read::PendingAltScreenRead::start(
                spec.terminal_id,
                success.id,
                msg.respond_to,
                response,
                read,
                spec.lines,
                spec.unwrap,
                spec.initial,
                spec.content_seq,
                Instant::now(),
            );
            self.pending_alt_screen_reads.push(pending);
            return changed;
        }
        let _ = msg.respond_to.send(response);

        if latest_shell_client(&self.clients).is_some() {
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
        // The config diagnostic never expires on a timer: it is fixed at launch
        // and carried per client in each shell snapshot.

        if self.has_app_client() {
            self.app.start_git_status_refresh_if_due(now);
        }

        if self
            .app
            .session_save_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.start_background_session_save();
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
        changed |= self.app.expire_due_managed_agents(now);
        changed |= self
            .app
            .start_pending_agent_resumes(now, self.app.pending_agent_resume_due(now));
        changed
    }
}

fn client_pane_input_releases_press(event: &protocol::ClientPaneInputEvent) -> bool {
    matches!(
        event,
        protocol::ClientPaneInputEvent::Key {
            kind: protocol::ClientKeyKind::Release,
            ..
        } | protocol::ClientPaneInputEvent::Mouse {
            kind: protocol::ClientMouseKind::Up(_),
            ..
        }
    )
}

fn client_pane_input_has_interaction(events: &[protocol::ClientPaneInputEvent]) -> bool {
    events
        .iter()
        .any(|event| !client_pane_input_releases_press(event))
}

impl Drop for HeadlessServer {
    fn drop(&mut self) {
        let _ = self.cleanup_sockets();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Installs the SIGINT/SIGTERM/SIGHUP handler (ctrlc's `termination`
/// feature). It marks the quit as signal-driven, sets the should_quit flag, and
/// wakes up the event loop by sending a QuitSignal on the server event channel.
fn ctrlc_handler(
    should_quit: Arc<AtomicBool>,
    signal_quit: Arc<AtomicBool>,
    server_event_tx: mpsc::Sender<ServerEvent>,
) {
    let _ = ctrlc::set_handler(move || {
        // Before `should_quit`, so the loop never sees the quit without it.
        signal_quit.store(true, Ordering::Release);
        should_quit.store(true, Ordering::Release);
        // Wake up the event loop so the quit flag is checked promptly.
        let _ = server_event_tx.try_send(ServerEvent::QuitSignal);
    });
}

/// Sleep until a deadline, or return pending if none.
async fn sleep_until_or_pending(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

/// Binds the client socket owner-only from the moment it is reachable (see
/// `ipc::bind_private_local_listener`), naming the server in the error when
/// another one won the race to the path.
fn bind_owner_only_listener(path: &Path) -> io::Result<LocalListener> {
    crate::ipc::bind_private_local_listener(path).map_err(|err| {
        if err.kind() == io::ErrorKind::AddrInUse {
            io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "shepr server is already running (socket busy at {})",
                    path.display()
                ),
            )
        } else {
            err
        }
    })
}

fn server_config_diagnostic_summaries(diagnostics: &[String]) -> (Option<String>, Option<String>) {
    (
        config::config_diagnostic_summary(diagnostics),
        config::config_diagnostic_summary_without_keybindings(diagnostics),
    )
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
