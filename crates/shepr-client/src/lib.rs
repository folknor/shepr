//! Thin client mode - connects to the server socket.
//!
//! The client:
//! - Connects to `shepr.sock`, sends the build preamble and terminal geometry, then reads
//!   the server's preamble
//! - Sets up the real terminal (raw mode, mouse capture, keyboard enhancements)
//! - Receives surface messages, composes them with the client shell chrome and blits the
//!   result to the terminal (diff against last frame)
//! - Reads stdin events (keystrokes, mouse, paste), routes them through the client shell and
//!   sends pane input as ClientMessage::ClientShellPaneInput
//! - Detects terminal resize and sends ClientMessage::ClientShellResize
//! - Restores terminal on exit (normal or error)
//! - Handles ServerShutdown gracefully (clean exit, informative message returned for the
//!   binary to print once the terminal is restored)
//! - Handles server unreachable (clear error screen, not blank/hang)
//! - Forwards server clipboard writes through the host clipboard helper when available, and
//!   uses OSC 52 when configured or as a fallback
//!
//! The binary launcher installs process-wide file logging before calling the
//! client; client startup reuses that subscriber instead of installing one.

mod clipboard_forwarding;
pub mod endpoint;
mod errors;
mod events;
mod handshake;
pub(crate) mod host_replies;
mod input;
pub(crate) mod input_wire;
mod limits;
pub(crate) mod logging;
mod loop_config;
mod reconcile;
mod shell;
mod shell_runtime;
mod startup;
mod state;
mod terminal_geometry;
mod terminal_setup;
mod timer;
mod transport;

use clipboard_forwarding::forward_clipboard;
use events::{ClientLoopEvent, ParsedHostInput};
use loop_config::{ClientLoopConfig, ClientSettings};
use reconcile::present_notice;
use shell_runtime::*;
use state::{ClientState, HostWriteFailure};
use transport::*;

pub use shell::{ClientShellConfig, ClientShellState};
pub use startup::run_client;

use terminal_geometry::query_host_terminal_appearance;
use terminal_geometry::{AtomicCellSize, reported_cell_size_from_events, store_reported_cell_size};
use terminal_geometry::{
    host_cell_size_query_required, initial_terminal_geometry, query_host_cell_size,
    query_host_terminal_theme, resize_poll_loop,
};
use terminal_setup::{HostMouseMode, TerminalGuard, setup_terminal, should_draw_host_cursor};

pub use errors::{ClientError, ClientExit, ClientRunError};
use handshake::do_handshake;
use limits::CLIENT_EVENT_QUEUE_CAPACITY;

use std::io::{self, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{info, warn};

use shepr_platform::ipc::LocalStream;
use shepr_protocol::{ClientMessage, ServerMessage, surface_reuse::DecodedServerMessage};
use shepr_termio::blit as render_ansi;

/// Runs the local shell client with startup settings already loaded by the
/// launch coordinator. The binary launcher installs the process-wide file
/// logger before calling this function. The machines are the launch
/// config's `[[machines]]`, fixed for the life of the client.
fn run_launched_client(
    config: &shepr_config::ValidatedClientConfig,
    paths: &shepr_config::AppPaths,
) -> Result<ClientExit, ClientRunError> {
    let settings = ClientSettings::resolve(config).map_err(io::Error::from)?;
    let socket_path = paths.server_address().socket().to_path_buf();
    let shell_config = shell::ClientShellConfig::from_validated_config(config)
        .with_local_endpoint(paths.state_dir(), &socket_path)?;
    let mouse_capture = settings.mouse_capture_active();
    let mut loop_config = ClientLoopConfig {
        settings,
        host_escape_disambiguation_active: false,
        initial_host_input: Vec::new(),
        paths: paths.clone(),
    };

    crate::logging::startup("client");
    info!(path = %socket_path.display(), "connecting to server");

    let machines = config.machines().to_vec();
    let local_failure_policy = endpoint::LocalFailurePolicy::for_machines(&machines);
    let mut initial_local_failure = None;

    let initial_stream = match shepr_platform::ipc::connect_trusted_local_stream(&socket_path) {
        Ok(stream) => Some(stream),
        Err(error) if !local_failure_policy.ends_client_for(&endpoint::ClientEndpointId::Local) => {
            // An absent or refusing Local socket is the ordinary "server not running" case:
            // Local shows as Connecting and the supervisor attempts it at once, with its own
            // guidance, rather than seeding a diagnostic from the raw connect error.
            warn!(%error, "Local is unavailable; keeping configured machines available");
            None
        }
        Err(error) => {
            return Err(ClientRunError::Launch(io::Error::new(
                error.kind(),
                ClientError::ConnectionFailed(error),
            )));
        }
    };

    // Get the terminal geometry before handshake (before raw mode).
    let geometry = initial_terminal_geometry()?;
    let (cols, rows) = (geometry.cols(), geometry.rows());

    let host_size = terminal_geometry::ClientHostSize::new(cols, rows);
    let shell_surface_size = shell_config.initial_surface_size(host_size.cols, host_size.rows);
    // Healthy Local attaches directly; only an actual failure enters background recovery.
    let mismatch_guidance = paths
        .server_address()
        .build_mismatch_guidance(&shepr_config::operator_entrypoint());
    let initial = match initial_stream {
        Some(mut stream) => match do_handshake(
            &mut stream,
            handshake::HandshakeGeometry {
                host: geometry,
                surface_size: shell_surface_size,
            },
            loop_config.settings.mouse_capture_active(),
            true,
            None,
        ) {
            Ok(()) => Some(stream),
            Err(error)
                if !local_failure_policy.ends_client_for(&endpoint::ClientEndpointId::Local) =>
            {
                let error = endpoint::handshake_error(error, Some(&mismatch_guidance));
                warn!(%error, "Local handshake failed; keeping configured machines available");
                initial_local_failure =
                    Some(shepr_remote::SshFailureDiagnostic::from_error(&error));
                None
            }
            Err(error) => {
                return Err(ClientRunError::Launch(endpoint::handshake_error(
                    error,
                    Some(&mismatch_guidance),
                )));
            }
        },
        None => None,
    };

    // A shell with configured machines can show connection notices without a server snapshot.
    let (mut terminal_guard, output_writer) =
        setup_terminal(mouse_capture, loop_config.settings.modify_other_keys_mode()).map_err(
            |err| io::Error::new(err.kind(), format!("failed to set up terminal: {err}")),
        )?;
    loop_config.host_escape_disambiguation_active =
        terminal_guard.host_escape_disambiguation_active();
    loop_config.initial_host_input = terminal_guard.take_buffered_host_input();

    // Install a panic hook so the foreground client always restores its terminal.
    let panic_restore = terminal_guard.panic_restore();
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        panic_restore();
        original_hook(info);
    }));

    // Create the tokio runtime.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;

    let should_quit = Arc::new(AtomicBool::new(false));
    let (event_tx, event_rx) =
        tokio::sync::mpsc::channel::<ClientLoopEvent>(CLIENT_EVENT_QUEUE_CAPACITY);

    // ctrlc's "termination" feature also catches SIGTERM/SIGHUP so direct
    // termination signals wake the event loop so it restores the terminal.
    let quit_flag = Arc::clone(&should_quit);
    let quit_event_tx = event_tx.clone();
    if let Err(err) = ctrlc::set_handler(move || {
        quit_flag.store(true, Ordering::Release);
        quit_event_tx.try_send(ClientLoopEvent::Quit).ok();
    }) {
        warn!(error = %err, "failed to install termination handler; terminal restore relies on TerminalGuard::Drop and the panic hook");
    }

    let result = rt.block_on(async {
        run_client_loop(
            initial,
            initial_local_failure,
            machines,
            local_failure_policy,
            geometry,
            should_quit,
            (event_tx, event_rx),
            loop_config,
            shell_config,
            output_writer,
            &terminal_guard,
        )
        .await
    });

    // Restore the terminal before the binary prints any final status message.
    let terminal_restore_failed = terminal_guard.restore().is_err();
    rt.shutdown_timeout(limits::CLIENT_RUNTIME_SHUTDOWN_TIMEOUT);
    shepr_remote::release_ssh_resources_before_exit(limits::SSH_RESOURCE_RELEASE_TIMEOUT);
    crate::logging::shutdown("client");

    let Err(err) = result else {
        return Ok(ClientExit::new(None));
    };
    let graceful_shutdown = matches!(&err, ClientError::ServerShutdown { .. });
    let connection_lost_during_terminal_hangup =
        terminal_restore_failed && matches!(&err, ClientError::ConnectionLost(_));
    let exit = ClientExit::new(Some(err.to_string()));
    if graceful_shutdown || connection_lost_during_terminal_hangup {
        Ok(exit)
    } else {
        Err(ClientRunError::Session(exit))
    }
}

