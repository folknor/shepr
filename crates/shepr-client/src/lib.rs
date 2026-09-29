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
//! - Forwards OSC 52 clipboard writes from server to its own stdout
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
mod workspace_label;

use clipboard_forwarding::forward_clipboard;
use events::{ClientLoopEvent, ParsedHostInput};
use loop_config::{ClientLoopConfig, ClientSettings};
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
use limits::{CLIENT_EVENT_QUEUE_CAPACITY, ENDPOINT_SUPERVISOR_EVENT_QUEUE_CAPACITY};

use std::io::{self, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use interprocess::TryClone as _;
use interprocess::local_socket::traits::Stream as _;
use tracing::{info, warn};

use shepr_platform::ipc::LocalStream;
use shepr_protocol::{ClientMessage, ServerMessage};
use shepr_termio::blit as render_ansi;

/// Runs the local shell client with startup settings already loaded by the
/// launch coordinator. The binary launcher installs the process-wide file
/// logger before calling this function.
pub fn run_client_with_launch_config(
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
    endpoint_catalog: endpoint::EndpointCatalog,
) -> Result<ClientExit, ClientRunError> {
    run_client_with_launch_state(config, paths, Some(endpoint_catalog))
}

fn run_client_with_launch_state(
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
    initial_catalog: Option<endpoint::EndpointCatalog>,
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
        local_socket_path: socket_path.clone(),
    };

    crate::logging::startup("client");
    info!(path = %socket_path.display(), "connecting to server");

    let endpoint_catalog = match initial_catalog {
        Some(catalog) => catalog,
        None => endpoint::EndpointCatalog::load(paths).map_err(ClientRunError::LaunchCatalog)?,
    };
    let local_failure_policy = endpoint::LocalFailurePolicy::for_catalog(&endpoint_catalog);

    let initial_stream = match shepr_platform::ipc::connect_local_stream(&socket_path) {
        Ok(stream) => Some(stream),
        Err(error) if !local_failure_policy.ends_client_for(&endpoint::ClientEndpointId::Local) => {
            warn!(%error, "Local is unavailable; keeping saved machines available");
            None
        }
        Err(error) => {
            return Err(ClientRunError::Launch(io::Error::other(
                ClientError::ConnectionFailed(error).to_string(),
            )));
        }
    };

    // Get the terminal geometry before handshake (before raw mode).
    let geometry = initial_terminal_geometry()?;
    let (cols, rows) = (geometry.cols(), geometry.rows());

    let host_size = terminal_geometry::ClientHostSize::new(cols, rows);
    let shell_surface_size = shell_config.initial_surface_size(host_size.cols, host_size.rows);
    // Healthy Local attaches directly; only an actual failure enters background recovery.
    let initial = initial_stream
        .map(|mut stream| {
            do_handshake(
                &mut stream,
                geometry,
                shell_surface_size,
                loop_config.settings.mouse_capture_active(),
                true,
                None,
            )
            .map_err(|error| io::Error::other(format!("endpoint local: {error}")))?;
            Ok(stream)
        })
        .transpose();
    let initial = match initial {
        Ok(initial) => initial,
        Err(error) if !local_failure_policy.ends_client_for(&endpoint::ClientEndpointId::Local) => {
            warn!(%error, "Local handshake failed; keeping saved machines available");
            None
        }
        Err(error) => return Err(ClientRunError::Launch(error)),
    };

    // A shell with saved machines can show connection notices without a server snapshot.
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
            endpoint_catalog,
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
    let connection_lost_during_terminal_hangup =
        terminal_restore_failed && matches!(&err, ClientError::ConnectionLost(_));
    let exit = ClientExit::new(Some(err.to_string()));
    if connection_lost_during_terminal_hangup {
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
    endpoint_catalog: endpoint::EndpointCatalog,
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
    let local_unavailable = initial.is_none();
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
        presentation_frozen: false,
        deferred_local_activation: None,
        draw_host_cursor,
        frame_write_failure: HostWriteFailure::default(),
        title_write_failure: HostWriteFailure::default(),
    };
    state.set_host_size(cols, rows);
    let catalog_watch = Some(endpoint::EndpointCatalogWatch::new(
        &config.paths,
        launch_now,
    ));
    let freeze_recovery_attempted = None;
    state.shell.set_endpoint_catalog(&endpoint_catalog.ssh);
    if local_unavailable {
        state.shell.set_endpoint_status(
            &endpoint::ClientEndpointId::Local,
            endpoint::ClientEndpointStatus::Connecting,
        );
    }
    // Cell size reported by the host terminal, packed as width<<32 | height.
    // Zero means the host has not reported one.
    let reported_cell_size = Arc::new(AtomicCellSize::new());
    let (stdin_mouse_capture_active, stdin_sgr_pixels_active) =
        state.host_modes.mouse_input_mirrors();

    // Channel for events from the resize and server reader threads.
    let (event_tx, event_rx) =
        tokio::sync::mpsc::channel::<ClientLoopEvent>(CLIENT_EVENT_QUEUE_CAPACITY);
    let (supervisor_tx, supervisor_rx) = tokio::sync::mpsc::channel::<
        endpoint::EndpointSupervisorEvent,
    >(ENDPOINT_SUPERVISOR_EVENT_QUEUE_CAPACITY);
    let stdin_tx = event_tx.clone();

    let endpoint_commands = endpoint::commands::EndpointCommands::default();

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
        let transport = start_endpoint_transport(
            stream,
            (),
            &event_tx,
            endpoint::ClientEndpointId::Local,
            1,
            surface_decoder,
        )?;
        let mut registry = endpoint::EndpointRegistry::new_at(transport, 1, launch_now);
        registry.send(&ClientMessage::ClientShellFocus { focused: true });
        registry
    } else {
        endpoint::EndpointRegistry::empty()
    };
    let mut supervisors = endpoint::EndpointSupervisors::with_ssh_settings(
        &config.paths,
        &endpoint_catalog.ssh,
        shepr_remote::SavedSshSettings {
            manage_ssh_config: config.settings.manage_ssh_config(),
        },
        launch_now,
    )
    .map_err(ClientError::EndpointSetup)?;
    if local_failure_policy.reconnects_local() {
        supervisors.add_local(
            config.paths.server_address().client_socket().to_path_buf(),
            write_stream
                .connection(&endpoint::ClientEndpointId::Local)
                .map(|connection| connection.generation.get()),
            launch_now,
        );
    }
    if local_unavailable
        && let Some(frame) = state.shell.compose(
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
        )
    {
        state.present_frame(frame);
    }
    let next_surface_serial = 1_u64;
    let pending_activation: Option<endpoint::PendingEndpointActivation> = None;
    let scheduled_activation = None;
    let selection = endpoint::selection::EndpointSelectionTracker::new(&endpoint_catalog);

    let client_timer = timer::ClientLoopTimer::new();
    let mut client_loop = ClientLoop {
        state,
        endpoint_catalog,
        local_failure_policy,
        should_quit,
        config,
        write_stream,
        supervisors,
        endpoint_commands,
        next_surface_serial,
        pending_activation,
        scheduled_activation,
        selection,
        client_timer,
        catalog_watch,
        freeze_recovery_attempted,
        reported_cell_size,
        event_tx,
        event_rx,
        supervisor_tx,
        supervisor_rx,
        will_query_host_cell_size,
    };
    client_loop.run().await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClientLoopAction {
    NextEvent,
    Exit,
}

