//! Thin client mode - connects to the server's client socket.
//!
//! The client:
//! - Connects to `shepr-client.sock`, checks the build preamble, then sends terminal geometry
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
use shell_runtime::*;
use state::{ClientState, HostWriteFailure, Presentation};
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
use limits::{CLIENT_EVENT_QUEUE_CAPACITY, ENDPOINT_SUPERVISOR_EVENT_QUEUE_CAPACITY};

use std::io::{self, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use interprocess::TryClone as _;
use interprocess::local_socket::traits::Stream as _;
use tracing::{info, warn};

use shepr_platform::ipc::LocalStream;
use shepr_protocol::{ClientMessage, ServerMessage, surface_reuse::DecodedServerMessage};
use shepr_termio::blit as render_ansi;

/// Runs the local shell client with startup settings already loaded by the
/// launch coordinator. The binary launcher installs the process-wide file
/// logger before calling this function. The machines are the launch
/// config's `[[machines]]`, fixed for the life of the client.
fn run_launched_client(
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
) -> Result<ClientExit, ClientRunError> {
    let settings = ClientSettings::resolve(config).map_err(io::Error::from)?;
    let socket_path = paths.server_address().client_socket().to_path_buf();
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

    // ctrlc's "termination" feature also catches SIGTERM/SIGHUP so direct
    // termination signals still run the quit path and TerminalGuard::Drop.
    let quit_flag = Arc::clone(&should_quit);
    if let Err(err) = ctrlc::set_handler(move || {
        quit_flag.store(true, Ordering::Release);
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
/// - main loop: coordinates input, output, and server communication
async fn run_client_loop(
    initial: Option<LocalStream>,
    mut initial_local_failure: Option<shepr_remote::SshFailureDiagnostic>,
    machines: Vec<shepr_config::MachineConfig>,
    local_failure_policy: endpoint::LocalFailurePolicy,
    initial_geometry: shepr_core::geometry::HostGeometry,
    should_quit: Arc<AtomicBool>,
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
        // An unreachable Local owns nothing until a handoff proves an endpoint.
        presentation: if local_unavailable {
            Presentation::Unavailable
        } else {
            Presentation::Owned
        },
        deferred_local: None,
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

    // Channel shared by the stdin, resize and server reader threads.
    let (event_tx, event_rx) =
        tokio::sync::mpsc::channel::<ClientLoopEvent>(CLIENT_EVENT_QUEUE_CAPACITY);
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
                registry.send(&ClientMessage::ClientShellFocus { focused: true });
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
                state.presentation = Presentation::Unavailable;
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
            config.paths.server_address().client_socket().to_path_buf(),
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
            present_handoff_unavailable(&mut state, format!("Local: {failure}"));
        } else if let Some(frame) = state.shell.compose(
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
        ) {
            state.present_chrome_through_freeze(frame);
        }
    }
    let selection = endpoint::selection::EndpointSelectionTracker::new(
        machines
            .iter()
            .map(|machine| machine.label.clone())
            .collect(),
    );

    let mut client_loop = ClientLoop::new(
        state,
        local_failure_policy,
        should_quit,
        write_stream,
        supervisors,
        selection,
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
    next_surface_serial: u64,
    /// An activation to handle before waiting for the next event: a shell pick, a handoff's
    /// successor, a ready deferred Local selection or an automatic activation. Presentation
    /// ownership itself lives in `ClientState::presentation`.
    scheduled_activation: Option<ClientLoopEvent>,
    selection: endpoint::selection::EndpointSelectionTracker,
    client_timer: timer::ClientLoopTimer,
    reported_cell_size: Arc<AtomicCellSize>,
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    event_rx: tokio::sync::mpsc::Receiver<ClientLoopEvent>,
    supervisor_tx: tokio::sync::mpsc::Sender<endpoint::EndpointSupervisorEvent>,
    supervisor_rx: tokio::sync::mpsc::Receiver<endpoint::EndpointSupervisorEvent>,
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
    /// what the caller has already wired up (the event channel the input,
    /// resize and transport threads hold a sender of, the shared cell size,
    /// the endpoints) and starts the loop's own state itself, so a test drives
    /// a loop that begins exactly as production's does.
    fn new(
        state: ClientState,
        local_failure_policy: endpoint::LocalFailurePolicy,
        should_quit: Arc<AtomicBool>,
        write_stream: endpoint::EndpointRegistry,
        supervisors: endpoint::EndpointSupervisors,
        selection: endpoint::selection::EndpointSelectionTracker,
        reported_cell_size: Arc<AtomicCellSize>,
        event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
        event_rx: tokio::sync::mpsc::Receiver<ClientLoopEvent>,
        will_query_host_cell_size: bool,
    ) -> Self {
        let (supervisor_tx, supervisor_rx) = tokio::sync::mpsc::channel::<
            endpoint::EndpointSupervisorEvent,
        >(ENDPOINT_SUPERVISOR_EVENT_QUEUE_CAPACITY);
        Self {
            state,
            local_failure_policy,
            should_quit,
            write_stream,
            supervisors,
            endpoint_commands: endpoint::commands::EndpointCommands::default(),
            next_surface_serial: 1,
            scheduled_activation: None,
            selection,
            client_timer: timer::ClientLoopTimer::new(),
            reported_cell_size,
            event_tx,
            event_rx,
            supervisor_tx,
            supervisor_rx,
            will_query_host_cell_size,
        }
    }

    fn next_timer_deadline(&mut self, now: std::time::Instant) -> Option<std::time::Instant> {
        earliest_client_timer_deadline([
            self.state.shell.next_timer_deadline(),
            self.state
                .presentation
                .handoff()
                .map(endpoint::PendingEndpointActivation::deadline),
            self.endpoint_commands.next_deadline(),
            self.write_stream.next_service_deadline(now),
            self.supervisors.next_retry_deadline(),
        ])
    }

    /// Waits for the loop's next event: a scheduled activation at once, else
    /// the timer armed from the earliest pending deadline as of `now`, a
    /// supervisor event or a client event, whichever comes first.
    async fn wait_for_next_event(&mut self, now: std::time::Instant) -> ClientLoopEvent {
        let timer_deadline = self.next_timer_deadline(now).map(|deadline| {
            self.client_timer
                .deadline(now, deadline.saturating_duration_since(now))
        });
        if timer_deadline.is_none() {
            self.client_timer.fired();
        }
        if let Some(event) = self.scheduled_activation.take() {
            return event;
        }

        tokio::select! {
            biased;
            _ = wait_for_client_timer(timer_deadline) => ClientLoopEvent::Timer,
            ev = self.supervisor_rx.recv() => ev.map_or(ClientLoopEvent::Timer, ClientLoopEvent::EndpointSupervisor),
            ev = self.event_rx.recv() => ev.unwrap_or(ClientLoopEvent::Timer),
        }
    }

    async fn run(&mut self) -> Result<(), ClientError> {
        while !self.should_quit.load(Ordering::Acquire) {
            // client-clock-sample-ok: the pre-wait sample for supervisors and timers.
            let loop_now = std::time::Instant::now();
            // Handoffs finish or roll back in many places; judge the requested selection once
            // nothing is in flight, so a rolled-back target does not stay selected, and only
            // then decide whether to start one automatically.
            let handoff_busy =
                self.state.presentation.handoff_in_flight() || self.state.deferred_local.is_some();
            self.selection.settle(
                handoff_busy || self.scheduled_activation.is_some(),
                self.write_stream.active_id(),
                active_endpoint_owns_presentation(&self.state.presentation, &self.write_stream),
            );
            if !handoff_busy && self.scheduled_activation.is_none() {
                self.scheduled_activation =
                    automatic_activation(&self.state, &self.write_stream, &self.selection);
            }
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
                &self.supervisor_tx,
            );
            let event = self.wait_for_next_event(loop_now).await;
            // client-clock-sample-ok: sample after waiting for the event to arrive.
            let now = std::time::Instant::now();
            if self.handle_event(event, now)? == ClientLoopAction::Exit {
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
            ClientLoopEvent::StdinInput(inputs) => self.handle_stdin_input(inputs, now),
            ClientLoopEvent::TerminalUnavailable(err) => self.handle_terminal_unavailable(&err),
            ClientLoopEvent::Resize(geometry) => self.handle_resize(
                geometry.cols(),
                geometry.rows(),
                geometry.cell_width(),
                geometry.cell_height(),
                geometry.exact,
                now,
            ),
            ClientLoopEvent::EndpointSupervisor(event) => {
                self.handle_endpoint_supervisor(event, now)
            }
            ClientLoopEvent::ActivateEndpoint {
                endpoint_id,
                target,
                force,
            } => self.handle_activate_endpoint(endpoint_id, target, force, now),
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
            scheduled_activation,
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
        if finish_client_shell_input(
            state,
            outcome,
            frame,
            write_stream,
            endpoint_commands,
            scheduled_activation,
            now,
        )? {
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
        now: std::time::Instant,
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
        if !resize_handoff(state, write_stream, now) {
            let msg = client_shell_resize_message(
                &state.shell,
                state.reported_geometry.cols(),
                state.reported_geometry.rows(),
                cell_width_px,
                cell_height_px,
                pixel_geometry_exact,
            );
            // A failed send surfaces through the registry's failure list.
            write_stream.send(&msg);
        }
        // The host has already reflowed the old frame; redraw the chrome at the new
        // size now rather than on the next input or surface. The pane cells are still
        // the retained surface (clipped), so this is chrome and passes the freeze while
        // nothing owns the presentation; otherwise the wrongly sized frame would stay up
        // until a handoff ended it.
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
                    present_handoff_unavailable(state, message);
                } else if let Some(frame) = state.shell.compose(
                    state.reported_geometry.cols(),
                    state.reported_geometry.rows(),
                ) {
                    // A status change is machine-list chrome; it must show even while
                    // no endpoint owns presentation.
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
                let frame = state.shell.compose(
                    state.reported_geometry.cols(),
                    state.reported_geometry.rows(),
                );
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
                        present_handoff_unavailable(state, message);
                    } else if let Some(frame) = state.shell.compose(
                        state.reported_geometry.cols(),
                        state.reported_geometry.rows(),
                    ) {
                        state.present_chrome(frame);
                    }
                    return Ok(ClientLoopAction::NextEvent);
                }
                write_stream.insert_native(endpoint_id.clone(), writer, generation, false, now);
                if let Some(frame) = frame {
                    // Connecting changes no pane projection (the connection has no
                    // surface yet), only the machine list.
                    state.present_chrome(frame);
                }
            }
        };
        Ok(ClientLoopAction::NextEvent)
    }

    fn handle_activate_endpoint(
        &mut self,
        endpoint_id: endpoint::ClientEndpointId,
        target: Option<shell::ClientEndpointFocusTarget>,
        force: bool,
        now: std::time::Instant,
    ) -> Result<ClientLoopAction, ClientError> {
        let Self {
            write_stream,
            selection,
            state,
            endpoint_commands,
            next_surface_serial,
            scheduled_activation,
            ..
        } = self;
        let generation = write_stream
            .connection(&endpoint_id)
            .map(|connection| connection.generation.get());
        // A failed handoff rolls the selection back; see `endpoint::selection`.
        if !selection.begin(&endpoint_id, generation) {
            return Ok(ClientLoopAction::NextEvent);
        }
        begin_endpoint_activation(
            state,
            write_stream,
            endpoint_commands,
            next_surface_serial,
            endpoint_id,
            target,
            force,
            now,
            scheduled_activation,
        )?;
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
            scheduled_activation,
            local_failure_policy,
            ..
        } = self;
        if !write_stream.accepts(endpoint_id, generation) {
            return Ok(ClientLoopAction::NextEvent);
        }
        write_stream.received(endpoint_id, generation, now);
        let endpoint_active = write_stream.active_id() == endpoint_id
            && write_stream
                .connection(endpoint_id)
                .is_some_and(|connection| connection.surface_active);
        let activation_message = state
            .presentation
            .handoff()
            .is_some_and(|pending| pending.accepts_endpoint(endpoint_id, generation));
        let buffer_surface_evidence = state
            .presentation
            .handoff()
            .is_some_and(|pending| pending.buffers_surface_evidence_for(endpoint_id, generation));
        let command_response = match message.as_ref() {
            DecodedServerMessage::Wire(ServerMessage::ClientShellEndpointResponse {
                boot_id,
                request_id,
                ..
            }) => endpoint_commands.accepts_response(endpoint_id, generation, boot_id, request_id),
            _ => false,
        };
        let presentation_decision = endpoint::PresentationGate::new(
            endpoint_active,
            state.presentation.owned(),
            activation_message,
            buffer_surface_evidence,
            command_response,
            state.presentation.frames_frozen(),
        )
        .decide(message.as_ref());
        if presentation_decision == endpoint::PresentationDecision::Drop {
            return Ok(ClientLoopAction::NextEvent);
        }
        let message = match *message {
            DecodedServerMessage::Wire(message) => message,
            DecodedServerMessage::PaneSurfacePatch(patch) => {
                if presentation_decision == endpoint::PresentationDecision::Buffer {
                    let progress = state
                        .presentation
                        .handoff_mut()
                        .map(|pending| pending.receive_patch(endpoint_id, generation, &patch));
                    if matches!(progress, Some(endpoint::SurfaceActivationProgress::Ready))
                        && let Some(event) = complete_endpoint_activation(
                            state,
                            write_stream,
                            endpoint_commands,
                            now,
                        )?
                    {
                        *scheduled_activation = Some(event);
                    }
                    return Ok(ClientLoopAction::NextEvent);
                }
                let outcome = state.shell.apply_pane_surface_patch(&patch);
                let compose_fallback = match outcome {
                    shell::ClientPaneSurfacePatchOutcome::Applied(Some(composed)) => {
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
                    shell::ClientPaneSurfacePatchOutcome::Applied(None) => true,
                    shell::ClientPaneSurfacePatchOutcome::Rejected => {
                        // The reader already accepted this patch against its connection
                        // baseline. The shell can reject it after presentation filtering has
                        // advanced that baseline without its display; no repaint request can
                        // repair the gap, so reconnect for a fresh full surface baseline.
                        tracing::error!(
                            endpoint = %endpoint_id.storage_key(),
                            generation,
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
                    let progress = state
                        .presentation
                        .handoff_mut()
                        .map(|pending| pending.receive_surface(endpoint_id, generation, surface));
                    if matches!(progress, Some(endpoint::SurfaceActivationProgress::Ready))
                        && let Some(event) = complete_endpoint_activation(
                            state,
                            write_stream,
                            endpoint_commands,
                            now,
                        )?
                    {
                        *scheduled_activation = Some(event);
                    }
                    return Ok(ClientLoopAction::NextEvent);
                }
                state.shell.set_pane_surface(surface);
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
                if state.shell.receive_server_notice(endpoint_id, &kind)
                    && let Some(frame) = state.shell.compose(
                        state.reported_geometry.cols(),
                        state.reported_geometry.rows(),
                    )
                {
                    // The error banner is chrome; it must show while nothing owns the
                    // presentation, like machine statuses do.
                    state.present_chrome(frame);
                }
            }
            ServerMessage::ClientShellEndpointResponse {
                boot_id,
                request_id,
                result,
            } => {
                if let Some(pending) = state.presentation.handoff_mut().filter(|pending| {
                    pending.accepts_response(endpoint_id, generation, &boot_id, &request_id)
                }) {
                    let progress = pending.receive_response_for_boot_at(
                        endpoint_id,
                        generation,
                        &boot_id,
                        &request_id,
                        result,
                        write_stream,
                        now,
                    );
                    match progress {
                        endpoint::SurfaceActivationProgress::Ready => {
                            if let Some(event) = complete_endpoint_activation(
                                state,
                                write_stream,
                                endpoint_commands,
                                now,
                            )? {
                                *scheduled_activation = Some(event);
                            }
                        }
                        endpoint::SurfaceActivationProgress::Rejected {
                            message,
                            source_release_rejected,
                        } => {
                            rollback_endpoint_activation(
                                state,
                                write_stream,
                                &message,
                                source_release_rejected,
                                now,
                            );
                        }
                        _ => {}
                    }
                    return Ok(ClientLoopAction::NextEvent);
                }
                if endpoint_commands.response_kind(endpoint_id, generation, &boot_id, &request_id)
                    == endpoint::commands::CommandResponseKind::Untracked
                {
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
                    shell.handle_endpoint_result_at(
                        &completed.boot_id,
                        &completed.request_id,
                        completed.result,
                        now,
                    )
                } else {
                    shell::ClientShellInput {
                        repaint: shell.cancel_endpoint_request(&completed.request_id),
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
                    scheduled_activation,
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
            ServerMessage::PresentationReady(data) => {
                let progress = state.presentation.handoff_mut().map(|activation| {
                    activation.receive_presentation_effects_ready(endpoint_id, generation, &data)
                });
                if matches!(progress, Some(endpoint::SurfaceActivationProgress::Ready))
                    && let Some(event) =
                        complete_endpoint_activation(state, write_stream, endpoint_commands, now)?
                {
                    *scheduled_activation = Some(event);
                }
                return Ok(ClientLoopAction::NextEvent);
            }
            ServerMessage::HealthPong | ServerMessage::EndpointWelcome(_) => {
                return Ok(ClientLoopAction::NextEvent);
            }
            ServerMessage::EndpointSnapshot(snapshot) => {
                let projection_pending = activation_message;
                let activation_progress = activation_message
                    .then(|| {
                        state.presentation.handoff_mut().map(|pending| {
                            pending.receive_snapshot(endpoint_id, generation, &snapshot)
                        })
                    })
                    .flatten();
                install_client_shell_snapshot(
                    state,
                    endpoint_id,
                    snapshot,
                    projection_pending,
                    write_stream,
                )?;
                if matches!(
                    activation_progress,
                    Some(endpoint::SurfaceActivationProgress::Ready)
                ) && let Some(event) =
                    complete_endpoint_activation(state, write_stream, endpoint_commands, now)?
                {
                    *scheduled_activation = Some(event);
                }
                write_stream.mark_ready(endpoint_id, generation);
                // A snapshot that brings the selected endpoint's metadata makes it eligible
                // for the automatic activation the loop judges before its next wait.
                if endpoint_id.is_local()
                    && let Some(event) = take_ready_local_activation(state, write_stream)
                {
                    *scheduled_activation = Some(event);
                }
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
            local_failure_policy,
            state,
            endpoint_commands,
            supervisors,
            scheduled_activation,
            ..
        } = self;
        client_timer.fired();
        write_stream.tick_health(now);
        for failure in write_stream.take_failures() {
            // `record_failure` removes the connection as it queues this failure, so `None` is
            // expected for its generation. Retirement follows this batch and purges queued
            // failures; handle_server_disconnected rejects later events without a live match.
            if write_stream.connection(&failure.endpoint_id).is_some()
                && !write_stream.accepts(&failure.endpoint_id, failure.generation)
            {
                continue;
            }
            warn!(
                endpoint = %failure.endpoint_id.storage_key(),
                error = %failure.message,
                "endpoint transport failed"
            );
            if local_failure_policy.ends_client_for(&failure.endpoint_id) {
                return Err(ClientError::ConnectionLost(io::Error::new(
                    failure.kind,
                    failure.message,
                )));
            }
            if handle_endpoint_disconnect(
                state,
                write_stream,
                endpoint_commands,
                supervisors,
                &failure.endpoint_id,
                failure.generation,
                now,
                &format!("{}; reconnecting", failure.message),
            ) {
                clear_endpoint_host_effects(state)?;
            }
        }
        // A revoked transport changes the safe rollback destination. Handle those
        // failures before applying a timeout to the remaining activation phase.
        if let Some(endpoint_id) = state
            .presentation
            .handoff()
            .filter(|activation| activation.expired(now))
            .map(|activation| activation.target().clone())
        {
            let error = format!(
                "{} did not produce a coherent surface in time",
                state.shell.endpoint_label(&endpoint_id)
            );
            rollback_endpoint_activation(state, write_stream, &error, false, now);
        }
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
                    outcome.repaint |= shell.cancel_endpoint_request(&expired.request_id);
                    continue;
                }
                let expired_outcome = shell.handle_endpoint_result_at(
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
        if finish_client_shell_input(
            state,
            outcome,
            frame,
            write_stream,
            endpoint_commands,
            scheduled_activation,
            now,
        )? {
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
        use shepr_test_fixtures::ValidatedConfigFixture as _;

        let config = shepr_config::ValidatedConfig::test_default();
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
                endpoint::selection::EndpointSelectionTracker::new(Vec::new()),
                Arc::new(AtomicCellSize::new()),
                event_tx.clone(),
                event_rx,
                false,
            ),
            event_tx,
        )
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