/// The main client event loop.
///
/// Uses a threaded architecture:
/// - stdin reader thread → sends parsed input events
/// - resize poller thread → sends resize events to main loop
/// - server reader thread → reads ServerMessages and sends to main loop
/// - one event channel: host input, resize, endpoint readers, connection supervisors, and quit
/// - main loop: coordinates input, output, and server communication
async fn run_client_loop(
    initial: Option<LocalStream>,
    mut initial_local_failure: Option<shepr_remote::SshFailureDiagnostic>,
    machines: Vec<shepr_config::MachineConfig>,
    local_failure_policy: endpoint::LocalFailurePolicy,
    initial_geometry: shepr_core::geometry::HostGeometry,
    should_quit: Arc<AtomicBool>,
    (event_tx, event_rx): (
        tokio::sync::mpsc::Sender<ClientLoopEvent>,
        tokio::sync::mpsc::Receiver<ClientLoopEvent>,
    ),
    mut config: ClientLoopConfig,
    shell_config: shell::ClientShellConfig,
    output_writer: terminal_setup::HostTerminalWriter,
    terminal_guard: &TerminalGuard,
) -> Result<(), ClientError> {
    let (cols, rows) = (initial_geometry.cols(), initial_geometry.rows());
    let (initial_cell_width_px, initial_cell_height_px, initial_pixel_geometry_exact) = (
        initial_geometry.cell_width(),
        initial_geometry.cell_height(),
        initial_geometry.exact,
    );
    let draw_host_cursor = should_draw_host_cursor(config.settings.host_cursor());
    let mut local_unavailable = initial.is_none();
    let (initial_cell_width_px, initial_cell_height_px, initial_pixel_geometry_exact) =
        terminal_geometry::bounded_cell_geometry(
            initial_cell_width_px,
            initial_cell_height_px,
            initial_pixel_geometry_exact,
        );

    let host_modes = terminal_guard.host_modes();
    host_modes.configure_mouse_mode(HostMouseMode::new(
        config.settings.mouse_capture_active(),
        config.settings.mouse_capture_active(),
    ));
    // client-clock-sample-ok: sample launch time for initial shell and endpoint state.
    let launch_now = std::time::Instant::now();
    let mut state = ClientState {
        blit_encoder: render_ansi::BlitEncoder::new(),
        output_writer: Box::new(output_writer),
        host_modes,
        host_theme_updates: Vec::new(),
        reported_geometry: shepr_core::geometry::HostGeometry::new(
            cols,
            rows,
            initial_cell_width_px,
            initial_cell_height_px,
            initial_pixel_geometry_exact,
        ),
        settings: config.settings,
        shell: Box::new(shell::ClientShellState::new_at(shell_config, launch_now)),
        repaint_pending: false,
        choice: if local_unavailable {
            endpoint::EndpointChoice::waiting_for(endpoint::ClientEndpointId::Local)
        } else {
            endpoint::EndpointChoice::showing(endpoint::ClientEndpointId::Local)
        },
        draw_host_cursor,
        frame_write_failure: HostWriteFailure::default(),
        title_write_failure: HostWriteFailure::default(),
    };
    state.set_host_size(cols, rows);
    state.shell.set_machines(&machines);
    if local_unavailable {
        let status = initial_local_failure.as_ref().map_or(
            endpoint::ClientEndpointStatus::Connecting,
            endpoint::ClientEndpointStatus::after_failure,
        );
        state
            .shell
            .set_endpoint_status(&endpoint::ClientEndpointId::Local, status);
    }
    // Cell size reported by the host terminal, packed as width<<32 | height.
    // Zero means the host has not reported one.
    let reported_cell_size = Arc::new(AtomicCellSize::new());
    let (stdin_mouse_capture_active, stdin_sgr_pixels_active) =
        state.host_modes.mouse_input_mirrors();

    // Channel shared by the host helpers, endpoint readers, supervisors and the signal handler.
    let stdin_tx = event_tx.clone();

    // Arm reply tracking only after the corresponding query was written successfully.
    let host_color_query_sent = query_host_terminal_theme(&mut state.output_writer);
    query_host_terminal_appearance(&mut state.output_writer);
    // Terminals that report no pixel size through the ioctl are asked directly
    // instead of falling back to an assumed cell size.
    let will_query_host_cell_size =
        host_cell_size_query_required() && query_host_cell_size(&mut state.output_writer);

    // Spawn the stdin reader after query writes so a failed write does not make
    // its parser wait for a host reply that cannot arrive.
    let stdin_quit = Arc::clone(&should_quit);
    let stdin_escape_disambiguation_active = config.host_escape_disambiguation_active;
    let stdin_initial_host_input = std::mem::take(&mut config.initial_host_input);
    std::thread::spawn(move || {
        input::stdin_reader_loop(
            &stdin_tx,
            &stdin_quit,
            host_color_query_sent,
            will_query_host_cell_size,
            &stdin_mouse_capture_active,
            &stdin_sgr_pixels_active,
            stdin_escape_disambiguation_active,
            &stdin_initial_host_input,
        );
    });

    // Spawn the resize poller thread.
    let resize_quit = Arc::clone(&should_quit);
    let resize_tx = event_tx.clone();
    let resize_cell_size = Arc::clone(&reported_cell_size);
    std::thread::spawn(move || {
        resize_poll_loop(
            &resize_tx,
            shepr_core::geometry::HostGeometry::new(
                cols,
                rows,
                initial_cell_width_px,
                initial_cell_height_px,
                initial_pixel_geometry_exact,
            ),
            &resize_cell_size,
            &resize_quit,
        );
    });

    let write_stream = if let Some(stream) = initial {
        let surface_decoder = shepr_protocol::surface_reuse::Decoder::default();
        match start_endpoint_transport(
            stream,
            &event_tx,
            endpoint::ClientEndpointId::Local,
            1,
            surface_decoder,
        ) {
            Ok(transport) => {
                let mut registry = endpoint::EndpointRegistry::new_at(transport, 1, launch_now);
                registry.send_to(
                    &endpoint::ClientEndpointId::Local,
                    &ClientMessage::ClientShellFocus { focused: true },
                );
                registry
            }
            Err(error)
                if !local_failure_policy.ends_client_for(&endpoint::ClientEndpointId::Local) =>
            {
                warn!(%error, "Local transport setup failed; keeping configured machines available");
                let diagnostic = errors::endpoint_setup_failure(&error);
                state.shell.set_endpoint_status(
                    &endpoint::ClientEndpointId::Local,
                    endpoint::ClientEndpointStatus::after_failure(&diagnostic),
                );
                state.choice =
                    endpoint::EndpointChoice::waiting_for(endpoint::ClientEndpointId::Local);
                initial_local_failure = Some(diagnostic);
                local_unavailable = true;
                endpoint::EndpointRegistry::empty()
            }
            Err(error) => return Err(error),
        }
    } else {
        endpoint::EndpointRegistry::empty()
    };
    let mut supervisors = endpoint::EndpointSupervisors::new(&config.paths, &machines, launch_now)
        .map_err(ClientError::EndpointSetup)?;
    if local_failure_policy.reconnects_local() {
        let connected_generation = write_stream
            .connection(&endpoint::ClientEndpointId::Local)
            .map(|connection| connection.generation.get());
        // A launch attempt that failed after connecting is recorded as the outcome of
        // generation 1, the way a supervisor Status event would record it, so the retry
        // follows the same backoff instead of starting a redundant attempt at once.
        let seeded_failure = connected_generation
            .is_none()
            .then(|| {
                initial_local_failure
                    .as_ref()
                    .map(endpoint::ClientEndpointStatus::after_failure)
            })
            .flatten();
        let generation = seeded_failure.map_or(connected_generation, |_| Some(1));
        supervisors.add_local(
            config.paths.server_address().socket().to_path_buf(),
            generation,
            launch_now,
        );
        if let Some(status) = seeded_failure {
            supervisors.record_status(&endpoint::ClientEndpointId::Local, 1, status, launch_now);
        }
    }
    if local_unavailable {
        if let Some(failure) = initial_local_failure.as_ref() {
            if failure.needs_attention() {
                warn!(endpoint = "local", error = %failure, "endpoint needs attention");
            }
            present_notice(&mut state, format!("Local: {failure}"));
        } else if let Some(frame) = state.shell.compose(
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
        ) {
            state.present_chrome(frame);
        }
    }
    let mut client_loop = ClientLoop::new(
        state,
        local_failure_policy,
        should_quit,
        write_stream,
        supervisors,
        reported_cell_size,
        event_tx,
        event_rx,
        will_query_host_cell_size,
    );
    client_loop.run().await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClientLoopAction {
    NextEvent,
    Exit,
}

struct ClientLoop {
    state: ClientState,
    local_failure_policy: endpoint::LocalFailurePolicy,
    should_quit: Arc<AtomicBool>,
    write_stream: endpoint::EndpointRegistry,
    supervisors: endpoint::EndpointSupervisors,
    endpoint_commands: endpoint::commands::EndpointCommands,
    next_view_serial: u64,
    client_timer: timer::ClientLoopTimer,
    reported_cell_size: Arc<AtomicCellSize>,
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    event_rx: tokio::sync::mpsc::Receiver<ClientLoopEvent>,
    will_query_host_cell_size: bool,
}

fn earliest_client_timer_deadline(
    deadlines: impl IntoIterator<Item = Option<std::time::Instant>>,
) -> Option<std::time::Instant> {
    deadlines.into_iter().flatten().min()
}

async fn wait_for_client_timer(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

impl ClientLoop {
    /// The one construction `run_client_loop` and the loop tests share. It takes
    /// what the caller has already wired up (the event channel host helpers,
    /// endpoint readers, supervisors and the signal handler share, the cell size,
    /// the endpoints) and starts the loop's own state itself, so a test drives
    /// a loop that begins exactly as production's does.
    fn new(
        state: ClientState,
        local_failure_policy: endpoint::LocalFailurePolicy,
        should_quit: Arc<AtomicBool>,
        write_stream: endpoint::EndpointRegistry,
        supervisors: endpoint::EndpointSupervisors,
        reported_cell_size: Arc<AtomicCellSize>,
        event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
        event_rx: tokio::sync::mpsc::Receiver<ClientLoopEvent>,
        will_query_host_cell_size: bool,
    ) -> Self {
        Self {
            state,
            local_failure_policy,
            should_quit,
            write_stream,
            supervisors,
            endpoint_commands: endpoint::commands::EndpointCommands::default(),
            next_view_serial: 1,
            client_timer: timer::ClientLoopTimer::new(),
            reported_cell_size,
            event_tx,
            event_rx,
            will_query_host_cell_size,
        }
    }

    fn next_timer_deadline(&mut self, now: std::time::Instant) -> Option<std::time::Instant> {
        earliest_client_timer_deadline([
            self.state.shell.next_timer_deadline(),
            self.state.choice.deadline(),
            self.endpoint_commands.next_deadline(),
            self.write_stream.next_service_deadline(now),
            self.supervisors.next_retry_deadline(),
        ])
    }

    /// Returns a quit request at once, else waits for the timer armed from the earliest
    /// pending deadline as of `now` or the shared event queue.
    async fn wait_for_next_event(&mut self, now: std::time::Instant) -> ClientLoopEvent {
        let timer_deadline = self.next_timer_deadline(now).map(|deadline| {
            self.client_timer
                .deadline(now, deadline.saturating_duration_since(now))
        });
        if timer_deadline.is_none() {
            self.client_timer.fired();
        }
        if self.should_quit.load(Ordering::Acquire) {
            return ClientLoopEvent::Quit;
        }

        tokio::select! {
            biased;
            _ = wait_for_client_timer(timer_deadline) => ClientLoopEvent::Timer,
            ev = self.event_rx.recv() => ev.unwrap_or(ClientLoopEvent::Timer),
        }
    }

    async fn run(&mut self) -> Result<(), ClientError> {
        while !self.should_quit.load(Ordering::Acquire) {
            // client-clock-sample-ok: the pre-wait sample for supervisors and timers.
            let loop_now = std::time::Instant::now();
            self.reconcile(loop_now)?;
            let cell = shepr_protocol::ProtocolCellSize::from_host(
                self.state.reported_geometry.cell_width(),
                self.state.reported_geometry.cell_height(),
                self.state.reported_geometry.exact,
            );
            self.supervisors.spawn_due(
                loop_now,
                endpoint::EndpointConnectOptions {
                    geometry: handshake::HandshakeGeometry {
                        host: shepr_core::geometry::HostGeometry::new(
                            self.state.reported_geometry.cols(),
                            self.state.reported_geometry.rows(),
                            cell.width(),
                            cell.height(),
                            cell.exact,
                        ),
                        surface_size: self.state.shell.surface_size(
                            self.state.reported_geometry.cols(),
                            self.state.reported_geometry.rows(),
                        ),
                    },
                    mouse_capture: self.state.host_modes.mouse_shell_preference(),
                },
                &self.event_tx,
            );
            let event = self.wait_for_next_event(loop_now).await;
            // client-clock-sample-ok: sample after waiting for the event to arrive.
            let now = std::time::Instant::now();
            if self.handle_event(event, now)? == ClientLoopAction::Exit {
                if self.should_quit.load(Ordering::Acquire) {
                    break;
                }
                return Ok(());
            }
        }

        // The registry owns the one best-effort Detach and flush during teardown.
        // Terminal restore writes and flushes through its clone of this output descriptor next
        // and logs its own failure, so a failure here would only be reported twice.
        self.state.output_writer.flush().ok();
        Ok(())
    }

    fn handle_event(
        &mut self,
        event: ClientLoopEvent,
        now: std::time::Instant,
    ) -> Result<ClientLoopAction, ClientError> {
        self.state.shell.now = now;
        match event {
            ClientLoopEvent::Quit => Ok(ClientLoopAction::Exit),
            ClientLoopEvent::StdinInput(inputs) => self.handle_stdin_input(inputs, now),
            ClientLoopEvent::TerminalUnavailable(err) => self.handle_terminal_unavailable(&err),
            ClientLoopEvent::Resize(geometry) => self.handle_resize(
                geometry.cols(),
                geometry.rows(),
                geometry.cell_width(),
                geometry.cell_height(),
                geometry.exact,
            ),
            ClientLoopEvent::EndpointSupervisor(event) => {
                self.handle_endpoint_supervisor(event, now)
            }
            ClientLoopEvent::ServerMessage {
                endpoint_id,
                generation,
                message,
            } => self.handle_server_message(&endpoint_id, generation, message, now),
            ClientLoopEvent::ServerDisconnected {
                endpoint_id,
                generation,
                error,
            } => self.handle_server_disconnected(&endpoint_id, generation, &error),
            ClientLoopEvent::Timer => self.handle_timer(now),
        }
    }

    fn handle_stdin_input(
        &mut self,
        inputs: Vec<ParsedHostInput>,
        now: std::time::Instant,
    ) -> Result<ClientLoopAction, ClientError> {
        let Self {
            state,
            write_stream,
            endpoint_commands,
            reported_cell_size,
            will_query_host_cell_size,
            ..
        } = self;
        let raw_events = inputs.iter().map(|input| &input.event);
        if *will_query_host_cell_size
            && let Some((width_px, height_px)) = reported_cell_size_from_events(raw_events)
        {
            store_reported_cell_size(reported_cell_size, width_px, height_px);
        }
        if shepr_termio::input::raw_input::events_require_host_mode_refresh(
            inputs.iter().map(|input| &input.event),
        ) && let Err(err) = state.host_modes.apply_mouse(
            &mut state.output_writer,
            state.reported_geometry.exact,
            true,
        ) {
            warn!(error = %err, "failed to re-assert host mouse capture");
        }
        let host_reports_all_keys = state.host_modes.keyboard_report_all_active();
        let shell = &mut state.shell;
        let outcome = shell.handle_host_input(inputs, host_reports_all_keys, now);
        let frame = outcome
            .repaint
            .then(|| {
                shell.compose(
                    state.reported_geometry.cols(),
                    state.reported_geometry.rows(),
                )
            })
            .flatten();
        if finish_client_shell_input(state, outcome, frame, write_stream, endpoint_commands, now)? {
            return Ok(ClientLoopAction::Exit);
        }
        Ok(ClientLoopAction::NextEvent)
    }

    fn handle_terminal_unavailable(
        &mut self,
        err: &io::Error,
    ) -> Result<ClientLoopAction, ClientError> {
        info!(error = %err, "client terminal unavailable; detaching");
        Ok(ClientLoopAction::Exit)
    }

    fn handle_resize(
        &mut self,
        new_cols: u16,
        new_rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        pixel_geometry_exact: bool,
    ) -> Result<ClientLoopAction, ClientError> {
        let Self {
            state,
            write_stream,
            ..
        } = self;
        let (cell_width_px, cell_height_px, pixel_geometry_exact) =
            terminal_geometry::bounded_cell_geometry(
                cell_width_px,
                cell_height_px,
                pixel_geometry_exact,
            );
        state.reported_geometry = shepr_core::geometry::HostGeometry::new(
            new_cols,
            new_rows,
            cell_width_px,
            cell_height_px,
            pixel_geometry_exact,
        );
        state
            .host_modes
            .apply_mouse(&mut state.output_writer, pixel_geometry_exact, false)
            .map_err(ClientError::HostTerminal)?;
        state.set_host_size(new_cols, new_rows);
        // Resizing invalidates the host-side blit baseline. The retained pane surface
        // stays: until the resized one arrives, `compose` draws it clipped to the new
        // pane area (with pane hits clipped to match) instead of dropping to the
        // machine-list placeholder.
        state.request_repaint();
        resize_views(state, write_stream);
        // The host has already reflowed the old frame; redraw the chrome at the new
        // size now rather than on the next input or surface. The pane cells are still
        // the retained surface (clipped), so this is chrome and presents even while
        // nothing is shown; otherwise the wrongly sized frame would stay up until a move
        // committed.
        if let Some(frame) = state.shell.compose(
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
        ) {
            state.present_chrome(frame);
        }
        Ok(ClientLoopAction::NextEvent)
    }

    fn handle_endpoint_supervisor(
        &mut self,
        event: endpoint::EndpointSupervisorEvent,
        now: std::time::Instant,
    ) -> Result<ClientLoopAction, ClientError> {
        let Self {
            supervisors,
            state,
            write_stream,
            event_tx,
            ..
        } = self;
        match event {
            endpoint::EndpointSupervisorEvent::Status {
                endpoint_id,
                generation,
                status,
                message,
                connector,
            } => {
                supervisors.return_connector(&endpoint_id, generation, connector);
                if !supervisors.record_status(&endpoint_id, generation, status, now) {
                    return Ok(ClientLoopAction::NextEvent);
                }
                if status == endpoint::ClientEndpointStatus::Attention {
                    warn!(endpoint = %endpoint_id.storage_key(), generation, error = %message, "endpoint needs attention");
                }
                let shell = &mut state.shell;
                shell.set_endpoint_status(&endpoint_id, status);
                shell.set_machine_diagnostic(&endpoint_id, &message);
                // Handshake diagnostics carry only the failing phase; the status line supplies
                // the configured endpoint label once.
                let unavailable = (status == endpoint::ClientEndpointStatus::Attention
                    && shell.endpoint_is_active(&endpoint_id))
                .then(|| format!("{}: {message}", shell.endpoint_label(&endpoint_id)));
                if let Some(message) = unavailable {
                    present_notice(state, message);
                } else if let Some(frame) = state.shell.compose(
                    state.reported_geometry.cols(),
                    state.reported_geometry.rows(),
                ) {
                    // A status change is machine-list chrome; it must show even while
                    // nothing is shown.
                    state.present_chrome(frame);
                }
            }
            endpoint::EndpointSupervisorEvent::Connected {
                endpoint_id,
                generation,
                reader,
                writer,
                connector,
            } => {
                supervisors.return_connector(&endpoint_id, generation, connector);
                if !supervisors.record_status(
                    &endpoint_id,
                    generation,
                    endpoint::ClientEndpointStatus::Online,
                    now,
                ) {
                    return Ok(ClientLoopAction::NextEvent);
                }
                let surface_decoder = shepr_protocol::surface_reuse::Decoder::default();
                if let Err(error) = spawn_endpoint_reader(
                    reader,
                    event_tx,
                    &writer,
                    endpoint_id.clone(),
                    generation,
                    surface_decoder,
                ) {
                    let failure = errors::endpoint_setup_failure(&error);
                    let status = endpoint::ClientEndpointStatus::after_failure(&failure);
                    supervisors.record_status(&endpoint_id, generation, status, now);
                    state.shell.set_endpoint_status(&endpoint_id, status);
                    state.shell.set_machine_diagnostic(&endpoint_id, &failure);
                    if status == endpoint::ClientEndpointStatus::Attention
                        && state.shell.endpoint_is_active(&endpoint_id)
                    {
                        let message =
                            format!("{}: {failure}", state.shell.endpoint_label(&endpoint_id));
                        present_notice(state, message);
                    } else if let Some(frame) = state.shell.compose(
                        state.reported_geometry.cols(),
                        state.reported_geometry.rows(),
                    ) {
                        state.present_chrome(frame);
                    }
                    return Ok(ClientLoopAction::NextEvent);
                }
                write_stream.insert_native(endpoint_id.clone(), writer, generation, false, now);
                // The supervisor is Online as soon as the transport connects. Reflect that
                // before composing so this repaint updates the client-owned machine list.
                state
                    .shell
                    .set_endpoint_status(&endpoint_id, endpoint::ClientEndpointStatus::Online);
                let frame = state.shell.compose(
                    state.reported_geometry.cols(),
                    state.reported_geometry.rows(),
                );
                if let Some(frame) = frame {
                    // Connecting changes no pane projection (the connection has no
                    // surface yet), only the machine list.
                    state.present_chrome(frame);
                }
            }
        };
        Ok(ClientLoopAction::NextEvent)
    }

    fn handle_server_message(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: u64,
        message: Box<DecodedServerMessage>,
        now: std::time::Instant,
    ) -> Result<ClientLoopAction, ClientError> {
        let Self {
            write_stream,
            endpoint_commands,
            state,
            local_failure_policy,
            ..
        } = self;
        if !write_stream.accepts(endpoint_id, generation) {
            return Ok(ClientLoopAction::NextEvent);
        }
        write_stream.received(endpoint_id, generation, now);
        let role = state.choice.role(endpoint_id);
        let move_response = match message.as_ref() {
            DecodedServerMessage::Wire(ServerMessage::ClientShellEndpointResponse {
                boot_id,
                request_id,
                ..
            }) => state
                .choice
                .preparing()
                .is_some_and(|p| p.accepts_response(endpoint_id, generation, boot_id, request_id)),
            _ => false,
        };
        let presentation_decision =
            endpoint::PresentationGate::new(role, move_response).decide(message.as_ref());
        if presentation_decision == endpoint::PresentationDecision::Drop {
            return Ok(ClientLoopAction::NextEvent);
        }
        let message = match *message {
            DecodedServerMessage::Wire(message) => message,
            DecodedServerMessage::PaneSurfacePatch(patch) => {
                if presentation_decision == endpoint::PresentationDecision::Buffer {
                    if let Some(pending) = state.choice.preparing_mut() {
                        pending.receive_patch(endpoint_id, generation, &patch);
                    }
                    return Ok(ClientLoopAction::NextEvent);
                }
                let outcome = state
                    .shell
                    .apply_pane_surface_patch_from(&patch, generation);
                let compose_fallback = match outcome {
                    shell::ClientPaneSurfacePatchOutcome::Applied(
                        shell::PatchPresentation::Rows(composed),
                    ) => {
                        match state.present_surface_patch(composed) {
                            Ok(presented) => !presented,
                            Err(error) => {
                                // Once per cause: a patch arrives with every pane update.
                                // The full repaint that follows reports the recovery.
                                let mut context = state.shell.presentation_log_context();
                                if !patch.panes.is_empty() {
                                    context.pane_ids = patch
                                        .panes
                                        .iter()
                                        .map(|pane| pane.pane_id.clone())
                                        .collect();
                                }
                                state.frame_write_failure.observe(
                                    "pane surface patch",
                                    &Err(error),
                                    Some(&context),
                                );
                                state.request_repaint();
                                false
                            }
                        }
                    }
                    shell::ClientPaneSurfacePatchOutcome::Applied(
                        shell::PatchPresentation::Compose,
                    ) => true,
                    shell::ClientPaneSurfacePatchOutcome::Applied(
                        shell::PatchPresentation::Held,
                    ) => false,
                    shell::ClientPaneSurfacePatchOutcome::Rejected(reason) => {
                        // The patch does not follow the shell's baseline, which mirrors the
                        // reader's. Either the two disagree about a baseline both derive from
                        // the same wire, or the server sent a patch the decoder accepts and the
                        // shell does not (a pane geometry change, or a row outside every
                        // patched pane; the decoder checks neither). Both are bugs, and
                        // reconnecting for a fresh full surface baseline is the one response.
                        tracing::error!(
                            endpoint = %endpoint_id.storage_key(),
                            generation,
                            ?reason,
                            "client shell rejected a pane surface patch; failing its connection"
                        );
                        write_stream.fail(
                            endpoint_id,
                            &io::Error::new(
                                io::ErrorKind::InvalidData,
                                "client shell rejected a pane surface patch",
                            ),
                        );
                        false
                    }
                };
                if compose_fallback {
                    let composed = state.shell.compose(
                        state.reported_geometry.cols(),
                        state.reported_geometry.rows(),
                    );
                    if let Some(frame) = composed {
                        state.present_frame(frame);
                    }
                }
                return Ok(ClientLoopAction::NextEvent);
            }
        };
        match message {
            ServerMessage::PaneSurface(surface) => {
                if presentation_decision == endpoint::PresentationDecision::Buffer {
                    let size = state.shell.surface_size(
                        state.reported_geometry.cols(),
                        state.reported_geometry.rows(),
                    );
                    if let Some(pending) = state.choice.preparing_mut() {
                        pending.receive_surface(endpoint_id, generation, surface, size);
                    }
                    return Ok(ClientLoopAction::NextEvent);
                }
                state.shell.receive_pane_surface_from(surface, generation);
                let composed = state.shell.compose(
                    state.reported_geometry.cols(),
                    state.reported_geometry.rows(),
                );
                if let Some(frame) = composed {
                    state.present_frame(frame);
                }
            }
            ServerMessage::ServerShutdown { reason } => {
                if local_failure_policy.ends_client_for(endpoint_id) {
                    return Err(ClientError::ServerShutdown { reason });
                }
                write_stream.fail(
                    endpoint_id,
                    &io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        reason.map_or_else(|| "server stopped".into(), |reason| reason.to_string()),
                    ),
                );
            }
            ServerMessage::ClientShellError { kind } => {
                if state.shell.receive_server_notice(&kind)
                    && let Some(frame) = state.shell.compose(
                        state.reported_geometry.cols(),
                        state.reported_geometry.rows(),
                    )
                {
                    // The error banner is chrome; it must show while nothing is shown,
                    // like machine statuses do.
                    state.present_chrome(frame);
                }
            }
            ServerMessage::ClientShellEndpointResponse {
                boot_id,
                request_id,
                result,
            } => {
                if presentation_decision == endpoint::PresentationDecision::Buffer {
                    if let Some(pending) = state.choice.preparing_mut() {
                        pending.receive_response(
                            endpoint_id,
                            generation,
                            &boot_id,
                            &request_id,
                            result,
                        );
                    }
                    return Ok(ClientLoopAction::NextEvent);
                }
                let completed = endpoint_commands.receive_response(
                    endpoint_id,
                    generation,
                    &boot_id,
                    &request_id,
                    result,
                );
                let Some(completed) = completed else {
                    return Ok(ClientLoopAction::NextEvent);
                };
                // The whole outcome goes through `finish_client_shell_input`: a
                // copy-mode response replays keys queued while it was in flight,
                // and those can carry pane input, a resize or a detach. It also
                // releases the next queued command in this endpoint's lane.
                let shell = &mut state.shell;
                let outcome = if shell.endpoint_is_active(&completed.endpoint_id) {
                    shell.answer_request(
                        &completed.boot_id,
                        &completed.request_id,
                        completed.result,
                        now,
                    )
                } else {
                    shell::ClientShellInput {
                        repaint: shell
                            .drop_request(&completed.request_id, shell::DropReason::Interrupted),
                        ..Default::default()
                    }
                };
                let frame = outcome
                    .repaint
                    .then(|| {
                        shell.compose(
                            state.reported_geometry.cols(),
                            state.reported_geometry.rows(),
                        )
                    })
                    .flatten();
                if finish_client_shell_input(
                    state,
                    outcome,
                    frame,
                    write_stream,
                    endpoint_commands,
                    now,
                )? {
                    return Ok(ClientLoopAction::Exit);
                }
            }
            ServerMessage::Clipboard { data } => {
                // write_clipboard_bytes flushes its own OSC 52 fallback, so no flush is
                // needed here. Once per user copy, so a warn cannot flood; only the
                // base64 length is logged because the payload is the user's selection.
                if let Err(error) = forward_clipboard(
                    &data,
                    state.settings.prefers_osc52_clipboard(),
                    &mut state.output_writer,
                ) {
                    warn!(
                        endpoint = %endpoint_id.storage_key(),
                        generation,
                        encoded_bytes = data.len(),
                        %error,
                        "clipboard copy from the server did not reach the host clipboard"
                    );
                }
            }
            ServerMessage::WindowTitle { title } => {
                // `None` is deliberate from the server (an API title was
                // cleared, or every template token resolved empty) and
                // resets to Shepr's default. A disabled `ui.window_title`
                // never reaches here: the server sends nothing at all.
                // A lost title write is cosmetic and the next title change retries it;
                // logged once per cause because titles can change with every agent state.
                let written = state
                    .host_modes
                    .write_window_title(&mut state.output_writer, title.as_deref());
                state
                    .title_write_failure
                    .observe("window title", &written, None);
            }
            ServerMessage::MouseCapture {
                enabled,
                sgr_pixels,
            } => {
                state
                    .host_modes
                    .set_mouse_endpoint_request(enabled, sgr_pixels);
                state
                    .host_modes
                    .apply_mouse(
                        &mut state.output_writer,
                        state.reported_geometry.exact,
                        false,
                    )
                    .map_err(ClientError::HostTerminal)?;
            }
            ServerMessage::ClientShellKeyboardReportAll { enabled } => {
                let shell_requests_report_all = state.shell.host_keyboard_report_all_requested();
                state
                    .host_modes
                    .set_pane_keyboard_report_all(
                        &mut state.output_writer,
                        enabled,
                        shell_requests_report_all,
                    )
                    .map_err(ClientError::HostTerminal)?;
            }
            ServerMessage::HealthPong | ServerMessage::EndpointWelcome(_) => {
                return Ok(ClientLoopAction::NextEvent);
            }
            ServerMessage::EndpointSnapshot(snapshot) => {
                if let Some(kind) = snapshot.restore_notice.as_ref() {
                    state
                        .shell
                        .receive_restore_notice(endpoint_id, &snapshot.boot_id, kind);
                }
                if role == endpoint::ConnectionRole::Target
                    && let Some(pending) = state.choice.preparing_mut()
                {
                    pending.receive_snapshot(endpoint_id, generation, &snapshot);
                }
                install_client_shell_snapshot(state, endpoint_id, snapshot, role, write_stream)?;
                write_stream.mark_ready(endpoint_id, generation);
            }
            ServerMessage::SurfaceUpdate(_) => {
                tracing::error!(
                    endpoint = %endpoint_id.storage_key(),
                    generation,
                    "surface update reached presentation before decoding; failing its connection"
                );
                write_stream.fail(
                    endpoint_id,
                    &io::Error::new(
                        io::ErrorKind::InvalidData,
                        "protocol error: surface update reached presentation before decoding",
                    ),
                );
                return Ok(ClientLoopAction::NextEvent);
            }
        }
        Ok(ClientLoopAction::NextEvent)
    }

    fn handle_server_disconnected(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: u64,
        error: &io::Error,
    ) -> Result<ClientLoopAction, ClientError> {
        let Self { write_stream, .. } = self;
        if !write_stream.accepts(endpoint_id, generation) {
            return Ok(ClientLoopAction::NextEvent);
        }
        write_stream.fail(endpoint_id, error);
        Ok(ClientLoopAction::NextEvent)
    }

    fn handle_timer(&mut self, now: std::time::Instant) -> Result<ClientLoopAction, ClientError> {
        let Self {
            client_timer,
            write_stream,
            state,
            endpoint_commands,
            ..
        } = self;
        client_timer.fired();
        write_stream.tick_health(now);
        let expired_endpoints = endpoint_commands
            .expire(now)
            .into_iter()
            .filter(|expired| write_stream.accepts(&expired.endpoint_id, expired.generation))
            .collect::<Vec<_>>();
        let shell = &mut state.shell;
        let (outcome, frame) = {
            let mut outcome = shell.tick_selection_autoscroll(now);
            for expired in expired_endpoints {
                if !shell.endpoint_is_active(&expired.endpoint_id) {
                    outcome.repaint |=
                        shell.drop_request(&expired.request_id, shell::DropReason::Interrupted);
                    continue;
                }
                let expired_outcome = shell.answer_request(
                    &expired.boot_id,
                    &expired.request_id,
                    expired.result,
                    now,
                );
                outcome.merge(expired_outcome);
            }
            outcome.repaint |= shell.tick_selection_highlight(now)
                | shell.tick_workspace_highlight(now)
                | shell.tick_endpoint_error(now)
                | shell.tick_transient_banners(now);
            let frame = outcome
                .repaint
                .then(|| {
                    shell.compose(
                        state.reported_geometry.cols(),
                        state.reported_geometry.rows(),
                    )
                })
                .flatten();
            (outcome, frame)
        };
        if finish_client_shell_input(state, outcome, frame, write_stream, endpoint_commands, now)? {
            return Ok(ClientLoopAction::Exit);
        }
        Ok(ClientLoopAction::NextEvent)
    }
}

#[cfg(test)]
use clipboard_forwarding::decode_clipboard_payload;
#[cfg(test)]
use terminal_geometry::{
    cell_size_fallback, current_terminal_geometry_with, ioctl_cell_size, pack_cell_size,
    resize_report_required, write_host_cell_size_query, write_host_terminal_appearance_query,
    write_host_terminal_theme_query,
};
#[cfg(test)]
use terminal_setup::{
    HostModes, effective_sgr_pixel_mouse, write_host_color_scheme_report_mode,
    write_terminal_restore_postlude,
};
#[cfg(test)]
mod tests;

#[cfg(test)]
mod client_timer_tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct TimerTransport(Arc<Mutex<Vec<ClientMessage>>>);

    impl endpoint::EndpointTransport for TimerTransport {
        fn send(&mut self, message: &ClientMessage) -> io::Result<()> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("test precondition: lock poisoned"))?
                .push(message.clone());
            Ok(())
        }

        fn disconnect(&mut self) {}

        fn flush(&mut self, _deadline: Instant) -> io::Result<()> {
            Ok(())
        }

        fn take_error(&mut self) -> Option<io::Error> {
            None
        }
    }

    fn test_client_loop(
        now: Instant,
        write_stream: endpoint::EndpointRegistry,
    ) -> (ClientLoop, tokio::sync::mpsc::Sender<ClientLoopEvent>) {
        use shepr_test_fixtures::ValidatedClientConfigFixture as _;

        let config = shepr_config::ValidatedClientConfig::test_default();
        let supervisors = endpoint::EndpointSupervisors::new(config.paths(), &[], now)
            .expect("test precondition: no configured supervisors");
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(1);
        (
            ClientLoop::new(
                ClientState::test_new(),
                endpoint::LocalFailurePolicy::Reconnect,
                Arc::new(AtomicBool::new(false)),
                write_stream,
                supervisors,
                Arc::new(AtomicCellSize::new()),
                event_tx.clone(),
                event_rx,
                false,
            ),
            event_tx,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn quit_event_wakes_a_deadline_free_loop() {
        let now = tokio::time::Instant::now().into_std();
        let (mut client_loop, event_tx) =
            test_client_loop(now, endpoint::EndpointRegistry::empty());
        // The wait future borrows the loop, so it lives in its own scope and
        // the loop is free again to handle the event it produced.
        let event = {
            let wait = client_loop.wait_for_next_event(now);
            tokio::pin!(wait);
            // Poll once so the loop is parked on its queue before quit arrives.
            assert!(
                tokio::time::timeout(Duration::from_secs(60), &mut wait)
                    .await
                    .is_err(),
                "a deadline-free client loop produced an event"
            );
            assert!(
                event_tx.try_send(ClientLoopEvent::Quit).is_ok(),
                "client event queue has room for quit"
            );
            wait.await
        };
        assert!(matches!(&event, ClientLoopEvent::Quit));
        assert!(matches!(
            client_loop
                .handle_event(event, now)
                .expect("quit event is handled"),
            ClientLoopAction::Exit
        ));
    }

    #[test]
    fn client_timer_uses_the_earliest_reported_deadline() {
        let now = Instant::now();
        let shell_deadline = now + Duration::from_secs(3);
        let health_deadline = now + Duration::from_secs(1);
        let retry_deadline = now + Duration::from_secs(2);

        assert_eq!(earliest_client_timer_deadline([None, None, None]), None);
        assert_eq!(
            earliest_client_timer_deadline([
                Some(shell_deadline),
                Some(health_deadline),
                Some(retry_deadline),
            ]),
            Some(health_deadline)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_loop_deadline_is_handled_once() {
        let now = tokio::time::Instant::now().into_std();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let endpoint_id = endpoint::ClientEndpointId::Ssh(
            shepr_config::MachineLabel::parse("timer-test").expect("test machine label"),
        );
        let mut registry = endpoint::EndpointRegistry::empty();
        registry.insert(
            endpoint_id,
            TimerTransport(Arc::clone(&sent)),
            1,
            false,
            now,
        );
        let (mut client_loop, event_tx) = test_client_loop(now, registry);
        let due_in = client_loop
            .next_timer_deadline(now)
            .expect("the SSH endpoint has a health deadline")
            .saturating_duration_since(now);
        assert!(due_in > Duration::from_millis(1));
        // The paused clock jumps straight to the earliest timer, so a loop
        // that armed nothing, or armed a later deadline, runs out the second
        // timeout, and one that armed an earlier deadline wakes inside the
        // first.
        let event = {
            let wait = client_loop.wait_for_next_event(now);
            tokio::pin!(wait);
            assert!(
                tokio::time::timeout(due_in - Duration::from_millis(1), &mut wait)
                    .await
                    .is_err(),
                "the health deadline fired before its time"
            );
            tokio::time::timeout(Duration::from_millis(2), &mut wait)
                .await
                .expect("the client loop arms its pending deadline")
        };
        assert!(matches!(&event, ClientLoopEvent::Timer));
        let fired_at = tokio::time::Instant::now().into_std();
        client_loop
            .handle_event(event, fired_at)
            .expect("health deadline handling succeeds");
        {
            let sent = sent.lock().expect("test precondition: lock is healthy");
            assert_eq!(sent.len(), 1);
            assert!(matches!(sent.first(), Some(ClientMessage::HealthPing)));
        }

        let next_deadline = client_loop
            .next_timer_deadline(fired_at)
            .expect("the outstanding health probe has a timeout deadline");
        assert!(next_deadline > fired_at);
        event_tx
            .send(ClientLoopEvent::Resize(
                shepr_core::geometry::HostGeometry::new(100, 30, 0, 0, false),
            ))
            .await
            .expect("client event receiver remains open");
        let event = client_loop.wait_for_next_event(fired_at).await;
        assert!(matches!(event, ClientLoopEvent::Resize(_)));
        assert_eq!(
            sent.lock()
                .expect("test precondition: lock is healthy")
                .len(),
            1,
            "servicing the deadline sent one health probe"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_loop_deadline_does_not_fire_a_timer() {
        let now = tokio::time::Instant::now().into_std();
        let (mut client_loop, event_tx) =
            test_client_loop(now, endpoint::EndpointRegistry::empty());
        let wait = client_loop.wait_for_next_event(now);
        tokio::pin!(wait);
        // The paused clock jumps to the earliest timer: any timer the loop
        // armed would wake it before this timeout runs out.
        assert!(
            tokio::time::timeout(Duration::from_secs(60), &mut wait)
                .await
                .is_err(),
            "a deadline-free client loop produced an event"
        );

        event_tx
            .send(ClientLoopEvent::Resize(
                shepr_core::geometry::HostGeometry::new(100, 30, 0, 0, false),
            ))
            .await
            .expect("client event receiver remains open");
        let event = wait.await;
        assert!(matches!(event, ClientLoopEvent::Resize(_)));
    }
}