fn spawn_workspace_label_lookup(
    (id, cwd): (u64, String),
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
) {
    tokio::spawn(async move {
        let label = tokio::task::spawn_blocking(move || {
            crate::workspace_label::derive_label_from_cwd(std::path::Path::new(&cwd))
        })
        .await;
        let Ok(label) = label else {
            tracing::debug!("workspace label lookup stopped; keeping the path-based suggestion");
            return;
        };
        event_tx
            .send(ClientLoopEvent::WorkspaceLabelLookupFinished { id, label })
            .await
            .ok();
    });
}

struct ClientLoop {
    state: ClientState,
    endpoint_catalog: endpoint::EndpointCatalog,
    local_failure_policy: endpoint::LocalFailurePolicy,
    should_quit: Arc<AtomicBool>,
    config: ClientLoopConfig,
    write_stream: endpoint::EndpointRegistry,
    supervisors: endpoint::EndpointSupervisors,
    endpoint_commands: endpoint::commands::EndpointCommands,
    next_surface_serial: u64,
    pending_activation: Option<endpoint::PendingEndpointActivation>,
    scheduled_activation: Option<ClientLoopEvent>,
    selection: endpoint::selection::EndpointSelectionTracker,
    client_timer: timer::ClientLoopTimer,
    catalog_watch: Option<endpoint::EndpointCatalogWatch>,
    freeze_recovery_attempted: Option<(endpoint::ClientEndpointId, u64)>,
    reported_cell_size: Arc<AtomicCellSize>,
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    event_rx: tokio::sync::mpsc::Receiver<ClientLoopEvent>,
    supervisor_tx: tokio::sync::mpsc::Sender<endpoint::EndpointSupervisorEvent>,
    supervisor_rx: tokio::sync::mpsc::Receiver<endpoint::EndpointSupervisorEvent>,
    will_query_host_cell_size: bool,
}

impl ClientLoop {
    async fn run(&mut self) -> Result<(), ClientError> {
        while !self.should_quit.load(Ordering::Acquire) {
            // client-clock-sample-ok: the pre-wait sample for supervisors and timers.
            let loop_now = std::time::Instant::now();
            // Handoffs finish or roll back in many places; judge the requested selection once
            // nothing is in flight, so a rolled-back target neither stays selected nor persists.
            self.selection.settle_and_persist(
                &self.endpoint_catalog,
                self.pending_activation.is_some()
                    || self.state.deferred_local_activation.is_some()
                    || self.scheduled_activation.is_some(),
                self.write_stream.active_id(),
                self.write_stream
                    .connection(self.write_stream.active_id())
                    .is_some_and(|connection| connection.surface_active),
            );
            if self.scheduled_activation.is_none() {
                self.scheduled_activation = stale_freeze_recovery(
                    &self.state,
                    &self.write_stream,
                    &self.selection.selected_endpoint(),
                    self.pending_activation.is_some()
                        || self.state.deferred_local_activation.is_some(),
                    &mut self.freeze_recovery_attempted,
                );
            }
            let cell = shepr_protocol::ProtocolCellSize::from_host(
                self.state.reported_geometry.cell_width(),
                self.state.reported_geometry.cell_height(),
                self.state.reported_geometry.exact,
            );
            self.supervisors.spawn_due(
                loop_now,
                endpoint::EndpointConnectOptions {
                    geometry: shepr_core::geometry::HostGeometry::new(
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
                    mouse_capture: self.state.host_modes.mouse_shell_preference(),
                },
                &self.supervisor_tx,
            );
            let timer_delay = self.state.shell.timer_delay(loop_now);
            let timer_deadline = self.client_timer.deadline(loop_now, timer_delay);
            let event = if let Some(event) = self.scheduled_activation.take() {
                event
            } else {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(timer_deadline.into()) => ClientLoopEvent::Timer,
                    ev = self.supervisor_rx.recv() => ev.map_or(ClientLoopEvent::Timer, ClientLoopEvent::EndpointSupervisor),
                    ev = self.event_rx.recv() => ev.unwrap_or(ClientLoopEvent::Timer),
                }
            };
            // client-clock-sample-ok: sample after waiting for the event to arrive.
            let now = std::time::Instant::now();
            if self.handle_event(event, now)? == ClientLoopAction::Exit {
                return Ok(());
            }
        }

        // Clean exit (Ctrl+C). Send Detach before closing. The registry records a failed
        // send against the endpoint, and its Drop sends Detach to every connection again.
        self.write_stream.send(&ClientMessage::Detach);
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
            ClientLoopEvent::WorkspaceLabelLookupFinished { id, label } => {
                self.handle_workspace_label_lookup_finished(id, label)
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

    fn handle_workspace_label_lookup_finished(
        &mut self,
        id: u64,
        label: String,
    ) -> Result<ClientLoopAction, ClientError> {
        let cols = self.state.reported_geometry.cols();
        let rows = self.state.reported_geometry.rows();
        let shell = &mut self.state.shell;
        let frame = shell
            .apply_workspace_label_lookup(id, label)
            .then(|| shell.compose(cols, rows))
            .flatten();
        if let Some(frame) = frame {
            self.state
                .present_chrome(frame, self.pending_activation.is_some());
        }
        Ok(ClientLoopAction::NextEvent)
    }

    fn handle_stdin_input(
        &mut self,
        inputs: Vec<ParsedHostInput>,
        now: std::time::Instant,
    ) -> Result<ClientLoopAction, ClientError> {
        let Self {
            state,
            write_stream,
            pending_activation,
            endpoint_commands,
            scheduled_activation,
            event_tx,
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
        let label_lookup = shell.take_workspace_label_lookup();
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
            pending_activation,
            endpoint_commands,
            scheduled_activation,
            now,
        )? {
            return Ok(ClientLoopAction::Exit);
        }
        if let Some(request) = label_lookup {
            spawn_workspace_label_lookup(request, event_tx.clone());
        }
        Ok(ClientLoopAction::NextEvent)
    }

    fn handle_terminal_unavailable(
        &mut self,
        err: &io::Error,
    ) -> Result<ClientLoopAction, ClientError> {
        let Self { write_stream, .. } = self;
        info!(error = %err, "client terminal unavailable; detaching");
        // A failed send is recorded against the endpoint, and the registry's Drop sends
        // Detach again on the way out.
        write_stream.send(&ClientMessage::Detach);
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
            pending_activation,
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
        let msg = client_shell_resize_message(
            &state.shell,
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
            cell_width_px,
            cell_height_px,
            pixel_geometry_exact,
        );
        if let Some(activation) = pending_activation.as_mut() {
            if let Err(error) = activation.update_resize_at(&msg, write_stream, now) {
                rollback_endpoint_activation(
                    state,
                    write_stream,
                    pending_activation,
                    &error,
                    false,
                    now,
                );
            }
        } else {
            // A failed send surfaces through the registry's failure list.
            write_stream.send(&msg);
        }
        // The host has already reflowed the old frame; redraw the chrome at the new
        // size now rather than on the next input or surface. The pane cells are still
        // the retained surface (clipped), so this is chrome and passes a freeze left by
        // an unavailable handoff; otherwise the wrongly sized frame would stay up until
        // that freeze ended.
        if let Some(frame) = state.shell.compose(
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
        ) {
            state.present_chrome(frame, pending_activation.is_some());
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
            pending_activation,
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
                    state.present_chrome(frame, pending_activation.is_some());
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
                let reader_quit = writer.stop_handle();
                write_stream.insert(endpoint_id.clone(), writer, generation, false, now);
                if let Some(frame) = frame {
                    // Connecting changes no pane projection (the connection has no
                    // surface yet), only the machine list.
                    state.present_chrome(frame, pending_activation.is_some());
                }
                let surface_decoder = shepr_protocol::surface_reuse::Decoder::default();
                spawn_endpoint_reader(
                    reader,
                    event_tx,
                    &reader_quit,
                    endpoint_id,
                    generation,
                    surface_decoder,
                )?;
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
            endpoint_catalog,
            state,
            endpoint_commands,
            pending_activation,
            next_surface_serial,
            scheduled_activation,
            ..
        } = self;
        let generation = write_stream
            .connection(&endpoint_id)
            .map(|connection| connection.generation.get());
        // Persisting waits for the handoff to commit; see `endpoint::selection`.
        if !selection.begin(endpoint_catalog, &endpoint_id, generation) {
            return Ok(ClientLoopAction::NextEvent);
        }
        begin_endpoint_activation(
            state,
            write_stream,
            endpoint_commands,
            pending_activation,
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
        message: Box<ServerMessage>,
        now: std::time::Instant,
    ) -> Result<ClientLoopAction, ClientError> {
        let Self {
            write_stream,
            pending_activation,
            endpoint_commands,
            state,
            scheduled_activation,
            local_failure_policy,
            selection,
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
        let activation_message = pending_activation
            .as_ref()
            .is_some_and(|pending| pending.accepts_endpoint(endpoint_id, generation));
        let command_response = match message.as_ref() {
            ServerMessage::ClientShellEndpointResponseChunk {
                boot_id,
                request_id,
                ..
            } => endpoint_commands.accepts_response(endpoint_id, generation, boot_id, request_id),
            _ => false,
        };
        let presentation_decision = endpoint::PresentationGate::new(
            endpoint_active,
            activation_message,
            command_response,
            state.presentation_frozen,
        )
        .decide(message.as_ref());
        if presentation_decision == endpoint::PresentationDecision::Drop {
            return Ok(ClientLoopAction::NextEvent);
        }
        match *message {
            ServerMessage::PaneSurface(surface) => {
                if presentation_decision == endpoint::PresentationDecision::Buffer {
                    let progress = pending_activation
                        .as_mut()
                        .map(|pending| pending.receive_surface(endpoint_id, generation, surface));
                    if matches!(progress, Some(endpoint::SurfaceActivationProgress::Ready))
                        && let Some(event) = complete_endpoint_activation(
                            state,
                            write_stream,
                            pending_activation,
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
            ServerMessage::PaneSurfacePatch(patch) => {
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
                    shell::ClientPaneSurfacePatchOutcome::Rejected => false,
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
                if state.shell.receive_endpoint_error(kind.to_string())
                    && let Some(frame) = state.shell.compose(
                        state.reported_geometry.cols(),
                        state.reported_geometry.rows(),
                    )
                {
                    // The error banner is chrome; it must show through an
                    // unavailable-handoff freeze like machine statuses do.
                    state.present_chrome(frame, pending_activation.is_some());
                }
            }
            ServerMessage::ClientShellEndpointResponseChunk {
                boot_id,
                request_id,
                final_chunk,
                data,
            } => {
                if pending_activation.as_ref().is_some_and(|pending| {
                    pending.accepts_response(endpoint_id, generation, &boot_id, &request_id)
                }) {
                    if !final_chunk {
                        rollback_endpoint_activation(
                            state,
                            write_stream,
                            pending_activation,
                            "endpoint returned a chunked activation acknowledgement",
                            false,
                            now,
                        );
                        return Ok(ClientLoopAction::NextEvent);
                    }
                    let progress = pending_activation.as_mut().map(|pending| {
                        pending.receive_response_for_boot_at(
                            endpoint_id,
                            generation,
                            &boot_id,
                            &request_id,
                            &data,
                            write_stream,
                            now,
                        )
                    });
                    match progress {
                        Some(endpoint::SurfaceActivationProgress::Ready) => {
                            if let Some(event) = complete_endpoint_activation(
                                state,
                                write_stream,
                                pending_activation,
                                endpoint_commands,
                                now,
                            )? {
                                *scheduled_activation = Some(event);
                            }
                        }
                        Some(endpoint::SurfaceActivationProgress::Rejected {
                            message,
                            source_release_rejected,
                        }) => {
                            rollback_endpoint_activation(
                                state,
                                write_stream,
                                pending_activation,
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
                let completed = endpoint_commands.receive_chunk(
                    endpoint_id,
                    generation,
                    &boot_id,
                    &request_id,
                    final_chunk,
                    data,
                );
                let Some(completed) = completed else {
                    return Ok(ClientLoopAction::NextEvent);
                };
                // The whole outcome goes through `finish_client_shell_input`: a
                // copy-mode response replays keys queued while it was in flight,
                // and those can carry pane input, a resize or a detach. It also
                // releases the next queued command in this endpoint's lane.
                let shell = &mut state.shell;
                let outcome = if completed.generation == generation
                    && shell.endpoint_is_active(&completed.endpoint_id)
                {
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
                    pending_activation,
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
                let progress = pending_activation.as_mut().map(|activation| {
                    activation.receive_presentation_effects_ready(endpoint_id, generation, &data)
                });
                if matches!(progress, Some(endpoint::SurfaceActivationProgress::Ready))
                    && let Some(event) = complete_endpoint_activation(
                        state,
                        write_stream,
                        pending_activation,
                        endpoint_commands,
                        now,
                    )?
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
                        pending_activation.as_mut().map(|pending| {
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
                ) && let Some(event) = complete_endpoint_activation(
                    state,
                    write_stream,
                    pending_activation,
                    endpoint_commands,
                    now,
                )? {
                    *scheduled_activation = Some(event);
                }
                write_stream.mark_ready(endpoint_id, generation);
                if endpoint_id.is_local()
                    && let Some(event) = take_ready_local_activation(state, write_stream)
                {
                    *scheduled_activation = Some(event);
                    return Ok(ClientLoopAction::NextEvent);
                }
                let selected_endpoint = selection.selected_endpoint();
                let activation_ready = state.shell.endpoint_has_snapshot(&selected_endpoint)
                    && (!write_stream
                        .connection(write_stream.active_id())
                        .is_some_and(|connection| connection.surface_active)
                        || state
                            .shell
                            .endpoint_boot_id(write_stream.active_id())
                            .is_some());
                let selected_connection = write_stream.connection(&selected_endpoint);
                let needs_surface =
                    selected_connection.is_some_and(|connection| !connection.surface_active);
                // A handoff to this connection already failed; retrying it on every
                // snapshot would freeze input and roll back again each time.
                let retry_suppressed = selection.suppresses(
                    &selected_endpoint,
                    selected_connection.map(|connection| connection.generation.get()),
                );
                if activation_ready
                    && needs_surface
                    && !retry_suppressed
                    && pending_activation.is_none()
                    && state.deferred_local_activation.is_none()
                {
                    *scheduled_activation = Some(ClientLoopEvent::ActivateEndpoint {
                        endpoint_id: selected_endpoint,
                        target: None,
                        force: false,
                    });
                }
            }
            ServerMessage::Welcome { .. } => {
                // A protocol violation by this one endpoint. Fail its connection so
                // the supervisor disconnects, reports and reconnects it; the client
                // and its other endpoints keep running.
                warn!(
                    endpoint = %endpoint_id.storage_key(),
                    generation,
                    "endpoint sent a Welcome after its handshake; failing its connection"
                );
                write_stream.fail(
                    endpoint_id,
                    &io::Error::new(
                        io::ErrorKind::InvalidData,
                        "protocol error: Welcome received after the handshake",
                    ),
                );
                return Ok(ClientLoopAction::NextEvent);
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
            pending_activation,
            endpoint_commands,
            supervisors,
            catalog_watch,
            endpoint_catalog,
            config,
            selection,
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
                pending_activation,
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
        if let Some(endpoint_id) = pending_activation
            .as_ref()
            .filter(|activation| activation.expired(now))
            .map(|activation| activation.target().clone())
        {
            let label = state.shell.endpoint_label(&endpoint_id).to_owned();
            rollback_endpoint_activation(
                state,
                write_stream,
                pending_activation,
                &format!("{label} did not produce a coherent surface in time"),
                false,
                now,
            );
        }
        match catalog_watch.as_mut().and_then(|watch| watch.poll(now)) {
            Some(Ok(profiles)) => {
                let active_retired = follow_endpoint_catalog(
                    state,
                    write_stream,
                    endpoint_commands,
                    supervisors,
                    pending_activation,
                    endpoint_catalog,
                    &config.local_socket_path,
                    profiles,
                    now,
                );
                selection.catalog_changed(endpoint_catalog);
                if active_retired {
                    clear_endpoint_host_effects(state)?;
                    if scheduled_activation.is_none() {
                        *scheduled_activation = Some(ClientLoopEvent::ActivateEndpoint {
                            endpoint_id: endpoint::ClientEndpointId::Local,
                            target: None,
                            force: false,
                        });
                    }
                }
                *local_failure_policy = endpoint::LocalFailurePolicy::for_catalog(endpoint_catalog);
                if local_failure_policy.ends_client_for(&endpoint::ClientEndpointId::Local)
                    && write_stream
                        .connection(&endpoint::ClientEndpointId::Local)
                        .is_none()
                {
                    return Err(ClientError::ConnectionLost(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "Local is unavailable and no saved machines remain",
                    )));
                }
            }
            Some(Err(error)) => {
                warn!(%error, "saved SSH endpoint catalog changed but is unusable; keeping the machines already loaded");
            }
            None => {}
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
            pending_activation,
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
